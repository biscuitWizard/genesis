//! The harness: everything the orchestrator knows how to do, in one place.
//!
//! Host imports, session actors, and the web layer all reach the system through
//! an `Arc<Harness>`. It owns the database, the LLM client, the component
//! registry, and the event fan-out to connected browsers.

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::bindings::types::{Attachment, OutboundEvent, SessionEvent, ToolManifest, TurnStats};
use crate::builder::Builder;
use crate::config::Config;
use crate::llm::LlmClient;
use crate::loader::Loader;
use crate::revisions::Revisions;
use crate::runtime::{Budget, Caps, Runtime};
use crate::session::SessionActors;
use crate::slot::Slot;
use crate::store::Store;
use crate::watchdog::Breakers;

/// Why a turn ended badly.
///
/// The distinction matters: a reported error usually means something outside
/// the agent went wrong (the model refused, the key is missing), while a trap
/// means this revision of the agent is itself faulty — only traps should count
/// against its circuit breaker.
#[derive(Debug, Clone)]
pub enum TurnError {
    Reported(String),
    Trapped(String),
}

impl TurnError {
    pub fn message(&self) -> &str {
        match self {
            TurnError::Reported(m) | TurnError::Trapped(m) => m,
        }
    }

    pub fn is_trap(&self) -> bool {
        matches!(self, TurnError::Trapped(_))
    }
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnError::Reported(m) => write!(f, "{m}"),
            TurnError::Trapped(m) => write!(f, "the agent trapped: {m}"),
        }
    }
}

/// A session event already rendered to a wire frame by the gateway.
#[derive(Debug, Clone)]
pub struct RenderedFrame {
    pub session_id: String,
    pub frame: String,
}

pub struct Harness {
    pub cfg: Arc<Config>,
    pub db: Arc<Store>,
    pub llm: Arc<LlmClient>,
    pub loader: Arc<Loader>,
    pub runtime: Arc<Runtime>,
    pub revisions: Arc<Revisions>,
    pub builder: Arc<Builder>,
    /// Raw events, consumed by the renderer task.
    pub events_tx: broadcast::Sender<OutboundEvent>,
    /// Rendered frames, consumed by websocket connections.
    pub frames_tx: broadcast::Sender<RenderedFrame>,
    pub sessions: SessionActors,
    pub breakers: Breakers,
    pub terminals: crate::terminal::Terminals,
    /// Slots whose file-change events should be ignored until the given time.
    /// A rollback rewrites the source tree, and without this the watcher would
    /// treat its own restore as a fresh edit and rebuild over it.
    watch_suppressed_until: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// Manifests of the loaded tool components, captured when each was
    /// validated. Cached because the agent reads the whole list on every loop
    /// iteration, and calling into each tool for it would be wasteful.
    tool_manifests: std::sync::RwLock<std::collections::HashMap<String, ToolManifest>>,
    /// Slots with a build in flight.
    ///
    /// Builds serialize on one lock, so an agent that keeps asking for the same
    /// slot would otherwise queue work behind a build that is already going to
    /// supersede it. Refusing the duplicate keeps the queue bounded no matter
    /// how the agent loops.
    building: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Marks a slot as building for as long as it is held.
pub struct BuildGuard {
    harness: Arc<Harness>,
    slot: String,
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.harness.building.lock() {
            in_flight.remove(&self.slot);
        }
    }
}

impl Harness {
    pub fn new(cfg: Arc<Config>, runtime: Arc<Runtime>) -> Result<Arc<Self>> {
        let db = Arc::new(Store::open(&cfg.db_path())?);
        let llm = Arc::new(LlmClient::new(cfg.clone())?);
        let (events_tx, _) = broadcast::channel(1024);
        let (frames_tx, _) = broadcast::channel(1024);

        let revisions = Arc::new(Revisions::new(cfg.clone(), db.clone()));
        let cfg_for_breakers = cfg.clone();

        Ok(Arc::new(Self {
            cfg,
            db,
            llm,
            loader: Arc::new(Loader::new()),
            runtime,
            revisions,
            builder: Arc::new(Builder::new()),
            events_tx,
            frames_tx,
            sessions: SessionActors::new(),
            terminals: crate::terminal::Terminals::new(),
            breakers: Breakers::new(
                cfg_for_breakers.watchdog.failure_window,
                cfg_for_breakers.watchdog.failure_threshold,
            ),
            watch_suppressed_until: std::sync::Mutex::new(std::collections::HashMap::new()),
            tool_manifests: std::sync::RwLock::new(std::collections::HashMap::new()),
            building: std::sync::Mutex::new(std::collections::HashSet::new()),
        }))
    }

    /// Claims the right to build a slot, or `None` if one is already running.
    pub fn begin_build(self: &Arc<Self>, slot: &Slot) -> Option<BuildGuard> {
        let mut in_flight = self.building.lock().ok()?;
        if !in_flight.insert(slot.key()) {
            return None;
        }
        Some(BuildGuard {
            harness: self.clone(),
            slot: slot.key(),
        })
    }

    // --- events ------------------------------------------------------------

    /// Persists an event and publishes it to connected clients.
    pub fn append_event(&self, session_id: &str, event: SessionEvent) -> Result<u64> {
        let record = self.db.append_event(session_id, event)?;
        let _ = self.events_tx.send(OutboundEvent {
            session_id: session_id.to_string(),
            seq: Some(record.seq),
            ts_ms: record.ts_ms,
            event: record.event,
        });
        Ok(record.seq)
    }

    /// Publishes without persisting. Used for streaming token deltas, which
    /// would otherwise bloat the log with thousands of fragments.
    pub fn publish_transient(&self, session_id: &str, event: SessionEvent) {
        let _ = self.events_tx.send(OutboundEvent {
            session_id: session_id.to_string(),
            seq: None,
            ts_ms: crate::store::now_ms(),
            event,
        });
    }

    // --- turns -------------------------------------------------------------

    /// Runs one agentic turn to completion inside a fresh store.
    ///
    /// A trap (guest panic, memory limit, blown budget) surfaces here as an
    /// `Err`, never as a process failure.
    pub async fn run_turn(self: &Arc<Self>, session_id: &str) -> Result<TurnStats, TurnError> {
        let loaded = self.loader.get(&Slot::Agent).ok_or_else(|| {
            TurnError::Reported("no agent component is loaded".to_string())
        })?;

        let budget = Budget::new(format!("agent turn ({session_id})"), self.cfg.wasm_slice);
        let mut store = self.runtime.new_store(
            self.clone(),
            Caps::Agent,
            budget,
            Some(session_id.to_string()),
        );

        let result = async {
            let agent = crate::bindings::agent::Agent::instantiate_async(
                &mut store,
                &loaded.component,
                self.runtime.linker(Caps::Agent),
            )
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating agent")?;

            agent
                .call_handle_turn(&mut store, session_id)
                .await
                .map_err(anyhow::Error::from)
                .context("calling handle-turn")
        }
        .await;

        match result {
            Ok(Ok(stats)) => Ok(stats),
            // The agent ran correctly and is telling us something went wrong.
            Ok(Err(msg)) => Err(TurnError::Reported(msg)),
            // The agent itself misbehaved: panic, blown budget, memory limit.
            Err(trap) => Err(TurnError::Trapped(format!("{trap:#}"))),
        }
    }

    /// Asks the agent which tools it would offer for this session's mode.
    ///
    /// The agent owns its tool surface, so this is a question rather than a
    /// guess — the panel can never drift from what the model actually sees.
    pub async fn agent_tools(self: &Arc<Self>, session_id: &str) -> Vec<ToolManifest> {
        let mode = self
            .db
            .get_session(session_id)
            .ok()
            .flatten()
            .map(|m| m.mode)
            .unwrap_or_else(|| self.cfg.default_mode.clone());

        let Some(loaded) = self.loader.get(&Slot::Agent) else {
            return Vec::new();
        };

        let budget = Budget::probe("agent list-tools", self.cfg.probe_budget);
        let mut store = self
            .runtime
            .new_store(self.clone(), Caps::Agent, budget, Some(session_id.to_string()));

        let result = async {
            let agent = crate::bindings::agent::Agent::instantiate_async(
                &mut store,
                &loaded.component,
                self.runtime.linker(Caps::Agent),
            )
            .await?;
            agent.call_list_tools(&mut store, &mode).await
        }
        .await;

        match result {
            Ok(tools) => tools,
            Err(e) => {
                tracing::warn!(error = %e, "agent could not list its tools");
                Vec::new()
            }
        }
    }

    // --- sessions ----------------------------------------------------------

    /// Routes a user message into a session, starting a turn or nudging one
    /// that is already running.
    pub fn submit(
        self: &Arc<Self>,
        session_id: &str,
        message: String,
        attachments: Vec<Attachment>,
    ) -> Result<()> {
        if self.db.get_session(session_id)?.is_none() {
            return Err(anyhow!("no such session: {session_id}"));
        }
        if attachments.len() > self.cfg.max_attachments {
            return Err(anyhow!(
                "too many attachments: {} (limit {})",
                attachments.len(),
                self.cfg.max_attachments
            ));
        }
        for a in &attachments {
            // base64 inflates by 4/3; compare against the decoded size the
            // limit is expressed in.
            let decoded = a.data_base64.len() / 4 * 3;
            if decoded > self.cfg.max_attachment_bytes {
                return Err(anyhow!(
                    "attachment '{}' is {} bytes, over the {} byte limit",
                    a.name,
                    decoded,
                    self.cfg.max_attachment_bytes
                ));
            }
        }
        self.sessions.submit(self, session_id, message, attachments);
        Ok(())
    }

    pub fn cancel(self: &Arc<Self>, session_id: &str) {
        self.sessions.cancel(session_id);
    }

    // --- tools ---------------------------------------------------------------

    /// Records a tool's manifest, captured when the component passed validation.
    pub fn set_tool_manifest(&self, name: &str, manifest: ToolManifest) {
        if let Ok(mut map) = self.tool_manifests.write() {
            map.insert(name.to_string(), manifest);
        }
    }

    pub fn forget_tool(&self, name: &str) {
        if let Ok(mut map) = self.tool_manifests.write() {
            map.remove(name);
        }
    }

    /// Installs a component, keeping the tool registry in step with the loader.
    ///
    /// These two must never disagree: a tool that is loaded but unregistered is
    /// invisible to the model, which is indistinguishable from it not being
    /// installed at all. Routing every install through here is what makes that
    /// impossible to forget.
    pub async fn install_component(self: &Arc<Self>, component: Arc<crate::loader::LoadedComponent>) {
        let slot = component.slot.clone();
        self.loader.install(component);

        if let Slot::Tool(name) = &slot {
            match self.describe_tool(&slot).await {
                Ok(manifest) => self.set_tool_manifest(name, manifest),
                Err(e) => {
                    // Loaded but unusable: say so rather than leaving a tool
                    // that silently never appears.
                    tracing::warn!(%slot, error = %e, "tool loaded but its manifest could not be read");
                }
            }
        }
    }

    async fn describe_tool(self: &Arc<Self>, slot: &Slot) -> Result<ToolManifest> {
        let loaded = self
            .loader
            .get(slot)
            .ok_or_else(|| anyhow!("{slot} is not loaded"))?;

        let budget = Budget::probe(format!("{slot} describe"), self.cfg.probe_budget);
        let mut store = self
            .runtime
            .new_store(self.clone(), Caps::Tool, budget, None);

        let tool = crate::bindings::tool::Tool::instantiate_async(
            &mut store,
            &loaded.component,
            self.runtime.linker(Caps::Tool),
        )
        .await
        .map_err(anyhow::Error::from)?;

        tool.call_describe(&mut store)
            .await
            .map_err(anyhow::Error::from)
            .context("describe")
    }

    /// Manifests for every tool currently loaded, in a stable order so the
    /// model's tool list does not churn between requests.
    pub fn tool_registry(&self) -> Vec<ToolManifest> {
        let Ok(map) = self.tool_manifests.read() else {
            return Vec::new();
        };
        let loaded: std::collections::HashSet<String> = self
            .loader
            .tools()
            .into_iter()
            .filter_map(|s| match s {
                Slot::Tool(name) => Some(name),
                _ => None,
            })
            .collect();

        let mut out: Vec<ToolManifest> = map
            .iter()
            .filter(|(name, _)| loaded.contains(*name))
            .map(|(_, m)| m.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Runs one tool component in its own store.
    ///
    /// Failures come back as tool results rather than traps, so a broken tool
    /// interrupts a sentence, not the conversation.
    pub async fn invoke_tool(
        self: &Arc<Self>,
        name: &str,
        session_id: &str,
        args_json: &str,
    ) -> std::result::Result<String, String> {
        let slot = Slot::tool(name);
        let Some(loaded) = self.loader.get(&slot) else {
            return Err(format!("no tool named '{name}' is loaded"));
        };

        // Scoped to this tool: it is handed its own settings and never sees
        // another's.
        let config_json = self.cfg.tool_config_json(name);

        let budget = Budget::new(format!("tool {name}"), self.cfg.tool_budget);
        let mut store = self.runtime.new_store(
            self.clone(),
            Caps::Tool,
            budget,
            Some(session_id.to_string()),
        );

        let result = async {
            let tool = crate::bindings::tool::Tool::instantiate_async(
                &mut store,
                &loaded.component,
                self.runtime.linker(Caps::Tool),
            )
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating")?;

            tool.call_invoke(&mut store, session_id, args_json, &config_json)
                .await
                .map_err(anyhow::Error::from)
                .context("invoke")
        }
        .await;

        match result {
            Ok(Ok(output)) => Ok(self.truncate(output)),
            Ok(Err(message)) => Err(self.truncate(message)),
            Err(trap) => {
                // A trapping tool is a faulty revision; let the breaker see it.
                let detail = format!("{trap:#}");
                let harness = self.clone();
                let slot = slot.clone();
                let reported = detail.clone();
                tokio::spawn(async move {
                    crate::watchdog::report_failure(&harness, &slot, &reported).await;
                });
                Err(format!("tool '{name}' crashed: {detail}"))
            }
        }
    }

    /// Caps anything headed for the model's context window.
    pub fn truncate(&self, text: String) -> String {
        let limit = self.cfg.max_tool_output_bytes;
        if text.len() <= limit {
            return text;
        }
        let mut cut = limit;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!(
            "{}\n\n[truncated: {} of {} bytes shown]",
            &text[..cut],
            cut,
            text.len()
        )
    }

    // --- watcher suppression ------------------------------------------------

    /// Tells the file watcher to ignore this slot for a while, because the
    /// orchestrator is about to rewrite its source itself.
    pub fn suppress_watch(&self, slot: &Slot, window: std::time::Duration) {
        if let Ok(mut map) = self.watch_suppressed_until.lock() {
            map.insert(slot.key(), std::time::Instant::now() + window);
        }
    }

    pub fn watch_suppressed(&self, slot: &Slot) -> bool {
        let Ok(mut map) = self.watch_suppressed_until.lock() else {
            return false;
        };
        match map.get(&slot.key()) {
            Some(until) if *until > std::time::Instant::now() => true,
            Some(_) => {
                map.remove(&slot.key());
                false
            }
            None => false,
        }
    }
}
