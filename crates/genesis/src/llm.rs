//! OpenRouter client.
//!
//! The host owns the socket and the API key; guests pass request JSON in and
//! pull typed chunks out. Partial tool-call deltas are reassembled here so the
//! agent only ever sees complete tool calls.

use anyhow::Result;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::bindings::types::{FinishInfo, LlmError, StreamChunk, TokenUsage, ToolCall};
use crate::config::Config;

pub struct LlmClient {
    http: reqwest::Client,
    cfg: Arc<Config>,
}

/// Receiving end of one in-flight completion.
pub struct StreamHandle {
    rx: mpsc::Receiver<Result<StreamChunk, LlmError>>,
    /// Set once a `finished` chunk has been handed to the guest.
    pub finished: bool,
}

impl StreamHandle {
    pub async fn next(&mut self) -> Result<StreamChunk, LlmError> {
        if self.finished {
            return Err(LlmError::BadRequest(
                "stream already finished; open a new one".into(),
            ));
        }
        match self.rx.recv().await {
            Some(Ok(chunk)) => {
                if matches!(chunk, StreamChunk::Finished(_)) {
                    self.finished = true;
                }
                Ok(chunk)
            }
            Some(Err(e)) => {
                self.finished = true;
                Err(e)
            }
            // Producer dropped without a terminal chunk.
            None => {
                self.finished = true;
                Ok(StreamChunk::Finished(FinishInfo {
                    reason: "eof".into(),
                    usage: None,
                    model: String::new(),
                }))
            }
        }
    }
}

impl LlmClient {
    pub fn new(cfg: Arc<Config>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .build()?;
        Ok(Self { http, cfg })
    }

    fn api_key(&self) -> Result<&str, LlmError> {
        self.cfg
            .openrouter_api_key
            .as_ref()
            .map(|key| key.expose())
            .ok_or_else(|| {
                LlmError::Auth(
                    "no API key: set llm.api_key in genesis.toml, or OPENROUTER_API_KEY in the environment"
                        .into(),
                )
            })
    }

    /// Applies harness defaults to a guest-supplied request body.
    fn prepare_body(&self, request_json: &str, stream: bool) -> Result<serde_json::Value, LlmError> {
        let mut body: serde_json::Value = serde_json::from_str(request_json)
            .map_err(|e| LlmError::BadRequest(format!("request is not valid JSON: {e}")))?;

        // Scoped so the borrow ends before caching walks the same value.
        let model = {
            let obj = body
                .as_object_mut()
                .ok_or_else(|| LlmError::BadRequest("request must be a JSON object".into()))?;

            if !obj.contains_key("model") {
                obj.insert("model".into(), self.cfg.model.clone().into());
            }
            obj.insert("stream".into(), stream.into());
            if stream {
                // Ask for a usage record on the final chunk, which is also the
                // only place cache hits are reported.
                obj.insert(
                    "stream_options".into(),
                    serde_json::json!({ "include_usage": true }),
                );
            }

            obj.get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };

        let repaired = normalize_system_roles(&mut body);
        if repaired > 0 {
            tracing::warn!(
                count = repaired,
                "moved stray system messages to the user role; a guest built a request \
                 the provider would have rejected"
            );
        }

        // Last, and only once the model is settled: which provider is about to
        // serve this decides whether breakpoints help or merely cost writes.
        let marked = crate::cache::apply(&mut body, &model, &self.cfg.cache);
        if marked > 0 {
            tracing::debug!(%model, breakpoints = marked, "prompt cache breakpoints applied");
        }

        Ok(body)
    }

    async fn send(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, LlmError> {
        let url = format!("{}/chat/completions", self.cfg.openrouter_base);
        let key = self.api_key()?;

        let mut attempt = 0;
        loop {
            let result = self
                .http
                .post(&url)
                .bearer_auth(key)
                .header("HTTP-Referer", "https://github.com/genesis")
                .header("X-Title", "Genesis")
                .json(body)
                .send()
                .await;

            let retryable = match &result {
                Ok(resp) => {
                    let s = resp.status();
                    s.as_u16() == 429 || s.is_server_error()
                }
                Err(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            };

            if retryable && attempt < self.cfg.max_retries {
                // Exponential backoff with a little jitter from the attempt index.
                let delay = Duration::from_millis(400 * (1 << attempt) + (attempt as u64 * 37));
                tracing::warn!(attempt, ?delay, "llm request failed, retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }

            let resp = result.map_err(|e| LlmError::Transport(e.to_string()))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }

            let detail = resp.text().await.unwrap_or_default();
            let detail = detail.chars().take(600).collect::<String>();
            return Err(match status.as_u16() {
                401 | 403 => LlmError::Auth(detail),
                429 => LlmError::RateLimited(detail),
                400 | 404 | 422 => LlmError::BadRequest(detail),
                _ => LlmError::ModelError(format!("http {status}: {detail}")),
            });
        }
    }

    /// Non-streaming completion; returns the raw provider JSON.
    pub async fn chat(&self, request_json: &str) -> Result<String, LlmError> {
        let body = self.prepare_body(request_json, false)?;
        let resp = self.send(&body).await?;
        resp.text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))
    }

    /// Opens a streaming completion. Chunks are pumped into the returned handle
    /// by a background task.
    pub async fn open_stream(&self, request_json: &str) -> Result<StreamHandle, LlmError> {
        let body = self.prepare_body(request_json, true)?;
        let resp = self.send(&body).await?;
        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(async move {
            let mut pump = SsePump::new(tx);
            let mut byte_stream = resp.bytes_stream();
            while let Some(next) = byte_stream.next().await {
                match next {
                    Ok(bytes) => {
                        if !pump.feed(&bytes).await {
                            return; // receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = pump.send(Err(LlmError::Transport(e.to_string()))).await;
                        return;
                    }
                }
            }
            pump.finish().await;
        });

        Ok(StreamHandle { rx, finished: false })
    }
}

// --- SSE parsing -----------------------------------------------------------

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

/// Incrementally turns an SSE byte stream into `StreamChunk`s.
struct SsePump {
    tx: mpsc::Sender<Result<StreamChunk, LlmError>>,
    buf: String,
    tool_calls: BTreeMap<u32, ToolCallAcc>,
    usage: Option<TokenUsage>,
    model: String,
    finish_reason: String,
    done: bool,
}

impl SsePump {
    fn new(tx: mpsc::Sender<Result<StreamChunk, LlmError>>) -> Self {
        Self {
            tx,
            buf: String::new(),
            tool_calls: BTreeMap::new(),
            usage: None,
            model: String::new(),
            finish_reason: String::new(),
            done: false,
        }
    }

    async fn send(&self, item: Result<StreamChunk, LlmError>) -> bool {
        self.tx.send(item).await.is_ok()
    }

    /// Returns false when the consumer has gone away.
    async fn feed(&mut self, bytes: &[u8]) -> bool {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        while let Some(idx) = self.buf.find('\n') {
            let line = self.buf[..idx].trim_end_matches('\r').to_string();
            self.buf.drain(..=idx);
            if !self.handle_line(&line).await {
                return false;
            }
        }
        true
    }

    async fn handle_line(&mut self, line: &str) -> bool {
        let Some(payload) = line.strip_prefix("data:") else {
            return true; // comments, empty lines, event: fields
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return true;
        }
        if payload == "[DONE]" {
            self.done = true;
            return true;
        }

        let parsed: SseChunk = match serde_json::from_str(payload) {
            Ok(p) => p,
            // A malformed chunk should not kill an otherwise healthy stream.
            Err(e) => {
                tracing::debug!(error = %e, "skipping unparseable sse chunk");
                return true;
            }
        };

        if let Some(m) = parsed.model {
            self.model = m;
        }
        if let Some(u) = parsed.usage {
            let details = u.prompt_tokens_details.as_ref();
            self.usage = Some(TokenUsage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                cost_usd: u.cost.unwrap_or(0.0),
                cached_tokens: details.and_then(|d| d.cached_tokens).unwrap_or(0),
                cache_write_tokens: details.and_then(|d| d.cache_write_tokens).unwrap_or(0),
            });
        }

        let Some(choice) = parsed.choices.into_iter().next() else {
            return true;
        };
        if let Some(reason) = choice.finish_reason {
            self.finish_reason = reason;
        }

        if let Some(delta) = choice.delta {
            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                if !self.send(Ok(StreamChunk::Delta(content))).await {
                    return false;
                }
            }
            for tc in delta.tool_calls.unwrap_or_default() {
                let entry = self.tool_calls.entry(tc.index).or_default();
                if let Some(id) = tc.id {
                    entry.id = id;
                }
                if let Some(f) = tc.function {
                    if let Some(name) = f.name {
                        entry.name.push_str(&name);
                    }
                    if let Some(args) = f.arguments {
                        entry.arguments.push_str(&args);
                    }
                }
            }
        }
        true
    }

    /// Emits the accumulated tool calls and the terminal chunk.
    async fn finish(&mut self) {
        if !self.tool_calls.is_empty() {
            let calls: Vec<ToolCall> = std::mem::take(&mut self.tool_calls)
                .into_values()
                .enumerate()
                .map(|(i, acc)| ToolCall {
                    id: if acc.id.is_empty() {
                        format!("call_{i}")
                    } else {
                        acc.id
                    },
                    name: acc.name,
                    arguments_json: if acc.arguments.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        acc.arguments
                    },
                })
                .collect();
            if !self.send(Ok(StreamChunk::ToolCalls(calls))).await {
                return;
            }
        }

        let reason = if self.finish_reason.is_empty() {
            if self.done {
                "stop".to_string()
            } else {
                "eof".to_string()
            }
        } else {
            std::mem::take(&mut self.finish_reason)
        };

        let usage = self.usage.take();
        let model = std::mem::take(&mut self.model);
        self.send(Ok(StreamChunk::Finished(FinishInfo {
            reason,
            usage,
            model,
        })))
        .await;
    }
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: Option<SseDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<SseToolCall>>,
}

#[derive(Deserialize)]
struct SseToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<SseFunction>,
}

#[derive(Deserialize)]
struct SseFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct SseUsage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    prompt_tokens_details: Option<SsePromptDetails>,
}

/// Cache accounting, reported the same way by every provider OpenRouter
/// fronts.
#[derive(Deserialize)]
struct SsePromptDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
}

/// Repairs a message array whose `system` messages sit where a provider will
/// refuse them, returning how many were moved.
///
/// Anthropic requires a `system` message to precede an `assistant` message or
/// end the array; one sitting before a `user` or a `tool` result is a hard 400
/// that costs the whole turn. A guest produces that innocently — a note appended
/// mid-conversation lands wherever the log had reached — and since the guest is
/// something the agent can rewrite, the check belongs here as well as there.
///
/// The repair changes the role and nothing else. The text is what carries the
/// meaning, and every provider accepts a user message in any position.
fn normalize_system_roles(body: &mut serde_json::Value) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };

    let role_at = |messages: &[serde_json::Value], i: usize| -> String {
        messages
            .get(i)
            .and_then(|m| m.get("role"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let mut moved = 0;
    // Index 0 is the system prompt, which is always valid where it is.
    for i in 1..messages.len() {
        if role_at(messages, i) != "system" {
            continue;
        }
        // Allowed: it ends the array, or an assistant turn follows it.
        let last = i + 1 == messages.len();
        if last || role_at(messages, i + 1) == "assistant" {
            continue;
        }
        if let Some(obj) = messages[i].as_object_mut() {
            obj.insert("role".into(), serde_json::Value::from("user"));
            moved += 1;
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(body: &serde_json::Value) -> Vec<String> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_system_note_before_a_user_message_is_moved() {
        // Exactly the shape that was rejected: a note appended after a tool
        // result, with the conversation carrying on afterwards.
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "user", "content": "go" },
            { "role": "assistant", "content": "working" },
            { "role": "system", "content": "Interrupted: Genesis restarted." },
            { "role": "user", "content": "carry on" },
        ]});

        assert_eq!(normalize_system_roles(&mut body), 1);
        assert_eq!(
            roles(&body),
            ["system", "user", "assistant", "user", "user"]
        );
        // The text is untouched; only where it is allowed to sit changed.
        assert_eq!(body["messages"][3]["content"], "Interrupted: Genesis restarted.");
    }

    #[test]
    fn the_leading_prompt_and_the_allowed_positions_are_left_alone() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "user", "content": "go" },
            // Allowed: an assistant turn follows.
            { "role": "system", "content": "before an assistant" },
            { "role": "assistant", "content": "hi" },
            // Allowed: it ends the array.
            { "role": "system", "content": "at the end" },
        ]});

        assert_eq!(normalize_system_roles(&mut body), 0);
        assert_eq!(roles(&body), ["system", "user", "system", "assistant", "system"]);
    }

    #[test]
    fn a_system_note_before_a_tool_result_is_moved() {
        let mut body = serde_json::json!({ "messages": [
            { "role": "system", "content": "prompt" },
            { "role": "assistant", "content": "", "tool_calls": [] },
            { "role": "system", "content": "note" },
            { "role": "tool", "tool_call_id": "c1", "content": "result" },
        ]});
        assert_eq!(normalize_system_roles(&mut body), 1);
        assert_eq!(roles(&body)[2], "user");
    }

    #[test]
    fn a_request_without_messages_is_not_a_problem() {
        let mut body = serde_json::json!({ "model": "x" });
        assert_eq!(normalize_system_roles(&mut body), 0);
    }

    async fn drain(sse: &str) -> Vec<StreamChunk> {
        let (tx, mut rx) = mpsc::channel(64);
        let mut pump = SsePump::new(tx);
        // Feed in awkward slices to prove the line buffer handles split frames.
        for piece in sse.as_bytes().chunks(7) {
            pump.feed(piece).await;
        }
        pump.finish().await;
        drop(pump);

        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item.expect("no error expected"));
        }
        out
    }

    #[tokio::test]
    async fn reassembles_content_deltas_across_split_frames() {
        let sse = "data: {\"model\":\"m1\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;

        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::Delta(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");

        match chunks.last().unwrap() {
            StreamChunk::Finished(f) => {
                assert_eq!(f.reason, "stop");
                assert_eq!(f.model, "m1");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reassembles_tool_call_argument_fragments() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"exec\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\
                   data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;

        let calls = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::ToolCalls(v) => Some(v.clone()),
                _ => None,
            })
            .expect("tool calls should be emitted");

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].arguments_json, r#"{"cmd":"ls"}"#);
        // Arguments must be complete, parseable JSON by the time the agent sees them.
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments_json).unwrap();
        assert_eq!(v["cmd"], "ls");
    }

    #[tokio::test]
    async fn captures_usage_for_spend_accounting() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4,\"cost\":0.002}}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;
        match chunks.last().unwrap() {
            StreamChunk::Finished(f) => {
                let u = f.usage.as_ref().expect("usage captured");
                assert_eq!(u.prompt_tokens, 11);
                assert_eq!(u.completion_tokens, 4);
                assert!((u.cost_usd - 0.002).abs() < 1e-9);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_chunk_does_not_abort_stream() {
        let sse = "data: {not json}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
                   data: [DONE]\n";
        let chunks = drain(sse).await;
        assert!(chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Delta(d) if d == "ok")));
    }
}
