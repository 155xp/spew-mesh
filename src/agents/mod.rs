//! Agent wiring: the MCP server, hook commands and setup for each client (8).
//!
//! Part 1 (this code): setup, uninstall and doctor for Claude Code, Codex and OpenCode, hook payload parsing and
//! output, card capture (8.5) and the skill text. The daemon side of hooks plugs in through [`hook::HookDaemon`].

pub mod cards;
pub mod claude;
pub mod codex;
pub mod fsx;
pub mod hook;
pub mod json;
pub mod manifest;
pub mod opencode;
pub mod plan;
pub mod runner;
pub mod setup;
#[cfg(test)]
mod setup_tests;
pub mod skill;

use std::fmt;

use serde::{Deserialize, Serialize};

/// An agent client setup can wire (2, 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Client {
    Claude,
    Codex,
    Opencode,
}

impl Client {
    /// Every client, in setup order.
    pub const ALL: [Client; 3] = [Client::Claude, Client::Codex, Client::Opencode];

    /// The client's slug, as in `spew mesh hook <client> <event>` and `--agents`; also its command on PATH.
    pub fn slug(self) -> &'static str {
        match self {
            Client::Claude => "claude",
            Client::Codex => "codex",
            Client::Opencode => "opencode",
        }
    }

    /// The name people know it by.
    pub fn display(self) -> &'static str {
        match self {
            Client::Claude => "Claude Code",
            Client::Codex => "Codex",
            Client::Opencode => "OpenCode",
        }
    }

    /// Parses a slug.
    pub fn from_slug(s: &str) -> Option<Client> {
        Client::ALL.into_iter().find(|c| c.slug() == s)
    }

    /// Parses the `--agents` list (`claude,codex,opencode`); an unknown name is an error.
    pub fn parse_list(s: &str) -> anyhow::Result<Vec<Client>> {
        let mut out = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let c = Client::from_slug(part).ok_or_else(|| {
                anyhow::anyhow!("unknown agent {part:?}: --agents takes claude, codex and opencode")
            })?;
            if !out.contains(&c) {
                out.push(c);
            }
        }
        if out.is_empty() {
            anyhow::bail!("--agents is empty: name at least one of claude, codex, opencode");
        }
        Ok(out)
    }
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_agents_list() {
        assert_eq!(
            Client::parse_list("codex, claude,codex").unwrap(),
            vec![Client::Codex, Client::Claude]
        );
        assert!(Client::parse_list("cursor").is_err());
        assert!(Client::parse_list(" , ").is_err());
    }
}
