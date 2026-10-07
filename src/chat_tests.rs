use super::*;

fn body(items: Vec<Value>) -> Value {
    json!({"model": "flash", "instructions": "sys", "input": items, "stream": true})
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
    assert!(turn.system.contains("exactly one tool: finish"));
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
fn read_lines_times_out_on_a_silent_child() {
    // A child that never prints and never exits must hit the deadline:
    // before the channel-based read, the clock was only re-checked when
    // a line arrived, so a silent runaway (a usage-limit backoff, a
    // stuck retry loop) parked the turn forever.
    let mut child = std::process::Command::new("sleep")
        .arg("2")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let e = read_lines(stdout, std::time::Duration::from_millis(50)).unwrap_err();
    assert_eq!(e, "Antigravity request timed out");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn read_lines_collects_until_eof() {
    let mut child = std::process::Command::new("printf")
        .arg("{\"event\":\"init\"}\n{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}\n")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let lines = read_lines(stdout, std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[1].pointer("/result/status").and_then(Value::as_str),
        Some("SUCCESS")
    );
    let _ = child.wait();
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
