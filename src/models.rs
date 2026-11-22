//! Model catalog: `agy models` is the live source of routable ids (see
//! [`crate::discover`]); the pinned table in [`crate::catalog`] only
//! fills the facts the CLI cannot report — context windows for known
//! ids — and carries the catalog when discovery cannot answer (missing
//! binary, offline: stale beats dead).
//!
//! `agy` bakes effort into the id itself (`gemini-3.8-flash-low`), so a
//! family collapses into ONE picker row: the base id is the model and
//! each tier a declared variant pinning its routable id — the picker
//! shows `gemini-3.8-flash` once and effort rides the levels knob
//! instead of three sibling rows.

use std::collections::BTreeMap;

use gray_plugin::{ModelSlot, ModelVariant, ProviderModel, ProviderModelCatalog, SlotOption};

use crate::catalog;
use crate::catalog::TIERS;

/// `id` → `(base, tier)` when it carries a baked-in effort suffix.
fn split_tier(id: &str) -> Option<(String, &'static str)> {
    TIERS.iter().find_map(|tier| {
        id.strip_suffix(&format!("-{tier}"))
            .map(|base| (base.to_string(), *tier))
    })
}

/// Display name for a collapsed family row: `agy`'s own member name with
/// the effort tag trimmed (`Gemini 3.8 Flash (Low)` → `Gemini 3.8
/// Flash`), else the title-cased base id.
fn family_name_in(snap: &catalog::Snapshot, base: &str) -> String {
    for tier in TIERS {
        let member = format!("{base}-{tier}");
        if let Some(name) = snap.live_name(&member) {
            let mut bare = name.trim_end().to_string();
            for t in TIERS {
                let tag = format!("({})", catalog::title_case(t));
                if let Some(stripped) = bare.strip_suffix(&tag) {
                    bare = stripped.trim_end().to_string();
                    break;
                }
            }
            return format!("{bare} (Antigravity)");
        }
    }
    format!("{} (Antigravity)", catalog::title_case(base))
}

/// The catalog: one row per base model — effort-suffixed ids collapse
/// under their family with each tier a declared variant — plus the
/// tierless ids as plain rows. One snapshot covers the whole listing so
/// a mid-build cache refresh cannot mix generations.
pub fn catalog() -> ProviderModelCatalog {
    catalog_in(&catalog::Snapshot::current())
}

pub(crate) fn catalog_in(snap: &catalog::Snapshot) -> ProviderModelCatalog {
    let mut models: Vec<ProviderModel> = Vec::new();
    let mut family_row: BTreeMap<String, usize> = BTreeMap::new();
    for id in catalog::all_ids_in(snap) {
        // Only routable ids earn a row. An alias either resolves onto an
        // already-listed member — a second row for the same route with
        // the same display name — or onto a dead pin, an unroutable id
        // the picker would show as "(unpinned)". Aliases still resolve
        // through `native_model`; they just aren't listed.
        if !snap.is_routable(&id) {
            continue;
        }
        if id.starts_with("gemini-pro-orchestrator-") {
            let opt_id = id
                .strip_prefix("gemini-pro-orchestrator-")
                .unwrap_or(&id)
                .to_string();
            let opt_name = match opt_id.as_str() {
                "neon" => "Gemini 4 Neon".to_string(),
                "g1-pro" | "argon" => "Gemini 4 Argon".to_string(),
                other => catalog::title_case(other),
            };
            let idx = models
                .iter()
                .position(|m| m.id == "orchestrator")
                .unwrap_or_else(|| {
                    models.push(ProviderModel {
                        id: "orchestrator".to_string(),
                        name: "Orchestrator (Antigravity)".to_string(),
                        context_window: catalog::context_window_in(snap, &id),
                        reasoning_efforts: TIERS.iter().map(|t| (*t).to_string()).collect(),
                        variants: vec![],
                        slots: vec![ModelSlot {
                            key: "planner".to_string(),
                            label: "Planner".to_string(),
                            options: vec![],
                        }],
                        price_cliff_tokens: None,
                    });
                    models.len() - 1
                });
            let row = &mut models[idx];
            if let Some(slot) = row.slots.first_mut()
                && !slot.options.iter().any(|o| o.id == opt_id)
            {
                slot.options.push(SlotOption {
                    id: opt_id.clone(),
                    name: opt_name,
                });
            }
            for tier in TIERS {
                let mut parts = std::collections::BTreeMap::new();
                parts.insert("planner".to_string(), opt_id.clone());
                row.variants.push(ModelVariant {
                    id: id.clone(),
                    effort: Some((*tier).to_string()),
                    fast: false,
                    parts,
                });
            }
            continue;
        }
        let Some((base, tier)) = split_tier(&id) else {
            models.push(ProviderModel {
                name: catalog::display_name_in(snap, &id),
                context_window: catalog::context_window_in(snap, &id),
                reasoning_efforts: Vec::new(),
                id,
                variants: vec![],
                slots: vec![],
                price_cliff_tokens: None,
            });
            continue;
        };
        let idx = match family_row.get(&base) {
            Some(&i) => i,
            None => {
                // A routable base id already listed (a tierless id sharing
                // the family name — `claude-sonnet-4-6` shape) becomes the
                // family row instead of duplicating.
                let i = models.iter().position(|m| m.id == base).unwrap_or_else(|| {
                    models.push(ProviderModel {
                        name: family_name_in(snap, &base),
                        context_window: None,
                        reasoning_efforts: Vec::new(),
                        id: base.clone(),
                        variants: vec![],
                        slots: vec![],
                        price_cliff_tokens: None,
                    });
                    models.len() - 1
                });
                family_row.insert(base.clone(), i);
                i
            }
        };
        let row = &mut models[idx];
        if row.context_window.is_none() {
            row.context_window = catalog::context_window_in(snap, &id);
        }
        row.variants.push(ModelVariant {
            id: id.clone(),
            effort: Some(tier.to_string()),
            fast: false,
            parts: Default::default(),
        });
    }
    // Tiers sort into catalog order on every family row, and the declared
    // levels mirror the variants (the host reads either surface).
    for row in &mut models {
        if row.variants.is_empty() {
            continue;
        }
        row.variants.sort_by_key(|v| {
            v.effort
                .as_deref()
                .and_then(|e| TIERS.iter().position(|t| *t == e))
                .unwrap_or(usize::MAX)
        });
        row.reasoning_efforts = row
            .variants
            .iter()
            .filter_map(|v| v.effort.clone())
            .collect();
    }
    ProviderModelCatalog { models }
}

#[path = "models_tests.rs"]
#[cfg(test)]
mod tests;
