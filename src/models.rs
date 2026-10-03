//! Pinned model catalog: no HTTP `/models` endpoint exists, so the pinned
//! route table IS the catalog. The CLI's own `agy models` list is the live
//! list; here discovery degrades to the pinned table when the CLI is
//! missing or logged out.

use gray_plugin::{ProviderModel, ProviderModelCatalog};

use crate::catalog;

/// Full `agy` ids already encode effort, so there is no effort knob: the
/// catalog advertises no reasoning efforts.
const EFFORTS: &[&str] = &[];

/// The pinned catalog: every routable id with its window.
pub fn catalog() -> ProviderModelCatalog {
    let models = catalog::all_ids()
        .into_iter()
        .map(|id| ProviderModel {
            name: catalog::display_name(&id),
            context_window: catalog::context_window(&id),
            reasoning_efforts: EFFORTS.iter().map(|s| s.to_string()).collect(),
            id,
        })
        .collect();
    ProviderModelCatalog { models }
}

#[path = "models_tests.rs"]
#[cfg(test)]
mod tests;
