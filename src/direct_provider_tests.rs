//! Tests for the Antigravity subscription provider: catalog routing, history
//! translation, schema normalization, and the native stream-json contract
//! against a fake `agy` executable (mirrors the verified funnel shape).

use super::*;
use gray_core::message::ToolDef;

use crate::ENV_LOCK as HOME_GUARD;

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

#[test]
fn pinned_routes_get_windows_and_full_id_passthrough() {
    assert_eq!(context_window("claude-opus-5-5-low"), Some(1_000_000));
    assert_eq!(context_window("claude-sonnet-5-5-low"), Some(200_000));
    assert_eq!(context_window("gemini-3.8-flash-low"), Some(1_048_576));
    assert_eq!(native_model("sonnet"), "claude-sonnet-5-5-low");
    assert_eq!(native_model("flash"), "gemini-3.8-flash-low");
    // Unpinned: unknown window, passthrough id (future `agy models` entries).
    assert_eq!(context_window("unqualified-future-model"), None);
    assert_eq!(
        native_model("unqualified-future-model"),
        "unqualified-future-model"
    );
}

#[test]
fn resolve_prefers_override_then_path() {
    // Scoped env without touching the process environment.
    assert_eq!(
        resolve_command_for(&[("AGY_SUB_COMMAND", "/tmp/fake-agy-test")]).as_deref(),
        Some("/tmp/fake-agy-test")
    );
}

// ---------------------------------------------------------------------------
// translation
// ---------------------------------------------------------------------------

fn req() -> ChatRequest {
    ChatRequest {
        system: Some("sys".into()),
        messages: vec![Message::user("hi")],
        tools: vec![ToolDef::new(
            "probe",
            "p",
            json!({"type": "object", "properties": {"v": {"type": "string"}}}),
        )],
        max_tokens: None,
    }
}

#[test]
fn prepare_turn_shapes_funnel_content_and_schema() {
    let turn = prepare_turn(&req(), "sonnet").unwrap();
    assert!(turn.system.contains("exactly one tool: finish"));
    assert!(turn.system.contains("sys"));
    assert_eq!(turn.native_model, "claude-sonnet-5-5-low");
    assert_eq!(turn.names, vec!["probe"]);
    assert!(turn.content_line.contains("[User]"));
    assert!(turn.content_line.contains("hi"));
    assert_eq!(turn.schema["required"], json!(["answer"]));
}

#[test]
fn tool_results_become_transcript_records() {
    let r = ChatRequest {
        system: None,
        messages: vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("c1", "probe", json!({"v": "x"}))],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    id: "c1".into(),
                    content: "out".into(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolDef::new("probe", "p", json!({"type": "object"}))],
        max_tokens: None,
    };
    let turn = prepare_turn(&r, "sonnet").unwrap();
    assert!(turn.content_line.contains("[Tool call probe id=c1]"));
    assert!(turn.content_line.contains("[Tool result id=c1]"));
}

#[test]
fn user_tool_use_is_rejected() {
    let r = ChatRequest {
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::tool_use("c1", "probe", json!({}))],
        }],
        tools: vec![],
        max_tokens: None,
    };
    assert!(prepare_turn(&r, "sonnet").is_err());
}

#[test]
fn empty_history_is_rejected() {
    let r = ChatRequest {
        system: None,
        messages: vec![],
        tools: vec![],
        max_tokens: None,
    };
    assert!(prepare_turn(&r, "sonnet").is_err());
}

#[test]
fn assistant_prefill_is_rejected() {
    let r = ChatRequest {
        system: None,
        messages: vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text("prefill")],
        }],
        tools: vec![],
        max_tokens: None,
    };
    assert!(prepare_turn(&r, "sonnet").is_err());
}

#[test]
fn duplicate_tool_names_rejected() {
    let r = ChatRequest {
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![
            ToolDef::new("a", "x", json!({"type": "object"})),
            ToolDef::new("a", "y", json!({"type": "object"})),
        ],
        max_tokens: None,
    };
    assert!(prepare_turn(&r, "sonnet").is_err());
}

#[test]
fn normalize_strips_combinators_and_repairs_object() {
    let v = normalize_input_schema(&json!({"oneOf": [{"type": "string"}], "description": "x"}));
    assert!(v.get("oneOf").is_none());
    assert_eq!(v["type"], "object");
    assert_eq!(v["properties"], json!({}));
}

// ---------------------------------------------------------------------------
// native stream-json contract (fake agy)
// ---------------------------------------------------------------------------

/// Minimal fake: asserts the inert funnel spawn shape (full id, no --effort,
/// slash commands disabled, json-schema envelope, single funnel line), then
/// replays one finish call + SUCCESS result with usage.
const FAKE: &str = r#"#!/usr/bin/env python3
import json, sys
argv = sys.argv[1:]
assert "--disable-slash-commands" in argv, argv
assert "--json-schema" in argv, argv
assert "--input-format" in argv and argv[argv.index("--input-format") + 1] == "stream-json", argv
assert "--output-format" in argv and argv[argv.index("--output-format") + 1] == "stream-json", argv
assert "--effort" not in argv, argv
mi = argv.index("--model") + 1
assert argv[mi] == "claude-sonnet-5-5-low", argv
assert argv[-2:] == ["-p", ""], argv
rows = [json.loads(l) for l in sys.stdin if l.strip()]
assert len(rows) == 1, rows
assert rows[0].get("event") == "user", rows
assert "content" in rows[0]["message"], rows
sys.stdout.write(json.dumps({"event": "init", "conversation_id": "c1",
    "init": {"model": "claude-sonnet-5-5-low", "tools": ["finish"]}}) + "\n")
sys.stdout.flush()
sys.stdout.write(json.dumps({"event": "step_update", "step_update": {
    "conversation_id": "c1", "step_index": 1, "state": "DONE", "step_type": "tool",
    "tool_name": "finish", "tool_info": {"name": "finish",
        "parameters": {"answer": "hello", "calls": [
            {"tool": "probe", "args": {"v": "x"}}]}}}}) + "\n")
sys.stdout.flush()
sys.stdout.write(json.dumps({"event": "result", "result": {
    "conversation_id": "c1", "status": "SUCCESS", "response": "hello",
    "usage": {"input_tokens": 10, "output_tokens": 3,
        "thinking_tokens": 1, "cache_read_tokens": 4}}}) + "\n")
sys.stdout.flush()
"#;

fn fake_provider(dir: &std::path::Path) -> AntigravitySubscriptionProvider {
    let bin = dir.join("agy");
    std::fs::write(&bin, FAKE).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    AntigravitySubscriptionProvider::new("sonnet", None, Some(bin.to_string_lossy().into_owned()))
        .unwrap()
}

#[tokio::test]
async fn live_turn_streams_text_and_completes_with_usage() {
    let dir = tempfile::Builder::new()
        .prefix("agy-sub-test-")
        .tempdir()
        .unwrap();
    // Stage HOME from the fake credential dir for this test only.
    let fake_home = dir.path().join("fake-home");
    let cli_dir = fake_home.join(".gemini").join("antigravity-cli");
    std::fs::create_dir_all(&cli_dir).unwrap();
    std::fs::write(cli_dir.join("antigravity-oauth-token"), "fake").unwrap();
    std::fs::write(cli_dir.join("antigravity-oauth-token"), "fake").unwrap();
    let _home_lock = HOME_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &fake_home) };
    let provider = fake_provider(dir.path());
    let events: Vec<_> = provider.stream(req()).collect().await;
    if let Some(h) = old_home {
        unsafe { std::env::set_var("HOME", h) };
    }
    assert!(!events.is_empty());
    let texts: String = events
        .iter()
        .filter_map(|e| match e {
            Ok(StreamEvent::TextDelta { delta }) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, "hello");
    // The funnel call surfaces as a tool-call delta for the host tool.
    assert!(events.iter().any(|e| matches!(
        e,
        Ok(StreamEvent::ToolCallDelta { name: Some(n), .. }) if n == "probe"
    )));
    let complete = events.iter().find_map(|e| match e {
        Ok(StreamEvent::MessageComplete { usage, .. }) => *usage,
        _ => None,
    });
    let usage = complete.expect("MessageComplete with usage");
    assert_eq!(usage.output_tokens, 4);
    assert_eq!(usage.cache_read_input_tokens, 4);
    assert_eq!(usage.input_tokens, 14);
    // Versioned marker item for same-model session continuity.
    assert!(events.iter().any(|e| matches!(
        e,
        Ok(StreamEvent::ReasoningItem { item_id, .. }) if item_id == NATIVE_ITEM_ID
    )));
}

#[tokio::test]
async fn missing_binary_is_a_clean_connection_error() {
    let provider =
        AntigravitySubscriptionProvider::new("sonnet", None, Some("/nonexistent/agy-xyz".into()))
            .unwrap();
    let events: Vec<_> = provider.stream(req()).collect().await;
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::Connection(_)))
    ));
}

#[tokio::test]
async fn quota_error_is_rate_limited() {
    let dir = tempfile::Builder::new()
        .prefix("agy-sub-quota-")
        .tempdir()
        .unwrap();
    let bin = dir.path().join("agy");
    std::fs::write(
        &bin,
        "#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps({'event':'result','result':{'status':'ERROR','response':'','error':'Individual quota reached. Resets in 1h'}}))\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let fake_home = dir.path().join("fake-home");
    let cli_dir = fake_home.join(".gemini").join("antigravity-cli");
    std::fs::create_dir_all(&cli_dir).unwrap();
    std::fs::write(cli_dir.join("antigravity-oauth-token"), "fake").unwrap();
    let _home_lock = HOME_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &fake_home) };
    let provider = AntigravitySubscriptionProvider::new(
        "sonnet",
        None,
        Some(bin.to_string_lossy().into_owned()),
    )
    .unwrap();
    let events: Vec<_> = provider.stream(req()).collect().await;
    if let Some(h) = old_home {
        unsafe { std::env::set_var("HOME", h) };
    } else {
        unsafe { std::env::remove_var("HOME") };
    }
    assert!(matches!(
        events.last(),
        Some(Err(ProviderError::RateLimited(_)))
    ));
}
