use super::*;

fn body(items: Vec<Value>) -> Value {
    json!({"model": "flash", "instructions": "sys", "input": items, "stream": true})
}

struct IsolatedCache {
    _lock: std::sync::MutexGuard<'static, ()>,
    _root: tempfile::TempDir,
    old_xdg: Option<std::ffi::OsString>,
    old_home: Option<std::ffi::OsString>,
}

impl IsolatedCache {
    fn new() -> Self {
        let lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let old_xdg = std::env::var_os("XDG_CACHE_HOME");
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", root.path().join("xdg"));
            std::env::set_var("HOME", root.path());
        }
        Self {
            _lock: lock,
            _root: root,
            old_xdg,
            old_home,
        }
    }
}

impl Drop for IsolatedCache {
    fn drop(&mut self) {
        match self.old_xdg.take() {
            Some(value) => unsafe { std::env::set_var("XDG_CACHE_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        match self.old_home.take() {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

#[test]
fn user_message_becomes_funnel_content() {
    let b = body(vec![json!({"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "hi"}]})]);
    let turn = prepare_turn(&b, "flash").unwrap();
    assert_eq!(turn.native_model, "gemini-3.8-flash-low");
    assert!(turn.content_line.contains("[User]"));
    assert!(turn.content_line.contains("hi"));
    // System text carries the funnel contract + host instructions.
    assert!(turn.system.contains("exactly one gray tool: finish"));
    assert!(turn.system.contains("sys"));
    // Schema is the {answer, calls[]} envelope.
    assert_eq!(turn.schema["required"], json!(["answer"]));
}

#[test]
fn short_form_user_message_becomes_funnel_content() {
    // EasyInputMessage short form: the host emits {"role","content"} with
    // no "type" field; it must still be read as a message.
    let b = body(vec![json!({"role": "user", "content": "hi"})]);
    let turn = prepare_turn(&b, "flash").unwrap();
    assert!(turn.content_line.contains("[User]"));
    assert!(turn.content_line.contains("hi"));
}

#[test]
fn short_form_assistant_then_user_render_both() {
    let b = body(vec![
        json!({"role": "assistant", "content": "earlier"}),
        json!({"role": "user", "content": "next"}),
    ]);
    let turn = prepare_turn(&b, "flash").unwrap();
    assert!(turn.content_line.contains("[Assistant]\nearlier"));
    assert!(turn.content_line.contains("[User]\nnext"));
}

#[test]
fn empty_history_is_rejected() {
    let b = body(vec![]);
    assert!(prepare_turn(&b, "flash").is_err());
}

#[test]
fn usage_reports_cache_reads() {
    // Native `result.usage` shape: flat counts, cache reads outside input.
    let lines = vec![
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 0, "state": "DONE", "step_type": "agent_response",
            "text_delta": "hello"}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "DONE", "step_type": "tool",
            "tool_name": "finish",
            "tool_info": {"name": "finish", "parameters": {"answer": "hello"}}}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "hello",
            "usage": {"cache_read_tokens": 20000, "input_tokens": 2184,
                "output_tokens": 65, "thinking_tokens": 5, "total_tokens": 22254}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (sse, _, _, _, usage, _) = fold_lines(&lines, &[], &say).unwrap();
    assert_eq!(usage.input_tokens, 22184);
    assert_eq!(usage.cached_tokens, 20000);
    assert_eq!(usage.output_tokens, 70);
    let s = String::from_utf8(sse).unwrap();
    assert!(s.contains("\"cached_tokens\":20000"));
}

#[test]
fn fold_emits_valid_responses_sse() {
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 0, "state": "DONE", "step_type": "agent_response",
            "text_delta": "hello"}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "DONE", "step_type": "tool",
            "tool_name": "finish",
            "tool_info": {"name": "finish",
                "parameters": {"answer": "hello"}}}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "hello",
            "usage": {"input_tokens": 10, "output_tokens": 3}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (sse, natives, text, calls, usage, stop) = fold_lines(&lines, &[], &say).unwrap();
    assert_eq!(text, "hello");
    assert!(calls.is_empty());
    assert!(natives.is_empty());
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(stop, "completed");
    let s = String::from_utf8(sse).unwrap();
    assert!(s.contains("response.created"));
    assert!(s.contains("response.output_text.done"));
    assert!(s.contains("response.completed"));
    assert!(s.ends_with("data: [DONE]\n\n"));
}

#[test]
fn fold_maps_finish_calls_to_host_names() {
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "DONE", "step_type": "tool",
            "tool_name": "finish",
            "tool_info": {"name": "finish",
                "parameters": {"answer": "done",
                    "calls": [{"tool": "probe", "args": {"v": "x"}}]}}}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "done",
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (_, _, text, calls, _, stop) = fold_lines(&lines, &["probe".to_string()], &say).unwrap();
    assert_eq!(text, "done");
    assert_eq!(stop, "tool_use");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "probe");
}

#[test]
fn fold_rejects_foreign_tools() {
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "DONE", "step_type": "tool",
            "tool_name": "finish",
            "tool_info": {"name": "finish",
                "parameters": {"answer": "done",
                    "calls": [{"tool": "rm_rf", "args": {}}]}}}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "done",
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    assert!(fold_lines(&lines, &["bash".to_string()], &say).is_err());
}

#[test]
fn fold_reads_finish_from_active_step() {
    // Current agy puts tool_info.parameters on the ACTIVE tool step and
    // closes the call with a bare "finish" DONE step.
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "ACTIVE", "step_type": "tool",
            "tool_name": "finish",
            "tool_info": {"name": "finish",
                "parameters": {"answer": "hello"}}}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "DONE", "step_type": "finish"}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "hello",
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (_, _, text, _, _, _) = fold_lines(&lines, &[], &say).unwrap();
    assert_eq!(text, "hello");
}

#[test]
fn fold_prefers_full_structured_output_over_clipped_step_echo() {
    // agy clips step_update tool_info.parameters to 512 chars + "…"; the
    // result's structured_output carries the whole envelope.
    let full = "word ".repeat(300);
    let clipped = format!("{}…", &full[..512]);
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 1, "state": "ACTIVE", "step_type": "tool",
            "tool_name": "finish",
            "tool_info": {"name": "finish", "parameters": {"answer": clipped}}}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "structured_output": {"answer": full.clone()},
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (_, _, text, _, _, _) = fold_lines(&lines, &[], &say).unwrap();
    assert_eq!(text, full.trim());
    assert!(!text.ends_with('…'));
}

#[test]
fn fold_usage_is_this_turns_calls_not_the_conversation_total() {
    // Captured live: result.usage sums every call in the pooled agy
    // conversation (12792 + 12898 + 13078 = 38768); the per-call DONE steps
    // carry the real context size.
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 6, "state": "DONE", "step_type": "agent_response",
            "usage": {"input_tokens": 13000, "output_tokens": 15, "cache_read_tokens": 0}}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 7, "state": "DONE", "step_type": "agent_response",
            "usage": {"input_tokens": 13078, "output_tokens": 20, "cache_read_tokens": 0}}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "structured_output": {"answer": "bye"},
            "usage": {"input_tokens": 38768, "output_tokens": 53}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (_, _, _, _, usage, _) = fold_lines(&lines, &[], &say).unwrap();
    assert_eq!(usage.input_tokens, 13078);
    assert_eq!(usage.output_tokens, 35);
}

#[test]
fn fold_rejects_missing_finish() {
    // Text with no finish call is incomplete: without finish there is no
    // answer and no calls.
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
            "step_index": 0, "state": "DONE", "step_type": "agent_response",
            "text_delta": "hello"}}),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "hello",
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    assert!(fold_lines(&lines, &[], &say).is_err());
}

#[test]
fn fold_maps_quota_to_a_clean_error() {
    let lines = vec![json!({"event": "result", "result": {"conversation_id": "",
            "status": "ERROR", "response": "",
            "error": "Individual quota reached. Please upgrade your subscription."}})];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let err = fold_lines(&lines, &[], &say).unwrap_err();
    assert!(err.contains("quota"), "unexpected: {err}");
}

#[test]
fn continuation_extracts_the_delta() {
    // absorbed [user "hi"] + echoed answer + new user item → tail only.
    let absorbed = vec![json!({"role": "user", "content": "hi"})];
    let input = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "answer"}),
        json!({"role": "user", "content": "more"}),
    ];
    let delta = continuation(&absorbed, &[], "answer", &input).unwrap();
    assert_eq!(delta, "[User]\nmore");
}

#[test]
fn continuation_checks_call_id_echo() {
    let absorbed = vec![json!({"role": "user", "content": "hi"})];
    let reply_ids = vec!["call_ab12_1".to_string()];
    let input = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"type": "function_call", "call_id": "call_ab12_1",
            "name": "bash", "arguments": "{}"}),
        json!({"type": "function_call_output", "call_id": "call_ab12_1",
            "output": "done"}),
    ];
    let delta = continuation(&absorbed, &reply_ids, "", &input).unwrap();
    assert_eq!(delta, "[Tool result id=call_ab12_1]\ndone");
    // A rewritten call id breaks the echo → prefix-miss class failure.
    let mut wrong = input.clone();
    wrong[1]["call_id"] = json!("call_zzz_1");
    assert_eq!(continuation(&absorbed, &reply_ids, "", &wrong), Err("echo"));
}

#[test]
fn continuation_undelivered_reply_gets_a_note() {
    // The session answered with a call + text but gray's history carries
    // no echo: continue with the note, not a respawn.
    let absorbed = vec![json!({"role": "user", "content": "hi"})];
    let input = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "user", "content": "again"}),
    ];
    let delta = continuation(&absorbed, &["call_ab12_1".to_string()], "answer", &input).unwrap();
    assert!(delta.starts_with(UNDELIVERED_NOTE));
    assert!(delta.ends_with("[User]\nagain"));
}

#[test]
fn continuation_rejects_diverged_and_empty_tails() {
    let absorbed = vec![json!({"role": "user", "content": "hi"})];
    // Identical request (a host retry): nothing extends the prefix.
    assert_eq!(continuation(&absorbed, &[], "", &absorbed), Err("prefix"));
    // Different history entirely.
    assert_eq!(
        continuation(
            &absorbed,
            &[],
            "",
            &[json!({"role": "user", "content": "other"})]
        ),
        Err("prefix")
    );
    // Only an echo, nothing new to ask.
    let echo_only = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "answer"}),
    ];
    assert_eq!(
        continuation(&absorbed, &[], "answer", &echo_only),
        Err("empty_delta")
    );
    // Two consecutive assistant items: the second can't belong to this
    // session's reply, so the echo zone itself diverges.
    let extra_echo = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "answer"}),
        json!({"role": "assistant", "content": "spliced"}),
        json!({"role": "user", "content": "tail"}),
    ];
    assert_eq!(
        continuation(&absorbed, &[], "answer", &extra_echo),
        Err("echo")
    );
    // An assistant item inside the new tail: mid-edited turn.
    let mid_edit = vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "answer"}),
        json!({"role": "user", "content": "tail"}),
        json!({"role": "assistant", "content": "spliced"}),
        json!({"role": "user", "content": "again"}),
    ];
    assert_eq!(
        continuation(&absorbed, &[], "answer", &mid_edit),
        Err("empty_delta")
    );
}

#[test]
fn fold_rejects_double_finish() {
    // Two DISTINCT finish calls violate single admission; identical params
    // would be the same call re-reported on a later step (an echo).
    let fin = |answer: &str| {
        json!({"event": "step_update", "step_update": {"conversation_id": "c1",
        "step_index": 1, "state": "DONE", "step_type": "tool",
        "tool_name": "finish",
        "tool_info": {"name": "finish", "parameters": {"answer": answer}}}})
    };
    let lines = vec![
        json!({"event": "init", "conversation_id": "c1",
            "init": {"model": "gemini-3.8-flash-low", "tools": ["finish"]}}),
        fin("x"),
        fin("y"),
        json!({"event": "result", "result": {"conversation_id": "c1",
            "status": "SUCCESS", "response": "x",
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    assert!(fold_lines(&lines, &[], &say).is_err());
}

#[test]
fn data_url_image_is_saved_and_renders_identically_on_replay() {
    let _cache = IsolatedCache::new();
    let encoded = "iVBORw0KGgo=";
    let bytes = [137, 80, 78, 71, 13, 10, 26, 10];
    let input = vec![json!({"type": "message", "role": "user", "content": [
        {"type": "input_image", "image_url": {"url": format!("data:image/png;base64,{encoded}")}},
    ]})];

    let first = prepare_turn(&body(input.clone()), "flash").unwrap();
    let dir = crate::session::attachments_dir().unwrap();
    let path = dir.join(format!("{:x}.png", Sha256::digest(bytes)));
    let marker = format!(
        "(image attachment saved at {}: view it with your read_file tool)",
        path.display()
    );
    assert!(first.content_line.contains(&marker));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let second = prepare_turn(&body(input), "flash").unwrap();
    assert_eq!(
        first.content_line.as_bytes(),
        second.content_line.as_bytes()
    );
}

#[test]
fn http_and_invalid_base64_images_are_omitted() {
    let _cache = IsolatedCache::new();
    let turn = prepare_turn(
        &body(vec![json!({"type": "message", "role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.test/image.png"}},
            {"type": "input_image", "image_url": "data:image/png;base64,%"},
        ]})]),
        "flash",
    )
    .unwrap();
    assert_eq!(
        turn.content_line
            .matches("(image attachment omitted")
            .count(),
        2
    );
    assert!(turn.system.contains("Attachments marked \"omitted\""));
}

#[test]
fn array_tool_output_never_dumps_base64() {
    let _cache = IsolatedCache::new();
    let b = body(vec![
        json!({"type": "function_call_output", "call_id": "c1", "output": [
            {"type": "input_text", "text": "shot taken"},
            {"type": "input_image", "image_url": "data:image/png;base64,QUJD"},
        ]}),
    ]);
    let turn = prepare_turn(&b, "flash").unwrap();
    assert!(turn.content_line.contains("[Tool result id=c1]"));
    assert!(turn.content_line.contains("shot taken"));
    assert!(!turn.content_line.contains("QUJD"));
}
