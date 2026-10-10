//! Library target for `gray-antigravity-sub`: the protocol-1.2 sidecar
//! plugin (`antigravity-sub` binary) for Gray's plugin system.

pub mod catalog;
pub mod chat;
pub mod discover;
pub mod live;
pub mod manifest;
pub mod models;
pub mod relay;
pub mod settings;
pub mod setup;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub mod usage;
