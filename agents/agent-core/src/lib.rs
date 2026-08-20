//! The Genesis agent.
//!
//! This is the loop the harness exists to run — and the code the agent can
//! rewrite. It holds no state between turns: every turn rehydrates the
//! conversation from the session log, so a crash, a hot swap, or an
//! orchestrator restart costs nothing.
//!
//! The shape of one turn:
//!   rehydrate -> assemble context -> stream a completion -> dispatch tool
//!   calls -> check the inbox for nudges -> repeat until the model stops.

wit_bindgen::generate!({
    world: "agent",
    path: "../../wit",
    generate_all,
});

use genesis::harness::llm;
use genesis::harness::session as host;
use genesis::harness::sys;
use genesis::harness::types::{
    AssistantMsg, InboxItem, LlmError, LogLevel, SessionEvent, StreamChunk, TokenUsage, ToolCall,
    ToolOutcome, UserMsg,
};
// `tool-manifest` comes in via the world's own `use types.{...}`.
use serde_json::{json, Value};

mod tools;

struct Component;

/// Hard ceiling on loop iterations, in case configuration is missing or absurd.
const ABSOLUTE_MAX_ITERATIONS: u32 = 64;

impl Guest for Component {
    fn health() -> String {
        "ok".to_string()
    }

    fn describe() -> AgentManifest {
        AgentManifest {
            name: "agent-core".to_string(),
            version_note: "streaming loop with nudges and memory tools".to_string(),
            skills: tools::available(tools::DEFAULT_MODE)
                .iter()
                .map(|t| t.name.to_string())
                .collect(),
        }
    }

    fn list_tools(mode: String) -> Vec<ToolManifest> {
        tools::manifests(&mode)
    }

    fn handle_turn(session_id: String) -> Result<TurnStats, String> {
        Turn::new(session_id).run()
    }
}

// --- configuration ----------------------------------------------------------

fn config_str(key: &str, fallback: &str) -> String {
    sys::config_get(key).unwrap_or_else(|| fallback.to_string())
}

fn config_u32(key: &str, fallback: u32) -> u32 {
    sys::config_get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

// --- the turn ---------------------------------------------------------------

struct Turn {
    session_id: String,
    model: String,
    /// How the user asked this session to behave. Decides which tools the
    /// model is offered.
    mode: String,
    max_iterations: u32,
    /// The conversation as the model sees it.
    messages: Vec<Value>,
    iterations: u32,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost_usd: f64,
    tools_used: Vec<String>,
    stopped_by: &'static str,
}

impl Turn {
    fn new(session_id: String) -> Self {
        // The session's own choices win over the harness defaults.
        let meta = host::get_session(&session_id);
        let model = meta
            .as_ref()
            .map(|m| m.model.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| config_str("model", "anthropic/claude-sonnet-4.5"));
        let mode = meta
            .as_ref()
            .map(|m| m.mode.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| tools::DEFAULT_MODE.to_string());

        Self {
            session_id,
            model,
            mode,
            max_iterations: config_u32("max_iterations", 32).min(ABSOLUTE_MAX_ITERATIONS),
            messages: Vec::new(),
            iterations: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cost_usd: 0.0,
            tools_used: Vec::new(),
            stopped_by: "stop",
        }
    }

    fn run(mut self) -> Result<TurnStats, String> {
        self.rehydrate();

        loop {
            if self.iterations >= self.max_iterations {
                self.stopped_by = "max-iterations";
                self.note(&format!(
                    "stopped after {} iterations",
                    self.max_iterations
                ));
                break;
            }
            self.iterations += 1;

            let reply = match self.stream_completion() {
                Ok(reply) => reply,
                // Returning the error is enough: the orchestrator records the
                // incident, so logging it here too would double-report it.
                Err(e) => {
                    self.stopped_by = "llm-error";
                    return Err(e);
                }
            };

            // Persist what the model said before acting on it, so the log is
            // truthful even if a tool call traps.
            host::append(
                &self.session_id,
                &SessionEvent::AssistantMessage(AssistantMsg {
                    content: reply.text.clone(),
                    tool_calls: reply.tool_calls.clone(),
                    model: reply.model.clone(),
                    usage: reply.usage.clone(),
                }),
            );
            self.record_usage(&reply.usage);
            self.messages.push(assistant_message(&reply));

            if reply.tool_calls.is_empty() {
                // The model is done talking. Only a nudge that landed while it
                // was finishing justifies another round trip.
                match self.drain_inbox() {
                    Interrupt::None => {
                        self.stopped_by = "stop";
                        break;
                    }
                    Interrupt::Cancelled => {
                        self.stopped_by = "cancelled";
                        break;
                    }
                    Interrupt::Nudged => continue,
                }
            }

            for call in &reply.tool_calls {
                self.dispatch(call);
            }

            if matches!(self.drain_inbox(), Interrupt::Cancelled) {
                self.stopped_by = "cancelled";
                break;
            }
        }

        Ok(TurnStats {
            iterations: self.iterations,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cost_usd: self.cost_usd,
            tools_used: self.tools_used,
            stopped_by: self.stopped_by.to_string(),
        })
    }

    /// Rebuilds the model's view of the conversation from the event log.
    fn rehydrate(&mut self) {
        self.messages.push(json!({
            "role": "system",
            "content": self.system_prompt(),
        }));

        for record in host::events(&self.session_id, 0) {
            match record.event {
                SessionEvent::UserMessage(msg) => {
                    self.messages
                        .push(json!({ "role": "user", "content": user_content(&msg) }));
                }
                SessionEvent::Nudge(text) => {
                    self.messages.push(json!({ "role": "user", "content": text }));
                }
                SessionEvent::AssistantMessage(msg) => {
                    let reply = Reply {
                        text: msg.content,
                        tool_calls: msg.tool_calls,
                        model: msg.model,
                        usage: msg.usage,
                    };
                    self.messages.push(assistant_message(&reply));
                }
                SessionEvent::ToolResult(out) => {
                    self.messages.push(json!({
                        "role": "tool",
                        "tool_call_id": out.call_id,
                        "content": out.content,
                    }));
                }
                SessionEvent::SystemNote(text) => {
                    self.messages
                        .push(json!({ "role": "system", "content": text }));
                }
                // Bookkeeping events carry no conversational meaning.
                _ => {}
            }
        }
    }

    /// The base prompt plus the instructions of every skill attached to this
    /// conversation.
    fn system_prompt(&self) -> String {
        let mut prompt = config_str("system_prompt", "You are a helpful assistant.");

        let attached: Vec<_> = host::list_skills(&self.session_id)
            .into_iter()
            .filter(|s| s.enabled)
            .collect();

        if !attached.is_empty() {
            prompt.push_str("

# Active skills
");
            for skill in attached {
                prompt.push_str(&format!("
## {}
{}
", skill.name, skill.instructions));
            }
        }
        prompt
    }

    fn stream_completion(&mut self) -> Result<Reply, String> {
        let request = json!({
            "model": self.model,
            "messages": self.messages,
            "tools": tools::definitions(&self.mode),
        });

        let stream = llm::stream_open(&request.to_string()).map_err(describe_llm_error)?;

        let mut reply = Reply::default();
        loop {
            match llm::stream_next(stream) {
                Ok(StreamChunk::Delta(chunk)) => {
                    // Straight to the browser: the user sees tokens as they land.
                    host::emit_output(&self.session_id, &chunk);
                    reply.text.push_str(&chunk);
                }
                Ok(StreamChunk::ToolCalls(calls)) => reply.tool_calls = calls,
                Ok(StreamChunk::Finished(info)) => {
                    reply.model = info.model;
                    reply.usage = info.usage;
                    break;
                }
                Err(e) => {
                    llm::stream_close(stream);
                    return Err(describe_llm_error(e));
                }
            }
        }
        llm::stream_close(stream);
        Ok(reply)
    }

    fn dispatch(&mut self, call: &ToolCall) {
        host::append(&self.session_id, &SessionEvent::ToolInvocation(call.clone()));
        if !self.tools_used.iter().any(|n| n == &call.name) {
            self.tools_used.push(call.name.clone());
        }

        let outcome = tools::invoke(&self.session_id, &self.mode, &call.name, &call.arguments_json);
        let result = ToolOutcome {
            call_id: call.id.clone(),
            name: call.name.clone(),
            ok: outcome.is_ok(),
            content: match &outcome {
                Ok(content) => content.clone(),
                Err(message) => message.clone(),
            },
        };

        host::append(&self.session_id, &SessionEvent::ToolResult(result.clone()));
        self.messages.push(json!({
            "role": "tool",
            "tool_call_id": result.call_id,
            "content": result.content,
        }));
    }

    /// Folds any mid-turn input into the conversation and reports what it found.
    fn drain_inbox(&mut self) -> Interrupt {
        let mut interrupt = Interrupt::None;

        for item in host::poll_inbox(&self.session_id) {
            match item {
                InboxItem::Nudge(text) => {
                    sys::log(LogLevel::Info, &format!("nudged mid-turn: {text}"));
                    self.messages.push(json!({ "role": "user", "content": text }));
                    // Cancellation outranks a nudge; never downgrade it.
                    if !matches!(interrupt, Interrupt::Cancelled) {
                        interrupt = Interrupt::Nudged;
                    }
                }
                InboxItem::Cancel => interrupt = Interrupt::Cancelled,
                InboxItem::Control(cmd) => {
                    sys::log(LogLevel::Debug, &format!("ignoring control item: {cmd}"));
                }
            }
        }

        interrupt
    }

    fn record_usage(&mut self, usage: &Option<TokenUsage>) {
        if let Some(u) = usage {
            self.prompt_tokens += u.prompt_tokens;
            self.completion_tokens += u.completion_tokens;
            self.cost_usd += u.cost_usd;
        }
    }

    fn note(&self, text: &str) {
        host::append(&self.session_id, &SessionEvent::SystemNote(text.to_string()));
    }
}

/// What, if anything, arrived from the user while the turn was running.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Interrupt {
    None,
    Nudged,
    Cancelled,
}

#[derive(Default)]
struct Reply {
    text: String,
    tool_calls: Vec<ToolCall>,
    model: String,
    usage: Option<TokenUsage>,
}

/// Builds the `content` field for a user message.
///
/// Plain text stays a bare string, which every provider accepts; attachments
/// promote it to the multi-part form with inline data URLs.
fn user_content(msg: &UserMsg) -> Value {
    if msg.attachments.is_empty() {
        return json!(msg.text);
    }

    let mut parts = Vec::new();
    if !msg.text.trim().is_empty() {
        parts.push(json!({ "type": "text", "text": msg.text }));
    }
    for attachment in &msg.attachments {
        if attachment.mime.starts_with("image/") {
            parts.push(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", attachment.mime, attachment.data_base64)
                }
            }));
        } else {
            // Nothing sensible to send inline, but the model should still know
            // the file was there.
            parts.push(json!({
                "type": "text",
                "text": format!("[attached file: {} ({})]", attachment.name, attachment.mime)
            }));
        }
    }
    json!(parts)
}

fn assistant_message(reply: &Reply) -> Value {
    let mut msg = json!({ "role": "assistant", "content": reply.text });
    if !reply.tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(
            reply
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments_json },
                    })
                })
                .collect(),
        );
    }
    msg
}

fn describe_llm_error(e: LlmError) -> String {
    match e {
        LlmError::Auth(d) => format!("authentication failed: {d}"),
        LlmError::RateLimited(d) => format!("rate limited: {d}"),
        LlmError::Transport(d) => format!("transport error: {d}"),
        LlmError::ModelError(d) => format!("model error: {d}"),
        LlmError::Budget(d) => format!("spend limit reached: {d}"),
        LlmError::BadRequest(d) => format!("bad request: {d}"),
    }
}

export!(Component);
