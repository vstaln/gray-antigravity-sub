//! Model catalog: `agy models` is the live source of routable ids (see
//! [`crate::discover`]); the pinned table in [`crate::catalog`] only
//! fills the facts the CLI cannot report — context windows for known
//! ids — and carries the catalog when discovery cannot answer (missing
//! binary, offline: stale beats dead).

use gray_plugin::{ProviderModel, ProviderModelCatalog};

use crate::catalog;

/// Full `agy` ids already encode effort, so there is no effort knob: the
/// catalog advertises no reasoning efforts.
const EFFORTS: &[&str] = &[];

/// The catalog: every routable id (live when `agy models` answers, else
/// the pinned fallback) with its window. One snapshot covers the whole
/// listing so a mid-build cache refresh cannot mix generations.
pub fn catalog() -> ProviderModelCatalog {
    let snap = catalog::Snapshot::current();
    let models = catalog::all_ids_in(&snap)
        .into_iter()
        .map(|id| ProviderModel {
            name: catalog::display_name_in(&snap, &id),
            context_window: catalog::context_window_in(&snap, &id),
            reasoning_efforts: EFFORTS.iter().map(|s| s.to_string()).collect(),
            id,
            // Full `agy` ids already encode effort: no effort-mapped
            // variants, no composite slots.
            variants: vec![],
            slots: vec![],
        })
        .collect();
    ProviderModelCatalog { models }
}

#[path = "models_tests.rs"]
#[cfg(test)]
mod tests;
