use super::*;

/// A Slot with a real staged `TurnIsolation` (fake HOME + fake token file)
/// and a neutral `PreparedTurn`. `AGY_SUB_COMMAND` points at a nonexistent
/// binary so a bug that reaches `spawn_turn` fails fast instead of
/// spawning a real `agy` from a unit test.
///
/// CALLER MUST HOLD `crate::ENV_LOCK` for the whole test (the lock guards
/// the HOME/AGY_SUB_COMMAND env staging AND serializes against other
/// modules' env tests, so a refresh can never see a real `agy`).
fn staged_slot(idle: Duration, warm_idle: Duration, disabled: bool) -> Slot {
    let home = tempfile::Builder::new()
        .prefix("agy-sub-ka-home-")
        .tempdir()
        .unwrap();
    let cli = home.path().join(".gemini").join("antigravity-cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("antigravity-oauth-token"), b"fake").unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", home.path()) };
    unsafe { std::env::set_var("AGY_SUB_COMMAND", "/nonexistent/agy") };
    let isolation = crate::chat::TurnIsolation::stage().unwrap();
    match old_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    let now = Instant::now();
    Slot {
        turn: PreparedTurn {
            system: "sys".to_string(),
            content_line: "line".to_string(),
            schema: serde_json::json!({"type": "object"}),
            names: vec!["probe".to_string()],
            native_model: "gemini-3.8-flash-low".to_string(),
        },
        isolation,
        last_contact: now - idle,
        last_real: now - warm_idle,
        disabled,
    }
}

fn entry_with(slot: Slot) -> Arc<Entry> {
    Arc::new(Entry {
        slot: Mutex::new(Some(slot)),
    })
}

#[test]
fn fresh_turn_is_not_due() {
    // Scheduling: idle below KEEPALIVE_EVERY is left alone.
    assert_eq!(
        verdict(Duration::ZERO, Duration::ZERO, false),
        Verdict::Keep
    );
    assert_eq!(
        verdict(
            KEEPALIVE_EVERY - Duration::from_secs(1),
            Duration::ZERO,
            false
        ),
        Verdict::Keep
    );
}

#[test]
fn idle_at_interval_is_due() {
    assert_eq!(
        verdict(KEEPALIVE_EVERY, KEEPALIVE_EVERY, false),
        Verdict::Refresh
    );
    assert_eq!(
        verdict(
            WARM_FOR - Duration::from_secs(1),
            WARM_FOR - Duration::from_secs(1),
            false
        ),
        Verdict::Refresh
    );
}

#[test]
fn past_warm_window_drops() {
    // Bounds: once the last REAL turn is past WARM_FOR the entry is
    // dropped, even though last_contact alone looks refreshable.
    assert_eq!(
        verdict(KEEPALIVE_EVERY, WARM_FOR + Duration::from_secs(1), false),
        Verdict::Drop
    );
    assert_eq!(verdict(WARM_FOR, WARM_FOR, false), Verdict::Refresh);
}

#[test]
fn disabled_key_never_refreshes_or_drops_early() {
    assert_eq!(
        verdict(KEEPALIVE_EVERY, KEEPALIVE_EVERY, true),
        Verdict::Keep
    );
    // Disabled still drops at the cap — the staged HOME must not leak.
    assert_eq!(
        verdict(
            WARM_FOR + Duration::from_secs(1),
            WARM_FOR + Duration::from_secs(1),
            true
        ),
        Verdict::Drop
    );
}

#[test]
fn refresh_skips_while_real_turn_holds_gate() {
    // Concurrency: the slot lock held by an admitted real turn makes the
    // keepalive's try_lock fail — skip, never overlap, never wait.
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let slot = staged_slot(
        KEEPALIVE_EVERY + Duration::from_secs(10),
        Duration::ZERO,
        false,
    );
    let before = slot.last_contact;
    let entry = entry_with(slot);
    {
        let _held = entry.slot.lock().unwrap_or_else(|p| p.into_inner());
        refresh(&entry);
    }
    let guard = entry.slot.lock().unwrap_or_else(|p| p.into_inner());
    let slot = guard.as_ref().unwrap();
    assert_eq!(
        slot.last_contact, before,
        "a skipped refresh touched contact"
    );
    assert!(!slot.disabled, "a skipped refresh must not latch disabled");
}

#[test]
fn refresh_skips_disabled_and_fresh_slots() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Disabled: verdict Keep → no spawn, state untouched.
    let slot = staged_slot(KEEPALIVE_EVERY * 2, Duration::ZERO, true);
    let before = slot.last_contact;
    let entry = entry_with(slot);
    refresh(&entry);
    let guard = entry.slot.lock().unwrap_or_else(|p| p.into_inner());
    let slot = guard.as_ref().unwrap();
    assert_eq!(slot.last_contact, before);
    assert!(slot.disabled);
    drop(guard);

    // Fresh contact (a real turn just ran): not due, untouched.
    let slot = staged_slot(Duration::ZERO, Duration::ZERO, false);
    let before = slot.last_contact;
    let entry = entry_with(slot);
    refresh(&entry);
    let guard = entry.slot.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(guard.as_ref().unwrap().last_contact, before);
}

#[test]
fn refresh_failure_latches_disabled() {
    // A due refresh that reaches spawn_turn but fails (AGY_SUB_COMMAND
    // points at a nonexistent binary) must latch disabled: fail closed,
    // never loop-refresh an erroring key.
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let slot = staged_slot(
        KEEPALIVE_EVERY + Duration::from_secs(10),
        Duration::ZERO,
        false,
    );
    let entry = entry_with(slot);
    refresh(&entry);
    let guard = entry.slot.lock().unwrap_or_else(|p| p.into_inner());
    assert!(guard.as_ref().unwrap().disabled);
}

#[test]
fn tick_drops_cold_entries_and_keeps_busy_ones() {
    // Bounds at sweep level: a session idle past WARM_FOR is removed from
    // the registry; an in-flight spawn (gate held) is kept for next tick.
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cold = "ka-test-cold";
    let busy_key = "ka-test-busy";
    {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        reg.insert(
            cold.to_string(),
            entry_with(staged_slot(
                WARM_FOR + Duration::from_secs(60),
                WARM_FOR + Duration::from_secs(60),
                false,
            )),
        );
        reg.insert(
            busy_key.to_string(),
            entry_with(staged_slot(
                WARM_FOR + Duration::from_secs(60),
                WARM_FOR + Duration::from_secs(60),
                false,
            )),
        );
    }
    let busy = {
        let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(reg.get(busy_key).unwrap())
    };
    let _held = busy.slot.lock().unwrap_or_else(|p| p.into_inner());
    tick();
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    assert!(!reg.contains_key(cold), "cold entry survived the sweep");
    assert!(reg.contains_key(busy_key), "busy entry was dropped");
    reg.clear(); // do not leak test entries into other tests' registry
}
