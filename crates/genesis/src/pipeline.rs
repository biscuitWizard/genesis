//! The one path a change takes to reach the running system.
//!
//! build -> record a revision -> validate -> activate -> swap
//!
//! Human edits (via the file watcher) and the agent's own self-modification
//! both go through here, so both get identical guarantees: a candidate that
//! fails any gate is recorded and set aside, and whatever was already running
//! keeps running.
//!
//! Swapping is safe at any moment because guests are instantiated per call. A
//! turn already in flight holds its own `Arc` to the old component and finishes
//! on it; the next call picks up the new one.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Instant;
use wasmtime::component::Component;

use crate::builder::BuildOptions;
use crate::harness::Harness;
use crate::loader::Loader;
use crate::revisions::{Origin, Status};
use crate::runtime::{Budget, Caps};
use crate::slot::Slot;

/// The result of pushing a change through the pipeline, shaped so it can be
/// handed straight back to the model as a compile report.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub success: bool,
    pub slot: String,
    pub revision: Option<u64>,
    pub stderr: String,
    pub duration_ms: u64,
    pub detail: String,
    pub pending_swap: bool,
}

impl Outcome {
    fn failure(slot: &Slot, detail: impl Into<String>, stderr: String, started: Instant) -> Self {
        Self {
            success: false,
            slot: slot.key(),
            revision: None,
            stderr,
            duration_ms: started.elapsed().as_millis() as u64,
            detail: detail.into(),
            pending_swap: false,
        }
    }
}

/// Builds a slot's current source and, if every gate passes, puts it live.
pub async fn build_and_activate(
    harness: &Arc<Harness>,
    slot: &Slot,
    origin: Origin,
    note: &str,
) -> Result<Outcome> {
    build_and_activate_with(harness, slot, origin, note, BuildOptions::default()).await
}

/// As [`build_and_activate`], with control over how cargo is invoked.
pub async fn build_and_activate_with(
    harness: &Arc<Harness>,
    slot: &Slot,
    origin: Origin,
    note: &str,
    opts: BuildOptions,
) -> Result<Outcome> {
    let started = Instant::now();

    // A build already running for this slot will pick up the same source tree,
    // so waiting for the lock only to repeat it is wasted work.
    let Some(_in_flight) = harness.begin_build(slot) else {
        return Ok(Outcome::failure(
            slot,
            "a build for this slot is already running; its result will cover this change too",
            String::new(),
            started,
        ));
    };

    // 1. Compile.
    let build = harness.builder.build_with(&harness.cfg, slot, opts).await?;
    if !build.success {
        return Ok(Outcome::failure(
            slot,
            "compilation failed; the running revision is unchanged",
            build.stderr,
            started,
        ));
    }
    let wasm = build
        .wasm_path
        .context("build reported success without an artifact")?;

    // 2. If cargo produced a byte-identical component, nothing actually
    //    changed. Recording it again would spend a revision on nothing — and,
    //    worse, a cargo run that decided the crate was fresh would quietly
    //    stamp a stale binary as a new revision.
    if let (Some(fresh), Ok(Some(active))) = (
        harness.revisions.fingerprint(&wasm),
        harness.revisions.active(slot),
    ) {
        // "Identical" only means "nothing to do" if that revision is also the
        // one actually in service. The registry can say a revision is active
        // while the loader holds nothing — a fresh boot whose artifacts were
        // rebuilt, for instance — and short-circuiting there would report
        // success while leaving the slot empty.
        let already_serving = harness
            .loader
            .get(slot)
            .is_some_and(|loaded| loaded.revision == active.revision);

        if fresh == active.hash && already_serving {
            return Ok(Outcome {
                success: true,
                slot: slot.key(),
                revision: Some(active.revision),
                stderr: build.stderr,
                duration_ms: started.elapsed().as_millis() as u64,
                detail: format!(
                    "no change: the build is identical to r{:04}",
                    active.revision
                ),
                pending_swap: false,
            });
        }
    }

    // 3. Freeze it as an immutable revision before touching anything live.
    let row = harness.revisions.record(slot, &wasm, origin, note)?;
    let artifact = harness.revisions.component_path(slot, row.revision);

    // 4. Does wasmtime accept it as a component for this world?
    let component = match Loader::compile(&harness.runtime.engine, slot, row.revision, &artifact) {
        Ok(c) => c,
        Err(e) => {
            harness
                .revisions
                .mark(slot, row.revision, Status::Disabled)?;
            return Ok(Outcome::failure(
                slot,
                format!("r{} is not a valid component: {e:#}", row.revision),
                build.stderr,
                started,
            ));
        }
    };

    // 5. Does it actually run?
    if let Err(e) = smoke_test(harness, slot, &component.component).await {
        {
            harness
                .revisions
                .mark(slot, row.revision, Status::Disabled)?;
            return Ok(Outcome::failure(
                slot,
                format!(
                    "r{} compiled but failed its smoke test: {e:#}",
                    row.revision
                ),
                build.stderr,
                started,
            ));
        }
    }

    // 6. Live. Installing through the harness keeps the tool registry in step.
    harness.install_component(component).await;
    harness
        .revisions
        .activate(slot, row.revision, &format!("{}: {note}", origin.label()))?;

    Ok(Outcome {
        success: true,
        slot: slot.key(),
        revision: Some(row.revision),
        stderr: build.stderr,
        duration_ms: started.elapsed().as_millis() as u64,
        detail: String::new(),
        // A turn in flight finishes on the old code, so from the agent's point
        // of view its own changes land on the next turn.
        pending_swap: matches!(slot, Slot::Agent),
    })
}

/// Exercises a candidate's exports before it is allowed to serve traffic.
///
/// This is what stops a self-modification from making the system unreachable:
/// a component that traps, hangs, or is missing an export never becomes active.
async fn smoke_test(harness: &Arc<Harness>, slot: &Slot, component: &Component) -> Result<()> {
    let caps = match slot {
        Slot::Agent => Caps::Agent,
        Slot::Gateway(_) => Caps::Gateway,
        Slot::Tool(_) => Caps::Tool,
    };
    let budget = Budget::probe(format!("{slot} smoke test"), harness.cfg.probe_budget);
    let mut store = harness
        .runtime
        .new_store(harness.clone(), caps, budget, None);
    let linker = harness.runtime.linker(caps);

    match slot {
        Slot::Agent => {
            let agent = crate::bindings::agent::Agent::instantiate_async(
                &mut store, component, linker,
            )
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating")?;

            let health = agent
                .call_health(&mut store)
                .await
                .map_err(anyhow::Error::from)
                .context("health probe")?;
            if health.trim().is_empty() {
                anyhow::bail!("health probe returned nothing");
            }
            agent
                .call_describe(&mut store)
                .await
                .map_err(anyhow::Error::from)
                .context("describe")?;
        }

        Slot::Gateway(_) => {
            let gw = crate::bindings::gateway::Gateway::instantiate_async(
                &mut store, component, linker,
            )
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating")?;

            // A gateway that cannot serve its own index page would leave the
            // user with a blank screen, so that is the gate.
            let index = gw
                .call_serve_asset(&mut store, "/")
                .await
                .map_err(anyhow::Error::from)
                .context("serve-asset")?;
            match index {
                Some(asset) if !asset.bytes.is_empty() => {}
                _ => anyhow::bail!("serve-asset(\"/\") returned no page"),
            }

            gw.call_on_client_message(&mut store, "smoke-test", r#"{"type":"list"}"#)
                .await
                .map_err(anyhow::Error::from)
                .context("on-client-message")?;
        }

        Slot::Tool(name) => {
            let tool =
                crate::bindings::tool::Tool::instantiate_async(&mut store, component, linker)
                    .await
                    .map_err(anyhow::Error::from)
                    .context("instantiating")?;

            let manifest = tool
                .call_describe(&mut store)
                .await
                .map_err(anyhow::Error::from)
                .context("describe")?;
            if manifest.name.trim().is_empty() {
                anyhow::bail!("tool manifest has no name");
            }
            // A mismatch here would make the tool uncallable: the model would
            // be told one name and the registry keyed by another.
            if &manifest.name != name {
                anyhow::bail!(
                    "tool manifest says '{}' but the slot is '{name}'",
                    manifest.name
                );
            }
            serde_json::from_str::<serde_json::Value>(&manifest.args_schema_json)
                .context("argument schema is not valid JSON")?;
        }
    }

    Ok(())
}

/// Restores a slot to an earlier revision: source tree, component, and registry
/// all move together so nothing drifts.
pub async fn rollback_slot(
    harness: &Arc<Harness>,
    slot: &Slot,
    revision: Option<u64>,
) -> Result<String> {
    let target = match revision {
        Some(r) => harness
            .revisions
            .history(slot)?
            .into_iter()
            .find(|row| row.revision == r)
            .with_context(|| format!("{slot} has no revision {r}"))?,
        None => harness
            .revisions
            .last_known_good(slot)?
            .with_context(|| format!("{slot} has no earlier revision to fall back to"))?,
    };

    let artifact = harness.revisions.component_path(slot, target.revision);
    let component = Loader::compile(&harness.runtime.engine, slot, target.revision, &artifact)
        .with_context(|| format!("reloading {slot} r{}", target.revision))?;

    smoke_test(harness, slot, &component.component)
        .await
        .with_context(|| format!("{slot} r{} failed its smoke test", target.revision))?;

    // Restoring the source tree looks exactly like a human edit to the file
    // watcher. Mute it first, or it would rebuild over the rollback we are
    // performing — and if the rollback was the breaker's doing, that would put
    // the broken revision straight back into service.
    harness.suppress_watch(slot, harness.cfg.watchdog.watch_suppression);

    // Source first: if this fails we have not yet changed what is running.
    if let Err(e) = harness.revisions.restore_source(slot, target.revision) {
        tracing::warn!(%slot, error = %e, "component restored without its source snapshot");
    }

    let previous = harness.revisions.active(slot)?.map(|r| r.revision);
    harness.install_component(component).await;
    harness.revisions.activate(
        slot,
        target.revision,
        &format!("rollback to r{}", target.revision),
    )?;
    if let Some(prev) = previous {
        if prev != target.revision {
            harness.revisions.mark(slot, prev, Status::RolledBack)?;
        }
    }

    Ok(format!(
        "{slot} rolled back to r{:04}{}",
        target.revision,
        previous
            .map(|p| format!(" (from r{p:04})"))
            .unwrap_or_default()
    ))
}

/// Restores every slot to the revisions recorded in a system snapshot.
pub async fn rollback_system(harness: &Arc<Harness>, snapshot_id: u64) -> Result<String> {
    let snapshot = harness
        .revisions
        .snapshot_by_id(snapshot_id)?
        .with_context(|| format!("no system snapshot {snapshot_id}"))?;

    let mut restored = Vec::new();
    let mut failed = Vec::new();

    for (slot_key, revision) in &snapshot.slots {
        let Ok(slot) = Slot::parse(slot_key) else {
            continue;
        };
        match rollback_slot(harness, &slot, Some(*revision)).await {
            Ok(_) => restored.push(format!("{slot_key}→r{revision:04}")),
            Err(e) => failed.push(format!("{slot_key}: {e:#}")),
        }
    }

    harness
        .revisions
        .snapshot(&format!("system rollback to snapshot {snapshot_id}"))?;

    if failed.is_empty() {
        Ok(format!(
            "system restored to snapshot {snapshot_id}: {}",
            restored.join(", ")
        ))
    } else {
        Ok(format!(
            "system partially restored to snapshot {snapshot_id}: {} restored; failures: {}",
            restored.join(", "),
            failed.join("; ")
        ))
    }
}
