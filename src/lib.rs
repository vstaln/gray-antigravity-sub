//! Library target for `gray-antigravity-sub`.
//!
//! Provides both:
//! 1. The protocol-1.2 sidecar plugin (`antigravity-sub` binary) for Gray's plugin system.
//! 2. The direct in-process `Provider` implementation (`direct_provider`) implementing `gray_core::agent::Provider`.

pub mod catalog;
pub mod chat;
pub mod direct_provider;
pub mod manifest;
pub mod models;
pub mod relay;
pub mod setup;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
