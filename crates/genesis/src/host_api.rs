//! Implementations of every host import.
//!
//! This module is the entire attack surface a guest has against the system, so
//! each function validates its arguments, scopes access to the session the call
//! was made for, and caps the size of anything it hands back.

use wasmtime::Result;

/// Bridges the orchestrator's `anyhow` errors into wasmtime's error type.
/// A host error becomes a trap, which the caller catches per-call — it never
/// escalates into a process failure.
trait IntoWasmtime<T> {
    fn wt(self) -> Result<T>;
}

impl<T> IntoWasmtime<T> for anyhow::Result<T> {
    fn wt(self) -> Result<T> {
        self.map_err(wasmtime::Error::from_anyhow)
    }
}

fn err(msg: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(msg.into())
}

use crate::bindings::types::{
    Attachment, CompileReport, ConfigEntry, Dependency, EventRecord, ExecResult, FsEntry, InboxItem,
    LlmError, LogLevel, ModeInfo, ModTarget, ModelInfo, RevisionInfo, RollbackTarget, SessionEvent,
    SessionMeta, SkillInfo, StreamChunk, TerminalInfo, TerminalOutput, ToolManifest,
};
use crate::bindings::{
    configuration, control, devkit, hostfs, llm, sandbox, session, sys, terminal, tooling,
};
use crate::harness::Harness;
use crate::runtime::HostState;

impl HostState {
    /// Rejects attempts to touch a session other than the one this call is for.
    ///
    /// Gateway calls run unscoped (`session_id == None`) because managing every
    /// session is their job; agent turns are pinned to a single session.
    fn scope_ok(&self, session_id: &str) -> Result<()> {
        match &self.session_id {
            Some(mine) if mine != session_id => Err(err(format!(
                "session {session_id} is out of scope for this call (scoped to {mine})"
            ))),
            _ => Ok(()),
        }
    }

    fn harness(&self) -> &Harness {
        &self.harness
    }

    /// Blanks a session's model override when that model is no longer offered.
    ///
    /// A conversation pinned to a model that has since been removed from the
    /// catalogue would otherwise keep sending an id the provider rejects. The
    /// stored value is left alone; reporting it as unset means both the agent
    /// and the chat surface see what will actually be used.
    fn drop_unavailable_model(&self, meta: &mut SessionMeta) {
        if meta.model.is_empty() {
            return;
        }
        let offered = self.harness.cfg.models.iter().any(|m| m.id == meta.model);
        if !offered {
            tracing::debug!(
                session = %meta.id,
                model = %meta.model,
                "session pinned to a model that is no longer configured; using the default"
            );
            meta.model.clear();
        }
    }

    /// Notes that this call changed a component, so the caller can tell the
    /// model when the change takes effect.
    fn note_pending_swap(&mut self, target: &ModTarget) {
        if let ModTarget::AgentSelf = target {
            if !self.pending_swaps.contains(&crate::slot::Slot::Agent) {
                self.pending_swaps.push(crate::slot::Slot::Agent);
            }
        }
    }
}

// --- sys -------------------------------------------------------------------

impl sys::Host for HostState {
    async fn log(&mut self, level: LogLevel, msg: String) -> Result<()> {
        let msg = msg.chars().take(4096).collect::<String>();
        match level {
            LogLevel::Trace => tracing::trace!(target: "guest", "{msg}"),
            LogLevel::Debug => tracing::debug!(target: "guest", "{msg}"),
            LogLevel::Info => tracing::info!(target: "guest", "{msg}"),
            LogLevel::Warn => tracing::warn!(target: "guest", "{msg}"),
            LogLevel::Error => tracing::error!(target: "guest", "{msg}"),
        }
        Ok(())
    }

    async fn now_ms(&mut self) -> Result<u64> {
        Ok(crate::store::now_ms())
    }

    async fn kv_get(&mut self, scope: String, key: String) -> Result<Option<String>> {
        if scope != "global" {
            self.scope_ok(&scope)?;
        }
        self.harness().db.kv_get(&scope, &key).wt()
    }

    async fn kv_put(&mut self, scope: String, key: String, value: String) -> Result<()> {
        if scope != "global" {
            self.scope_ok(&scope)?;
        }
        if value.len() > 1 << 20 {
            return Err(err("kv value exceeds 1 MiB"));
        }
        self.harness().db.kv_put(&scope, &key, &value).wt()?;
        Ok(())
    }

    /// Non-secret configuration. Secrets are deliberately unreachable: the
    /// OpenRouter key never crosses this boundary.
    async fn config_get(&mut self, key: String) -> Result<Option<String>> {
        let cfg = &self.harness().cfg;
        Ok(match key.as_str() {
            "model" => Some(cfg.model.clone()),
            "system_prompt" => Some(cfg.system_prompt.clone()),
            "max_iterations" => Some(cfg.max_iterations.to_string()),
            "max_tool_output_bytes" => Some(cfg.max_tool_output_bytes.to_string()),
            "sandbox_available" => Some(cfg.sandbox_available.to_string()),
            // The dev kit is wired up; the agent uses this to decide whether to
            // offer itself the self-modification tools.
            "devkit_available" => Some(cfg.devkit.enabled.to_string()),
            _ => None,
        })
    }

    async fn list_models(&mut self) -> Result<Vec<ModelInfo>> {
        Ok(self
            .harness()
            .cfg
            .models
            .iter()
            .map(|m| ModelInfo {
                id: m.id.clone(),
                label: m.label.clone(),
            })
            .collect())
    }

    async fn list_modes(&mut self) -> Result<Vec<ModeInfo>> {
        Ok(self
            .harness()
            .cfg
            .modes
            .iter()
            .map(|m| ModeInfo {
                id: m.id.clone(),
                label: m.label.clone(),
                description: m.description.clone(),
                read_only: m.read_only,
                prompt: m.prompt.clone(),
            })
            .collect())
    }
}

// --- session ---------------------------------------------------------------

impl session::Host for HostState {
    async fn events(&mut self, session_id: String, from_seq: u64) -> Result<Vec<EventRecord>> {
        self.scope_ok(&session_id)?;
        self.harness().db.events(&session_id, from_seq).wt()
    }

    async fn append(&mut self, session_id: String, event: SessionEvent) -> Result<u64> {
        self.scope_ok(&session_id)?;
        self.harness().append_event(&session_id, event).wt()
    }

    async fn emit_output(&mut self, session_id: String, chunk: String) -> Result<()> {
        self.scope_ok(&session_id)?;
        self.harness()
            .publish_transient(&session_id, SessionEvent::StreamDelta(chunk));
        Ok(())
    }

    async fn poll_inbox(&mut self, session_id: String) -> Result<Vec<InboxItem>> {
        self.scope_ok(&session_id)?;
        let items = self.harness().sessions.drain_inbox(&session_id);
        // A cancel request must also stop the guest even if it ignores the
        // item, so arm the budget's cancellation flag.
        if items.iter().any(|i| matches!(i, InboxItem::Cancel)) {
            self.budget.cancelled = true;
        }
        Ok(items)
    }

    async fn list_sessions(&mut self, include_archived: bool) -> Result<Vec<SessionMeta>> {
        let mut sessions = self.harness().db.list_sessions(include_archived).wt()?;
        for meta in &mut sessions {
            self.drop_unavailable_model(meta);
        }
        Ok(sessions)
    }

    async fn get_session(&mut self, session_id: String) -> Result<Option<SessionMeta>> {
        let mut meta = self.harness().db.get_session(&session_id).wt()?;
        if let Some(meta) = meta.as_mut() {
            self.drop_unavailable_model(meta);
        }
        Ok(meta)
    }

    async fn create_session(&mut self, title: Option<String>) -> Result<String> {
        let mode = self.harness().cfg.default_mode.clone();
        self.harness()
            .db
            .create_session(title, &mode)
            .map(|s| s.id)
            .wt()
    }

    async fn rename_session(&mut self, session_id: String, title: String) -> Result<()> {
        self.harness().db.rename_session(&session_id, &title).wt()?;
        Ok(())
    }

    async fn archive_session(&mut self, session_id: String, archived: bool) -> Result<()> {
        self.harness().db.archive_session(&session_id, archived).wt()?;
        Ok(())
    }

    async fn submit(
        &mut self,
        session_id: String,
        message: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        let harness = self.harness.clone();
        harness.submit(&session_id, message, attachments).wt()?;
        Ok(())
    }

    async fn set_session_mode(&mut self, session_id: String, mode: String) -> Result<()> {
        // Only offered modes are accepted, so a guest cannot invent one the
        // agent has no handling for.
        let known = self.harness().cfg.mode(&mode).is_some();
        if !known {
            return Err(err(format!("unknown mode: {mode}")));
        }
        self.harness().db.set_mode(&session_id, &mode).wt()?;
        Ok(())
    }

    async fn list_skills(&mut self, session_id: String) -> Result<Vec<SkillInfo>> {
        let harness = self.harness();
        let enabled = crate::skills::enabled_ids(&harness.db, &session_id);

        Ok(crate::skills::discover(&harness.cfg.paths.skills)
            .into_iter()
            .map(|s| SkillInfo {
                enabled: enabled.iter().any(|id| id == &s.id),
                id: s.id,
                name: s.name,
                description: s.description,
                instructions: s.instructions,
            })
            .collect())
    }

    async fn set_skill_enabled(
        &mut self,
        session_id: String,
        skill_id: String,
        enabled: bool,
    ) -> Result<()> {
        let harness = self.harness();
        // Only a skill that exists may be attached, so a stale id cannot leave
        // an entry the panel can never clear.
        let known = crate::skills::discover(&harness.cfg.paths.skills)
            .iter()
            .any(|s| s.id == skill_id);
        if !known {
            return Err(err(format!("unknown skill: {skill_id}")));
        }
        crate::skills::set_enabled(&harness.db, &session_id, &skill_id, enabled).wt()
    }

    async fn available_tools(&mut self, session_id: String) -> Result<Vec<ToolManifest>> {
        let harness = self.harness.clone();
        let tools = harness.agent_tools(&session_id).await;
        self.yielded();
        Ok(tools)
    }

    async fn set_session_model(&mut self, session_id: String, model: String) -> Result<()> {
        let known = model.is_empty()
            || self.harness().cfg.models.iter().any(|m| m.id == model);
        if !known {
            return Err(err(format!("unknown model: {model}")));
        }
        self.harness().db.set_model(&session_id, &model).wt()?;
        Ok(())
    }
}

// --- llm -------------------------------------------------------------------

impl HostState {
    /// Refuses a call that would push the session past its spend ceiling.
    fn check_budget(&self) -> std::result::Result<(), LlmError> {
        let limit = self.harness.cfg.session_spend_limit_usd;
        if limit <= 0.0 {
            return Ok(());
        }
        let Some(sid) = &self.session_id else {
            return Ok(());
        };
        let spent = self.harness.db.get_spend(sid).unwrap_or(0.0);
        if spent >= limit {
            return Err(LlmError::Budget(format!(
                "session has spent ${spent:.4} of its ${limit:.4} limit"
            )));
        }
        Ok(())
    }

    fn record_usage(&self, chunk: &StreamChunk) {
        let (StreamChunk::Finished(info), Some(sid)) = (chunk, &self.session_id) else {
            return;
        };
        if let Some(usage) = &info.usage {
            if usage.cost_usd > 0.0 {
                let _ = self.harness.db.add_spend(sid, usage.cost_usd);
            }
        }
    }
}

impl llm::Host for HostState {
    async fn chat(&mut self, request_json: String) -> Result<std::result::Result<String, LlmError>> {
        if let Err(e) = self.check_budget() {
            return Ok(Err(e));
        }
        let llm = self.harness.llm.clone();
        let result = llm.chat(&request_json).await;
        self.yielded();
        Ok(result)
    }

    async fn stream_open(
        &mut self,
        request_json: String,
    ) -> Result<std::result::Result<u64, LlmError>> {
        if let Err(e) = self.check_budget() {
            return Ok(Err(e));
        }
        let llm = self.harness.llm.clone();
        let opened = llm.open_stream(&request_json).await;
        self.yielded();

        match opened {
            Ok(handle) => {
                let id = self.next_stream_id;
                self.next_stream_id += 1;
                self.streams.insert(id, handle);
                Ok(Ok(id))
            }
            Err(e) => Ok(Err(e)),
        }
    }

    async fn stream_next(
        &mut self,
        stream_id: u64,
    ) -> Result<std::result::Result<StreamChunk, LlmError>> {
        let chunk = {
            let Some(handle) = self.streams.get_mut(&stream_id) else {
                return Ok(Err(LlmError::BadRequest(format!(
                    "unknown stream id {stream_id}"
                ))));
            };
            handle.next().await
        };
        // Time spent waiting on the model is not the guest spinning.
        self.yielded();

        if let Ok(chunk) = &chunk {
            self.record_usage(chunk);
        }
        Ok(chunk)
    }

    async fn stream_close(&mut self, stream_id: u64) -> Result<()> {
        self.streams.remove(&stream_id);
        Ok(())
    }
}

// --- sandbox (M3) ----------------------------------------------------------

const SANDBOX_UNAVAILABLE: &str =
    "the docker exec sandbox is not configured; code execution is unavailable";

impl sandbox::Host for HostState {
    async fn exec(
        &mut self,
        _session_id: String,
        _command: String,
        _stdin: Option<String>,
        _timeout_ms: u32,
    ) -> Result<ExecResult> {
        Ok(ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: SANDBOX_UNAVAILABLE.to_string(),
            timed_out: false,
            truncated: false,
        })
    }

    async fn write_file(
        &mut self,
        _session_id: String,
        _path: String,
        _contents: String,
    ) -> Result<std::result::Result<(), String>> {
        Ok(Err(SANDBOX_UNAVAILABLE.to_string()))
    }

    async fn read_file(
        &mut self,
        _session_id: String,
        _path: String,
    ) -> Result<std::result::Result<String, String>> {
        Ok(Err(SANDBOX_UNAVAILABLE.to_string()))
    }

    async fn list_files(
        &mut self,
        _session_id: String,
        _path: String,
    ) -> Result<std::result::Result<Vec<String>, String>> {
        Ok(Err(SANDBOX_UNAVAILABLE.to_string()))
    }

    async fn available(&mut self) -> Result<bool> {
        Ok(false)
    }
}

// --- tooling (M3) ----------------------------------------------------------

impl tooling::Host for HostState {
    async fn registry(&mut self) -> Result<Vec<ToolManifest>> {
        Ok(self.harness().tool_registry())
    }

    async fn invoke(
        &mut self,
        name: String,
        session_id: String,
        args_json: String,
    ) -> Result<std::result::Result<String, String>> {
        self.scope_ok(&session_id)?;
        let harness = self.harness.clone();
        let result = harness.invoke_tool(&name, &session_id, &args_json).await;
        // Running a tool can take real time; that is not the agent spinning.
        self.yielded();
        Ok(result)
    }

    async fn mcp_list_tools(&mut self) -> Result<Vec<ToolManifest>> {
        Ok(Vec::new())
    }

    async fn mcp_call_tool(
        &mut self,
        name: String,
        _args_json: String,
    ) -> Result<std::result::Result<String, String>> {
        Ok(Err(format!("no mcp server provides {name}")))
    }
}

// --- devkit ----------------------------------------------------------------

impl devkit::Host for HostState {
    async fn new_tool(&mut self, name: String, description: String) -> Result<CompileReport> {
        let harness = self.harness.clone();
        let report = crate::devkit::new_tool(&harness, &name, &description).await;
        // Compiling is slow by nature; do not charge it to the guest's budget.
        self.yielded();
        Ok(report)
    }

    async fn write_file(
        &mut self,
        target: ModTarget,
        path: String,
        contents: String,
    ) -> Result<CompileReport> {
        let harness = self.harness.clone();
        let report = crate::devkit::write_file(&harness, &target, &path, &contents).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn patch_file(
        &mut self,
        target: ModTarget,
        path: String,
        old_text: String,
        new_text: String,
    ) -> Result<CompileReport> {
        let harness = self.harness.clone();
        let report =
            crate::devkit::patch_file(&harness, &target, &path, &old_text, &new_text).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn add_dependency(
        &mut self,
        target: ModTarget,
        dep: Dependency,
    ) -> Result<CompileReport> {
        let harness = self.harness.clone();
        let dep = crate::manifest::Dependency {
            name: dep.name,
            version: dep.version,
            features: dep.features,
            default_features: dep.default_features,
        };
        let report = crate::devkit::add_dependency(&harness, &target, &dep).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn remove_dependency(
        &mut self,
        target: ModTarget,
        name: String,
    ) -> Result<CompileReport> {
        let harness = self.harness.clone();
        let report = crate::devkit::remove_dependency(&harness, &target, &name).await;
        self.yielded();
        self.note_pending_swap(&target);
        Ok(report)
    }

    async fn list_dependencies(
        &mut self,
        target: ModTarget,
    ) -> Result<std::result::Result<Vec<Dependency>, String>> {
        let harness = self.harness.clone();
        Ok(
            crate::devkit::list_dependencies(&harness, &target).map(|deps| {
                deps.into_iter()
                    .map(|d| Dependency {
                        name: d.name,
                        version: d.version,
                        features: d.features,
                        default_features: d.default_features,
                    })
                    .collect()
            }),
        )
    }

    async fn read_file(
        &mut self,
        target: ModTarget,
        path: String,
    ) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        Ok(crate::devkit::read_file(&harness, &target, &path).map(|text| harness.truncate(text)))
    }

    async fn list_files(
        &mut self,
        target: ModTarget,
    ) -> Result<std::result::Result<Vec<String>, String>> {
        let harness = self.harness.clone();
        Ok(crate::devkit::list_files(&harness, &target))
    }

    async fn rollback(
        &mut self,
        target: RollbackTarget,
        revision: Option<u64>,
    ) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();

        let result = match &target {
            RollbackTarget::WholeSystem => {
                // Without an explicit id, the most recent snapshot before the
                // current one is what "undo the last change" means.
                let snapshots = harness.revisions.snapshots().unwrap_or_default();
                let chosen = match revision {
                    Some(id) => Some(id),
                    None => snapshots
                        .iter()
                        .rev()
                        .nth(1)
                        .map(|s| s.id),
                };
                match chosen {
                    Some(id) => crate::pipeline::rollback_system(&harness, id).await,
                    None => Err(anyhow::anyhow!("there is no earlier system snapshot")),
                }
            }
            other => {
                let slot = match other {
                    RollbackTarget::AgentSelf => crate::slot::Slot::Agent,
                    RollbackTarget::Tool(n) => crate::slot::Slot::tool(n),
                    RollbackTarget::Gateway(n) => crate::slot::Slot::gateway(n),
                    RollbackTarget::WholeSystem => unreachable!(),
                };
                crate::pipeline::rollback_slot(&harness, &slot, revision).await
            }
        };
        self.yielded();

        Ok(result.map_err(|e| format!("{e:#}")))
    }

    async fn history(&mut self, target: RollbackTarget) -> Result<Vec<RevisionInfo>> {
        let harness = self.harness.clone();

        let rows = match &target {
            RollbackTarget::WholeSystem => harness
                .revisions
                .snapshots()
                .unwrap_or_default()
                .into_iter()
                .map(|s| RevisionInfo {
                    slot: "system".to_string(),
                    revision: s.id,
                    status: String::new(),
                    origin: "snapshot".to_string(),
                    note: format!(
                        "{} [{}]",
                        s.cause,
                        s.slots
                            .iter()
                            .map(|(k, v)| format!("{k}=r{v:04}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    created_ms: s.created_ms,
                })
                .collect(),
            other => {
                let slot = match other {
                    RollbackTarget::AgentSelf => crate::slot::Slot::Agent,
                    RollbackTarget::Tool(n) => crate::slot::Slot::tool(n),
                    RollbackTarget::Gateway(n) => crate::slot::Slot::gateway(n),
                    RollbackTarget::WholeSystem => unreachable!(),
                };
                harness
                    .revisions
                    .history(&slot)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| RevisionInfo {
                        slot: r.slot,
                        revision: r.revision,
                        status: r.status.label().to_string(),
                        origin: r.origin.label().to_string(),
                        note: r.note,
                        created_ms: r.created_ms,
                    })
                    .collect()
            }
        };

        Ok(rows)
    }
}

// --- host filesystem --------------------------------------------------------

impl hostfs::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        Ok(self.harness().cfg.filesystem.enabled)
    }

    async fn read_file(&mut self, path: String) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        Ok(crate::hostfs::read_file(&harness.cfg, &path)
            .map(|text| harness.truncate(text))
            .map_err(|e| format!("{e:#}")))
    }

    async fn write_file(
        &mut self,
        path: String,
        contents: String,
    ) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        Ok(crate::hostfs::write_file(&harness.cfg, &path, &contents).map_err(|e| format!("{e:#}")))
    }

    async fn list_dir(&mut self, path: String) -> Result<std::result::Result<Vec<FsEntry>, String>> {
        let harness = self.harness.clone();
        Ok(crate::hostfs::list_dir(&harness.cfg, &path).map_err(|e| format!("{e:#}")))
    }

    async fn delete_path(
        &mut self,
        path: String,
        recursive: bool,
    ) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        let result = crate::hostfs::delete_path(&harness.cfg, &path, recursive);
        if let Ok(message) = &result {
            // Deletions are worth a line in the log whoever asked for them.
            tracing::warn!(%path, "agent deleted a path: {message}");
        }
        Ok(result.map_err(|e| format!("{e:#}")))
    }
}

// --- terminals --------------------------------------------------------------

impl terminal::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        Ok(self.harness().cfg.terminal.enabled)
    }

    async fn open(&mut self, cwd: Option<String>) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        let result = harness
            .terminals
            .open(&harness.cfg, cwd.as_deref())
            .await
            .map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn run(
        &mut self,
        id: String,
        command: String,
        timeout_ms: u32,
    ) -> Result<std::result::Result<TerminalOutput, String>> {
        let harness = self.harness.clone();
        let timeout = if timeout_ms == 0 {
            harness.cfg.terminal.default_timeout
        } else {
            std::time::Duration::from_millis(timeout_ms as u64)
        };

        tracing::info!(terminal = %id, %command, "running a command");
        let result = harness
            .terminals
            .run(&harness.cfg, &id, &command, timeout)
            .await
            .map_err(|e| format!("{e:#}"));
        // A command can legitimately take a long time; that is not the guest
        // spinning, so the budget's spin timer restarts here.
        self.yielded();
        Ok(result)
    }

    async fn read(&mut self, id: String) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        Ok(harness
            .terminals
            .read(&id)
            .await
            .map(|text| harness.truncate(text))
            .map_err(|e| format!("{e:#}")))
    }

    async fn close(&mut self, id: String) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        let result = harness.terminals.close(&id).await.map_err(|e| format!("{e:#}"));
        self.yielded();
        Ok(result)
    }

    async fn sessions(&mut self) -> Result<Vec<TerminalInfo>> {
        let harness = self.harness.clone();
        let list = harness.terminals.list().await;
        self.yielded();
        Ok(list)
    }
}

// --- process control --------------------------------------------------------

impl control::Host for HostState {
    async fn available(&mut self) -> Result<bool> {
        Ok(self.harness().cfg.control.allow_restart)
    }

    async fn restart(
        &mut self,
        reason: String,
        resume: bool,
    ) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        let session = self.session_id.clone();
        Ok(
            crate::control::request_restart(&harness, &reason, resume, session.as_deref())
                .map_err(|e| format!("{e:#}")),
        )
    }
}

// --- configuration ----------------------------------------------------------

fn entry(setting: crate::settings::Setting) -> ConfigEntry {
    ConfigEntry {
        key: setting.key,
        value: setting.value,
        editable: setting.editable,
        live: setting.live,
    }
}

impl configuration::Host for HostState {
    async fn settings(&mut self, prefix: Option<String>) -> Result<Vec<ConfigEntry>> {
        let harness = self.harness();
        Ok(crate::settings::list(&harness.cfg, prefix.as_deref())
            .unwrap_or_default()
            .into_iter()
            .map(entry)
            .collect())
    }

    async fn get(&mut self, key: String) -> Result<Option<ConfigEntry>> {
        let harness = self.harness();
        Ok(crate::settings::get(&harness.cfg, &key)
            .ok()
            .flatten()
            .map(entry))
    }

    async fn set(&mut self, key: String, value: String) -> Result<std::result::Result<String, String>> {
        let harness = self.harness.clone();
        let result = crate::settings::set(&harness.cfg, &key, &value);
        self.yielded();
        Ok(result.map_err(|e| format!("{e:#}")))
    }
}
