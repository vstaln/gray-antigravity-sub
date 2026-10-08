use super::*;

/// A fake `agy` whose `models` subcommand prints canned TSV: the
/// discovery path is exercised end-to-end without the real binary.
fn stub_agy(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let script = dir.join("agy-stub.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = models ]; then\nprintf '%s' '{body}'\nexit 0\nfi\nexit 1\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

#[test]
fn parse_tsv_reads_id_and_name() {
    let rows = parse_tsv(
        "gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
         claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n\
         gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n",
    );
    assert_eq!(
        rows,
        vec![
            DiscoveredModel {
                id: "gemini-3.8-flash-low".into(),
                name: "Gemini 3.8 Flash (Low)".into()
            },
            DiscoveredModel {
                id: "claude-sonnet-4-6".into(),
                name: "Claude Sonnet 4.6 (Thinking)".into()
            },
            DiscoveredModel {
                id: "gpt-oss-120b-medium".into(),
                name: "GPT-OSS 120B (Medium)".into()
            },
        ]
    );
}

#[test]
fn parse_tsv_skips_noise_and_blank_lines() {
    // Spinner/banner text leaking to stdout must not become a model.
    let rows = parse_tsv(
        "Fetching available models...\n\
         \n\
         WARNING something\n\
         nodigithere\n\
         gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
         has space in id\tbad row\n",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "gemini-3.8-flash-low");
}

#[test]
fn parse_tsv_tolerates_missing_name_column() {
    let rows = parse_tsv("gemini-3.8-flash-low\n");
    assert_eq!(
        rows,
        vec![DiscoveredModel {
            id: "gemini-3.8-flash-low".into(),
            name: String::new()
        }]
    );
    assert!(parse_tsv("").is_empty());
    assert!(parse_tsv("   \n\n").is_empty());
}

#[test]
fn live_uses_models_file_override() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::Builder::new()
        .prefix("agy-sub-models-")
        .tempdir()
        .unwrap();
    let file = tmp.path().join("models.tsv");
    std::fs::write(&file, "test-1-model\tTest One\ntest-2-model\tTest Two\n").unwrap();
    unsafe { std::env::set_var("AGY_SUB_MODELS_FILE", &file) };
    let got = live();
    unsafe { std::env::remove_var("AGY_SUB_MODELS_FILE") };
    let got = got.expect("file override yields a live list");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].id, "test-1-model");
    assert_eq!(got[0].name, "Test One");
}

#[test]
fn missing_or_empty_models_file_is_no_override() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::Builder::new()
        .prefix("agy-sub-models-")
        .tempdir()
        .unwrap();
    let file = tmp.path().join("models.tsv");
    std::fs::write(&file, "").unwrap();
    unsafe { std::env::set_var("AGY_SUB_MODELS_FILE", &file) };
    // Empty file: treated as absent, the spawn path runs instead. With
    // no binary that is still a discovery failure → None.
    unsafe { std::env::set_var("AGY_SUB_COMMAND", "/nonexistent/agy") };
    reset_cache();
    let got = live();
    unsafe { std::env::remove_var("AGY_SUB_MODELS_FILE") };
    unsafe { std::env::remove_var("AGY_SUB_COMMAND") };
    reset_cache();
    assert_eq!(got, None);
}

#[test]
fn fetch_parses_stub_binary_output() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::Builder::new()
        .prefix("agy-sub-stub-")
        .tempdir()
        .unwrap();
    let stub = stub_agy(
        tmp.path(),
        "stub-1-flash-low\tStub Flash (Low)\nstub-1-flash-high\tStub Flash (High)\n",
    );
    unsafe { std::env::set_var("AGY_SUB_COMMAND", &stub) };
    let got = fetch(Duration::from_secs(10));
    unsafe { std::env::remove_var("AGY_SUB_COMMAND") };
    let got = got.expect("stub agy yields rows");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].id, "stub-1-flash-low");
    assert_eq!(got[1].name, "Stub Flash (High)");
}

#[test]
fn fetch_failure_is_none_not_panic() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::Builder::new()
        .prefix("agy-sub-stub-")
        .tempdir()
        .unwrap();
    // A stub that prints garbage for `models` → non-empty but rowless
    // output still counts as discovery failure.
    let stub = stub_agy(tmp.path(), "not a model row\n");
    unsafe { std::env::set_var("AGY_SUB_COMMAND", &stub) };
    let got = fetch(Duration::from_secs(10));
    unsafe { std::env::remove_var("AGY_SUB_COMMAND") };
    // "not a model row" has whitespace in the would-be id → no rows.
    assert_eq!(got, None);
    unsafe { std::env::set_var("AGY_SUB_COMMAND", "/nonexistent/agy") };
    let got = fetch(Duration::from_secs(10));
    unsafe { std::env::remove_var("AGY_SUB_COMMAND") };
    assert_eq!(got, None);
}
