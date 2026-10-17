//! spew mesh lets coding-agent sessions on your computers and your team's ask each other questions.
//! The design is in `docs/design.md`; section numbers in comments refer to it.

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
