//! The control socket between the daemon and everything local that talks to it: the CLI, MCP servers, hooks and
//! responders (6.1, 6.2).

pub mod blocking;
pub mod client;
pub mod place;
pub mod protocol;
pub mod server;

pub use client::{Client, ClientError, ConnectOptions};
pub use protocol::{Reason, RpcError};
