//! Direct in-process Antigravity subscription provider: implements
//! `gray_core::agent::Provider` by spawning the official `agy` CLI per turn
//! as a fully inert funnel — its only tool is `finish`, and gray owns all
//! real tools, approvals and compaction.
//!
//! `agy` owns credentials (OS keyring + browser sign-in): this provider
//! never reads, stores, logs, or forwards any token. Auth reaches the child
//! through a staged HOME holding a symlink to the user's own
//! `~/.gemini/antigravity-cli` credential dir — token bytes are never
//! opened by this process. Fail-closed probes (`conflicting_env`, staged
//! `settings.json`, missing credential dir) refuse to spawn rather than
//! run degraded.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::process::Stdio;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use gray_core::agent::{Provider, ProviderError};
use gray_core::event::{StopReason, StreamEvent, Usage};
use gray_core::message::{ChatRequest, ContentBlock, Message, Role};
use serde_json::{Value, json};

use crate::{catalog, chat, setup};

/// `agy` is missing (or not on PATH): install hint, never a spawn panic.
pub use crate::setup::INSTALL_HINT;

/// Tool-name namespace: gray tools live in the `finish` schema's `calls[]`
/// array (host names, unprefixed). There is no native tool prefix because
/// gray tools are never passed as native tools.
pub const NATIVE_ITEM_ID: &str = "antigravity-subscription-native";

const CARRIER_VERSION: u32 = 1;

pub fn resolve_command_for(vars: &[(&str, &str)]) -> Option<String> {
    for (k, v) in vars {
        if (*k == "AGY_SUB_COMMAND" || *k == "ANTIGRAVITY_SUB_COMMAND") && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    setup::resolve_command()
}

fn resolve_command() -> Option<String> {
    setup::resolve_command()
}

pub fn context_window(model: &str) -> Option<u32> {
    catalog::context_window(model)
}

pub fn native_model(model: &str) -> String {
    catalog::native_model(model)
}

/// One translated turn: funnel content line + schema + host tool names.
pub(crate) struct PreparedTurn {
    pub system: String,
    pub content_line: String,
    pub schema: Value,
    pub names: Vec<String>,
    pub native_model: String,
}

/// Normalize a tool input schema: strip top-level `oneOf`/`allOf`/`anyOf`
/// and guarantee object schemas carry `properties`.
pub fn normalize_input_schema(schema: &Value) -> Value {
    chat::normalize_input_schema(schema)
}

fn is_name_ok(name: &str, seen: &HashSet<String>) -> bool {
    !name.is_empty()
        && name.len() <= 50
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !seen.contains(name)
}

/// Render one gray message to funnel transcript text. Tool calls and results
/// render as explicit records; thinking carriers restore nothing (agy has
/// no replay) but their text still carries the prior answer.
fn render_message(msg: &Message) -> Result<Option<String>, ProviderError> {
    match msg.role {
        Role::System => {
            // System messages fold into the system text, not the transcript.
            Ok(None)
        }
        Role::Assistant => {
            let mut blocks: Vec<String> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.trim().is_empty() {
                            blocks.push(text.clone());
                        }
                    }
                    ContentBlock::ToolUse { id, name, args } => {
                        blocks.push(format!(
                            "[Tool call {name} id={id}]\n{}",
                            serde_json::to_string(args).unwrap_or_else(|_| "{}".into())
                        ));
                    }
                    ContentBlock::Thinking { text, .. } => {
                        // Prior answer text (carrier restores nothing; the
                        // text still anchors the transcript).
                        if !text.trim().is_empty() {
                            blocks.push(text.clone());
                        }
                    }
                    ContentBlock::ToolResult { .. } => {
                        return Err(ProviderError::BadRequest(
                            "assistant messages cannot carry tool results".into(),
                        ));
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                Ok(None)
            } else {
                Ok(Some(format!("[Assistant]\n{}", blocks.join("\n"))))
            }
        }
        Role::User => {
            let mut blocks: Vec<String> = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.trim().is_empty() {
                            blocks.push(text.clone());
                        }
                    }
                    ContentBlock::StructuredInput { .. } => {
                        if let Some(text) = block.provider_text()
                            && !text.is_empty()
                        {
                            blocks.push(text);
                        }
                    }
                    ContentBlock::Image { .. } | ContentBlock::Media { .. } => {
                        return Err(ProviderError::BadRequest(
                            "image/media input is unsupported by the Antigravity funnel".into(),
                        ));
                    }
                    ContentBlock::ToolResult {
                        id,
                        content,
                        is_error,
                    } => {
                        let text = content.clone();
                        let text = if text.is_empty() {
                            "(no output)".to_string()
                        } else {
                            text
                        };
                        let flag = if *is_error { " (error)" } else { "" };
                        blocks.push(format!("[Tool result id={id}]{flag}\n{text}"));
                    }
                    ContentBlock::ToolUse { .. } | ContentBlock::Thinking { .. } => {
                        return Err(ProviderError::BadRequest(
                            "user messages cannot carry tool calls or thinking".into(),
                        ));
                    }
                }
            }
            if blocks.is_empty() {
                Ok(None)
            } else {
                Ok(Some(format!("[User]\n{}", blocks.join("\n"))))
            }
        }
    }
}

/// Translate a `ChatRequest` into one funnel turn. Assistant tool calls keep
/// host names (no prefix: gray tools are never native tools); reasoning
/// carriers restore nothing (agy has no replay event) — anything else
/// re-derives.
pub(crate) fn prepare_turn(req: &ChatRequest, model: &str) -> Result<PreparedTurn, ProviderError> {
    let mut names: Vec<String> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    for t in &req.tools {
        if !is_name_ok(&t.name, &seen_names) {
            return Err(ProviderError::BadRequest(format!(
                "tool names must be unique ASCII identifiers of at most 50 characters: {:?}",
                t.name
            )));
        }
        seen_names.insert(t.name.clone());
        names.push(t.name.clone());
    }
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(s) = &req.system
        && !s.is_empty()
    {
        system_parts.push(s.clone());
    }
    let mut transcript: Vec<String> = Vec::new();
    for msg in &req.messages {
        if msg.role == Role::System {
            for b in &msg.content {
                if let ContentBlock::Text { text } = b
                    && !text.is_empty()
                {
                    system_parts.push(text.clone());
                }
            }
            continue;
        }
        match render_message(msg)? {
            Some(t) => transcript.push(t),
            None => continue,
        }
    }
    if transcript.is_empty() {
        return Err(ProviderError::BadRequest(
            "history must end in a nonempty user/tool-result message".into(),
        ));
    }
    let Some(last) = req.messages.last() else {
        return Err(ProviderError::BadRequest(
            "history must end in a nonempty user/tool-result message".into(),
        ));
    };
    if last.role != Role::User
        || last.content.is_empty()
        || last.content.iter().all(|b| match b {
            ContentBlock::Text { text } => text.trim().is_empty(),
            ContentBlock::StructuredInput { .. } => false,
            _ => true,
        })
    {
        // The last message must carry something answerable: user text, a
        // structured input, or a tool result. Assistant prefill (or a
        // tool-use-only tail) is unsupported.
        let answerable = last.content.iter().any(|b| match b {
            ContentBlock::Text { text } => !text.trim().is_empty(),
            ContentBlock::StructuredInput { .. } => true,
            ContentBlock::ToolResult { .. } => true,
            _ => false,
        });
        if last.role != Role::User || !answerable {
            return Err(ProviderError::BadRequest(
                "history must end in a nonempty user/tool-result message; assistant prefill is unsupported".into(),
            ));
        }
    }
    // Tool manifest for the system text: name + description + schema.
    let mut tool_specs: Vec<String> = Vec::new();
    for t in &req.tools {
        tool_specs.push(format!(
            "- {}: {} Args: {}",
            t.name,
            t.description,
            normalize_input_schema(&t.parameters)
        ));
    }
    let mut funnel_system: Vec<String> = Vec::new();
    funnel_system.push(
        "You are a model provider inside the gray agent harness. You have exactly one tool: finish. \
        You have NO other tools - never call any native tool for any reason. Gray owns all tools, approvals, and the filesystem. \
        To use a gray tool, list it in the calls array with its name and args object, and put your reply text in answer."
            .to_string(),
    );
    if !tool_specs.is_empty() {
        funnel_system.push(format!("Available gray tools:\n{}", tool_specs.join("\n")));
    } else {
        funnel_system.push("Available gray tools: none for this request.".to_string());
    }
    if !system_parts.is_empty() {
        funnel_system.push(system_parts.join("\n\n"));
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "calls": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": {"type": "string"},
                        "args": {"type": "object"},
                    },
                    "required": ["tool"],
                },
            },
        },
        "required": ["answer"],
    });
    Ok(PreparedTurn {
        system: funnel_system.join("\n\n"),
        content_line: transcript.join("\n\n"),
        schema,
        names,
        native_model: native_model(model),
    })
}

/// Map native `result` usage onto gray's inclusive `Usage`.
pub(crate) fn map_usage(u: &Value) -> Usage {
    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as usize;
    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as usize;
    let thinking = u
        .get("thinking_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let read = u
        .get("cache_read_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    Usage {
        input_tokens: input.saturating_add(read),
        output_tokens: output.saturating_add(thinking),
        reasoning_tokens: thinking.min(output.saturating_add(thinking)),
        cached_tokens: read,
        non_cached_input_tokens: input,
        cache_read_input_tokens: read,
        cache_write_input_tokens: 0,
        total_tokens: input
            .saturating_add(read)
            .saturating_add(output)
            .saturating_add(thinking),
    }
}

/// Subscription provider: spawns the `agy` CLI per turn, fully inert.
#[derive(Clone)]
pub struct AntigravitySubscriptionProvider {
    command: Option<String>,
    model: String,
    // Kept for API parity with the Claude-subscription provider this was
    // ported from: full `agy` ids already encode effort, so this is stored
    // and ignored (never passed as `--effort`, which would conflict).
    #[allow(dead_code)]
    reasoning_effort: Option<String>,
}

impl std::fmt::Debug for AntigravitySubscriptionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntigravitySubscriptionProvider")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl AntigravitySubscriptionProvider {
    pub fn new(
        model: impl Into<String>,
        reasoning_effort: Option<String>,
        command: Option<String>,
    ) -> Result<Self, String> {
        Ok(Self {
            command,
            model: model.into(),
            reasoning_effort,
        })
    }

    /// The native route id behind the `antigravity-sub/` prefix (what
    /// `native_model` resolves to `--model`).
    pub fn native_model_id(&self) -> &str {
        &self.model
    }

    fn agy_binary(&self) -> Result<String, ProviderError> {
        if let Some(c) = &self.command
            && !c.is_empty()
        {
            return Ok(c.clone());
        }
        resolve_command().ok_or_else(|| ProviderError::Connection(INSTALL_HINT.into()))
    }
}

struct Collector {
    text: String,
    calls: Vec<(String, String, String)>,
    usage: Usage,
    stop: StopReason,
}

impl Default for Collector {
    fn default() -> Self {
        Self {
            text: String::new(),
            calls: Vec::new(),
            usage: Usage::default(),
            stop: StopReason::EndTurn,
        }
    }
}

/// Feed one native stream-json line; returns stream events to forward, if any.
/// `names` is the host tool inventory: a `finish` call naming a tool outside
/// it is a hard error. Foreign native tools are ignored (the staged HOME
/// denies them; only `finish` answers).
fn feed_line(
    line: &Value,
    names: &[String],
    out: &mut Vec<StreamEvent>,
    col: &mut Collector,
) -> Result<(), ProviderError> {
    // Native envelope key is "event" (flat fakes may use "type").
    let kind = line
        .get("event")
        .or_else(|| line.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "init" => Ok(()),
        "step_update" => {
            let su = line.get("step_update");
            let step_type = su
                .and_then(|su| su.get("step_type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if step_type == "agent_response" {
                if let Some(t) = su
                    .and_then(|su| su.get("text_delta"))
                    .and_then(Value::as_str)
                    && !t.is_empty()
                {
                    // Incremental parity: surface funnel chatter as it lands.
                    out.push(StreamEvent::text_delta(t));
                }
                return Ok(());
            }
            if step_type != "tool" {
                return Ok(());
            }
            let tool_name = su
                .and_then(|su| su.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // Only `finish` answers the funnel; anything native ran is not
            // a gray call (staged HOME denies it anyway).
            if tool_name != "finish" {
                return Ok(());
            }
            let state = su
                .and_then(|su| su.get("state"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if state != "DONE" {
                return Ok(());
            }
            let params = su
                .and_then(|su| su.get("tool_info"))
                .and_then(|ti| ti.get("parameters"))
                .cloned()
                .unwrap_or(Value::Null);
            if params.is_null() {
                return Ok(());
            }
            // One funnel answer per turn: a second finish is a protocol error.
            if !col.text.is_empty() || !col.calls.is_empty() {
                return Err(ProviderError::Stream(
                    "native called finish more than once: single admission violated".into(),
                ));
            }
            let answer = params
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if answer.trim().is_empty() {
                return Err(ProviderError::Stream(
                    "incomplete native response: finish carried no answer".into(),
                ));
            }
            col.text = answer.clone();
            out.push(StreamEvent::text_delta(answer));
            let wanted = params
                .get("calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for want in &wanted {
                let tool = want.get("tool").and_then(Value::as_str).unwrap_or("");
                if !names.contains(&tool.to_string()) {
                    return Err(ProviderError::BadRequest(format!(
                        "native requested a tool outside the current host inventory: {tool:?}"
                    )));
                }
                let call_args = want.get("args").cloned().unwrap_or(json!({}));
                let args = serde_json::to_string(&call_args).unwrap_or_else(|_| "{}".into());
                col.calls.push((
                    format!("call_{}", col.calls.len() + 1),
                    tool.to_string(),
                    args.clone(),
                ));
                let (id, name) = (col.calls.len() - 1, tool.to_string());
                out.push(StreamEvent::tool_call_delta(
                    id,
                    Some(format!("call_{}", col.calls.len())),
                    Some(name),
                    args,
                ));
            }
            Ok(())
        }
        "result" => {
            // Two envelope shapes: nested ({result:{status,...}}) from agy,
            // flat ({status,...}) from minimal fakes — accept both.
            let flat_status = line.get("status").and_then(Value::as_str);
            let r = line.get("result");
            let status = r
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .or(flat_status)
                .unwrap_or("");
            if status != "SUCCESS" {
                let detail = r
                    .and_then(|r| r.get("error").or_else(|| r.get("response")))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        line.get("error")
                            .or_else(|| line.get("response"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("")
                    .to_string();
                if detail.contains("authentication failed or timed out")
                    || detail.contains("Authentication required")
                {
                    return Err(ProviderError::Auth(format!(
                        "Antigravity CLI has no usable login here; run `agy` once and complete the Google sign-in (native: {detail})"
                    )));
                }
                if detail.contains("Individual quota reached")
                    || detail.contains("RESOURCE_EXHAUSTED")
                    || detail.contains("code 429")
                {
                    return Err(ProviderError::RateLimited(format!(
                        "Antigravity quota exhausted (native: {detail})"
                    )));
                }
                if detail.contains("invalid model selection") || detail.contains("conflicts with") {
                    return Err(ProviderError::BadRequest(format!(
                        "native request failed: {detail}"
                    )));
                }
                if detail.is_empty() {
                    return Err(ProviderError::ServerError(
                        "native request failed (nonzero exit without a success result)".into(),
                    ));
                }
                return Err(ProviderError::ServerError(format!(
                    "native request failed: {detail}"
                )));
            }
            if let Some(u) = r.and_then(|r| r.get("usage")).or_else(|| line.get("usage")) {
                col.usage = map_usage(u);
            }
            col.stop = if col.calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            };
            Ok(())
        }
        _ => Ok(()),
    }
}

fn run_turn(
    provider: AntigravitySubscriptionProvider,
    turn: PreparedTurn,
    req: ChatRequest,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    // Fail fast before spawning: a gateway override would redirect the
    // subscription bearer somewhere else.
    if let Some(key) = setup::conflicting_env() {
        let msg = format!(
            "subscription provider refuses conflicting {key}: unset it so native uses your Antigravity login"
        );
        return stream::once(async move { Err(ProviderError::Auth(msg)) }).boxed();
    }
    let binary = match provider.agy_binary() {
        Ok(b) => b,
        Err(e) => return stream::once(async move { Err(e) }).boxed(),
    };
    // Same-model gate inputs for the carrier (captured at finalize).
    let model_for_carrier = provider.model.clone();
    // Per-request staging only; native runs in the real cwd (its env block
    // carries the cwd into the prompt-cache prefix, so a stable cwd keeps
    // the cache warm across turns).
    let isolation = match chat::TurnIsolation::stage() {
        Ok(i) => i,
        Err(e) => {
            // Missing credential dir reads as auth, not connection.
            return stream::once(async move { Err(ProviderError::Auth(e)) }).boxed();
        }
    };
    let schema_str = match serde_json::to_string(&turn.schema) {
        Ok(s) => s,
        Err(e) => {
            return stream::once(async move {
                Err(ProviderError::BadRequest(format!("schema encode: {e}")))
            })
            .boxed();
        }
    };
    let content_line = turn.content_line.clone();
    let system = turn.system.clone();
    let native_model = turn.native_model.clone();
    let names = turn.names.clone();
    let _ = req;
    // Blocking spawn would stall the loop: run the whole turn on a worker.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<StreamEvent, ProviderError>>();
    let _handle = tokio::task::spawn_blocking(move || {
        let _isolation = isolation;
        let line = match serde_json::to_string(&json!({"event": "user",
            "message": {"content": content_line}}))
        {
            Ok(l) => l + "\n",
            Err(e) => {
                let _ = tx.send(Err(ProviderError::BadRequest(format!("frame encode: {e}"))));
                return;
            }
        };
        // NOTE: system prompt goes through the funnel content line (agy has
        // no --system-prompt flag); `system` is kept for the carrier echo.
        let _ = system;
        let argv: Vec<String> = vec![
            "--model".into(),
            native_model,
            "--disable-slash-commands".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--json-schema".into(),
            schema_str,
            "-p".into(),
            String::new(),
        ];
        let mut child = match std::process::Command::new(&binary)
            .args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .envs(setup::child_env())
            .env("HOME", &_isolation.home)
            .env("XDG_CONFIG_HOME", _isolation.home.join(".config"))
            .env("XDG_DATA_HOME", _isolation.home.join(".local/share"))
            .env("XDG_CACHE_HOME", _isolation.home.join(".cache"))
            // A turn must never pop a browser (same neutralization as the
            // sidecar relay spawn in chat.rs).
            .env("BROWSER", "/bin/true")
            .env("DISPLAY", "")
            .env("WAYLAND_DISPLAY", "")
            .current_dir(&_isolation.cwd)
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                let _ = tx.send(Err(ProviderError::Connection(INSTALL_HINT.into())));
                return;
            }
        };
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = tx.send(Err(ProviderError::Connection(
                    "native stdin unavailable".into(),
                )));
                return;
            }
        };
        if stdin.write_all(line.as_bytes()).is_err() {
            let _ = tx.send(Err(ProviderError::Connection("native stdin closed".into())));
            return;
        }
        drop(stdin);
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = tx.send(Err(ProviderError::Connection(
                    "native stdout unavailable".into(),
                )));
                return;
            }
        };
        let reader = std::io::BufReader::new(stdout);
        let mut col = Collector::default();
        let mut saw_result = false;
        let mut conversations: HashSet<String> = HashSet::new();
        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    let _ = tx.send(Err(ProviderError::Stream(format!(
                        "invalid native stream-json output: {:?}",
                        line.chars().take(300).collect::<String>()
                    ))));
                    return;
                }
            };
            // Single-admission audit: exactly one upstream request means
            // exactly one conversation per turn.
            let ev = v
                .get("event")
                .or_else(|| v.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if ev == "init" {
                let id = v
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    // Id-less init carries no identity; never trips the audit.
                } else if !conversations.insert(id) {
                    let _ = tx.send(Err(ProviderError::Stream(
                        "native opened more than one conversation: single admission violated"
                            .into(),
                    )));
                    return;
                }
            }
            if ev == "result" {
                saw_result = true;
            }
            let mut out: Vec<StreamEvent> = Vec::new();
            if let Err(e) = feed_line(&v, &names, &mut out, &mut col) {
                let _ = tx.send(Err(e));
                return;
            }
            for ev in out {
                if tx.send(Ok(ev)).is_err() {
                    return;
                }
            }
        }
        let status = child.wait();
        let ok = matches!(status, Ok(s) if s.success());
        if !saw_result || col.text.is_empty() {
            let _ = tx.send(Err(ProviderError::Stream(
                "incomplete native response: finish and one result required".into(),
            )));
            return;
        }
        if !ok {
            // Nonzero exit with a finish + result is still a native failure
            // (quota/auth/model errors surface here when the envelope lacks
            // a parsable error string).
            let _ = tx.send(Err(ProviderError::ServerError(
                "native request failed (nonzero exit without a success result)".into(),
            )));
            return;
        }
        // Finalize: the funnel has no replay carrier (agy has no
        // assistant-replay event), so emit a versioned marker item for
        // same-model session continuity instead of byte-identical frames.
        let carrier = json!({"type": NATIVE_ITEM_ID, "version": CARRIER_VERSION,
            "model": model_for_carrier,
            "answer": col.text.clone(),
        });
        let _ = tx.send(Ok(StreamEvent::ReasoningItem {
            item_id: NATIVE_ITEM_ID.to_string(),
            encrypted_content: carrier.to_string(),
        }));
        let _ = tx.send(Ok(StreamEvent::MessageComplete {
            stop_reason: Some(if col.calls.is_empty() {
                col.stop
            } else {
                StopReason::ToolUse
            }),
            usage: Some(col.usage),
        }));
    });
    // Bridge the worker channel onto the provider stream: each step polls one
    // worker send; the worker drops the sender when the turn ends.
    futures::stream::unfold(rx, |mut rx| async move {
        let ev = rx.recv().await?;
        Some((ev, rx))
    })
    .boxed()
}

impl Provider for AntigravitySubscriptionProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn stream(&self, req: ChatRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let model = self.model.clone();
        let turn = match prepare_turn(&req, &model) {
            Ok(t) => t,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let provider = self.clone();
        run_turn(provider, turn, req)
    }
}

#[path = "direct_provider_tests.rs"]
#[cfg(test)]
mod tests;
