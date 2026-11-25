//! Library target for `gray-antigravity-sub`: the protocol-1.2 sidecar
//! plugin (`antigravity-sub` binary) for Gray's plugin system.

pub mod accounts;
pub mod agy;
pub mod catalog;
pub mod discover;
pub mod manifest;
pub mod models;
pub mod settings;
pub mod setup;
pub mod store;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub mod status;
pub mod usage;
