//! spew mesh lets coding-agent sessions on your computers and your team's ask each other questions.
//! The design is in `docs/design.md`; section numbers in comments refer to it.

pub mod address;
pub mod agents;
pub mod cli;
pub mod control;
pub mod courier;
pub mod daemon;
pub mod identity;
pub mod link;
pub mod paths;
pub mod responder;
pub mod roster;
pub mod sessions;
pub mod settings;
pub mod spew_api;
pub mod store;
pub mod text;
pub mod wire;

/// Installs ring as the process-wide rustls provider (16.1). reqwest is built without a provider and panics
/// without one, so this runs before any client exists. Calling it again is harmless.
pub fn install_crypto_provider() {
    // An error only means a provider is already installed, which is what we want.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
