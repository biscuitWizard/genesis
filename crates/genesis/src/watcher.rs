//! Hot reload.
//!
//! Watches the guest source trees and pushes anything that changes through the
//! same pipeline the agent's self-modification uses. Editing a file is
//! therefore exactly as safe as the agent editing itself: a broken edit is
//! caught by the gates and the running system is untouched.

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;


use crate::config::Config;
use crate::harness::Harness;
use crate::pipeline;
use crate::revisions::Origin;
use crate::slot::Slot;

pub type WatchHandle = Debouncer<notify::RecommendedWatcher, RecommendedCache>;

/// Starts watching. The returned handle must be kept alive: dropping it stops
/// the watcher.
pub fn spawn(harness: Arc<Harness>) -> Result<WatchHandle> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Slot>();
    let cfg = harness.cfg.clone();

    let debounce = harness.cfg.watchdog.debounce;
    let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
        let Ok(events) = result else { return };
        let mut slots = HashSet::new();
        for event in events {
            for path in &event.paths {
                slots.extend(slots_for_path(&cfg, path));
            }
        }
        for slot in slots {
            let _ = tx.send(slot);
        }
    })
    .context("creating file watcher")?;

    for path in harness.cfg.watched_dirs() {
        if path.is_dir() {
            debouncer
                .watch(&path, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", path.display()))?;
            tracing::debug!(dir = %path.display(), "watching for changes");
        }
    }

    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            // Coalesce everything already queued so a multi-file save triggers
            // one build per slot, not one per file.
            let mut pending: HashSet<Slot> = HashSet::from([first]);
            while let Ok(more) = rx.try_recv() {
                pending.insert(more);
            }

            for slot in pending {
                rebuild(&harness, &slot).await;
            }
        }
    });

    Ok(debouncer)
}

async fn rebuild(harness: &Arc<Harness>, slot: &Slot) {
    if harness.watch_suppressed(slot) {
        tracing::debug!(%slot, "ignoring change: the orchestrator wrote this itself");
        return;
    }
    tracing::info!(%slot, "source changed, rebuilding");

    match pipeline::build_and_activate(harness, slot, Origin::HumanEdit, "file changed on disk")
        .await
    {
        Ok(outcome) if outcome.success => {
            tracing::info!(
                %slot,
                revision = outcome.revision.unwrap_or(0),
                took_ms = outcome.duration_ms,
                "hot swapped"
            );
        }
        Ok(outcome) => {
            // Deliberately not fatal: the previous revision is still serving.
            tracing::warn!(%slot, detail = %outcome.detail, "rebuild rejected");
            if !outcome.stderr.is_empty() {
                tracing::warn!(%slot, "\n{}", outcome.stderr);
            }
        }
        Err(e) => tracing::error!(%slot, error = %e, "rebuild pipeline failed"),
    }
}

/// Which slots a changed path affects.
///
/// A change under `wit/` alters the contract every guest is compiled against,
/// so it rebuilds all of them.
fn slots_for_path(cfg: &Config, path: &Path) -> Vec<Slot> {
    // Build output and version control churn are not source changes.
    if path.components().any(|c| {
        let part = c.as_os_str().to_string_lossy();
        part == "target" || part == ".git" || part.ends_with(".lock")
    }) {
        return Vec::new();
    }

    // A change to the contract recompiles every guest against it.
    if path.starts_with(&cfg.paths.wit) {
        return all_source_slots(cfg);
    }
    if path.starts_with(&cfg.paths.agent) {
        return vec![Slot::Agent];
    }

    for (root, prefix, make) in source_roots(cfg) {
        if let Ok(relative) = path.strip_prefix(root) {
            let Some(dir) = relative.components().next() else {
                continue;
            };
            let name = dir.as_os_str().to_string_lossy();
            if let Some(short) = name.strip_prefix(prefix.as_str()) {
                return vec![make(short)];
            }
        }
    }

    Vec::new()
}

/// Where gateways and tools live, with the naming convention each follows.
fn source_roots(cfg: &Config) -> [(&Path, &String, fn(&str) -> Slot); 2] {
    // Concrete wrappers: the generic constructors cannot coerce to a fn pointer.
    fn gateway(name: &str) -> Slot {
        Slot::Gateway(name.to_string())
    }
    fn tool(name: &str) -> Slot {
        Slot::Tool(name.to_string())
    }

    [
        (cfg.paths.gateways.as_path(), &cfg.paths.gateway_prefix, gateway),
        (cfg.paths.tools.as_path(), &cfg.paths.tool_prefix, tool),
    ]
}

/// Every slot that currently has a source tree on disk.
fn all_source_slots(cfg: &Config) -> Vec<Slot> {
    let mut slots = Vec::new();
    if cfg.paths.agent.join("Cargo.toml").is_file() {
        slots.push(Slot::Agent);
    }

    for (root, prefix, make) in source_roots(cfg) {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().join("Cargo.toml").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(short) = name.strip_prefix(prefix.as_str()) {
                slots.push(make(short));
            }
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> Config {
        let root = Path::new("C:/proj");
        let mut cfg = Config::load().unwrap();
        cfg.root = root.to_path_buf();
        cfg.paths.wit = root.join("wit");
        cfg.paths.agent = root.join("agents/agent-core");
        cfg.paths.gateways = root.join("gateways");
        cfg.paths.tools = root.join("tools");
        cfg
    }

    #[test]
    fn maps_source_paths_to_their_slots() {
        let cfg = test_cfg();
        assert_eq!(
            slots_for_path(&cfg, Path::new("C:/proj/agents/agent-core/src/lib.rs")),
            vec![Slot::Agent]
        );
        assert_eq!(
            slots_for_path(&cfg, Path::new("C:/proj/gateways/gateway-web/src/ui/app.js")),
            vec![Slot::gateway("web")]
        );
        assert_eq!(
            slots_for_path(&cfg, Path::new("C:/proj/tools/weather/src/lib.rs")),
            vec![Slot::tool("weather")]
        );
    }

    #[test]
    fn ignores_build_output_and_unrelated_paths() {
        let cfg = test_cfg();
        for path in [
            "C:/proj/target-wasm/wasm32-wasip2/release/agent_core.wasm",
            "C:/proj/agents/agent-core/target/debug/x.rlib",
            "C:/proj/data/genesis.redb",
            "C:/elsewhere/agents/agent-core/src/lib.rs",
        ] {
            assert!(
                slots_for_path(&cfg, Path::new(path)).is_empty(),
                "should have ignored {path}"
            );
        }
    }
}
