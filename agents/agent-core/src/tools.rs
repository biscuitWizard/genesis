//! The agent's tool surface.
//!
//! Tools are advertised only when the capability behind them is actually
//! available, so the model is never offered something that will just fail. As
//! the orchestrator gains capabilities (the sandbox, the dev kit), the flags
//! flip and the tools appear without the agent needing to change.

use crate::genesis::harness::types::{
    CompileReport, ConfigEntry, Dependency, ExecResult, FsEntry, LogLevel, ModTarget,
    RollbackTarget, TerminalOutput, ToolManifest,
};
use crate::genesis::harness::{
    configuration, control, devkit, hostfs, sandbox, sys, terminal, tooling,
};
use serde_json::{json, Value};

/// The mode assumed when a session has not chosen one.
pub const DEFAULT_MODE: &str = "agent";

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// Whether calling this changes something outside the conversation.
    /// Read-only modes withhold the ones that do.
    pub mutating: bool,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn string_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn sandbox_available() -> bool {
    sandbox::available()
}

fn devkit_available() -> bool {
    sys::config_get("devkit_available").as_deref() == Some("true")
}

fn filesystem_available() -> bool {
    hostfs::available()
}

fn terminal_available() -> bool {
    terminal::available()
}

fn restart_available() -> bool {
    control::available()
}

/// Whether a mode withholds tools that change things.
///
/// Asked of the harness rather than hardcoded, so adding a read-only mode is a
/// configuration change and needs nothing here.
fn read_only(mode: &str) -> bool {
    sys::list_modes()
        .into_iter()
        .find(|m| m.id == mode)
        .map(|m| m.read_only)
        .unwrap_or(false)
}

/// Whether a tool name changes something outside the conversation.
///
/// Built-ins are checked against their own declarations; anything else is a
/// hot-loaded tool component, whose behaviour is opaque and so treated as
/// mutating.
fn is_mutating(name: &str) -> bool {
    match all_builtins().into_iter().find(|t| t.name == name) {
        Some(tool) => tool.mutating,
        None => true,
    }
}

/// Every built-in the agent knows about, whether or not it is currently
/// offered.
///
/// `available` already includes the dev kit when it is enabled; this adds it
/// back when it is not, so a name can still be classified as mutating even in
/// a configuration where it is never offered.
fn all_builtins() -> Vec<ToolDef> {
    let mut tools = available(DEFAULT_MODE);
    if !devkit_available() {
        tools.extend(devkit_tools());
    }
    if !filesystem_available() {
        tools.extend(filesystem_tools());
    }
    if !terminal_available() {
        tools.extend(terminal_tools());
    }
    tools
}

/// Reading and changing the harness's own settings.
///
/// Writes land in the config file with its comments intact and are refused
/// unless the result would still load; nothing takes effect until a restart.
fn configuration_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_config",
            description:
                "List Genesis's settings as dotted paths with their current values. Pass a \
                 prefix such as 'llm' or 'terminal' to narrow it.",
            mutating: false,
            parameters: obj(
                json!({ "prefix": string_prop("Section to narrow to, e.g. 'budgets'. Omit for all.") }),
                &[],
            ),
        },
        ToolDef {
            name: "read_config",
            description: "Read one setting by its dotted path, e.g. 'llm.model'.",
            mutating: false,
            parameters: obj(
                json!({ "key": string_prop("Dotted path of the setting.") }),
                &["key"],
            ),
        },
        ToolDef {
            name: "set_config",
            description:
                "Change one setting and write it back to the config file. The change is \
                 refused if the result would not load. Settings are read at startup, so call \
                 restart_orchestrator afterwards for it to take effect.",
            mutating: true,
            parameters: obj(
                json!({
                    "key": string_prop("Dotted path of the setting."),
                    "value": string_prop("New value. Its type is taken from the existing one."),
                }),
                &["key", "value"],
            ),
        },
    ]
}

/// Every tool the agent can call in this mode.
pub fn available(mode: &str) -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "remember",
            description:
                "Save a durable note for this conversation. Survives restarts and self-modification.",
            mutating: false,
            parameters: obj(
                json!({
                    "key": string_prop("Short identifier for the note."),
                    "value": string_prop("The content to remember."),
                }),
                &["key", "value"],
            ),
        },
        ToolDef {
            name: "recall",
            description:
                "Read back a saved note. Omit the key to list everything remembered here.",
            mutating: false,
            parameters: obj(
                json!({ "key": string_prop("The note to read; omit to list all keys.") }),
                &[],
            ),
        },
    ];

    if sandbox_available() {
        tools.push(ToolDef {
            name: "exec",
            description: "Run a shell command in this session's isolated container.",
            mutating: true,
            parameters: obj(
                json!({
                    "command": string_prop("The command line to run."),
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds." },
                }),
                &["command"],
            ),
        });
        tools.push(ToolDef {
            name: "write_file",
            description: "Write a file in the session's container workspace.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path inside the workspace."),
                    "contents": string_prop("Full file contents."),
                }),
                &["path", "contents"],
            ),
        });
        tools.push(ToolDef {
            name: "read_file",
            description: "Read a file from the session's container workspace.",
            mutating: false,
            parameters: obj(json!({ "path": string_prop("Path inside the workspace.") }), &["path"]),
        });
    }

    if devkit_available() {
        tools.extend(devkit_tools());
    }
    if filesystem_available() {
        tools.extend(filesystem_tools());
    }
    if terminal_available() {
        tools.extend(terminal_tools());
    }
    tools.extend(configuration_tools());
    if restart_available() {
        tools.push(ToolDef {
            name: "restart_orchestrator",
            description:
                "Restart the Genesis process. Needed for changes to the native binary or to \
                 settings only read at startup. The chat reconnects on its own, and this \
                 turn continues afterwards unless you say otherwise; say why first, because \
                 the restart happens just after your turn ends.",
            mutating: true,
            parameters: obj(
                json!({
                    "reason": string_prop("Why a restart is needed."),
                    "resume": {
                        "type": "boolean",
                        "description": "Carry this turn on once Genesis is back, which is the default. Set false only if the restart is the last thing you mean to do.",
                    },
                }),
                &["reason"],
            ),
        });
    }

    // In a read-only mode the tools that would change something are simply not
    // offered, rather than offered and then refused.
    if read_only(mode) {
        tools.retain(|t| !t.mutating);
    }

    tools
}

/// Reading and writing files on the machine Genesis is running on, confined to
/// the roots named in configuration.
fn filesystem_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "read_path",
            description: "Read a file from the host filesystem.",
            mutating: false,
            parameters: obj(
                json!({ "path": string_prop("Path, relative to the project root unless absolute.") }),
                &["path"],
            ),
        },
        ToolDef {
            name: "write_path",
            description:
                "Write a file on the host filesystem, creating parent directories as needed. \
                 Replaces the whole file.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path to write."),
                    "contents": string_prop("The complete new file contents."),
                }),
                &["path", "contents"],
            ),
        },
        ToolDef {
            name: "list_path",
            description: "List a directory on the host filesystem.",
            mutating: false,
            parameters: obj(
                json!({ "path": string_prop("Directory to list; '.' for the project root.") }),
                &["path"],
            ),
        },
        ToolDef {
            name: "delete_path",
            description:
                "Delete a file or directory on the host filesystem. This cannot be undone, so \
                 check the path first.",
            mutating: true,
            parameters: obj(
                json!({
                    "path": string_prop("Path to delete."),
                    "recursive": {
                        "type": "boolean",
                        "description": "Required to delete a directory that is not empty.",
                    },
                }),
                &["path"],
            ),
        },
    ]
}

/// Shell sessions. A session keeps its working directory and shell state
/// between commands, which is the point of opening one rather than running a
/// series of unrelated commands.
fn terminal_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "terminal_open",
            description:
                "Open a shell session and return its id. Reuse the id for related commands so \
                 the working directory and environment carry over.",
            mutating: true,
            parameters: obj(
                json!({ "cwd": string_prop("Directory to start in; defaults to the project root.") }),
                &[],
            ),
        },
        ToolDef {
            name: "terminal_run",
            description:
                "Run a command in an open shell session and wait for it to finish. Returns \
                 whatever it printed. A command that outlives the timeout keeps running; read \
                 the session again later for the rest.",
            mutating: true,
            parameters: obj(
                json!({
                    "id": string_prop("Session id from terminal_open."),
                    "command": string_prop("The command line to run."),
                    "timeout_ms": {
                        "type": "integer",
                        "description": "How long to wait. Omit for the configured default.",
                    },
                }),
                &["id", "command"],
            ),
        },
        ToolDef {
            name: "terminal_read",
            description:
                "Read anything a session has printed since the last read, without running \
                 anything. Use it to collect output from a command that timed out.",
            mutating: false,
            parameters: obj(json!({ "id": string_prop("Session id.") }), &["id"]),
        },
        ToolDef {
            name: "terminal_close",
            description: "Close a shell session and stop its process.",
            mutating: true,
            parameters: obj(json!({ "id": string_prop("Session id.") }), &["id"]),
        },
        ToolDef {
            name: "terminal_list",
            description: "List the open shell sessions.",
            mutating: false,
            parameters: obj(json!({}), &[]),
        },
    ]
}

/// Tools that edit the running system. Every mutating one rebuilds immediately
/// and returns the compiler's verdict in its result.
fn devkit_tools() -> Vec<ToolDef> {
    let target_prop = json!({
        "type": "string",
        "description": "What to edit: 'self' for your own loop, 'gateway:<name>' for a chat \
                        interface, or 'tool:<name>' for one of your tools.",
    });

    vec![
        ToolDef {
            name: "new_tool",
            description:
                "Scaffold a new tool component: creates the crate, builds it, and loads it. \
                 Returns the compile result.",
            mutating: true,
            parameters: obj(
                json!({
                    "name": string_prop("Lowercase name, hyphens allowed."),
                    "description": string_prop("What the tool does; shown to you later."),
                }),
                &["name", "description"],
            ),
        },
        ToolDef {
            name: "write_code",
            description:
                "Replace a whole file in a component, then rebuild and hot-swap it. The compile \
                 result comes back immediately, so fix errors and call again until it builds.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "path": string_prop("File path within the component's source tree."),
                    "contents": string_prop("The complete new file contents."),
                }),
                &["target", "path", "contents"],
            ),
        },
        ToolDef {
            name: "patch_code",
            description:
                "Replace an exact snippet in a component file, then rebuild and hot-swap it. \
                 Returns the compile result.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "path": string_prop("File path within the component's source tree."),
                    "old_text": string_prop("Exact text to find; must appear exactly once."),
                    "new_text": string_prop("Replacement text."),
                }),
                &["target", "path", "old_text", "new_text"],
            ),
        },
        ToolDef {
            name: "add_dependency",
            description:
                "Add a crate from crates.io to a component's dependencies, then rebuild it. Any \
                 published crate that supports wasm32-wasip2 will work; pure-computation crates \
                 almost always do. This fetches over the network, so it is slower than an \
                 ordinary edit. If the build fails the manifest is put back as it was.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "name": string_prop("Crate name as published, e.g. 'regex'."),
                    "version": string_prop("Version requirement, e.g. '1' or '0.4.31'."),
                    "features": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Cargo features to enable. Omit for none.",
                    },
                    "default_features": {
                        "type": "boolean",
                        "description": "Keep the crate's default features. Defaults to true.",
                    },
                }),
                &["target", "name", "version"],
            ),
        },
        ToolDef {
            name: "remove_dependency",
            description: "Drop a crate from a component's dependencies and rebuild it.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": target_prop,
                    "name": string_prop("Crate name to remove."),
                }),
                &["target", "name"],
            ),
        },
        ToolDef {
            name: "list_dependencies",
            description: "List the crates a component currently depends on.",
            mutating: false,
            parameters: obj(json!({ "target": target_prop }), &["target"]),
        },
        ToolDef {
            name: "read_code",
            description: "Read one of your own source files.",
            mutating: false,
            parameters: obj(
                json!({ "target": target_prop, "path": string_prop("File path to read.") }),
                &["target", "path"],
            ),
        },
        ToolDef {
            name: "list_code",
            description: "List the source files of a component.",
            mutating: false,
            parameters: obj(json!({ "target": target_prop }), &["target"]),
        },
        ToolDef {
            name: "rollback",
            description:
                "Restore a component (or the whole system) to an earlier revision. Use when a \
                 change made things worse.",
            mutating: true,
            parameters: obj(
                json!({
                    "target": {
                        "type": "string",
                        "description": "'self', 'gateway:<name>', 'tool:<name>', or 'system'.",
                    },
                    "revision": { "type": "integer", "description": "Revision to restore; omit for the last good one." },
                }),
                &["target"],
            ),
        },
        ToolDef {
            name: "history",
            description: "List the revision history of a component or of the whole system.",
            mutating: false,
            parameters: obj(
                json!({
                    "target": {
                        "type": "string",
                        "description": "'self', 'gateway:<name>', 'tool:<name>', or 'system'.",
                    },
                }),
                &["target"],
            ),
        },
    ]
}

/// Every tool offered in this mode, described for a human reader rather than
/// for the model. Built-ins are tagged so the panel can group them, and
/// mutating ones are tagged so it can explain what a read-only mode withholds.
pub fn manifests(mode: &str) -> Vec<ToolManifest> {
    let mut out: Vec<ToolManifest> = available(mode)
        .iter()
        .map(|t| ToolManifest {
            name: t.name.to_string(),
            description: t.description.to_string(),
            args_schema_json: t.parameters.to_string(),
            capabilities: {
                let mut caps = vec!["built-in".to_string()];
                if t.mutating {
                    caps.push("mutating".to_string());
                }
                caps
            },
        })
        .collect();

    if !read_only(mode) {
        for mut manifest in tooling::registry() {
            manifest.capabilities.push("component".to_string());
            out.push(manifest);
        }
    }
    out
}

/// Tool definitions in the format the chat completions API expects, including
/// any hot-loaded tool components the orchestrator has registered.
pub fn definitions(mode: &str) -> Vec<Value> {
    let mut defs: Vec<Value> = available(mode)
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect();

    // Hot-loaded tools are opaque: a read-only mode cannot tell what they do,
    // so it does not offer them.
    if read_only(mode) {
        return defs;
    }

    for manifest in tooling::registry() {
        let parameters = serde_json::from_str::<Value>(&manifest.args_schema_json)
            .unwrap_or_else(|_| obj(json!({}), &[]));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": manifest.name,
                "description": manifest.description,
                "parameters": parameters,
            },
        }));
    }

    defs
}

// --- dispatch ---------------------------------------------------------------

pub fn invoke(
    session_id: &str,
    mode: &str,
    name: &str,
    args_json: &str,
) -> Result<String, String> {
    // Withholding a tool from the definitions is not enough on its own: a model
    // can still name one it saw earlier in the conversation. The mode is
    // enforced here, where the call would actually happen.
    if read_only(mode) && is_mutating(name) {
        return Err(format!(
            "'{name}' changes things, and this conversation is in {mode} mode. Switch to agent mode to run it."
        ));
    }

    let args: Value = serde_json::from_str(args_json)
        .map_err(|e| format!("arguments were not valid JSON: {e}"))?;

    match name {
        "remember" => remember(session_id, &args),
        "recall" => recall(session_id, &args),

        "exec" => {
            let command = req_str(&args, "command")?;
            let timeout = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(30_000) as u32;
            Ok(format_exec(sandbox::exec(session_id, &command, None, timeout)))
        }
        "write_file" => sandbox::write_file(
            session_id,
            &req_str(&args, "path")?,
            &req_str(&args, "contents")?,
        )
        .map(|_| "written".to_string()),
        "read_file" => sandbox::read_file(session_id, &req_str(&args, "path")?),

        "new_tool" => Ok(format_report(devkit::new_tool(
            &req_str(&args, "name")?,
            &req_str(&args, "description")?,
        ))),
        "write_code" => Ok(format_report(devkit::write_file(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "path")?,
            &req_str(&args, "contents")?,
        ))),
        "patch_code" => Ok(format_report(devkit::patch_file(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "path")?,
            &req_str(&args, "old_text")?,
            &req_str(&args, "new_text")?,
        ))),
        "add_dependency" => Ok(format_report(devkit::add_dependency(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &Dependency {
                name: req_str(&args, "name")?,
                version: req_str(&args, "version")?,
                features: args
                    .get("features")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
                default_features: args
                    .get("default_features")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            },
        ))),
        "remove_dependency" => Ok(format_report(devkit::remove_dependency(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "name")?,
        ))),
        "list_dependencies" => {
            let deps = devkit::list_dependencies(&parse_mod_target(&req_str(&args, "target")?)?)?;
            if deps.is_empty() {
                return Ok("no dependencies".to_string());
            }
            Ok(deps
                .iter()
                .map(format_dependency)
                .collect::<Vec<_>>()
                .join("\n"))
        }

        "read_code" => devkit::read_file(
            &parse_mod_target(&req_str(&args, "target")?)?,
            &req_str(&args, "path")?,
        ),
        "list_code" => devkit::list_files(&parse_mod_target(&req_str(&args, "target")?)?)
            .map(|files| files.join("\n")),

        "rollback" => devkit::rollback(
            &parse_rollback_target(&req_str(&args, "target")?)?,
            args.get("revision").and_then(Value::as_u64),
        ),
        "history" => {
            let entries = devkit::history(&parse_rollback_target(&req_str(&args, "target")?)?);
            if entries.is_empty() {
                return Ok("no revisions recorded".to_string());
            }
            Ok(entries
                .iter()
                .map(|r| {
                    format!(
                        "r{:04}  {:<12} {:<10} {}",
                        r.revision, r.status, r.origin, r.note
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }

        "read_path" => hostfs::read_file(&req_str(&args, "path")?),
        "write_path" => hostfs::write_file(
            &req_str(&args, "path")?,
            &req_str(&args, "contents")?,
        ),
        "list_path" => hostfs::list_dir(&req_str(&args, "path")?).map(format_listing),
        "delete_path" => hostfs::delete_path(
            &req_str(&args, "path")?,
            args.get("recursive").and_then(Value::as_bool).unwrap_or(false),
        ),

        "terminal_open" => terminal::open(args.get("cwd").and_then(Value::as_str)),
        "terminal_run" => terminal::run(
            &req_str(&args, "id")?,
            &req_str(&args, "command")?,
            args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
        )
        .map(format_terminal),
        "terminal_read" => terminal::read(&req_str(&args, "id")?).map(|text| {
            if text.trim().is_empty() {
                "[nothing new]".to_string()
            } else {
                text
            }
        }),
        "terminal_close" => terminal::close(&req_str(&args, "id")?),
        "terminal_list" => {
            let sessions = terminal::sessions();
            if sessions.is_empty() {
                return Ok("no open sessions".to_string());
            }
            Ok(sessions
                .iter()
                .map(|s| {
                    format!(
                        "{}  {}  {} command(s)  {}",
                        s.id,
                        s.cwd,
                        s.commands,
                        if s.alive { "running" } else { "exited" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }

        "list_config" => {
            let entries = configuration::settings(args.get("prefix").and_then(Value::as_str));
            if entries.is_empty() {
                return Ok("no settings found".to_string());
            }
            Ok(entries
                .iter()
                .map(format_setting)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "read_config" => {
            let key = req_str(&args, "key")?;
            configuration::get(&key)
                .map(|e| format_setting(&e))
                .ok_or_else(|| format!("no setting named '{key}'"))
        }
        "set_config" => {
            configuration::set(&req_str(&args, "key")?, &req_str(&args, "value")?)
        }

        "restart_orchestrator" => control::restart(
            &req_str(&args, "reason")?,
            // Carrying on is the sensible default: a restart is usually a step
            // in the middle of doing something, not the end of it.
            args.get("resume").and_then(Value::as_bool).unwrap_or(true),
        ),

        // Anything else must be a hot-loaded tool component.
        other => tooling::invoke(other, session_id, args_json),
    }
}

// --- memory tools -----------------------------------------------------------

const INDEX_KEY: &str = "__memory_index";

fn remember(session_id: &str, args: &Value) -> Result<String, String> {
    let key = req_str(args, "key")?;
    let value = req_str(args, "value")?;
    if key.starts_with("__") {
        return Err("keys beginning with '__' are reserved".to_string());
    }

    sys::kv_put(session_id, &key, &value);

    // Maintain an index so `recall` can enumerate; the KV store itself has no
    // listing operation.
    let mut keys = memory_keys(session_id);
    if !keys.iter().any(|k| k == &key) {
        keys.push(key.clone());
        keys.sort();
        sys::kv_put(session_id, INDEX_KEY, &keys.join("\n"));
    }

    sys::log(LogLevel::Debug, &format!("remembered '{key}'"));
    Ok(format!("remembered '{key}'"))
}

fn recall(session_id: &str, args: &Value) -> Result<String, String> {
    match args.get("key").and_then(Value::as_str) {
        Some(key) => sys::kv_get(session_id, key)
            .ok_or_else(|| format!("nothing remembered under '{key}'")),
        None => {
            let keys = memory_keys(session_id);
            if keys.is_empty() {
                return Ok("nothing remembered yet".to_string());
            }
            Ok(keys
                .iter()
                .map(|k| {
                    let value = sys::kv_get(session_id, k).unwrap_or_default();
                    format!("{k}: {value}")
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

fn memory_keys(session_id: &str) -> Vec<String> {
    sys::kv_get(session_id, INDEX_KEY)
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// --- formatting -------------------------------------------------------------

fn req_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument '{key}'"))
}

fn parse_mod_target(raw: &str) -> Result<ModTarget, String> {
    match raw {
        "self" | "agent" => Ok(ModTarget::AgentSelf),
        other => match other.split_once(':') {
            Some(("tool", name)) => Ok(ModTarget::Tool(name.to_string())),
            Some(("gateway", name)) => Ok(ModTarget::Gateway(name.to_string())),
            _ => Err(format!(
                "unknown target '{other}'; use 'self', 'tool:<name>', or 'gateway:<name>'"
            )),
        },
    }
}

fn parse_rollback_target(raw: &str) -> Result<RollbackTarget, String> {
    match raw {
        "self" | "agent" => Ok(RollbackTarget::AgentSelf),
        "system" | "all" => Ok(RollbackTarget::WholeSystem),
        other => match other.split_once(':') {
            Some(("tool", name)) => Ok(RollbackTarget::Tool(name.to_string())),
            Some(("gateway", name)) => Ok(RollbackTarget::Gateway(name.to_string())),
            _ => Err(format!(
                "unknown target '{other}'; use 'self', 'system', 'tool:<name>', or 'gateway:<name>'"
            )),
        },
    }
}

/// Renders a build result the way the model needs to read it: verdict first,
/// then the compiler's own words.
fn format_report(report: CompileReport) -> String {
    let mut out = String::new();
    if report.success {
        out.push_str(&format!(
            "BUILD OK — {} r{} in {:.1}s",
            report.slot,
            report
                .revision
                .map(|r| r.to_string())
                .unwrap_or_else(|| "?".into()),
            report.duration_ms as f64 / 1000.0
        ));
        if report.pending_swap {
            out.push_str("\nThe new version takes effect when this turn ends.");
        }
    } else {
        out.push_str(&format!(
            "BUILD FAILED — {} (unchanged, still running the previous revision)",
            report.slot
        ));
    }

    if !report.detail.is_empty() {
        out.push_str(&format!("\n{}", report.detail));
    }
    if !report.stderr.is_empty() {
        out.push_str(&format!("\n\n{}", report.stderr));
    }
    out
}

/// One setting on a line, marked where it cannot be changed.
fn format_setting(entry: &ConfigEntry) -> String {
    let mut line = format!("{} = {}", entry.key, entry.value);
    if !entry.editable {
        line.push_str("   [read-only]");
    } else if !entry.live {
        line.push_str("   [needs restart]");
    }
    line
}

/// One dependency on a line, in the shape it takes in the manifest.
fn format_dependency(dep: &Dependency) -> String {
    let mut line = format!("{} = \"{}\"", dep.name, dep.version);
    if !dep.features.is_empty() {
        line.push_str(&format!("   features: {}", dep.features.join(", ")));
    }
    if !dep.default_features {
        line.push_str("   [no default features]");
    }
    line
}

/// A directory listing, sized and marked so it reads at a glance.
fn format_listing(entries: Vec<FsEntry>) -> String {
    if entries.is_empty() {
        return "[empty directory]".to_string();
    }
    entries
        .iter()
        .map(|e| {
            if e.is_dir {
                format!("{}/", e.name)
            } else {
                format!("{}  ({})", e.name, human_size(e.size))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn format_terminal(result: TerminalOutput) -> String {
    let mut out = String::new();
    if result.truncated {
        out.push_str("[earlier output trimmed]\n");
    }
    if result.output.trim().is_empty() {
        out.push_str("[no output]");
    } else {
        out.push_str(&result.output);
    }
    if result.timed_out {
        out.push_str(
            "\n\n[still running: the timeout elapsed. Use terminal_read to collect the rest.]",
        );
    }
    out
}

fn format_exec(result: ExecResult) -> String {
    let mut out = String::new();
    if result.timed_out {
        out.push_str("[timed out]\n");
    }
    if !result.stdout.is_empty() {
        out.push_str(&result.stdout);
    }
    if !result.stderr.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[stderr]\n{}", result.stderr));
    }
    if result.exit_code != 0 {
        out.push_str(&format!("\n[exit {}]", result.exit_code));
    }
    if result.truncated {
        out.push_str("\n[output truncated]");
    }
    if out.is_empty() {
        out.push_str("[no output]");
    }
    out
}
