//! Skills: named instruction sets that shape how the agent works.
//!
//! Each is a markdown file in `skills/` with a small frontmatter block:
//!
//! ```text
//! ---
//! name: Concise replies
//! description: Answer in as few words as the question needs.
//! ---
//!
//! The instructions the agent receives.
//! ```
//!
//! They are read from disk on demand rather than cached, so editing one takes
//! effect on the next turn without a restart.

use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub instructions: String,
}

/// Every skill in `dir`, ordered by name so the list is stable between reads.
pub fn discover(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut skills: Vec<Skill> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|entry| {
            let id = entry.path().file_stem()?.to_string_lossy().to_string();
            let body = std::fs::read_to_string(entry.path()).ok()?;
            Some(parse(&id, &body))
        })
        .collect();

    skills.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    skills
}

/// Splits frontmatter from body. A file without frontmatter still loads — it
/// just falls back to its filename for a title.
fn parse(id: &str, source: &str) -> Skill {
    let mut name = id.replace(['-', '_'], " ");
    let mut description = String::new();

    let text = source.replace("\r\n", "\n");
    let body = match text.strip_prefix("---\n") {
        Some(rest) => match rest.split_once("\n---") {
            Some((front, remainder)) => {
                for line in front.lines() {
                    let Some((key, value)) = line.split_once(':') else {
                        continue;
                    };
                    let value = value.trim().trim_matches('"').to_string();
                    match key.trim().to_lowercase().as_str() {
                        "name" if !value.is_empty() => name = value,
                        "description" => description = value,
                        _ => {}
                    }
                }
                remainder.trim_start_matches('-').trim_start().to_string()
            }
            None => text.clone(),
        },
        None => text.clone(),
    };

    Skill {
        id: id.to_string(),
        name,
        description,
        instructions: body.trim().to_string(),
    }
}

// --- per-session attachment -------------------------------------------------

/// Skills are enabled per conversation, stored as a comma-separated id list in
/// the session's own KV scope.
const KEY: &str = "__skills";

pub fn enabled_ids(db: &crate::store::Store, session_id: &str) -> Vec<String> {
    db.kv_get(session_id, KEY)
        .ok()
        .flatten()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn set_enabled(
    db: &crate::store::Store,
    session_id: &str,
    skill_id: &str,
    enabled: bool,
) -> Result<()> {
    let mut ids = enabled_ids(db, session_id);
    ids.retain(|id| id != skill_id);
    if enabled {
        ids.push(skill_id.to_string());
    }
    ids.sort();
    db.kv_put(session_id, KEY, &ids.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_frontmatter_and_body() {
        let skill = parse(
            "concise",
            "---\nname: Concise replies\ndescription: Short answers.\n---\n\nLead with the answer.\n",
        );
        assert_eq!(skill.id, "concise");
        assert_eq!(skill.name, "Concise replies");
        assert_eq!(skill.description, "Short answers.");
        assert_eq!(skill.instructions, "Lead with the answer.");
    }

    #[test]
    fn a_file_without_frontmatter_still_loads() {
        let skill = parse("my-house-style", "Always use British spelling.");
        // The filename becomes a readable title rather than the skill vanishing.
        assert_eq!(skill.name, "my house style");
        assert_eq!(skill.instructions, "Always use British spelling.");
        assert!(skill.description.is_empty());
    }

    #[test]
    fn handles_windows_line_endings() {
        let skill = parse("x", "---\r\nname: Windows\r\n---\r\n\r\nBody text.\r\n");
        assert_eq!(skill.name, "Windows");
        assert_eq!(skill.instructions, "Body text.");
    }

    #[test]
    fn discovers_and_sorts_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("zed.md"), "---\nname: Zed\n---\nz").unwrap();
        std::fs::write(skills_dir.join("alpha.md"), "---\nname: Alpha\n---\na").unwrap();
        // Not markdown: ignored.
        std::fs::write(skills_dir.join("notes.txt"), "ignore me").unwrap();

        let found = discover(&skills_dir);
        assert_eq!(
            found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "Zed"]
        );
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(&dir.path().join("skills")).is_empty());
    }

    #[test]
    fn enabling_is_idempotent_and_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::Store::open(&dir.path().join("t.redb")).unwrap();
        let s = db.create_session(None, "agent").unwrap();

        set_enabled(&db, &s.id, "concise", true).unwrap();
        set_enabled(&db, &s.id, "concise", true).unwrap();
        assert_eq!(enabled_ids(&db, &s.id), vec!["concise"]);

        set_enabled(&db, &s.id, "careful", true).unwrap();
        assert_eq!(enabled_ids(&db, &s.id), vec!["careful", "concise"]);

        set_enabled(&db, &s.id, "concise", false).unwrap();
        assert_eq!(enabled_ids(&db, &s.id), vec!["careful"]);
    }
}
