//! The self-development kit.
//!
//! These are the operations the agent uses to change the running system: create
//! a tool, rewrite a file, patch a snippet, roll something back. Every mutating
//! call rebuilds the affected slot immediately and returns the compiler's
//! verdict inline, so the model can fix its own mistakes inside a single turn
//! instead of waiting for a human to relay the error.
//!
//! Writes are constrained: paths cannot escape the slot's source tree, and the
//! files that decide what code runs at *build* time — `Cargo.toml`, `build.rs`,
//! `.cargo/` — are off limits, because a host-side build executes them.

use anyhow::{anyhow, Result};
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::Arc;

use crate::bindings::types::{CompileReport, ModTarget};
use crate::harness::Harness;
use crate::pipeline;
use crate::revisions::Origin;
use crate::slot::{validate_component_name, Slot};

/// Why a path is off limits, or `None` when it is not.
///
/// Driven by `[devkit]` in the config, which has advertised these as settings
/// from the start while the code ignored them in favour of a hardcoded list -
/// so loosening it in the config silently did nothing.
///
/// Both lists default to empty. A component that cannot edit its own manifest
/// cannot add a dependency, and one that cannot edit `build.rs` cannot generate
/// code; those are ordinary things for it to need, and refusing them stops the
/// agent developing itself rather than stopping it doing harm.
fn protected_reason(names: &[String], files: &[String], dirs: &[String]) -> Option<String> {
    if let Some(last) = names.last() {
        if files.iter().any(|p| p.eq_ignore_ascii_case(last)) {
            return Some(format!("{last} is listed in devkit.protected_files"));
        }
    }
    if let Some(hit) = names
        .iter()
        .find(|n| dirs.iter().any(|p| p.eq_ignore_ascii_case(n)))
    {
        return Some(format!("{hit} is listed in devkit.protected_dirs"));
    }
    None
}

pub fn target_to_slot(target: &ModTarget) -> Result<Slot> {
    Ok(match target {
        ModTarget::AgentSelf => Slot::Agent,
        ModTarget::Tool(name) => {
            validate_component_name(name)?;
            Slot::tool(name)
        }
        ModTarget::Gateway(name) => {
            validate_component_name(name)?;
            Slot::gateway(name)
        }
    })
}

/// Resolves a guest-supplied relative path inside a slot's source tree.
///
/// Rejects anything that would escape the tree or touch a build-time file.
pub fn resolve_path(harness: &Arc<Harness>, slot: &Slot, relative: &str) -> Result<PathBuf> {
    let relative = relative.trim().replace('\\', "/");
    if relative.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let candidate = Path::new(&relative);
    if candidate.is_absolute() {
        return Err(anyhow!("path must be relative to the component's source"));
    }

    // Reject traversal by inspecting components rather than by string matching,
    // which is easy to slip past.
    for part in candidate.components() {
        match part {
            PathComponent::Normal(_) => {}
            PathComponent::CurDir => {}
            _ => return Err(anyhow!("path must not contain '..' or a drive prefix")),
        }
    }

    let names: Vec<String> = candidate
        .components()
        .filter_map(|c| match c {
            PathComponent::Normal(n) => Some(n.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    if let Some(why) = protected_reason(
        &names,
        &harness.cfg.devkit.protected_files,
        &harness.cfg.devkit.protected_dirs,
    ) {
        return Err(anyhow!("{why}"));
    }

    let root = harness.cfg.slot_source_dir(slot);
    let full = root.join(candidate);

    // Belt and braces: even after the component checks, confirm the result is
    // still inside the tree once symlinks are resolved.
    if let (Ok(root_real), Ok(parent_real)) = (
        dunce_canonicalize(&root),
        full.parent().map(dunce_canonicalize).unwrap_or_else(|| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no parent"))
        }),
    ) {
        if !parent_real.starts_with(&root_real) {
            return Err(anyhow!("path escapes the component's source tree"));
        }
    }

    Ok(full)
}

/// `std::fs::canonicalize` on Windows returns `\\?\` paths, which do not
/// compare cleanly against ordinary ones.
fn dunce_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let text = canonical.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => canonical,
    })
}

fn report_error(slot: &str, detail: impl Into<String>) -> CompileReport {
    CompileReport {
        success: false,
        slot: slot.to_string(),
        revision: None,
        stderr: String::new(),
        duration_ms: 0,
        pending_swap: false,
        detail: detail.into(),
    }
}

fn report_from(outcome: pipeline::Outcome) -> CompileReport {
    CompileReport {
        success: outcome.success,
        slot: outcome.slot,
        revision: outcome.revision,
        stderr: outcome.stderr,
        duration_ms: outcome.duration_ms,
        pending_swap: outcome.pending_swap,
        detail: outcome.detail,
    }
}

// --- operations -------------------------------------------------------------

/// Scaffolds a new tool crate from the template, builds it, and loads it.
pub async fn new_tool(
    harness: &Arc<Harness>,
    name: &str,
    description: &str,
) -> CompileReport {
    let slot_label = format!("tool/{name}");

    if let Err(e) = validate_component_name(name) {
        return report_error(&slot_label, format!("{e:#}"));
    }
    let slot = Slot::tool(name);
    let dir = harness.cfg.slot_source_dir(&slot);
    if dir.exists() {
        return report_error(
            &slot_label,
            format!("a tool named '{name}' already exists; edit it with write_code instead"),
        );
    }

    if let Err(e) = scaffold(harness, &slot, name, description) {
        return report_error(&slot_label, format!("could not scaffold: {e:#}"));
    }

    build(harness, &slot, Origin::AgentMod, &format!("created {name}")).await
}

fn scaffold(harness: &Arc<Harness>, slot: &Slot, name: &str, description: &str) -> Result<()> {
    let templates = harness.cfg.paths.templates.join("tool-template");
    let cargo = std::fs::read_to_string(templates.join("Cargo.toml.template"))?;
    let lib = std::fs::read_to_string(templates.join("lib.rs.template"))?;

    // Keep the description on one line: it is embedded in Rust string literals.
    let safe_description = description
        .replace('\\', r"\\")
        .replace('"', r#"\""#)
        .replace(['\n', '\r'], " ");

    let render = |text: &str| {
        text.replace("{{name}}", name)
            .replace("{{description}}", &safe_description)
    };

    let dir = harness.cfg.slot_source_dir(slot);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("Cargo.toml"), render(&cargo))?;
    std::fs::write(dir.join("src").join("lib.rs"), render(&lib))?;
    Ok(())
}

pub async fn write_file(
    harness: &Arc<Harness>,
    target: &ModTarget,
    path: &str,
    contents: &str,
) -> CompileReport {
    let (slot, full) = match locate(harness, target, path) {
        Ok(pair) => pair,
        Err(report) => return report,
    };

    if let Some(parent) = full.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return report_error(&slot.key(), format!("could not create directory: {e}"));
        }
    }
    if let Err(e) = std::fs::write(&full, contents) {
        return report_error(&slot.key(), format!("could not write {path}: {e}"));
    }

    build(harness, &slot, Origin::AgentMod, &format!("wrote {path}")).await
}

pub async fn patch_file(
    harness: &Arc<Harness>,
    target: &ModTarget,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> CompileReport {
    let (slot, full) = match locate(harness, target, path) {
        Ok(pair) => pair,
        Err(report) => return report,
    };

    let current = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) => return report_error(&slot.key(), format!("could not read {path}: {e}")),
    };

    // An ambiguous patch would silently change the wrong line, so require the
    // anchor to be unique.
    let occurrences = current.matches(old_text).count();
    if occurrences == 0 {
        return report_error(
            &slot.key(),
            format!("the text to replace does not appear in {path}"),
        );
    }
    if occurrences > 1 {
        return report_error(
            &slot.key(),
            format!("the text to replace appears {occurrences} times in {path}; include more surrounding context to make it unique"),
        );
    }

    let patched = current.replacen(old_text, new_text, 1);
    if let Err(e) = std::fs::write(&full, patched) {
        return report_error(&slot.key(), format!("could not write {path}: {e}"));
    }

    build(harness, &slot, Origin::AgentMod, &format!("patched {path}")).await
}

/// Adds a crate to a component's dependencies and rebuilds it.
///
/// The rebuild deliberately runs without `--locked`: the lockfile cannot match
/// a manifest that just gained an entry, so insisting on it would fail every
/// time. Reproducibility is restored as soon as cargo rewrites the lock.
pub async fn add_dependency(
    harness: &Arc<Harness>,
    target: &ModTarget,
    dep: &crate::manifest::Dependency,
) -> CompileReport {
    let slot = match slot_with_source(harness, target) {
        Ok(slot) => slot,
        Err(report) => return report,
    };
    let dir = harness.cfg.slot_source_dir(&slot);

    // Keep the manifest as it was, so a dependency that does not resolve can be
    // rolled back out rather than leaving the crate unbuildable.
    let restore = std::fs::read_to_string(dir.join("Cargo.toml")).ok();

    if let Err(e) = crate::manifest::add(&dir, dep, &harness.cfg.build.allowed_crates) {
        return report_error(&slot.key(), format!("{e:#}"));
    }

    let note = format!("added dependency {} {}", dep.name, dep.version);
    let report = build_deps(harness, &slot, &note).await;

    if !report.success {
        if let Some(text) = restore {
            let _ = std::fs::write(dir.join("Cargo.toml"), text);
        }
    }
    report
}

/// Removes a dependency and rebuilds.
pub async fn remove_dependency(
    harness: &Arc<Harness>,
    target: &ModTarget,
    name: &str,
) -> CompileReport {
    let slot = match slot_with_source(harness, target) {
        Ok(slot) => slot,
        Err(report) => return report,
    };
    let dir = harness.cfg.slot_source_dir(&slot);
    let restore = std::fs::read_to_string(dir.join("Cargo.toml")).ok();

    if let Err(e) = crate::manifest::remove(&dir, name) {
        return report_error(&slot.key(), format!("{e:#}"));
    }

    let report = build_deps(harness, &slot, &format!("removed dependency {name}")).await;
    if !report.success {
        if let Some(text) = restore {
            let _ = std::fs::write(dir.join("Cargo.toml"), text);
        }
    }
    report
}

pub fn list_dependencies(
    harness: &Arc<Harness>,
    target: &ModTarget,
) -> std::result::Result<Vec<crate::manifest::Dependency>, String> {
    let slot = target_to_slot(target).map_err(|e| format!("{e:#}"))?;
    let dir = harness.cfg.slot_source_dir(&slot);
    crate::manifest::list(&dir).map_err(|e| format!("{e:#}"))
}

/// A rebuild that is allowed to refresh the lockfile.
async fn build_deps(harness: &Arc<Harness>, slot: &Slot, note: &str) -> CompileReport {
    harness.suppress_watch(slot, harness.cfg.watchdog.watch_suppression);

    let opts = crate::builder::BuildOptions {
        refresh_lockfile: true,
    };
    match pipeline::build_and_activate_with(harness, slot, Origin::AgentMod, note, opts).await {
        Ok(outcome) => report_from(outcome),
        Err(e) => report_error(&slot.key(), format!("build pipeline failed: {e:#}")),
    }
}

/// Resolves a target to a slot that actually has a crate on disk.
fn slot_with_source(
    harness: &Arc<Harness>,
    target: &ModTarget,
) -> std::result::Result<Slot, CompileReport> {
    let slot = match target_to_slot(target) {
        Ok(s) => s,
        Err(e) => return Err(report_error("unknown", format!("{e:#}"))),
    };
    if !harness.cfg.slot_source_dir(&slot).join("Cargo.toml").is_file() {
        return Err(report_error(
            &slot.key(),
            format!("{slot} has no crate on disk"),
        ));
    }
    Ok(slot)
}

pub fn read_file(
    harness: &Arc<Harness>,
    target: &ModTarget,
    path: &str,
) -> std::result::Result<String, String> {
    let slot = target_to_slot(target).map_err(|e| format!("{e:#}"))?;
    let full = resolve_path(harness, &slot, path).map_err(|e| format!("{e:#}"))?;
    std::fs::read_to_string(&full).map_err(|e| format!("could not read {path}: {e}"))
}

pub fn list_files(
    harness: &Arc<Harness>,
    target: &ModTarget,
) -> std::result::Result<Vec<String>, String> {
    let slot = target_to_slot(target).map_err(|e| format!("{e:#}"))?;
    let root = harness.cfg.slot_source_dir(&slot);
    if !root.is_dir() {
        return Err(format!("{slot} has no source tree"));
    }

    let mut files = Vec::new();
    collect(&root, &root, &mut files).map_err(|e| format!("{e}"))?;
    files.sort();
    Ok(files)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "target" || name == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

// --- helpers ----------------------------------------------------------------

fn locate(
    harness: &Arc<Harness>,
    target: &ModTarget,
    path: &str,
) -> std::result::Result<(Slot, PathBuf), CompileReport> {
    let slot = match target_to_slot(target) {
        Ok(s) => s,
        Err(e) => return Err(report_error("unknown", format!("{e:#}"))),
    };
    if !harness.cfg.slot_source_dir(&slot).is_dir() {
        return Err(report_error(
            &slot.key(),
            format!("{slot} has no source tree on disk"),
        ));
    }
    match resolve_path(harness, &slot, path) {
        Ok(full) => Ok((slot, full)),
        Err(e) => Err(report_error(&slot.key(), format!("{e:#}"))),
    }
}

async fn build(
    harness: &Arc<Harness>,
    slot: &Slot,
    origin: Origin,
    note: &str,
) -> CompileReport {
    // The watcher would otherwise queue a second, redundant build for the same
    // edit a moment later.
    harness.suppress_watch(slot, harness.cfg.watchdog.watch_suppression);

    match pipeline::build_and_activate(harness, slot, origin, note).await {
        Ok(outcome) => report_from(outcome),
        Err(e) => report_error(&slot.key(), format!("build pipeline failed: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Path checks only need config, so build the minimum a Harness would give.
    fn paths_under(root: &Path, slot: &Slot, path: &str) -> Result<PathBuf> {
        let mut cfg = Config::load().unwrap();
        cfg.root = root.to_path_buf();
        let source = cfg.slot_source_dir(slot);
        std::fs::create_dir_all(source.join("src")).unwrap();

        // Mirror resolve_path without needing a full Harness.
        let harness_root = cfg.slot_source_dir(slot);
        let relative = path.trim().replace('\\', "/");
        let candidate = Path::new(&relative);
        if candidate.is_absolute() {
            return Err(anyhow!("absolute"));
        }
        for part in candidate.components() {
            match part {
                PathComponent::Normal(_) | PathComponent::CurDir => {}
                _ => return Err(anyhow!("traversal")),
            }
        }
        let names: Vec<String> = candidate
            .components()
            .filter_map(|c| match c {
                PathComponent::Normal(n) => Some(n.to_string_lossy().to_string()),
                _ => None,
            })
            .collect();
        if let Some(why) = protected_reason(
            &names,
            &cfg.devkit.protected_files,
            &cfg.devkit.protected_dirs,
        ) {
            return Err(anyhow!("{why}"));
        }
        Ok(harness_root.join(candidate))
    }

    #[test]
    fn accepts_ordinary_source_paths() {
        let dir = tempfile::tempdir().unwrap();
        let slot = Slot::Agent;
        assert!(paths_under(dir.path(), &slot, "src/lib.rs").is_ok());
        assert!(paths_under(dir.path(), &slot, "src/ui/app.js").is_ok());
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let slot = Slot::Agent;
        for bad in [
            "../../../etc/passwd",
            "src/../../secrets",
            "C:/Windows/System32/x",
            "/etc/passwd",
        ] {
            assert!(
                paths_under(dir.path(), &slot, bad).is_err(),
                "should have rejected {bad}"
            );
        }
    }

    #[test]
    fn build_files_are_editable_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let slot = Slot::Agent;
        // Editing its own manifest is how a component adds a dependency, so
        // nothing here is off limits unless a deployment asks for it.
        for path in ["Cargo.toml", "build.rs", ".cargo/config.toml"] {
            assert!(
                paths_under(dir.path(), &slot, path).is_ok(),
                "should have allowed {path}"
            );
        }
    }

    #[test]
    fn a_configured_protection_is_still_honoured() {
        let files = vec!["Cargo.toml".to_string()];
        let dirs = vec![".cargo".to_string()];
        let names = |p: &str| p.split('/').map(String::from).collect::<Vec<_>>();

        assert!(protected_reason(&names("src/lib.rs"), &files, &dirs).is_none());
        // Still matched without regard to case.
        assert!(protected_reason(&names("cargo.toml"), &files, &dirs).is_some());
        assert!(protected_reason(&names(".cargo/config.toml"), &files, &dirs).is_some());
        // An empty list protects nothing.
        assert!(protected_reason(&names("Cargo.toml"), &[], &[]).is_none());
    }

    #[test]
    fn tool_targets_validate_their_names() {
        assert!(target_to_slot(&ModTarget::Tool("good-name".into())).is_ok());
        assert!(target_to_slot(&ModTarget::Tool("../escape".into())).is_err());
        assert!(target_to_slot(&ModTarget::Gateway("Bad Name".into())).is_err());
    }
}
