//! Live model discovery: `agy models` prints the routable `--model` ids
//! and their display names as `id<TAB>name` rows on stdout (the fetch
//! spinner goes to stderr, so stdout is a clean TSV). The subcommand is
//! unauthenticated — it answers with an empty HOME, verified on agy
//! 1.3.1 — so discovery needs only the binary: no token, no staging.
//! Because the listing is a network fetch (~seconds), results are
//! cached: a fresh cache is served directly, a stale one is served while
//! a background refresh runs, and a failed fetch simply leaves the
//! pinned table in charge — stale beats dead.
//!
//! `AGY_SUB_MODELS_FILE` replaces the spawn entirely with a TSV file of
//! the same `id<TAB>name` shape — for air-gapped setups and for tests,
//! which can pin a deterministic "live" list without a real `agy`.
//! `AGY_SUB_MODELS_TTL_SECS` (default 300) sets cache freshness and
//! `AGY_SUB_MODELS_TIMEOUT_SECS` (default 10) the child budget.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// One `agy models` row: a routable `--model` id plus its display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub name: String,
}

/// Last-good listing plus fetch bookkeeping: `attempted` rate-limits
/// retries so a dead CLI or dead network cannot cost a child spawn on
/// every turn, and `refreshing` dedupes background fetches.
#[derive(Default)]
struct Cache {
    fetched: Option<(Instant, Vec<DiscoveredModel>)>,
    attempted: Option<Instant>,
    refreshing: bool,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    fetched: None,
    attempted: None,
    refreshing: false,
});

/// Minimum spacing between fetch attempts when the last one produced
/// nothing (no binary, timeout, garbage stdout).
const RETRY: Duration = Duration::from_secs(30);

fn lock() -> MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

fn ttl() -> Duration {
    Duration::from_secs(
        std::env::var("AGY_SUB_MODELS_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    )
}

fn timeout() -> Duration {
    Duration::from_secs(
        std::env::var("AGY_SUB_MODELS_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
    )
}

/// The live model list, or `None` when discovery cannot answer (no `agy`
/// on PATH, fetch failed, no usable rows): callers then fall back to the
/// pinned table. Cold callers pay one bounded fetch — their own, or a
/// wait on an in-flight one (the startup warmer) so the first real call
/// still answers live; warm callers never block — stale data is served
/// while a refresh runs.
pub fn live() -> Option<Vec<DiscoveredModel>> {
    if let Some(models) = file_override() {
        return Some(models);
    }
    // Joining an in-flight fetch waits at most its child timeout.
    let wait_deadline = Instant::now() + timeout() + Duration::from_secs(2);
    loop {
        let mut c = lock();
        let stale = match &c.fetched {
            Some((at, models)) if at.elapsed() < ttl() => return Some(models.clone()),
            Some((_, models)) => Some(models.clone()),
            None => None,
        };
        if let Some(models) = stale {
            // Stale: serve last-good now, refresh in the background.
            kick_refresh(&mut c);
            return Some(models);
        }
        if c.refreshing {
            if Instant::now() < wait_deadline {
                drop(c);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // The fetch thread died with the flag set (panic): take over.
            c.refreshing = false;
        }
        if c.attempted.is_some_and(|t| t.elapsed() < RETRY) {
            return None;
        }
        c.attempted = Some(Instant::now());
        c.refreshing = true;
        drop(c);
        let got = fetch(timeout());
        let mut c = lock();
        c.refreshing = false;
        return match got {
            Some(models) => {
                c.fetched = Some((Instant::now(), models.clone()));
                Some(models)
            }
            None => None,
        };
    }
}

/// Refresh the stale cache without making the caller wait. Rate-limited
/// like the cold path; a failure just keeps the stale rows.
fn kick_refresh(c: &mut Cache) {
    if c.refreshing || c.attempted.is_some_and(|t| t.elapsed() < RETRY) {
        return;
    }
    c.refreshing = true;
    c.attempted = Some(Instant::now());
    std::thread::spawn(|| {
        let got = fetch(timeout());
        let mut c = lock();
        c.refreshing = false;
        if let Some(models) = got {
            c.fetched = Some((Instant::now(), models));
        }
    });
}

/// `AGY_SUB_MODELS_FILE` — a TSV file of `id<TAB>name` rows standing in
/// for `agy models`. Read on every call (it is a dev/test knob, and a
/// live edit should take effect immediately); unreadable or row-less
/// files count as "no override", not as a failure — the spawn path then
/// runs as usual.
fn file_override() -> Option<Vec<DiscoveredModel>> {
    let path = std::env::var_os("AGY_SUB_MODELS_FILE")?;
    let text = std::fs::read_to_string(path).ok()?;
    let parsed = parse_tsv(&text);
    (!parsed.is_empty()).then_some(parsed)
}

/// `agy models` → parsed rows. Any failure (missing binary, nonzero
/// exit, timeout, empty stdout) is `None` — the caller falls back to the
/// pinned table. Real HOME is used deliberately: the listing is
/// unauthenticated but may be account-scoped, and a signed-in user must
/// see their own routes. The browser is neutralized anyway so a CLI that
/// ever decided to authenticate could not pop a window mid-fetch.
fn fetch(timeout: Duration) -> Option<Vec<DiscoveredModel>> {
    let binary = crate::setup::resolve_command()?;
    let mut child = Command::new(binary)
        .arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(crate::setup::child_env())
        // Insurance against an auth flow: `agy` calls `xdg-open`
        // directly (ignores `BROWSER`), and `xdg-open` with no display
        // and `BROWSER=/bin/true` exits without touching Firefox.
        .env("BROWSER", "/bin/true")
        .env("DISPLAY", "")
        .env("WAYLAND_DISPLAY", "")
        .spawn()
        .ok()?;
    // Read stdout on a pump thread: the parent never waits on a child
    // that is itself blocked writing to a full pipe.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let buf = match rx.recv_timeout(timeout) {
        Ok(buf) => buf,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    // stdout hit EOF: the child is exiting (or gone). Give it a beat to
    // report its status, then treat a still-running child as failed.
    let deadline = Instant::now() + Duration::from_secs(2);
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => break false,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    if !ok {
        return None;
    }
    let parsed = parse_tsv(&String::from_utf8_lossy(&buf));
    (!parsed.is_empty()).then_some(parsed)
}

/// `agy models` stdout → rows. Tolerant: blank lines and non-row noise
/// (a warning or the spinner line, if it ever leaked to stdout) are
/// skipped. A row is `id<TAB>name`; the id must be a single
/// whitespace-free token carrying a digit — every `agy` id does
/// (`gemini-3.8-flash-low`), which keeps word-shaped noise out.
pub fn parse_tsv(out: &str) -> Vec<DiscoveredModel> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (id, name) = match line.split_once('\t') {
            Some((id, name)) => (id.trim(), name.trim()),
            None => (line, ""),
        };
        if id.is_empty()
            || id.chars().any(char::is_whitespace)
            || !id.bytes().any(|b| b.is_ascii_digit())
        {
            continue;
        }
        rows.push(DiscoveredModel {
            id: id.to_string(),
            name: name.to_string(),
        });
    }
    rows
}

#[cfg(test)]
pub(crate) fn reset_cache() {
    *lock() = Cache::default();
}

#[path = "discover_tests.rs"]
#[cfg(test)]
mod tests;
