//! Filesystem access on the host.
//!
//! Distinct from `sandbox`, which runs code in a container: these operations
//! touch the machine the orchestrator runs on, so the whole interface is off
//! unless configuration turns it on.
//!
//! The real boundary is the configured roots: every path is resolved and must
//! land inside one of them, checked after symlinks are followed so a link
//! cannot be used to step outside. The `protected` list is a smaller thing —
//! it stops the system from deleting its own database by accident, and is not
//! a security control, because a terminal session can reach those paths anyway.

use anyhow::{anyhow, Result};
use std::path::{Component, Path, PathBuf};

use crate::bindings::types::FsEntry;
use crate::config::Config;

/// Resolves a guest-supplied path against the configured roots.
///
/// Relative paths are taken against the first root, which is the project root
/// unless configuration says otherwise.
pub fn resolve(cfg: &Config, raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let first_root = cfg
        .filesystem
        .roots
        .first()
        .ok_or_else(|| anyhow!("no filesystem roots are configured"))?;

    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        first_root.join(candidate)
    };

    let normalised = normalise(&joined);

    // Compare against the roots using whichever form exists on disk: an
    // existing path is canonicalised so symlinks cannot escape, while a path
    // being created is checked by its parent.
    let probe = if normalised.exists() {
        canonical(&normalised)
    } else {
        match normalised.parent() {
            Some(parent) if parent.exists() => canonical(parent).join(
                normalised
                    .file_name()
                    .map(Path::new)
                    .unwrap_or_else(|| Path::new("")),
            ),
            _ => normalised.clone(),
        }
    };

    let inside = cfg
        .filesystem
        .roots
        .iter()
        .any(|root| probe.starts_with(canonical(root)));

    if !inside {
        return Err(anyhow!(
            "{} is outside the allowed roots ({})",
            normalised.display(),
            cfg.filesystem
                .roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(normalised)
}

/// Removes `.` and `..` without touching the disk, so a path that does not
/// exist yet is still checked properly.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `std::fs::canonicalize` returns `\\?\` paths on Windows, which do not
/// compare cleanly against ordinary ones.
fn canonical(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => resolved,
    }
}

/// Whether any part of the path is on the protected list.
fn is_protected(cfg: &Config, path: &Path) -> Option<String> {
    for root in &cfg.filesystem.roots {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        for part in relative.components() {
            let name = part.as_os_str().to_string_lossy().to_string();
            if cfg
                .filesystem
                .protected
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&name))
            {
                return Some(name);
            }
        }
    }
    None
}

fn require_enabled(cfg: &Config) -> Result<()> {
    if cfg.filesystem.enabled {
        Ok(())
    } else {
        Err(anyhow!(
            "filesystem access is off; set filesystem.enabled in genesis.toml to turn it on"
        ))
    }
}

/// Shows a path relative to its root, so messages stay readable.
fn display(cfg: &Config, path: &Path) -> String {
    for root in &cfg.filesystem.roots {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    path.display().to_string()
}

// --- operations -------------------------------------------------------------

pub fn read_file(cfg: &Config, raw: &str) -> Result<String> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    let size = std::fs::metadata(&path)
        .map_err(|e| anyhow!("cannot read {}: {e}", display(cfg, &path)))?
        .len() as usize;
    if size > cfg.filesystem.max_read_bytes {
        return Err(anyhow!(
            "{} is {size} bytes, over the {} byte read limit",
            display(cfg, &path),
            cfg.filesystem.max_read_bytes
        ));
    }

    std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("cannot read {}: {e}", display(cfg, &path)))
}

pub fn write_file(cfg: &Config, raw: &str, contents: &str) -> Result<String> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    if let Some(name) = is_protected(cfg, &path) {
        return Err(anyhow!("{name} is protected and cannot be written"));
    }
    if path.is_dir() {
        return Err(anyhow!("{} is a directory", display(cfg, &path)));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", display(cfg, parent)))?;
    }
    std::fs::write(&path, contents)
        .map_err(|e| anyhow!("cannot write {}: {e}", display(cfg, &path)))?;

    Ok(format!(
        "wrote {} ({} bytes)",
        display(cfg, &path),
        contents.len()
    ))
}

pub fn list_dir(cfg: &Config, raw: &str) -> Result<Vec<FsEntry>> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    let entries = std::fs::read_dir(&path)
        .map_err(|e| anyhow!("cannot list {}: {e}", display(cfg, &path)))?;

    let mut out: Vec<FsEntry> = entries
        .flatten()
        .map(|entry| {
            let meta = entry.metadata().ok();
            let full = entry.path();
            FsEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: display(cfg, &full),
                is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            }
        })
        .collect();

    // Directories first, then by name: the order someone reading a listing expects.
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(out)
}

pub fn delete_path(cfg: &Config, raw: &str, recursive: bool) -> Result<String> {
    require_enabled(cfg)?;
    if !cfg.filesystem.allow_delete {
        return Err(anyhow!("deleting is off; set filesystem.allow_delete to turn it on"));
    }

    let path = resolve(cfg, raw)?;
    if let Some(name) = is_protected(cfg, &path) {
        return Err(anyhow!("{name} is protected and cannot be deleted"));
    }
    // Deleting a root would take the workspace with it.
    if cfg.filesystem.roots.iter().any(|r| canonical(r) == canonical(&path)) {
        return Err(anyhow!("refusing to delete a configured root"));
    }
    if !path.exists() {
        return Err(anyhow!("{} does not exist", display(cfg, &path)));
    }

    if path.is_dir() {
        if !recursive {
            let count = std::fs::read_dir(&path).map(|e| e.count()).unwrap_or(0);
            if count > 0 {
                return Err(anyhow!(
                    "{} is a directory with {count} entries; pass recursive to delete it",
                    display(cfg, &path)
                ));
            }
        }
        std::fs::remove_dir_all(&path)
            .map_err(|e| anyhow!("cannot delete {}: {e}", display(cfg, &path)))?;
    } else {
        std::fs::remove_file(&path)
            .map_err(|e| anyhow!("cannot delete {}: {e}", display(cfg, &path)))?;
    }

    Ok(format!("deleted {}", display(cfg, &path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("data/genesis.redb"), "state").unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.root = root.clone();
        cfg.filesystem.roots = vec![root];
        cfg.filesystem.enabled = true;
        (cfg, dir)
    }

    #[test]
    fn reads_and_writes_inside_a_root() {
        let (cfg, _d) = fixture();
        assert_eq!(read_file(&cfg, "src/main.rs").unwrap(), "fn main() {}");

        write_file(&cfg, "notes/todo.md", "- ship it").unwrap();
        assert_eq!(read_file(&cfg, "notes/todo.md").unwrap(), "- ship it");
    }

    #[test]
    fn refuses_to_escape_the_roots() {
        let (cfg, _d) = fixture();
        for bad in [
            "../outside.txt",
            "src/../../outside.txt",
            "/etc/passwd",
            "C:/Windows/System32/drivers/etc/hosts",
        ] {
            let err = resolve(&cfg, bad).unwrap_err();
            assert!(
                format!("{err:#}").contains("outside the allowed roots"),
                "{bad} gave: {err:#}"
            );
        }
    }

    #[test]
    fn traversal_that_stays_inside_is_allowed() {
        let (cfg, _d) = fixture();
        // Normalises to src/main.rs, which is within the root.
        assert_eq!(read_file(&cfg, "src/../src/main.rs").unwrap(), "fn main() {}");
    }

    #[test]
    fn protected_paths_survive_writes_and_deletes() {
        let (cfg, _d) = fixture();
        let write = write_file(&cfg, "data/genesis.redb", "clobbered").unwrap_err();
        assert!(format!("{write:#}").contains("protected"), "{write:#}");

        let delete = delete_path(&cfg, "data", true).unwrap_err();
        assert!(format!("{delete:#}").contains("protected"), "{delete:#}");

        // The database is still intact.
        assert_eq!(read_file(&cfg, "data/genesis.redb").unwrap(), "state");
    }

    #[test]
    fn a_root_itself_cannot_be_deleted() {
        let (cfg, _d) = fixture();
        let err = delete_path(&cfg, ".", true).unwrap_err();
        assert!(format!("{err:#}").contains("configured root"), "{err:#}");
    }

    #[test]
    fn a_non_empty_directory_needs_recursive() {
        let (cfg, _d) = fixture();
        let err = delete_path(&cfg, "src", false).unwrap_err();
        assert!(format!("{err:#}").contains("recursive"), "{err:#}");

        delete_path(&cfg, "src", true).unwrap();
        assert!(read_file(&cfg, "src/main.rs").is_err());
    }

    #[test]
    fn listing_puts_directories_first() {
        let (cfg, _d) = fixture();
        let entries = list_dir(&cfg, ".").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["data", "src"]);
        assert!(entries[0].is_dir);
    }

    #[test]
    fn everything_is_refused_when_turned_off() {
        let (mut cfg, _d) = fixture();
        cfg.filesystem.enabled = false;

        for result in [
            read_file(&cfg, "src/main.rs").err(),
            write_file(&cfg, "x.txt", "x").err(),
            delete_path(&cfg, "src/main.rs", false).err(),
            list_dir(&cfg, ".").err(),
        ] {
            let err = result.expect("should be refused");
            assert!(format!("{err:#}").contains("filesystem access is off"));
        }
    }

    #[test]
    fn deleting_can_be_turned_off_on_its_own() {
        let (mut cfg, _d) = fixture();
        cfg.filesystem.allow_delete = false;

        let err = delete_path(&cfg, "src/main.rs", false).unwrap_err();
        assert!(format!("{err:#}").contains("deleting is off"), "{err:#}");
        // Reading and writing still work.
        assert!(read_file(&cfg, "src/main.rs").is_ok());
    }

    #[test]
    fn a_read_over_the_limit_is_refused_rather_than_truncated() {
        let (mut cfg, _d) = fixture();
        cfg.filesystem.max_read_bytes = 4;
        let err = read_file(&cfg, "src/main.rs").unwrap_err();
        assert!(format!("{err:#}").contains("read limit"), "{err:#}");
    }
}
