use super::*;

#[test]
fn keepalive_state_bounds_warming() {
    let now = Instant::now();
    let active = now;
    let stale_contact = Some(now - KEEPALIVE_AFTER - Duration::from_secs(1));
    // No settled contact yet: never warm a session that hasn't answered.
    assert_eq!(
        keepalive_state(None, active, false, false, true),
        Keepalive::Unsettled
    );
    // Fresh contact: not due.
    assert_eq!(
        keepalive_state(Some(now), active, false, false, true),
        Keepalive::Fresh
    );
    // Stale contact, active user, live child, not disabled: due.
    assert_eq!(
        keepalive_state(stale_contact, active, false, false, true),
        Keepalive::Due
    );
    // Disabled latch, dead flag, dead child: never due.
    assert_eq!(
        keepalive_state(stale_contact, active, true, false, true),
        Keepalive::Latched
    );
    assert_eq!(
        keepalive_state(stale_contact, active, false, true, true),
        Keepalive::Gone
    );
    assert_eq!(
        keepalive_state(stale_contact, active, false, false, false),
        Keepalive::Gone
    );
    // User idle past KEEPALIVE_IDLE_MAX: the reaper owns it now.
    let idle = now - KEEPALIVE_IDLE_MAX - Duration::from_secs(1);
    assert_eq!(
        keepalive_state(stale_contact, idle, false, false, true),
        Keepalive::Idle
    );
}
