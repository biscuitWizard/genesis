//! Revision history and rollback.
//!
//! Every build that passes validation becomes an immutable revision: the
//! component plus a complete snapshot of the source it came from. Nothing is
//! ever overwritten or deleted, and revision 1 of each slot is pinned, so there
//! is always a floor to fall back to.
//!
//! Each activation also records a *system snapshot* — the full map of which
//! revision every slot was running — which is what makes "roll the whole system
//! back to how it was ten minutes ago" a single operation.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::slot::Slot;
use crate::store::{now_ms, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Built and validated, not yet activated.
    Candidate,
    /// Currently running.
    Active,
    /// Ran successfully before; the target a rollback aims for.
    KnownGood,
    /// Was active, then rolled away from.
    RolledBack,
    /// Failed badly enough that the watchdog took it out of service.
    Disabled,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Candidate => "candidate",
            Status::Active => "active",
            Status::KnownGood => "known-good",
            Status::RolledBack => "rolled-back",
            Status::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// The first revision built at startup.
    Bootstrap,
    /// A human edited the source on disk.
    HumanEdit,
    /// The agent rewrote it through the dev kit.
    AgentMod,
    /// Produced by restoring an earlier revision.
    Rollback,
}

impl Origin {
    pub fn label(&self) -> &'static str {
        match self {
            Origin::Bootstrap => "bootstrap",
            Origin::HumanEdit => "human-edit",
            Origin::AgentMod => "agent-mod",
            Origin::Rollback => "rollback",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionRow {
    pub slot: String,
    pub revision: u64,
    pub status: Status,
    pub origin: Origin,
    pub note: String,
    pub created_ms: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub id: u64,
    pub created_ms: u64,
    pub cause: String,
    /// slot key -> active revision
    pub slots: BTreeMap<String, u64>,
}

pub struct Revisions {
    cfg: Arc<Config>,
    db: Arc<Store>,
}

impl Revisions {
    pub fn new(cfg: Arc<Config>, db: Arc<Store>) -> Self {
        Self { cfg, db }
    }

    // --- artifact layout ---------------------------------------------------

    pub fn artifact_dir(&self, slot: &Slot, revision: u64) -> PathBuf {
        self.cfg.slot_artifact_dir(slot, revision)
    }

    pub fn component_path(&self, slot: &Slot, revision: u64) -> PathBuf {
        self.artifact_dir(slot, revision).join("component.wasm")
    }

    fn source_dir(&self, slot: &Slot, revision: u64) -> PathBuf {
        self.artifact_dir(slot, revision).join("src-snapshot")
    }

    // --- recording ---------------------------------------------------------

    /// Fingerprint of a freshly built component, for comparison against what is
    /// already recorded.
    pub fn fingerprint(&self, wasm: &Path) -> Option<String> {
        hash_file(wasm).ok()
    }

    /// Freezes a successful build into a new revision.
    ///
    /// The component *and* its source are copied, which is what lets a rollback
    /// restore a working tree the agent can keep editing rather than just a
    /// binary it cannot read.
    pub fn record(
        &self,
        slot: &Slot,
        wasm: &Path,
        origin: Origin,
        note: &str,
    ) -> Result<RevisionRow> {
        let revision = self.db.next_revision(&slot.key())?;
        let dir = self.artifact_dir(slot, revision);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating artifact dir {}", dir.display()))?;

        let component = self.component_path(slot, revision);
        std::fs::copy(wasm, &component)
            .with_context(|| format!("copying component to {}", component.display()))?;

        let src = self.cfg.slot_source_dir(slot);
        if src.is_dir() {
            copy_tree(&src, &self.source_dir(slot, revision))
                .with_context(|| format!("snapshotting source of {slot}"))?;
        }

        let row = RevisionRow {
            slot: slot.key(),
            revision,
            status: Status::Candidate,
            origin,
            note: note.to_string(),
            created_ms: now_ms(),
            hash: hash_file(&component).unwrap_or_default(),
        };
        self.db.put_revision(&slot.key(), revision, &row)?;
        Ok(row)
    }

    /// Promotes a revision to active, demoting whatever was active before to
    /// known-good, then records a system snapshot.
    pub fn activate(&self, slot: &Slot, revision: u64, cause: &str) -> Result<()> {
        let key = slot.key();
        let rows: Vec<RevisionRow> = self.db.list_revisions(&key)?;

        for mut row in rows {
            let new_status = if row.revision == revision {
                Status::Active
            } else if row.status == Status::Active {
                // The version we are replacing has actually run, so it is the
                // best rollback target available.
                Status::KnownGood
            } else {
                continue;
            };
            row.status = new_status;
            self.db.put_revision(&key, row.revision, &row)?;
        }

        self.snapshot(cause)?;
        Ok(())
    }

    pub fn mark(&self, slot: &Slot, revision: u64, status: Status) -> Result<()> {
        let key = slot.key();
        let mut row: RevisionRow = self
            .db
            .get_revision(&key, revision)?
            .ok_or_else(|| anyhow!("{key} has no revision {revision}"))?;
        row.status = status;
        self.db.put_revision(&key, revision, &row)?;
        Ok(())
    }

    // --- queries -----------------------------------------------------------

    pub fn history(&self, slot: &Slot) -> Result<Vec<RevisionRow>> {
        self.db.list_revisions(&slot.key())
    }

    pub fn active(&self, slot: &Slot) -> Result<Option<RevisionRow>> {
        Ok(self
            .history(slot)?
            .into_iter()
            .find(|r| r.status == Status::Active))
    }

    /// The revision a rollback should target when none is named: the most
    /// recent one that is known to have worked.
    pub fn last_known_good(&self, slot: &Slot) -> Result<Option<RevisionRow>> {
        let history = self.history(slot)?;
        Ok(history
            .iter()
            .rev()
            .find(|r| r.status == Status::KnownGood)
            .or_else(|| {
                // Nothing has been demoted yet; fall back to the pinned genesis
                // revision, which is never deleted.
                history.first().filter(|r| r.status != Status::Active)
            })
            .cloned())
    }

    pub fn slots_with_history(&self) -> Result<Vec<Slot>> {
        let mut slots = Vec::new();
        for candidate in self.known_slot_keys()? {
            if let Ok(slot) = Slot::parse(&candidate) {
                slots.push(slot);
            }
        }
        Ok(slots)
    }

    /// Slot keys are discovered from the artifacts directory so history is
    /// visible even for slots that failed to load this boot.
    fn known_slot_keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let root = &self.cfg.paths.artifacts;
        if !root.is_dir() {
            return Ok(keys);
        }
        if root.join("agent").is_dir() {
            keys.push("agent".to_string());
        }
        for (dir, prefix) in [("gateways", "gateway"), ("tools", "tool")] {
            let path = root.join(dir);
            if !path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(path)?.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        keys.push(format!("{prefix}/{name}"));
                    }
                }
            }
        }
        keys.sort();
        Ok(keys)
    }

    // --- snapshots ---------------------------------------------------------

    pub fn snapshot(&self, cause: &str) -> Result<u64> {
        let mut slots = BTreeMap::new();
        for slot in self.slots_with_history()? {
            if let Some(active) = self.active(&slot)? {
                slots.insert(slot.key(), active.revision);
            }
        }

        let id = self.db.next_snapshot_id()?;
        self.db.put_snapshot(
            id,
            &SystemSnapshot {
                id,
                created_ms: now_ms(),
                cause: cause.to_string(),
                slots,
            },
        )?;
        Ok(id)
    }

    pub fn snapshots(&self) -> Result<Vec<SystemSnapshot>> {
        self.db.list_snapshots()
    }

    pub fn snapshot_by_id(&self, id: u64) -> Result<Option<SystemSnapshot>> {
        self.db.get_snapshot(id)
    }

    // --- restoring ---------------------------------------------------------

    /// Puts the source tree back to how it looked at `revision`.
    ///
    /// Restoring source alongside the binary is what keeps the two from
    /// drifting: after a rollback the agent reads the same code that is running.
    pub fn restore_source(&self, slot: &Slot, revision: u64) -> Result<()> {
        let snapshot = self.source_dir(slot, revision);
        if !snapshot.is_dir() {
            return Err(anyhow!(
                "{slot} r{revision} has no source snapshot to restore"
            ));
        }
        let dest = self.cfg.slot_source_dir(slot);
        replace_tree(&snapshot, &dest)
            .with_context(|| format!("restoring source of {slot} to r{revision}"))
    }
}

// --- filesystem helpers -----------------------------------------------------

/// Recursively copies a crate directory, skipping build output.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Build artifacts are reproducible and enormous; never snapshot them.
        if name_str == "target" || name_str == ".git" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {}", src.display()))?;
            // Windows' CopyFileEx carries the source timestamps across. On a
            // restore that would hand cargo a file older than its last build,
            // so it would skip recompiling and the stale binary would survive
            // the rollback. Stamp every copy as new.
            touch(&dst)?;
        }
    }
    Ok(())
}

/// Replaces the contents of `dest` with `from`, leaving `target/` alone so a
/// rollback does not throw away the incremental build cache.
fn replace_tree(from: &Path, dest: &Path) -> Result<()> {
    if dest.is_dir() {
        for entry in std::fs::read_dir(dest)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "target" || name_str == ".git" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
    }
    copy_tree(from, dest)
}

/// Marks a file as modified now, so build tools notice it changed.
fn touch(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to update its timestamp", path.display()))?;
    file.set_modified(std::time::SystemTime::now())
        .with_context(|| format!("touching {}", path.display()))?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    Ok(hex::encode(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture() -> (Revisions, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("wit")).unwrap();
        std::fs::write(root.join("wit/genesis.wit"), "package genesis:harness;").unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.root = root.clone();
        cfg.paths.data = root.join("data");
        cfg.paths.artifacts = root.join("artifacts");
        cfg.paths.agent = root.join("agents/agent-core");
        cfg.paths.gateways = root.join("gateways");
        cfg.paths.tools = root.join("tools");

        let db = Arc::new(Store::open(&cfg.db_path()).unwrap());
        (Revisions::new(Arc::new(cfg), db), dir)
    }

    /// Builds a fake component + source tree so `record` has something to copy.
    fn stage_build(revs: &Revisions, slot: &Slot, contents: &str) -> PathBuf {
        let src = revs.cfg.slot_source_dir(slot);
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("Cargo.toml"), "[package]\nname='x'").unwrap();
        std::fs::write(src.join("src/lib.rs"), contents).unwrap();

        let wasm = revs.cfg.root.join("build-output.wasm");
        std::fs::write(&wasm, contents.as_bytes()).unwrap();
        wasm
    }

    #[test]
    fn revisions_increment_and_never_reuse_numbers() {
        let (revs, _d) = fixture();
        let slot = Slot::Agent;

        let wasm = stage_build(&revs, &slot, "v1");
        let r1 = revs.record(&slot, &wasm, Origin::Bootstrap, "first").unwrap();
        let r2 = revs.record(&slot, &wasm, Origin::AgentMod, "second").unwrap();
        assert_eq!((r1.revision, r2.revision), (1, 2));

        revs.activate(&slot, 2, "test").unwrap();
        let r3 = revs.record(&slot, &wasm, Origin::Rollback, "third").unwrap();
        assert_eq!(r3.revision, 3, "numbers keep climbing across activations");
    }

    #[test]
    fn activation_demotes_the_previous_version_to_known_good() {
        let (revs, _d) = fixture();
        let slot = Slot::Agent;
        let wasm = stage_build(&revs, &slot, "v1");

        revs.record(&slot, &wasm, Origin::Bootstrap, "r1").unwrap();
        revs.activate(&slot, 1, "boot").unwrap();
        revs.record(&slot, &wasm, Origin::AgentMod, "r2").unwrap();
        revs.activate(&slot, 2, "self-mod").unwrap();

        let history = revs.history(&slot).unwrap();
        assert_eq!(history[0].status, Status::KnownGood);
        assert_eq!(history[1].status, Status::Active);
        assert_eq!(revs.active(&slot).unwrap().unwrap().revision, 2);
        assert_eq!(revs.last_known_good(&slot).unwrap().unwrap().revision, 1);
    }

    #[test]
    fn source_snapshots_round_trip_through_rollback() {
        let (revs, _d) = fixture();
        let slot = Slot::Agent;

        let wasm = stage_build(&revs, &slot, "the original code");
        revs.record(&slot, &wasm, Origin::Bootstrap, "r1").unwrap();

        // The agent rewrites itself, badly.
        let src = revs.cfg.slot_source_dir(&slot);
        std::fs::write(src.join("src/lib.rs"), "broken code").unwrap();
        std::fs::write(src.join("src/extra.rs"), "leftover").unwrap();

        revs.restore_source(&slot, 1).unwrap();

        let restored = std::fs::read_to_string(src.join("src/lib.rs")).unwrap();
        assert_eq!(restored, "the original code");
        assert!(
            !src.join("src/extra.rs").exists(),
            "files added after the snapshot must not survive a restore"
        );
    }

    #[test]
    fn restored_files_look_newly_modified_to_the_build_tool() {
        let (revs, _d) = fixture();
        let slot = Slot::Agent;

        let wasm = stage_build(&revs, &slot, "good code");
        revs.record(&slot, &wasm, Origin::Bootstrap, "r1").unwrap();

        // Simulate an edit, then wait long enough that timestamps differ.
        let src = revs.cfg.slot_source_dir(&slot);
        std::fs::write(src.join("src/lib.rs"), "bad code").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let before_restore = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(20));

        revs.restore_source(&slot, 1).unwrap();

        // If the snapshot's original timestamps came across (as Windows'
        // CopyFileEx does by default), cargo would consider the crate fresh and
        // keep serving the binary built from the bad code.
        let modified = std::fs::metadata(src.join("src/lib.rs"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            modified > before_restore,
            "restored source must look newer than the last build"
        );
    }

    #[test]
    fn system_snapshots_capture_every_active_slot() {
        let (revs, _d) = fixture();
        let agent = Slot::Agent;
        let gw = Slot::gateway("web");

        let w1 = stage_build(&revs, &agent, "agent");
        revs.record(&agent, &w1, Origin::Bootstrap, "").unwrap();
        revs.activate(&agent, 1, "boot").unwrap();

        let w2 = stage_build(&revs, &gw, "gateway");
        revs.record(&gw, &w2, Origin::Bootstrap, "").unwrap();
        let id = {
            revs.activate(&gw, 1, "boot").unwrap();
            revs.snapshots().unwrap().last().unwrap().id
        };

        let snap = revs.snapshot_by_id(id).unwrap().unwrap();
        assert_eq!(snap.slots.get("agent"), Some(&1));
        assert_eq!(snap.slots.get("gateway/web"), Some(&1));
    }

    #[test]
    fn snapshots_track_the_change_over_time() {
        let (revs, _d) = fixture();
        let slot = Slot::Agent;
        let wasm = stage_build(&revs, &slot, "v");

        revs.record(&slot, &wasm, Origin::Bootstrap, "").unwrap();
        revs.activate(&slot, 1, "boot").unwrap();
        revs.record(&slot, &wasm, Origin::AgentMod, "").unwrap();
        revs.activate(&slot, 2, "self-mod").unwrap();

        let snaps = revs.snapshots().unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].slots.get("agent"), Some(&1));
        assert_eq!(snaps[1].slots.get("agent"), Some(&2));
        // An earlier snapshot is exactly what "roll the system back" targets.
        assert_eq!(snaps[0].cause, "boot");
    }
}
