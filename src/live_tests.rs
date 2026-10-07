use super::*;

#[test]
fn keepalive_due_bounds_warming() {
    let now = Instant::now();
    let active = now;
    let stale_contact = Some(now - KEEPALIVE_AFTER - Duration::from_secs(1));
    // No settled contact yet: never warm a session that hasn't answered.
    assert!(!keepalive_due(None, active, false, false, true));
    // Fresh contact: not due.
    assert!(!keepalive_due(Some(now), active, false, false, true));
    // Stale contact, active user, live child, not disabled: due.
    assert!(keepalive_due(stale_contact, active, false, false, true));
    // Disabled latch, dead flag, dead child: never due.
    assert!(!keepalive_due(stale_contact, active, true, false, true));
    assert!(!keepalive_due(stale_contact, active, false, true, true));
    assert!(!keepalive_due(stale_contact, active, false, false, false));
    // User idle past KEEPALIVE_IDLE_MAX: the reaper owns it now.
    let idle = now - KEEPALIVE_IDLE_MAX - Duration::from_secs(1);
    assert!(!keepalive_due(stale_contact, idle, false, false, true));
}
