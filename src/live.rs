//! A pooled `agy` child per conversation: one persistent process answers
//! consecutive host turns when each request's history is a strict
//! continuation of the one the session last answered.
//!
//! Verified `agy` 1.3.1 stream-json behaviour this relies on:
//! * `agy -p --input-format stream-json --output-format stream-json` reads
//!   one NDJSON `{"event":"user","message":{"content":…}}` per stdin line
//!   and runs a turn for each, staying alive between them (stdin EOF exits
//!   it cleanly). Conversation state persists between lines: turns share
//!   one conversation_id and later turns recall earlier content — so a
//!   continuation prompts with only the delta, the transcript prefix is
//!   already upstream.
//! * one `init` line fires at process start (first turn only); each turn's
//!   terminal line is `{"event":"result",…}` — success or failure.
//! * a failed turn exits the process right after its `result` line (a
//!   quota wall, an undecodable input line, an empty prompt all end the
//!   child) — a result is a settle, an EOF after it is just that death.
//!
//! Failure taxonomy:
//! * a SUCCESS result folds into SSE; the turn's `input` is absorbed with
//!   the emitted reply (call ids + answer text) so the next request's
//!   strict-continuation check recognizes its echo.
//! * a settled failure (a non-SUCCESS result, or a result whose reply
//!   can't fold — no `finish` call, a foreign tool) records the input as
//!   absorbed with NO reply on record — the host's identical retry then
//!   misses (`prefix`: nothing extends it) and respawns, exactly like the
//!   spawn-per-turn days, while a later turn of the same conversation
//!   continues the warm session with only its new tail. The session keeps
//!   only if the child actually survived the error (it usually doesn't).
//! * dead wire — stdin write fails, stdout EOFs or garbles mid-turn, or
//!   the deadline expires with the prompt possibly in flight — leaves
//!   upstream state unknown: kill + drop, never pool a session the next
//!   turn can't trust. ONE exception: a stdin write failure on a pooled
//!   continuation means the delta never landed (a partial line is invalid
//!   NDJSON, which `agy` rejects then exits on), so the turn falls back to
//!   a fresh child + the full transcript — still answerable.
//!
//! Pool rules: at most `MAX_LIVE` idle children; a session leaves the pool
//! for its turn/keepalive (exclusive use) and returns if the wire stayed
//! clean. Sessions idle past `IDLE_TTL` or whose child exited are reaped
//! on every pool access and by a background sweep; [`shutdown_all`]
//! closes everything on `plugin/shutdown` or host-stdin EOF.
//!
//! Keepalive: Gemini's implicit prompt cache outlives a turn by only
//! ~minutes, so pooled sessions idle past `KEEPALIVE_AFTER` get one fixed
//! ping line on stdin — a real turn upstream (every stdin line is), worded
//! so the model reads it as automated. The tradeoff vs the old
//! re-spawn-replay keepalive: the ping lands in the child's transcript —
//! harmless, self-describing, and matching keys off gray's history
//! (`absorbed` + the reply echo) which the ping never touches. A failed
//! ping latches `keepalive_disabled` (fail closed: an erroring session
//! must not refresh on a loop); the next real turn re-arms it. Warming
//! stops once user idleness passes `KEEPALIVE_IDLE_MAX` — set to
//! `IDLE_TTL`, so a session stays warm for its whole reusable life.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::chat::{self, PreparedTurn, TurnIsolation, Usage};
use crate::setup;

/// Pool bound: at most this many idle `agy` children.
const MAX_LIVE: usize = 4;
/// A pooled session idle this long (no real turn) is closed.
const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// Reaper sweep period.
const REAPER_SWEEP: Duration = Duration::from_secs(60);
/// How often the keepalive sweep checks pooled sessions for stale
/// upstream contact.
const KEEPALIVE_SWEEP: Duration = Duration::from_secs(30);
/// Fire a keepalive once the session's last settled upstream contact is
/// this old — comfortably under the implicit prompt-cache TTL, so the
/// next continuation still hits a warm prefix.
const KEEPALIVE_AFTER: Duration = Duration::from_secs(4 * 60);
/// Keep warming for as long as a pooled session can still answer a turn:
/// stopping earlier leaves a live session whose ~minutes cache has
/// already lapsed, so a turn landing in the gap re-bills the whole
/// transcript on a session that looks warm. `IDLE_TTL` reaps it either
/// way.
const KEEPALIVE_IDLE_MAX: Duration = IDLE_TTL;
/// A turn waits this long for a matching session to come back from a
/// keepalive before spawning fresh instead.
const KEEPALIVE_WAIT: Duration = Duration::from_secs(60);
/// One keepalive ping must settle inside this window.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(120);
/// Fixed keepalive line: refreshes the upstream cache for a near-empty
/// reply. It stays in the child's transcript, so it says what it is —
/// a bare "." would read as a user nudge and invite a full reply.
const KEEPALIVE_TEXT: &str =
    "[automated cache refresh — not from the user: reply with only \"ok\"]";
/// Wait granularity while a matching session is out on a keepalive.
const TAKE_POLL: Duration = Duration::from_millis(500);
/// Grace for a child to exit after stdin closes before `close` kills it.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// After a settled-but-failed result the child usually exits: this grace
/// lets the wire prove death (EOF) before the session is called alive.
const ERROR_GRACE: Duration = Duration::from_millis(800);

/// One stdout line, parsed: `Err` is a garbage line (dead wire).
type PumpMsg = Result<Value, String>;

/// One live `agy` child plus the replay state needed to recognize and
/// answer its strict-continuation turns.
pub struct LiveSession {
    child: Child,
    /// `Option` so `close` can drop it: stdin EOF is the clean exit signal.
    stdin: Option<ChildStdin>,
    /// Parsed stream-json lines off the stdout pump thread.
    rx: Receiver<PumpMsg>,
    /// Lines pulled while checking post-error liveness; drained before
    /// the channel on the next turn.
    pending: VecDeque<PumpMsg>,
    pid: u32,
    /// From the `init` line (first turn): observability only — matching
    /// keys off the transcript, never this id.
    conversation_id: String,
    /// Route + funnel text the session runs under: a reused session only
    /// matches a request resolving to the same model and system text.
    native_model: String,
    system: String,
    /// The exact `input` array of the last request this session absorbed.
    absorbed: Vec<Value>,
    /// Call ids emitted in that answer, in order.
    reply_call_ids: Vec<String>,
    /// The (trimmed) answer text emitted in that answer.
    reply_text: String,
    /// User-activity clock for the `IDLE_TTL` reaper: bumped by `absorb`/
    /// `absorb_failed` only — a keepalive must never touch it or a warmed
    /// session would never die.
    last_used: Instant,
    /// Last settled upstream contact, real turn or keepalive: the clock
    /// `KEEPALIVE_AFTER` measures. `None` until the first turn settles.
    last_contact: Option<Instant>,
    /// Keepalive pings absorbed; debug trace only — never into `absorbed`,
    /// the reply echo, or host usage accounting.
    keepalive_turns: u32,
    /// Fail-closed latch on a failed ping; the next real turn re-arms it.
    keepalive_disabled: bool,
    /// Wire went dead or we killed the child: never re-pool.
    dead: bool,
    /// Keeps the staged HOME alive for the child's whole life.
    _isolation: TurnIsolation,
}

/// A session out of the pool for a keepalive: enough to tell whether a
/// waiting turn could continue it (model + system, a cheap stand-in for
/// the full continuation check — an unrelated conversation sharing both
/// just waits out one ping).
struct Warming {
    arc: Arc<Mutex<LiveSession>>,
    native_model: String,
    system: String,
}

#[derive(Default)]
struct Pool {
    /// Idle sessions only — a turn or keepalive takes its session out for
    /// exclusive use and returns it via [`give_back`].
    live: Vec<Arc<Mutex<LiveSession>>>,
    /// Sessions currently out for a keepalive ping.
    warming: Vec<Warming>,
}

impl Pool {
    fn warming_for(&self, turn: &PreparedTurn) -> bool {
        self.warming
            .iter()
            .any(|w| w.native_model == turn.native_model && w.system == turn.system)
    }
}

static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
/// Signalled whenever a session comes back from a keepalive, so a waiting
/// `take` re-checks.
static POOL_CHANGED: Condvar = Condvar::new();
/// Set by `shutdown_all`: sessions out of the pool close instead of
/// returning.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn locked_pool() -> MutexGuard<'static, Pool> {
    let pool = POOL.get_or_init(|| Mutex::new(Pool::default()));
    // Tiny reaper + cache-keepalive sweepers: started on first pool access.
    static START: Once = Once::new();
    START.call_once(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(REAPER_SWEEP);
                let dead = {
                    let Some(pool) = POOL.get() else { continue };
                    let mut pool = pool.lock().unwrap_or_else(|e| e.into_inner());
                    reap(&mut pool.live)
                };
                for s in dead {
                    s.lock().unwrap_or_else(|e| e.into_inner()).close();
                }
            }
        });
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(KEEPALIVE_SWEEP);
                keepalive_sweep();
            }
        });
    });
    pool.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock(s: &Arc<Mutex<LiveSession>>) -> MutexGuard<'_, LiveSession> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pull dead/idle-out sessions out of a locked pool; the caller closes
/// them AFTER releasing the pool lock (`close` waits on the child).
fn reap(live: &mut Vec<Arc<Mutex<LiveSession>>>) -> Vec<Arc<Mutex<LiveSession>>> {
    let mut dead = Vec::new();
    let mut i = 0;
    while i < live.len() {
        // Pooled sessions are idle, so the try_lock always wins; a lost
        // race means someone else holds it — leave it be. A poisoned
        // lock means a panic mid-turn: the absorb state can't be trusted,
        // reap it too.
        let stale = match live[i].try_lock() {
            Ok(mut s) => {
                s.dead
                    || s.last_used.elapsed() > IDLE_TTL
                    || !matches!(s.child.try_wait(), Ok(None))
            }
            Err(std::sync::TryLockError::Poisoned(_)) => true,
            Err(std::sync::TryLockError::WouldBlock) => false,
        };
        if stale {
            dead.push(live.remove(i));
        } else {
            i += 1;
        }
    }
    dead
}

/// The first pooled session this turn is a strict continuation of (same
/// native model, same system text, `continuation` yields a delta): its
/// index plus the delta. On a miss, the deepest reason any candidate
/// reached (`no_session` when the pool was empty).
fn find(
    live: &[Arc<Mutex<LiveSession>>],
    turn: &PreparedTurn,
) -> Result<(usize, String), &'static str> {
    let mut reason = "no_session";
    let mut depth = 0;
    for (i, arc) in live.iter().enumerate() {
        let Ok(s) = arc.try_lock() else {
            if depth == 0 {
                reason = "locked";
            }
            continue;
        };
        let (miss, d) = if s.native_model != turn.native_model {
            ("model", 1)
        } else if s.system != turn.system {
            ("system", 2)
        } else {
            match chat::continuation(&s.absorbed, &s.reply_call_ids, &s.reply_text, &turn.input) {
                Ok(delta) => return Ok((i, delta)),
                Err(m) => (
                    m,
                    match m {
                        "prefix" => 3,
                        "echo" => 4,
                        _ => 5,
                    },
                ),
            }
        };
        if d > depth {
            depth = d;
            reason = miss;
        }
    }
    Err(reason)
}

/// Take the pooled session this turn continues (see [`find`]), plus the
/// delta to prompt with. While a session that could answer it is out for
/// a keepalive, wait for it to come back — up to `KEEPALIVE_WAIT` —
/// rather than miss and re-bill the whole transcript on a fresh child.
/// On a miss the second item is the reason.
fn take(turn: &PreparedTurn) -> (Option<(Arc<Mutex<LiveSession>>, String)>, &'static str) {
    let deadline = Instant::now() + KEEPALIVE_WAIT;
    let mut dead = Vec::new();
    let result = {
        let mut pool = locked_pool();
        loop {
            dead.extend(reap(&mut pool.live));
            let reason = match find(&pool.live, turn) {
                Ok((i, delta)) => break (Some((pool.live.remove(i), delta)), "reuse"),
                Err(reason) => reason,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if !pool.warming_for(turn) {
                break (None, reason);
            }
            if left.is_zero() {
                break (None, "keepalive_busy");
            }
            pool = POOL_CHANGED
                .wait_timeout(pool, left.min(TAKE_POLL))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    };
    for s in dead {
        lock(&s).close();
    }
    result
}

/// Return a session to the pool after a turn or keepalive, evicting the
/// least-recently-used session past `MAX_LIVE`. A `dead` session (or a
/// post-shutdown return) is closed instead of pooled.
fn give_back(arc: Arc<Mutex<LiveSession>>) {
    {
        let mut s = lock(&arc);
        if s.dead || SHUTDOWN.load(Ordering::SeqCst) {
            s.close();
            return;
        }
    }
    let mut dead = {
        let mut pool = locked_pool();
        pool.warming.retain(|w| !Arc::ptr_eq(&w.arc, &arc));
        let mut dead = reap(&mut pool.live);
        pool.live.push(arc);
        if pool.live.len() > MAX_LIVE {
            // Evict the LRU *idle* session — a locked entry (shouldn't
            // exist in `live`, but never block on it) isn't a victim.
            let lru = pool
                .live
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.try_lock().ok().map(|g| (i, g.last_used)))
                .min_by_key(|(_, used)| *used)
                .map(|(i, _)| i);
            if let Some(lru) = lru {
                dead.push(pool.live.remove(lru));
            }
        }
        dead
    };
    POOL_CHANGED.notify_all();
    for s in dead.drain(..) {
        lock(&s).close();
    }
}

/// Drop a warming listing without pooling the session (failed ping).
fn unlist_warming(arc: &Arc<Mutex<LiveSession>>) {
    locked_pool().warming.retain(|w| !Arc::ptr_eq(&w.arc, arc));
    POOL_CHANGED.notify_all();
}

/// Keepalive predicate, field-level so tests can drive it without a live
/// child: the session settled ≥1 real contact, its upstream clock is
/// stale past `KEEPALIVE_AFTER`, the user is still active enough for a
/// warm cache to pay off, refresh isn't latched off, and the child is
/// alive (dead children are the reaper's job, not the sweep's).
fn keepalive_due(
    last_contact: Option<Instant>,
    last_used: Instant,
    disabled: bool,
    dead: bool,
    alive: bool,
) -> bool {
    alive
        && !dead
        && !disabled
        && last_contact.is_some_and(|t| t.elapsed() > KEEPALIVE_AFTER)
        && last_used.elapsed() <= KEEPALIVE_IDLE_MAX
}

/// Remove the first keepalive-due session from the pool. Out of the pool
/// is exclusive use — exactly the serialization `take` gives a real
/// turn — so no real prompt can overlap a ping on the same child; it is
/// listed as warming so a host turn arriving meanwhile waits for it.
fn take_keepalive() -> Option<Arc<Mutex<LiveSession>>> {
    let pool = POOL.get()?;
    let mut pool = pool.lock().unwrap_or_else(|e| e.into_inner());
    let i = pool.live.iter().position(|arc| {
        arc.try_lock().is_ok_and(|mut s| {
            keepalive_due(
                s.last_contact,
                s.last_used,
                s.keepalive_disabled,
                s.dead,
                matches!(s.child.try_wait(), Ok(None)),
            )
        })
    })?;
    let arc = pool.live.remove(i);
    let (native_model, system) = {
        let s = lock(&arc);
        (s.native_model.clone(), s.system.clone())
    };
    pool.warming.push(Warming {
        arc: Arc::clone(&arc),
        native_model,
        system,
    });
    Some(arc)
}

/// Warm every due session, one at a time. A settled ping returns through
/// `give_back` so `MAX_LIVE` still bounds the pool; a dead wire (or a
/// settled error that killed the child) closes the session — it is never
/// pooled in a state the next turn can't trust.
fn keepalive_sweep() {
    while let Some(arc) = take_keepalive() {
        let mut s = lock(&arc);
        match s.keepalive() {
            Ping::Ok => {
                drop(s);
                give_back(arc);
            }
            Ping::Settled(e) => {
                trace_turn(
                    &format!("keepalive:{}", s.keepalive_turns + 1),
                    &s,
                    0,
                    None,
                    Some(&e),
                );
                if s.settled_alive() {
                    // Fail closed: never loop-refresh an erroring session;
                    // the next real turn re-arms it.
                    s.keepalive_disabled = true;
                    drop(s);
                    give_back(arc);
                } else {
                    s.close();
                    drop(s);
                    unlist_warming(&arc);
                }
            }
            Ping::Wire(e) => {
                trace_turn(
                    &format!("keepalive:{}", s.keepalive_turns + 1),
                    &s,
                    0,
                    None,
                    Some(&e),
                );
                s.close();
                drop(s);
                unlist_warming(&arc);
            }
        }
    }
}

/// Close every pooled session. Called on `plugin/shutdown` and when the
/// host's stdin goes away. Sessions out on a turn/keepalive close on
/// their return path (`SHUTDOWN`); a surviving `agy` without its parent
/// still exits on stdin EOF.
pub fn shutdown_all() {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let dead = {
        let mut pool = locked_pool();
        pool.warming.clear();
        std::mem::take(&mut pool.live)
    };
    for s in dead {
        lock(&s).close();
    }
    POOL_CHANGED.notify_all();
}

/// Spawn a fresh `agy` child under a staged HOME (one staging per
/// session now, not per turn) with a stdout pump feeding parsed
/// stream-json lines. The argv is the old one-shot contract —
/// `--json-schema` forcing the `{answer, calls[]}` envelope — minus the
/// stdin close: this child answers every turn of the conversation.
fn spawn(turn: &PreparedTurn) -> Result<Arc<Mutex<LiveSession>>, String> {
    if let Some(key) = setup::conflicting_env() {
        return Err(format!(
            "subscription provider refuses conflicting {key}: unset it so native uses your Antigravity login"
        ));
    }
    let binary = setup::resolve_command().ok_or_else(|| setup::INSTALL_HINT.to_string())?;
    let schema_str =
        serde_json::to_string(&turn.schema).map_err(|e| format!("schema encode: {e}"))?;
    let isolation = TurnIsolation::stage()?;
    let mut child = Command::new(&binary)
        .args([
            "--model",
            turn.native_model.as_str(),
            "--disable-slash-commands",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--json-schema",
            schema_str.as_str(),
            "-p",
            "",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(setup::child_env())
        // Staged HOME: token symlink (auth) + skeleton settings.json
        // (toolPermission request-review). Cwd stays the real one so the
        // prompt-cache prefix is stable across turns.
        .env("HOME", &isolation.home)
        .env("XDG_CONFIG_HOME", isolation.home.join(".config"))
        .env("XDG_DATA_HOME", isolation.home.join(".local/share"))
        .env("XDG_CACHE_HOME", isolation.home.join(".cache"))
        // A turn must never pop a browser: `agy` calls `xdg-open`
        // directly (ignores `BROWSER`), and `xdg-open` with no display
        // and `BROWSER=/bin/true` exits without touching Firefox.
        .env("BROWSER", "/bin/true")
        .env("DISPLAY", "")
        .env("WAYLAND_DISPLAY", "")
        .current_dir(&isolation.cwd)
        .spawn()
        .map_err(|_| setup::INSTALL_HINT.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "native stdout unavailable".to_string())?;
    let (tx, rx) = std::sync::mpsc::channel::<PumpMsg>();
    std::thread::spawn(move || pump(stdout, tx));
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "native stdin unavailable".to_string())?;
    Ok(Arc::new(Mutex::new(LiveSession {
        pid: child.id(),
        child,
        stdin: Some(stdin),
        rx,
        pending: VecDeque::new(),
        conversation_id: String::new(),
        native_model: turn.native_model.clone(),
        system: turn.system.clone(),
        absorbed: Vec::new(),
        reply_call_ids: Vec::new(),
        reply_text: String::new(),
        last_used: Instant::now(),
        last_contact: None,
        keepalive_turns: 0,
        keepalive_disabled: false,
        dead: false,
        _isolation: isolation,
    })))
}

/// The stdout pump: one parsed stream-json value per line, a `PumpMsg`
/// error for garbage, and a closed channel at EOF.
fn pump(stdout: ChildStdout, tx: Sender<PumpMsg>) {
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let msg = match serde_json::from_str::<Value>(line) {
                    Ok(v) => Ok(v),
                    Err(_) => Err(format!(
                        "invalid native stream-json output: {:?}",
                        line.chars().take(300).collect::<String>()
                    )),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(format!("native stdout: {e}")));
                return;
            }
        }
    }
}

/// How a keepalive ping ended. `Settled` means the result line arrived
/// (whatever it says); `Wire` means the session can't be trusted.
enum Ping {
    Ok,
    Settled(String),
    Wire(String),
}

impl LiveSession {
    /// Write one `user` funnel line: the pending stash first, then stale
    /// channel lines are cleared — except an `init`, which fires once at
    /// process start and may arrive before the first send. Anything else
    /// queued is post-`result` noise that must not leak into this turn.
    fn send_content(&mut self, content: &str) -> Result<(), String> {
        self.pending.clear();
        while let Ok(m) = self.rx.try_recv() {
            if m.as_ref()
                .ok()
                .and_then(|v| v.get("event").or_else(|| v.get("type")))
                .and_then(Value::as_str)
                == Some("init")
            {
                self.pending.push_back(m);
            }
        }
        let line = serde_json::to_string(&json!({"event": "user",
            "message": {"content": content}}))
        .map_err(|e| format!("frame encode: {e}"))?
            + "\n";
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "native stdin closed".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|_| "native stdin closed".to_string())
    }

    /// Read this turn's lines until its `result` — the per-turn terminal
    /// line. The channel gives the deadline teeth: a silent child (quota
    /// backoff, a stuck retry) can't park the turn forever.
    fn drain(&mut self, deadline: Instant) -> Result<Vec<Value>, String> {
        let mut lines = Vec::new();
        loop {
            let msg = if let Some(m) = self.pending.pop_front() {
                m
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("Antigravity request timed out".into());
                }
                match self.rx.recv_timeout(left) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => {
                        return Err("Antigravity request timed out".into());
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("native stdout closed".into());
                    }
                }
            };
            let v = msg?;
            if self.conversation_id.is_empty() {
                self.conversation_id = chat::conversation_of(&v);
            }
            let terminal = v
                .get("event")
                .or_else(|| v.get("type"))
                .and_then(Value::as_str)
                == Some("result");
            lines.push(v);
            if terminal {
                return Ok(lines);
            }
        }
    }

    /// After a settled-but-failed result: `agy` exits on turn errors, so
    /// the child is usually already dead — give the wire a beat to prove
    /// it before the session is called alive. A late line is stashed, not
    /// lost.
    fn settled_alive(&mut self) -> bool {
        if self.dead || !matches!(self.child.try_wait(), Ok(None)) {
            return false;
        }
        match self.rx.recv_timeout(ERROR_GRACE) {
            Ok(m) => {
                self.pending.push_back(m);
                true
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Record what this session just answered so the next request can be
    /// recognized as its strict continuation.
    fn absorb(&mut self, turn: &PreparedTurn, text: &str, calls: &[(String, String, String)]) {
        self.system = turn.system.clone();
        self.absorbed = turn.input.clone();
        self.reply_call_ids = calls.iter().map(|(id, _, _)| id.clone()).collect();
        self.reply_text = text.trim().to_string();
        let now = Instant::now();
        self.last_used = now;
        self.last_contact = Some(now);
        self.keepalive_disabled = false;
    }

    /// Record a failed turn whose prompt still settled upstream: the
    /// input counts as absorbed (its text may already sit in the child's
    /// transcript) but no reply is on record, so a later request
    /// continues with only its own tail. An identical retry still misses
    /// by design — nothing extends `absorbed` — and respawns.
    fn absorb_failed(&mut self, turn: &PreparedTurn) {
        self.system = turn.system.clone();
        self.absorbed = turn.input.clone();
        self.reply_call_ids = Vec::new();
        self.reply_text = String::new();
        let now = Instant::now();
        self.last_used = now;
        self.last_contact = Some(now);
        self.keepalive_disabled = false;
    }

    /// Record a settled ping: bumps only the upstream-contact clock and
    /// the internal counter. `absorbed`, the reply echo, `last_used` and
    /// the fail-closed latch stay as the last real turn left them — the
    /// ping lives only in the child's transcript, so `take`'s match
    /// keys are untouched.
    fn absorb_keepalive(&mut self) {
        self.keepalive_turns += 1;
        self.last_contact = Some(Instant::now());
    }

    /// One cache-refresh line while this session is out of the pool:
    /// fixed marker text under the same schema (the reply is discarded —
    /// never folded, never counted as host usage). Bounded by
    /// `KEEPALIVE_TIMEOUT`.
    fn keepalive(&mut self) -> Ping {
        let deadline = Instant::now() + KEEPALIVE_TIMEOUT;
        if let Err(e) = self.send_content(KEEPALIVE_TEXT) {
            return Ping::Wire(e);
        }
        match self.drain(deadline) {
            Err(e) => Ping::Wire(e),
            Ok(lines) => {
                let status = lines
                    .iter()
                    .rev()
                    .find(|v| {
                        v.get("event")
                            .or_else(|| v.get("type"))
                            .and_then(Value::as_str)
                            == Some("result")
                    })
                    .and_then(|v| {
                        v.pointer("/result/status")
                            .or_else(|| v.get("status"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("");
                if status == "SUCCESS" {
                    self.absorb_keepalive();
                    trace_turn(
                        &format!("keepalive:{}", self.keepalive_turns),
                        self,
                        KEEPALIVE_TEXT.len(),
                        lines
                            .iter()
                            .rev()
                            .find_map(|v| v.pointer("/result/usage").or_else(|| v.get("usage")))
                            .map(|u| chat::map_usage(u))
                            .as_ref(),
                        None,
                    );
                    Ping::Ok
                } else {
                    let detail = lines
                        .iter()
                        .rev()
                        .find_map(|v| {
                            v.pointer("/result/error")
                                .or_else(|| v.pointer("/result/response"))
                                .or_else(|| v.get("error"))
                                .and_then(Value::as_str)
                        })
                        .unwrap_or("non-success result")
                        .to_string();
                    Ping::Settled(detail)
                }
            }
        }
    }

    /// Trace tag: `pid` plus the conversation's short id once known —
    /// the live-test hook for "same child, same conversation".
    fn tag(&self) -> String {
        let short: String = self.conversation_id.chars().take(8).collect();
        if short.is_empty() {
            format!("{}", self.pid)
        } else {
            format!("{}:{}", self.pid, short)
        }
    }

    /// Best-effort teardown: stdin closes (agy exits on EOF), a short
    /// grace, then kill. Idempotent.
    fn close(&mut self) {
        if self.dead {
            return;
        }
        self.dead = true;
        drop(self.stdin.take());
        let until = Instant::now() + CLOSE_GRACE;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveSession {
    /// Backstop: a session value must never drop with its child alive.
    fn drop(&mut self) {
        self.close();
    }
}

/// Run one turn: prompt a pooled continuation session with only the
/// delta when this request's history strictly extends what it last
/// answered, else spawn a fresh `agy` under one staged HOME and send
/// `system` + the whole transcript. Returns the folded Responses SSE
/// (the fold runs here because `absorb` needs its answer + call ids).
/// `effort` is the host's `reasoning.effort` pick, resolved onto pinned
/// route ids only. `timeout` is the whole-turn budget.
pub fn run_turn(
    body: &Value,
    model: &str,
    effort: Option<&str>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut turn = chat::prepare_turn(body, model)?;
    turn.native_model = crate::catalog::native_model_for_effort(model, effort);
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    let (claimed, mut reason) = take(&turn);
    if let Some((arc, delta)) = claimed {
        let chars = delta.chars().count();
        let mut s = lock(&arc);
        match s.send_content(&delta) {
            Ok(()) => {
                let out = settle(&mut s, &turn, &say, deadline);
                trace_result("reuse", &s, chars, &out);
                let poolable = !s.dead;
                drop(s);
                if poolable {
                    give_back(arc);
                }
                return out;
            }
            Err(_) => {
                // The delta never reached the child (a partial line is
                // invalid NDJSON — `agy` errors on it and exits anyway):
                // close it and replay the whole transcript on a fresh
                // session — the request is still answerable.
                s.close();
                drop(s);
                drop(arc);
                reason = "send_failed";
            }
        }
    }
    let mode = format!("fresh:{reason}");
    let content = if turn.system.is_empty() {
        turn.content_line.clone()
    } else {
        format!("{}\n\n{}", turn.system, turn.content_line)
    };
    let chars = content.chars().count();
    let arc = match spawn(&turn) {
        Ok(arc) => arc,
        Err(e) => {
            trace(&mode, "-", chars, None, Some(&e));
            return Err(e);
        }
    };
    let mut s = lock(&arc);
    if let Err(e) = s.send_content(&content) {
        // The prompt never reached the child: nothing to pool, and no
        // fresh fallback left — the turn fails.
        trace(&mode, "-", chars, None, Some(&e));
        s.close();
        return Err(e);
    }
    let out = settle(&mut s, &turn, &say, deadline);
    trace_result(&mode, &s, chars, &out);
    let poolable = !s.dead;
    drop(s);
    if poolable {
        give_back(arc);
    }
    out
}

/// Drain a turn to its `result` line and fold it. On success the session
/// absorbs input + reply; on a settled-but-failed result it absorbs the
/// input with no reply and pools only if the child survived; on dead
/// wire (timeout, EOF, garbage) the session is killed — the error return
/// carries the detail either way.
fn settle(
    s: &mut LiveSession,
    turn: &PreparedTurn,
    say: &Arc<dyn Fn(String) + Send + Sync>,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let lines = match s.drain(deadline) {
        Ok(lines) => lines,
        Err(e) => {
            s.close();
            return Err(e);
        }
    };
    match chat::fold_lines(&lines, &turn.names, say) {
        Ok((sse, _, text, calls, _, _)) => {
            s.absorb(turn, &text, &calls);
            Ok(sse)
        }
        Err(e) => {
            if s.settled_alive() {
                // The turn settled upstream — the session's state is
                // known, so the host's identical retry continues it on
                // this warm child (well, misses and respawns — but a
                // LATER distinct turn continues with only its tail).
                s.absorb_failed(turn);
            } else {
                s.close();
            }
            Err(e)
        }
    }
}

/// Trace one turn outcome under `ANTIGRAVITY_SUB_DEBUG`.
fn trace_result(mode: &str, s: &LiveSession, chars: usize, out: &Result<Vec<u8>, String>) {
    match out {
        Ok(_) => trace(mode, &s.tag(), chars, None, None),
        Err(e) => trace(mode, &s.tag(), chars, None, Some(e)),
    }
}

/// One line per turn/ping when `ANTIGRAVITY_SUB_DEBUG` is set: mode,
/// `pid:conversation` session tag, prompt size and usage — never prompt
/// content. Appends (mode 0600) to `<tempdir>/antigravity-sub-<pid>.log`.
fn trace_turn(mode: &str, s: &LiveSession, chars: usize, usage: Option<&Usage>, err: Option<&str>) {
    trace(mode, &s.tag(), chars, usage, err);
}

fn trace(mode: &str, tag: &str, chars: usize, usage: Option<&Usage>, err: Option<&str>) {
    if std::env::var_os("ANTIGRAVITY_SUB_DEBUG").is_none() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:03}", d.as_secs(), d.subsec_millis()))
        .unwrap_or_default();
    let outcome = match (usage, err) {
        (Some(u), _) => format!(
            "usage={}/{}/{}",
            u.input_tokens, u.cached_tokens, u.output_tokens
        ),
        (None, Some(e)) => format!("error:{}", e.chars().take(120).collect::<String>()),
        (None, None) => "ok".to_string(),
    };
    let line = format!("{ts}\t{mode}\tsession={tag}\tchars={chars}\t{outcome}\n");
    let path = std::env::temp_dir().join(format!("antigravity-sub-{}.log", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
