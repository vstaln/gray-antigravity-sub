//! Chat turn over the loopback relay: the OpenAI Responses body the host
//! POSTs is answered by a pooled `agy` print-mode child (see
//! [`crate::live`]) — one persistent process per conversation, one NDJSON
//! funnel line per turn.
//!
//! The funnel contract (all verified against `agy` behaviour — the
//! multi-turn parts re-verified on 1.3.1):
//! * history frames collapse into `{"event":"user","message":{"content":
//!   ...}}` lines — one upstream request per turn. `agy` has no
//!   assistant-replay event (fabricated `{"event":"assistant"}` lines are
//!   ignored with a warning), so byte-identical replay is impossible; the
//!   transcript is re-derived for a cold spawn instead. Verified: context
//!   persists across stdin lines in one process (same conversation_id,
//!   earlier content is recalled), so a pooled session continuing a
//!   history receives only the delta — see [`continuation`].
//! * gray tools are NOT passed as native tools. They are described in the
//!   system text and the `finish` JSON schema's `calls[]` array; `agy`
//!   calls `finish(answer, calls)` once and gray executes the calls itself.
//!   `finish` is a REAL native tool call (tool_info fires), so a turn with
//!   no `finish` call is incomplete — never an answer.
//! * the child runs under a staged HOME whose
//!   `~/.gemini/antigravity-cli/settings.json` holds exactly
//!   `{"toolPermission":"request-review"}`. Verified semantics: headless
//!   mode auto-denies every tool that needs a prompt — `run_command`
//!   fails with "permission check failed ... user denied", file reads and
//!   writes are also denied in the staged HOME (the earlier write success
//!   ran under the user's real HOME with its own allow-rules). Native tools
//!   can neither execute nor exfiltrate; `finish` stays callable because it
//!   needs no permission. A skeleton file is required: `agy` ignores an
//!   absent settings.json and falls back to defaults. One staged HOME now
//!   serves the whole conversation — staging happens once per session,
//!   not per turn.
//!
//! The relay speaks the OpenAI Responses SSE wire the host already streams,
//! so no host changes are needed: the declared transport points at the
//! per-turn relay URL and the host POSTs its standard body with the
//! per-turn bearer.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::setup;

/// Relay rejection when native retries past the single admitted request.
pub const ADMISSION_CONSUMED: &str = "ANTIGRAVITY_MODEL_ADMISSION_CONSUMED";

/// One translated turn: funnel content line, schema, host tool names.
pub struct PreparedTurn {
    pub system: String,
    pub content_line: String,
    /// The request's raw `input` array: continuation matching keys off it.
    pub input: Vec<Value>,
    pub schema: Value,
    pub names: Vec<String>,
    pub native_model: String,
}

/// Normalize a tool input schema: strip top-level `oneOf`/`allOf`/`anyOf`
/// and guarantee object schemas carry `properties`.
pub fn normalize_input_schema(schema: &Value) -> Value {
    let mut out = schema.clone();
    if let Some(obj) = out.as_object_mut() {
        for key in ["oneOf", "allOf", "anyOf"] {
            obj.remove(key);
        }
        obj.entry("type".to_string())
            .or_insert(Value::String("object".to_string()));
        if obj.get("type").and_then(Value::as_str) == Some("object")
            && !matches!(obj.get("properties"), Some(Value::Object(_)))
        {
            obj.insert("properties".to_string(), json!({}));
        }
    }
    out
}

fn check_tool_name(name: &str, seen: &HashSet<String>) -> Result<(), String> {
    if name.len() > 50
        || name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        || seen.contains(name)
    {
        return Err(format!(
            "tool names must be unique ASCII identifiers of at most 50 characters: {name:?}"
        ));
    }
    Ok(())
}

fn text_of(blocks: &Value) -> String {
    match blocks {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str),
                // OpenAI Responses wire parts the host actually sends.
                Some("input_text" | "output_text") => b.get("text").and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Kind of a Responses input item: the `type` field, or "message" for the
/// EasyInputMessage short form (role present, type absent) the host emits.
fn item_kind(item: &Value) -> &str {
    match item.get("type").and_then(Value::as_str) {
        Some(k) => k,
        None if item.get("role").is_some() => "message",
        None => "",
    }
}

/// Render one Responses `input` item to transcript text. Tool calls and
/// results render as explicit records so the model can reference them;
/// reasoning carriers restore nothing (agy has no replay) but their text
/// still carries the prior answer.
fn render_item(item: &Value) -> Option<String> {
    let kind = item_kind(item);
    match kind {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let text = text_of(item.get("content").unwrap_or(&Value::Null));
            if text.trim().is_empty() {
                return None;
            }
            let who = match role {
                "assistant" => "Assistant",
                "system" | "developer" => "System",
                _ => "User",
            };
            Some(format!("[{who}]\n{text}"))
        }
        "function_call" => {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
            let args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            Some(format!("[Tool call {name} id={call_id}]\n{args}"))
        }
        "function_call_output" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let out = item
                .get("output")
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default();
            Some(format!("[Tool result id={call_id}]\n{out}"))
        }
        "reasoning" => {
            let text = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                None
            } else {
                Some(format!("[Assistant reasoning]\n{text}"))
            }
        }
        _ => None,
    }
}

/// Render a Responses `input` array to transcript text.
fn render_items(items: &[Value]) -> String {
    items
        .iter()
        .filter_map(render_item)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Translate an OpenAI Responses body into one funnel turn.
pub fn prepare_turn(body: &Value, model: &str) -> Result<PreparedTurn, String> {
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            // Responses tools are functions; anything else is host-owned.
            if t.get("type")
                .and_then(Value::as_str)
                .is_some_and(|k| k != "function")
            {
                continue;
            }
            check_tool_name(name, &seen_names)?;
            seen_names.insert(name.to_string());
            names.push(name.to_string());
        }
    }
    if input.is_empty() {
        return Err("history must end in a nonempty user/tool-result message".into());
    }
    // The transcript must end in something the model should answer: a
    // user/assistant message, a tool call, or a tool result. A trailing
    // reasoning item alone answers nothing.
    let last_kind = input.last().map(item_kind).unwrap_or("");
    if !matches!(
        last_kind,
        "message" | "function_call" | "function_call_output"
    ) {
        return Err(
            "history must end in a nonempty user/tool-result message; assistant prefill is unsupported"
                .into(),
        );
    }
    let transcript = render_items(&input);
    if transcript.is_empty() {
        return Err("history must end in a nonempty user/tool-result message".into());
    }
    // Assistant prefill (trailing assistant message, no tool result after
    // it) is unsupported: the funnel answers the transcript as-is.
    if input
        .last()
        .and_then(|i| i.get("role"))
        .and_then(Value::as_str)
        == Some("assistant")
    {
        return Err(
            "history must end in a nonempty user/tool-result message; assistant prefill is unsupported"
                .into(),
        );
    }
    // Tool manifest for the system text: name + description + schema.
    let mut tool_specs: Vec<String> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for t in tools {
            if t.get("type")
                .and_then(Value::as_str)
                .is_some_and(|k| k != "function")
            {
                continue;
            }
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
            let params = normalize_input_schema(t.get("parameters").unwrap_or(&json!({})));
            tool_specs.push(format!("- {name}: {desc} Args: {params}"));
        }
    }
    let mut system_parts: Vec<String> = Vec::new();
    system_parts.push(
        "You are a model provider inside the gray agent harness. You have exactly one tool: finish. \
        You have NO other tools - never call any native tool for any reason. Gray owns all tools, approvals, and the filesystem. \
        To use a gray tool, list it in the calls array with its name and args object, and put your reply text in answer. \
        Later messages in this conversation come from gray too: tool results arrive as `[Tool result id=…]` records \
        and new user input as `[User]` records — always answer the latest."
            .to_string(),
    );
    if !tool_specs.is_empty() {
        system_parts.push(format!("Available gray tools:\n{}", tool_specs.join("\n")));
    } else {
        system_parts.push("Available gray tools: none for this request.".to_string());
    }
    if !instructions.is_empty() {
        system_parts.push(instructions.to_string());
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
        system: system_parts.join("\n\n"),
        content_line: transcript,
        input,
        schema,
        names,
        native_model: crate::catalog::native_model(model),
    })
}

/// An input item the assistant side produced: the replayed echo of a
/// session's own answer (assistant message, its calls, reasoning carriers).
fn assistant_side(item: &Value) -> bool {
    match item_kind(item) {
        "function_call" | "reasoning" => true,
        "message" => item.get("role").and_then(Value::as_str) == Some("assistant"),
        _ => false,
    }
}

/// Prepended to a continuation delta whose previous reply never reached
/// the host.
pub(crate) const UNDELIVERED_NOTE: &str = "[Harness note] Your previous reply was interrupted and never delivered; none of its tool calls ran.";

/// Strict-continuation check: `input` is `absorbed` plus the echo of the
/// session's own last answer plus a non-assistant tail. Returns the tail
/// rendered to transcript text — the only thing the session still needs
/// to see (the conversation context persists upstream in the pooled
/// child). Miss reasons feed the ANTIGRAVITY_SUB_DEBUG trace: `prefix`
/// (history diverged), `echo` (the replayed answer isn't what this session
/// sent), `empty_delta` (nothing new to ask, or an assistant item sits in
/// the new tail, which means the history mid-edited a turn).
///
/// The host echoes our answer as `{"role":"assistant","content":<text>}`
/// (only when the text is non-empty) then one `{"type":"function_call",...}`
/// item per call; reasoning items may interleave and carry nothing here.
pub(crate) fn continuation(
    absorbed: &[Value],
    reply_call_ids: &[String],
    reply_text: &str,
    input: &[Value],
) -> Result<String, &'static str> {
    if input.len() <= absorbed.len() || !input.starts_with(absorbed) {
        return Err("prefix");
    }
    let rest = &input[absorbed.len()..];
    let mut i = 0;
    let mut call_ids: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    while i < rest.len() && assistant_side(&rest[i]) {
        let item = &rest[i];
        match item_kind(item) {
            "function_call" => call_ids.push(
                item.get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            "message" => texts.push(text_of(item.get("content").unwrap_or(&Value::Null))),
            _ => {}
        }
        i += 1;
    }
    // An empty echo zone against a non-empty recorded reply: the reply
    // never reached gray (interrupted turn, relay client gone before
    // delivery). The child still holds it, so continue with a note
    // instead of re-billing the whole prefix on a fresh session.
    let undelivered = i == 0 && (!reply_call_ids.is_empty() || !reply_text.trim().is_empty());
    if !undelivered
        && (call_ids.as_slice() != reply_call_ids || texts.join("\n").trim() != reply_text.trim())
    {
        return Err("echo");
    }
    let tail = &rest[i..];
    if tail.is_empty() || tail.iter().any(assistant_side) {
        return Err("empty_delta");
    }
    let delta = render_items(tail);
    if delta.trim().is_empty() {
        return Err("empty_delta");
    }
    if undelivered {
        return Ok(format!("{UNDELIVERED_NOTE}\n\n{delta}"));
    }
    Ok(delta)
}

/// Per-session isolation: a staged HOME (token symlink + skeleton
/// settings.json) plus the real cwd for a stable cache prefix.
pub struct TurnIsolation {
    pub home: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
    // Held alive for the whole session: dropping the TempDir deletes the
    // staged HOME out from under the running child.
    _stage: tempfile::TempDir,
}

impl TurnIsolation {
    /// Stage a fresh HOME: symlink just the user's `agy` OAuth token file
    /// into it (auth without ever reading token bytes) and write the
    /// skeleton settings.json. Verified: a staged HOME with only the token
    /// link + skeleton authenticates and answers; conversations then land
    /// in the staged HOME, never in the user's real store. Fails closed
    /// when the token file is absent.
    pub fn stage() -> Result<Self, String> {
        let stage = tempfile::Builder::new()
            .prefix("antigravity-sub-")
            .tempdir()
            .map_err(|e| format!("staging dir: {e}"))?;
        let home = stage.path().to_path_buf();
        let token_src = credential_file().ok_or_else(|| setup::LOGIN_HINT.to_string())?;
        let cli_dir = home.join(".gemini").join("antigravity-cli");
        std::fs::create_dir_all(&cli_dir).map_err(|e| format!("staging dir: {e}"))?;
        link_token(&token_src, &cli_dir.join("antigravity-oauth-token"))?;
        std::fs::write(
            cli_dir.join("settings.json"),
            r#"{"toolPermission":"request-review"}"#,
        )
        .map_err(|e| format!("staging settings.json: {e}"))?;
        let cwd = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
        Ok(Self {
            home,
            cwd,
            _stage: stage,
        })
    }
}

/// Link the token file into the staged HOME. Unix-only in v1: Windows has
/// no `HOME`-relocatable equivalent verified, so it fails closed there.
#[cfg(unix)]
fn link_token(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::os::unix::fs::symlink(src, dst).map_err(|e| format!("staging auth: {e}"))
}

#[cfg(not(unix))]
fn link_token(_src: &std::path::Path, _dst: &std::path::Path) -> Result<(), String> {
    Err(
        "antigravity-sub staging is unix-only in v1 (token symlink); Windows support pending"
            .to_string(),
    )
}

/// The user's `agy` OAuth token file (`~/.gemini/antigravity-cli/`).
/// `None` when HOME is unset or the file is absent — fail closed, never
/// proceed without auth (an unstaged run would burn quota-less errors or,
/// worse, trigger an OAuth flow inside a harness turn).
pub fn credential_file() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let file: std::path::PathBuf = std::path::Path::new(&home)
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    file.is_file().then_some(file)
}

/// Fold native stream-json lines into a Responses SSE stream.
/// Returns (sse_bytes, natives, text, calls, usage, stop_reason).
///
/// The only tool that can fire is `finish`: its `structured_output` carries
/// `{answer, calls[]}`. Anything else native ran (run_command, view_file,
/// …) is ignored — the staged HOME denies it anyway. A turn with no
/// `finish` call is incomplete: without it there is no answer and no calls.
#[allow(clippy::type_complexity)]
pub fn fold_lines(
    lines: &[Value],
    names: &[String],
    say: &Arc<dyn Fn(String) + Send + Sync>,
) -> Result<
    (
        Vec<u8>,
        Vec<Value>,
        String,
        Vec<(String, String, String)>,
        Usage,
        String,
    ),
    String,
> {
    // Auth failures surface as result envelopes, not CLI prose.
    for line in lines {
        let ev = line
            .get("event")
            .or_else(|| line.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if ev != "result" {
            continue;
        }
        // Nested ({result:{...}}) from agy, flat ({status,...}) from fakes.
        let flat_status = line.get("status").and_then(Value::as_str);
        let r = line.get("result");
        let status = r
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str)
            .or(flat_status)
            .unwrap_or("");
        if status == "SUCCESS" {
            continue;
        }
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
            return Err(format!(
                "Antigravity CLI has no usable login here; run `agy` once and complete the Google sign-in (native: {detail})"
            ));
        }
        if detail.contains("Individual quota reached")
            || detail.contains("RESOURCE_EXHAUSTED")
            || detail.contains("code 429")
        {
            return Err(format!("Antigravity quota exhausted (native: {detail})"));
        }
        if !detail.is_empty() {
            return Err(format!("native request failed: {detail}"));
        }
        return Err("native request failed (nonzero exit without a success result)".into());
    }
    let mut usage = Usage::default();
    let mut finish_args: Option<Value> = None;
    // Schema-driven finish: when the model answers as structured output
    // instead of calling the finish tool, agy auto-closes with a bare
    // "finish" step and the envelope lands in result.structured_output.
    let mut structured: Option<Value> = None;
    let mut conversation: Option<String> = None;
    for line in lines {
        // Native envelope key is "event" (flat fakes may use "type").
        let kind = line
            .get("event")
            .or_else(|| line.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "init" => {
                // Single-admission audit: exactly one upstream request means
                // exactly one conversation per turn (id-less lines carry no
                // identity and never trip the audit).
                let id = conversation_of(line);
                if id.is_empty() {
                    continue;
                }
                if conversation.replace(id).is_some() {
                    return Err(
                        "native opened more than one conversation: single admission violated"
                            .into(),
                    );
                }
            }
            "step_update" => {
                let su = line.get("step_update");
                let step_type = su
                    .and_then(|su| su.get("step_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if step_type != "tool" {
                    continue;
                }
                let tool_name = su
                    .and_then(|su| su.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if tool_name != "finish" {
                    // Foreign native tool (denied in the staged HOME, or a
                    // retry echo): never a gray call.
                    continue;
                }
                // The call's parameters ride whichever step_update carries
                // tool_info: current agy puts them on the ACTIVE tool step and
                // closes with a bare "finish" DONE step; older builds put them
                // on the DONE tool step itself. Either way, one logical call.
                let params = su
                    .and_then(|su| su.get("tool_info"))
                    .and_then(|ti| ti.get("parameters"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if params.is_null() {
                    continue;
                }
                match &finish_args {
                    // The same call re-reported on a later step is an echo,
                    // not a second call.
                    Some(seen) if *seen == params => {}
                    Some(_) => {
                        return Err(
                            "native called finish more than once: single admission violated".into(),
                        );
                    }
                    None => finish_args = Some(params),
                }
            }
            "result" => {
                if let Some(u) = line.pointer("/result/usage").or_else(|| line.get("usage")) {
                    usage = map_usage(u);
                }
                if let Some(so) = line.pointer("/result/structured_output")
                    && !so.is_null()
                {
                    structured = Some(so.clone());
                }
            }
            _ => {}
        }
    }
    let args = finish_args
        .or(structured)
        .ok_or_else(|| "incomplete native response: finish was never called".to_string())?;
    // The schema envelopes the answer; a schema-less `response` string is
    // the fallback (older CLI shape). Either way the answer is required.
    let answer = args
        .get("answer")
        .and_then(Value::as_str)
        .or_else(|| {
            lines
                .iter()
                .rev()
                .filter(|v| v.get("type").and_then(Value::as_str) == Some("result"))
                .filter_map(|v| v.get("result"))
                .filter_map(|r| r.get("response"))
                .filter_map(Value::as_str)
                .find(|s| !s.trim().is_empty())
        })
        .unwrap_or("")
        .to_string();
    if answer.trim().is_empty() {
        return Err("incomplete native response: finish carried no answer".into());
    }
    say(format!("…{answer}"));
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let wanted = args
        .get("calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Unique per fold, not per-turn deterministic: pooled sessions answer
    // several turns, and identical call ids across turns would make the
    // echoed history ambiguous (and collide in the host transcript).
    let call_tag = rand_hex(8);
    for (i, want) in wanted.iter().enumerate() {
        let tool = want.get("tool").and_then(Value::as_str).unwrap_or("");
        if !names.contains(&tool.to_string()) {
            return Err(format!(
                "native requested a tool outside the current host inventory: {tool:?}"
            ));
        }
        let call_args = want.get("args").cloned().unwrap_or(json!({}));
        // Deterministic within the fold; unique across the session.
        let id = format!("call_{call_tag}_{}", i + 1);
        let args_str = serde_json::to_string(&call_args).unwrap_or_else(|_| "{}".into());
        calls.push((id, tool.to_string(), args_str));
    }
    let stop = if calls.is_empty() {
        "completed".to_string()
    } else {
        "tool_use".to_string()
    };
    let mut sse = Vec::new();
    let emit = |sse: &mut Vec<u8>, payload: &Value| {
        sse.extend_from_slice(b"data: ");
        sse.extend_from_slice(payload.to_string().as_bytes());
        sse.extend_from_slice(b"\n\n");
    };
    // response.created first so the host stream has an id to attach to.
    let resp_id = format!("resp_{}", rand_hex(12));
    emit(
        &mut sse,
        &json!({"type": "response.created", "response": {"id": resp_id, "model": "", "status": "in_progress"}}),
    );
    let mut item_id = 0;
    for (id, name, args) in &calls {
        item_id += 1;
        emit(
            &mut sse,
            &json!({"type": "response.output_item.added",
                "output_index": item_id - 1,
                "item": {"type": "function_call", "id": format!("fc_{item_id}"),
                    "call_id": id, "name": name, "arguments": args}}),
        );
        emit(
            &mut sse,
            &json!({"type": "response.function_call_arguments.done",
                "output_index": item_id - 1,
                "item_id": format!("fc_{item_id}"), "call_id": id,
                "name": name, "arguments": args}),
        );
        emit(
            &mut sse,
            &json!({"type": "response.output_item.done",
                "output_index": item_id - 1,
                "item": {"type": "function_call", "id": format!("fc_{item_id}"),
                    "call_id": id, "name": name, "arguments": args}}),
        );
    }
    emit(
        &mut sse,
        &json!({"type": "response.output_text.delta", "output_index": 0, "delta": answer}),
    );
    emit(
        &mut sse,
        &json!({"type": "response.output_text.done", "output_index": 0, "text": answer}),
    );
    // No reasoning carrier: agy has no assistant-replay event, so there is
    // nothing byte-identical to restore next turn; the transcript is
    // re-derived every turn. `natives` stays empty by design.
    let natives: Vec<Value> = Vec::new();
    let usage_val = json!({"input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "input_tokens_details": {"cached_tokens": usage.cached_tokens}});
    emit(
        &mut sse,
        &json!({"type": "response.completed",
            "response": {"id": resp_id, "status": stop.clone(), "usage": usage_val}}),
    );
    sse.extend_from_slice(b"data: [DONE]\n\n");
    Ok((sse, natives, answer, calls, usage, stop))
}

pub(crate) fn conversation_of(line: &Value) -> String {
    line.get("conversation_id")
        .and_then(Value::as_str)
        .or_else(|| {
            line.get("init")
                .and_then(|i| i.get("conversation_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            line.get("result")
                .and_then(|r| r.get("conversation_id"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string()
}

/// Map native `result` usage onto inclusive counts.
/// Native reports flat input/output/thinking/cache_read tokens.
#[derive(Default, Clone, Debug)]
pub struct Usage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    /// `cache_read_tokens` — a subset of `input_tokens`. Native reports
    /// no cache writes.
    pub cached_tokens: usize,
}

pub fn map_usage(u: &Value) -> Usage {
    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as usize;
    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as usize;
    let thinking = u
        .get("thinking_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    // Cache-read field spellings differ across upstreams. Flat counters
    // (`cache_read_tokens` & friends) sit next to an exclusive
    // `input_tokens`, so the inclusive total is input + read. Gemini
    // usageMetadata passthrough reports `cached_content_token_count`
    // next to an already-inclusive `input_tokens` — add nothing there.
    let (read, inclusive) = [
        ("cache_read_tokens", false),
        ("cached_tokens", false),
        ("cache_read_input_tokens", false),
        ("cached_content_token_count", true),
        ("cachedContentTokenCount", true),
    ]
    .iter()
    .find_map(|(k, incl)| u.get(*k).and_then(Value::as_u64).map(|v| (v, *incl)))
    .unwrap_or((0, false));
    let read = read as usize;
    Usage {
        input_tokens: if inclusive {
            input.max(read)
        } else {
            input.saturating_add(read)
        },
        output_tokens: output.saturating_add(thinking),
        cached_tokens: read,
    }
}

fn rand_hex(n: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut s = format!("{t:08x}{:08x}", std::process::id());
    while s.len() < n {
        s.push('0');
    }
    s[..n].to_string()
}

#[path = "chat_tests.rs"]
#[cfg(test)]
mod tests;
