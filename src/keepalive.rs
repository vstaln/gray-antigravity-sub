//! Implicit prompt-cache keepalive: replay the last turn of an idle session
//! so Gemini's short-lived implicit cache never expires between real turns.
//!
//! Every admitted relay turn produces one byte-stable funnel line (the whole
//! transcript is re-derived, cwd and staged-HOME semantics are constant), so
//! re-running `spawn_turn` with the retained `PreparedTurn` issues exactly
//! one upstream request whose stable prefix is served from the implicit
//! cache — refreshing its TTL. The keepalive response is read to completion
//! and discarded: it never produces host-visible output or usage.
//!
//! Bounds and safety:
//! * keyed by the host `session-id` header; only sessions that completed
//!   at least one real turn have anything to replay;
//! * a refresh fires once `last_contact` (any upstream contact, real or
//!   keepalive) is `KEEPALIVE_EVERY` old — under the implicit-cache TTL;
//! * warming stops once `last_real` (the last REAL turn) exceeds
//!   `WARM_FOR`: beyond ~30min idle the cumulative cache-read cost of
//!   refreshing exceeds the one cache-write a miss would cost;
//! * a refresh NEVER overlaps an admitted real turn: both run under the
//!   per-session slot lock, which keepalives only `try_lock` (a busy gate
//!   means a real turn owns it — skip, the turn itself is the refresh);
//! * a failed refresh disables the key (fail closed): auth/quota errors
//!   must not loop-refresh on a timer; the next real turn re-arms it;
//! * keepalive spawns reuse the retained `TurnIsolation` — the SAME
//!   staged HOME the real turn used, never a fresh staging — and inherit
//!   `spawn_turn`'s fail-closed credential checks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::chat::{PreparedTurn, TurnIsolation};

/// Refresh cadence: well under the Gemini implicit-cache TTL (~minutes)
/// so each refresh lands before the cache would expire.
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(240);
/// Warm-window cap, measured from the last real turn: past ~30min idle the
/// cumulative refresh cost exceeds the single cache-write a miss costs.
pub const WARM_FOR: Duration = Duration::from_secs(30 * 60);
const _: () = assert!(WARM_FOR.as_secs() > KEEPALIVE_EVERY.as_secs());
/// Worker sweep period.
const TICK: Duration = Duration::from_secs(30);
/// Keepalive spawn timeout, shorter than a real turn's 300s: the upstream
/// request is already sent once the funnel line is written, so this bound
/// only caps how long an admitted turn may queue behind a refresh.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(120);

/// What a keepalive needs to replay one session's last real turn.
struct Slot {
    /// The real turn verbatim: same funnel content line, finish schema and
    /// tool-name list — the replay is byte-identical by construction.
    turn: PreparedTurn,
    /// The staged HOME the real turn ran under (kept alive by its TempDir).
    isolation: TurnIsolation,
    /// Last upstream contact of any kind: drives the KEEPALIVE_EVERY cadence.
    last_contact: Instant,
    /// Last REAL turn: drives the WARM_FOR cap (a refresh must not extend
    /// the warm window, or idle sessions would warm forever).
    last_real: Instant,
    /// Fail-closed latch: set on a refresh error, cleared by the next real
    /// turn. Disabled entries keep their staged HOME until the cap drops
    /// them, so a real turn can replace the slot without re-entry churn.
    disabled: bool,
}

struct Entry {
    /// Serialization point shared with the relay's spawn path: real turns
    /// hold it for their whole `spawn_turn`; keepalives only `try_lock`.
    /// `None` only in the brief window between entry creation and the
    /// first real turn populating it.
    slot: Mutex<Option<Slot>>,
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Entry>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<Entry>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

static WORKER: Once = Once::new();

/// Run an admitted real turn under the session's spawn gate, retaining its
/// `PreparedTurn` + staged-HOME isolation for keepalive replays. Blocks
/// only while a keepalive for the same session is mid-spawn (bounded by
/// `KEEPALIVE_TIMEOUT`). An empty `key` (no `session-id` header) keeps the
/// turn working but warms nothing.
pub fn run_real_turn(
    key: &str,
    turn: PreparedTurn,
    isolation: TurnIsolation,
    timeout: Duration,
) -> Result<Vec<Value>, String> {
    if key.is_empty() {
        return crate::chat::spawn_turn(&turn, &isolation, timeout);
    }
    WORKER.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("agy-cache-keepalive".to_string())
            .spawn(worker);
    });
    let entry = {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(reg.entry(key.to_string()).or_insert_with(|| {
            Arc::new(Entry {
                slot: Mutex::new(None),
            })
        }))
    };
    // Waits out an in-flight keepalive; the gate is held across the spawn
    // so a refresh can never overlap this admitted turn.
    let mut guard = entry.slot.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    // A real turn resets the clock and replaces the stored turn — both
    // timestamps, plus re-arming a disabled key.
    *guard = Some(Slot {
        turn,
        isolation,
        last_contact: now,
        last_real: now,
        disabled: false,
    });
    let slot = guard.as_mut().expect("slot just populated");
    let out = crate::chat::spawn_turn(&slot.turn, &slot.isolation, timeout);
    slot.last_contact = Instant::now();
    out
}

/// Scheduling decision for one session at sweep time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Idle in [KEEPALIVE_EVERY, WARM_FOR]: refresh the implicit cache.
    Refresh,
    /// Too soon, disabled, or nothing retained: leave the entry alone.
    Keep,
    /// Past the warm window: drop the entry (frees the staged HOME).
    Drop,
}

fn verdict(idle: Duration, warm_idle: Duration, disabled: bool) -> Verdict {
    if disabled {
        return Verdict::Keep;
    }
    if warm_idle > WARM_FOR {
        return Verdict::Drop;
    }
    if idle >= KEEPALIVE_EVERY {
        Verdict::Refresh
    } else {
        Verdict::Keep
    }
}

fn worker() {
    loop {
        std::thread::sleep(TICK);
        tick();
    }
}

/// One sweep: drop sessions past the warm window, spawn a detached refresh
/// for every session due. Slot locks are only `try_lock`ed here — a locked
/// slot means a spawn (real or refresh) is in flight, so the entry is kept
/// and revisited next tick.
fn tick() {
    let now = Instant::now();
    let due: Vec<Arc<Entry>> = {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        let mut due = Vec::new();
        reg.retain(|_, entry| {
            let guard = match entry.slot.try_lock() {
                Ok(g) => g,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => return true,
            };
            let Some(slot) = guard.as_ref() else {
                return true; // mid-population in run_real_turn
            };
            match verdict(
                now.saturating_duration_since(slot.last_contact),
                now.saturating_duration_since(slot.last_real),
                slot.disabled,
            ) {
                Verdict::Drop => false,
                Verdict::Keep => true,
                Verdict::Refresh => {
                    due.push(Arc::clone(entry));
                    true
                }
            }
        });
        due
    };
    for entry in due {
        std::thread::spawn(move || refresh(&entry));
    }
}

/// One refresh for one session: re-run the retained turn under its gate and
/// discard the response entirely. Skips (never waits) when a real turn or
/// another refresh holds the gate.
fn refresh(entry: &Arc<Entry>) {
    let mut guard = match entry.slot.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => return, // a real turn owns it: skip
    };
    let Some(slot) = guard.as_mut() else {
        return;
    };
    let now = Instant::now();
    if verdict(
        now.saturating_duration_since(slot.last_contact),
        now.saturating_duration_since(slot.last_real),
        slot.disabled,
    ) != Verdict::Refresh
    {
        return;
    }
    // Same staged HOME, same spawn path, same fail-closed credential
    // checks as a real turn — exactly one upstream request.
    match crate::chat::spawn_turn(&slot.turn, &slot.isolation, KEEPALIVE_TIMEOUT) {
        Ok(_) => slot.last_contact = Instant::now(),
        Err(_) => slot.disabled = true,
    }
}

#[path = "keepalive_tests.rs"]
#[cfg(test)]
mod tests;
