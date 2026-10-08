//! Route catalog: live first, pinned as fallback.
//!
//! `agy models` is the source of truth for routable ids (see
//! [`crate::discover`]): it reports exactly the `--model` values the
//! installed CLI and account serve, so release-shaped facts — which ids
//! exist, their display names, which effort tiers each family carries —
//! are never hardcoded here. The pinned table remains only for what the
//! CLI cannot tell us: context windows for ids we happen to know,
//! alias → cheap-default policy, and the offline/last-ditch fallback
//! (stale beats dead).
//!
//! `agy` couples model and effort in the id itself
//! (`gemini-3.8-flash-low`): full ids run WITHOUT `--effort`, base ids
//! REQUIRE it (and reject a mismatched one). The spawn path therefore
//! never passes `--effort` — effort levels are separate models, not a
//! knob — and effort mapping only swaps between ids the current
//! routable set (live, else pinned) says exist.

use crate::discover::{self, DiscoveredModel};

/// Pinned fallback ids + context windows (`None` = unknown, never
/// guessed). Discovery replaces this list wholesale once `agy models`
/// answers; the entries stay so a CLI that cannot list (missing binary,
/// older agy without the subcommand, no network) still yields a working
/// catalog, and so windows for known ids remain known.
fn pinned() -> &'static [(&'static str, Option<u32>)] {
    &[
        ("gemini-3.8-flash-high", Some(1_048_576)),
        ("gemini-3.8-flash-medium", Some(1_048_576)),
        ("gemini-3.8-flash-low", Some(1_048_576)),
        ("gemini-3.7-flash-high", Some(1_048_576)),
        ("gemini-3.7-flash-medium", Some(1_048_576)),
        ("gemini-3.7-flash-low", Some(1_048_576)),
        ("gemini-3.6-flash-high", Some(1_048_576)),
        ("gemini-3.6-flash-medium", Some(1_048_576)),
        ("gemini-3.6-flash-low", Some(1_048_576)),
        ("gemini-3.1-pro-high", Some(1_048_576)),
        ("gemini-3.1-pro-low", Some(1_048_576)),
        ("claude-opus-5-5-low", Some(1_000_000)),
        ("claude-opus-5-5-medium", Some(1_000_000)),
        ("claude-opus-5-5-high", Some(1_000_000)),
        ("claude-sonnet-5-5-low", Some(200_000)),
        ("claude-sonnet-5-5-medium", Some(200_000)),
        ("claude-sonnet-5-5-high", Some(200_000)),
        // Routable per `agy models` 1.3.1 but with no verified window:
        // kept so the offline fallback reflects the current release.
        ("claude-sonnet-4-6", None),
        ("claude-opus-4-6-thinking", None),
        ("gpt-oss-120b-medium", None),
    ]
}

/// `alias → (family substring, pinned default)`. Short names pin the
/// cheap default route; when discovery answers and the pin is no longer
/// routable (a renamed generation), the alias degrades to the best live
/// family member — `-low` first, then `-medium`, `-high`, then first
/// listed — instead of pointing at a dead id.
fn aliases() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("flash", "flash", "gemini-3.8-flash-low"),
        ("pro", "-pro-", "gemini-3.1-pro-low"),
        ("sonnet", "sonnet", "claude-sonnet-5-5-low"),
        ("opus", "opus", "claude-opus-5-5-low"),
    ]
}

/// One decision point's view of routable ids: the live list when
/// `agy models` answered, else `None` and the pinned table governs.
/// Taking it once per decision keeps a catalog or a turn consistent even
/// if the cache refreshes mid-work.
pub(crate) struct Snapshot {
    live: Option<Vec<DiscoveredModel>>,
}

impl Snapshot {
    pub(crate) fn current() -> Self {
        Self {
            live: discover::live(),
        }
    }

    #[cfg(test)]
    fn pinned_only() -> Self {
        Self { live: None }
    }

    #[cfg(test)]
    fn with_live(live: &[(&str, &str)]) -> Self {
        Self {
            live: Some(
                live.iter()
                    .map(|(id, name)| DiscoveredModel {
                        id: (*id).to_string(),
                        name: (*name).to_string(),
                    })
                    .collect(),
            ),
        }
    }

    /// Routable `--model` ids in reported order (live list, or the
    /// pinned table when discovery failed).
    fn routable(&self) -> Vec<String> {
        match &self.live {
            Some(models) => models.iter().map(|m| m.id.clone()).collect(),
            None => pinned().iter().map(|(id, _)| (*id).to_string()).collect(),
        }
    }

    /// `id` is a real route: on the live list, or pinned when discovery
    /// is down. A pinned id absent from an answered live list no longer
    /// counts — the CLI's own listing outranks our memory of it.
    fn is_routable(&self, id: &str) -> bool {
        match &self.live {
            Some(models) => models.iter().any(|m| m.id == id),
            None => pinned().iter().any(|(pin, _)| *pin == id),
        }
    }

    /// The display name `agy` itself reports for a live id.
    fn live_name(&self, id: &str) -> Option<&str> {
        self.live
            .as_ref()?
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| (!m.name.is_empty()).then_some(m.name.as_str()))
    }

    /// Alias → routable id: the pinned default while it is still
    /// routable (or while discovery is down and the pin is all we have),
    /// else the best live member of the same family. Returns the pin
    /// when nothing better exists — a dead pin and a bare alias name
    /// fail the same way upstream, and the pin errors clearer.
    fn resolve_alias(&self, alias: &str) -> Option<String> {
        let (_, family, pin) = aliases().iter().find(|(a, _, _)| *a == alias)?;
        if self.is_routable(pin) {
            return Some((*pin).to_string());
        }
        if let Some(live) = &self.live {
            let family: Vec<&str> = live
                .iter()
                .map(|m| m.id.as_str())
                .filter(|id| id.contains(family))
                .collect();
            for tier in ["low", "medium", "high"] {
                let suffix = format!("-{tier}");
                if let Some(id) = family.iter().find(|id| id.ends_with(&suffix)) {
                    return Some((*id).to_string());
                }
            }
            if let Some(id) = family.first() {
                return Some((*id).to_string());
            }
        }
        Some((*pin).to_string())
    }
}

fn canonical_in(snap: &Snapshot, model: &str) -> String {
    snap.resolve_alias(model)
        .unwrap_or_else(|| model.to_string())
}

/// Context window for a route: pinned table, `None` when unknown.
/// Unpinned future ids also report `None` — never a guessed window.
pub fn context_window(model: &str) -> Option<u32> {
    context_window_in(&Snapshot::current(), model)
}

pub(crate) fn context_window_in(snap: &Snapshot, model: &str) -> Option<u32> {
    let canon = canonical_in(snap, model);
    pinned()
        .iter()
        .find(|(id, _)| *id == canon)
        .and_then(|(_, w)| *w)
}

/// Native `--model` selection: aliases resolve to routable full ids
/// (degrading within the family when the pin aged out), everything else
/// passes through untouched so future `agy models` entries keep working.
pub fn native_model(model: &str) -> String {
    canonical_in(&Snapshot::current(), model)
}

/// Effort-aware route: the host's `/thinking` pick (`reasoning.effort`)
/// swaps the resolved id's effort tier — but only onto another routable
/// id (the live list when discovery answered, else the pinned table).
/// agy couples effort into the model id (`gemini-3.8-flash-low`) and
/// full ids run without `--effort`, so an unverifiable swap would route
/// somewhere untrusted; those keep the resolved id instead (`xhigh`/`max`
/// match no listed tier today, and `pro` lists no `-medium`).
pub fn native_model_for_effort(model: &str, effort: Option<&str>) -> String {
    native_model_for_effort_in(&Snapshot::current(), model, effort)
}

pub(crate) fn native_model_for_effort_in(
    snap: &Snapshot,
    model: &str,
    effort: Option<&str>,
) -> String {
    let canon = canonical_in(snap, model);
    let Some(effort) = effort else { return canon };
    for tier in ["low", "medium", "high"] {
        let suffix = format!("-{tier}");
        if let Some(base) = canon.strip_suffix(&suffix) {
            let candidate = format!("{base}-{effort}");
            if snap.is_routable(&candidate) {
                return candidate;
            }
            return canon;
        }
    }
    canon
}

/// Every routable id: the live list (or the pinned fallback) plus the
/// alias names the host may address them by.
pub fn all_ids() -> Vec<String> {
    all_ids_in(&Snapshot::current())
}

pub(crate) fn all_ids_in(snap: &Snapshot) -> Vec<String> {
    let mut out: Vec<String> = snap.routable();
    for (alias, _, _) in aliases() {
        out.push((*alias).to_string());
    }
    out.sort();
    out.dedup();
    out
}

/// Display name for a route id: `agy`'s own name for live ids, a
/// title-cased id otherwise (aliases show their resolved route).
pub fn display_name(id: &str) -> String {
    display_name_in(&Snapshot::current(), id)
}

pub(crate) fn display_name_in(snap: &Snapshot, id: &str) -> String {
    let canon = canonical_in(snap, id);
    if let Some(name) = snap.live_name(&canon) {
        return format!("{name} (Antigravity)");
    }
    let titled = title_case(&canon);
    if snap.is_routable(&canon) {
        format!("{titled} (Antigravity)")
    } else {
        format!("{titled} (Antigravity, unpinned)")
    }
}

/// Title-case each word without pulling in a dependency.
/// `5-5` in a claude id is `5.5` upstream: a lone digit folds onto a
/// digit-ending word. (`3.8` gemini ids already carry real dots.)
fn title_case(id: &str) -> String {
    let short = id.replace('-', " ");
    let mut words: Vec<String> = Vec::new();
    for w in short.split_whitespace() {
        let lone_digit = w.len() == 1 && w.bytes().next().is_some_and(|b| b.is_ascii_digit());
        if lone_digit
            && words
                .last()
                .is_some_and(|p: &String| p.ends_with(|c: char| c.is_ascii_digit()))
        {
            words.last_mut().unwrap().push('.');
            words.last_mut().unwrap().push_str(w);
            continue;
        }
        let mut c = w.chars();
        words.push(match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        });
    }
    words.join(" ")
}

#[path = "catalog_tests.rs"]
#[cfg(test)]
mod tests;
