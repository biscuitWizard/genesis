use anyhow::Result;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use genesis::config::Config;
use genesis::harness::Harness;
use genesis::loader::Loader;
use genesis::pipeline;
use genesis::revisions::Origin;
use genesis::runtime::Runtime;
use genesis::slot::Slot;
use genesis::{watchdog, watcher, web};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GENESIS_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,genesis=debug")),
        )
        .with_target(false)
        .init();

    genesis::control::mark_start();

    let cfg = Arc::new(Config::load()?);
    tracing::info!(root = %cfg.root.display(), "starting genesis");

    let runtime = Runtime::new(cfg.clone())?;
    let harness = Harness::new(cfg.clone(), runtime)?;

    // Bring every slot up. A slot that will not start leaves the rest of the
    // system running: the gateway falls back to a host-rendered page and
    // /admin stays available for a manual rollback.
    for slot in discover_slots(&harness) {
        if let Err(e) = bring_up(&harness, &slot).await {
            tracing::error!(%slot, error = %e, "slot failed to start");
        }
    }

    // A turn cut short by a restart or a crash left the log mid-sentence.
    // Repair it, then carry those turns on: the agent is stateless between
    // turns, so resuming is just running one again against a log that now
    // records what happened.
    resume_interrupted_turns(&harness);

    if harness.db.list_sessions(true)?.is_empty() {
        let first = harness
            .db
            .create_session(Some("Welcome".into()), &cfg.default_mode)?;
        tracing::info!(session = %first.id, "created first session");
    }

    // The agent creates tools here. Make sure it exists before the watcher
    // starts, or newly scaffolded tools would not be watched until a restart.
    if let Err(e) = std::fs::create_dir_all(&cfg.paths.tools) {
        tracing::warn!(error = %e, "could not create the tools directory");
    }

    // Held for the process lifetime: dropping this stops hot reload.
    let _watch = match watcher::spawn(harness.clone()) {
        Ok(handle) => {
            tracing::info!("hot reload active");
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %e, "hot reload unavailable");
            None
        }
    };
    watchdog::spawn_prober(harness.clone());

    tracing::info!("open http://{} in a browser", cfg.bind_addr);
    web::serve(harness).await
}

/// Repairs and restarts turns that were interrupted.
///
/// Resuming is deferred briefly so the web layer is serving first: the events a
/// resumed turn produces are persisted either way, but a browser reconnecting
/// into a live renderer sees them stream rather than only on its next reload.
fn resume_interrupted_turns(harness: &Arc<Harness>) {
    let interrupted = match harness.db.reconcile_interrupted_turns(
        "This turn was interrupted when Genesis restarted. Carry on from where you \
         left off; anything you were part-way through may need doing again.",
    ) {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "could not reconcile interrupted turns");
            return;
        }
    };

    if interrupted.is_empty() {
        return;
    }

    let resuming: Vec<String> = interrupted
        .iter()
        .filter(|i| i.resume)
        .map(|i| i.session_id.clone())
        .collect();

    tracing::info!(
        interrupted = interrupted.len(),
        resuming = resuming.len(),
        "reconciled turns cut short by the last shutdown"
    );

    if resuming.is_empty() {
        return;
    }

    let harness = harness.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        for session_id in resuming {
            tracing::info!(session = %session_id, "resuming");
            harness.sessions.resume(&harness, &session_id);
        }
    });
}

/// The agent, plus every gateway and tool with a crate in the configured
/// directories.
fn discover_slots(harness: &Arc<Harness>) -> Vec<Slot> {
    let cfg = &harness.cfg;
    let mut slots = vec![Slot::Agent];

    // Concrete wrappers: the generic constructors cannot coerce to a fn pointer.
    fn gateway(name: &str) -> Slot {
        Slot::Gateway(name.to_string())
    }
    fn tool(name: &str) -> Slot {
        Slot::Tool(name.to_string())
    }

    let sources: [(&std::path::Path, &str, fn(&str) -> Slot); 2] = [
        (&cfg.paths.gateways, &cfg.paths.gateway_prefix, gateway),
        (&cfg.paths.tools, &cfg.paths.tool_prefix, tool),
    ];

    for (dir, prefix, make) in sources {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().join("Cargo.toml").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // The prefix is a naming convention, not part of the slot's name.
            if let Some(short) = name.strip_prefix(prefix) {
                slots.push(make(short));
            }
        }
    }

    // The gateway comes up first so the UI is reachable as early as possible.
    slots.sort_by_key(|s| match s {
        Slot::Gateway(_) => 0,
        Slot::Agent => 1,
        Slot::Tool(_) => 2,
    });
    slots
}

async fn bring_up(harness: &Arc<Harness>, slot: &Slot) -> Result<()> {
    // Reuse the last known-good artifact when the source has not changed, so a
    // restart is fast and does not depend on the toolchain being present.
    if let Some(active) = harness.revisions.active(slot)? {
        let artifact = harness.revisions.component_path(slot, active.revision);
        if artifact.is_file() {
            match Loader::compile(&harness.runtime.engine, slot, active.revision, &artifact) {
                Ok(component) => {
                    harness.install_component(component).await;
                    tracing::info!(%slot, revision = active.revision, "restored active revision");
                    // Still rebuild in the background: if the source moved on
                    // while we were down, the change lands without a restart.
                    let harness = harness.clone();
                    let slot = slot.clone();
                    tokio::spawn(async move {
                        match pipeline::build_and_activate(
                            &harness,
                            &slot,
                            Origin::HumanEdit,
                            "startup rebuild",
                        )
                        .await
                        {
                            Ok(outcome) if outcome.success && outcome.detail.is_empty() => {
                                tracing::info!(
                                    %slot,
                                    revision = outcome.revision.unwrap_or(0),
                                    "startup rebuild picked up newer source"
                                );
                            }
                            Ok(outcome) if outcome.success => {
                                tracing::debug!(%slot, detail = %outcome.detail, "startup rebuild");
                            }
                            // Not fatal — the restored revision is still serving
                            // — but silence here would hide a broken tree.
                            Ok(outcome) => {
                                tracing::warn!(
                                    %slot,
                                    detail = %outcome.detail,
                                    "startup rebuild rejected; still running the stored revision"
                                );
                                if !outcome.stderr.is_empty() {
                                    tracing::warn!(%slot, "\n{}", outcome.stderr);
                                }
                            }
                            Err(e) => {
                                tracing::error!(%slot, error = %e, "startup rebuild failed");
                            }
                        }
                    });
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(%slot, error = %e, "stored artifact would not load; rebuilding");
                }
            }
        }
    }

    tracing::info!(%slot, "building");
    let outcome = pipeline::build_and_activate(harness, slot, Origin::Bootstrap, "startup").await?;

    if !outcome.success {
        // Last resort: fall back to any earlier revision that still works.
        if let Ok(message) = pipeline::rollback_slot(harness, slot, None).await {
            tracing::warn!(%slot, "{message} (current source does not build)");
            anyhow::bail!("{}\n{}", outcome.detail, outcome.stderr);
        }
        anyhow::bail!("{}\n{}", outcome.detail, outcome.stderr);
    }

    tracing::info!(
        %slot,
        revision = outcome.revision.unwrap_or(0),
        took_ms = outcome.duration_ms,
        "loaded"
    );
    Ok(())
}
