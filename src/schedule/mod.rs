//! Scheduled runs (spew#237): a person drops a spew card into a 5-hour session on the app's Schedule page and
//! picks the agent, model and effort; while this computer is on, the daemon runs that card headless at that time.
//!
//! The daemon tells spew what this computer can run ([`DeviceCapabilities`]: the agent CLIs, their live model
//! lists, effort levels, the plan each one is signed in with and its usage, and the projects it has a folder
//! for), polls for due runs, takes each one it can run, runs it in the project's folder and reports how it
//! ended. The shapes here mirror spew-app's `shared/schedule.ts`; change both together.
//!
//! Only capability lists and plan names go to spew. Tokens in the CLIs' credential files are never sent or
//! logged: detection reads the plan fields alone.

pub mod command;
pub mod detect;
pub mod folders;
pub mod scheduler;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::responder::ServeClient;
use crate::store::ScheduleRunRecord;

pub use scheduler::{Scheduler, SchedulerConfig};

/// The agents a computer can run headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
    /// OpenCode.
    Opencode,
}

impl AgentKind {
    /// Every kind, in the order they are detected and shown.
    pub const ALL: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Opencode];

    /// The kind as spew writes it, and the executable looked up on PATH.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
        }
    }

    /// The name people know it by.
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::Opencode => "OpenCode",
        }
    }

    /// The serve client with the same CLI, whose list of parent-session variables a run drops.
    pub fn serve_client(self) -> ServeClient {
        match self {
            AgentKind::Claude => ServeClient::Claude,
            AgentKind::Codex => ServeClient::Codex,
            AgentKind::Opencode => ServeClient::Opencode,
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AgentKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("{s:?} is not claude, codex or opencode"))
    }
}

/// A model as the computer reports it, so the app never carries a stale list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    /// What the CLI takes: `opus`, `gpt-6.1-sol`, `opencode-go/kimi-k3`.
    pub id: String,
    /// `Opus 5.5`, `GPT-6.1-Sol`.
    pub label: String,
    /// Effort levels the CLI accepts for this model, lowest first.
    pub efforts: Vec<String>,
    /// The CLI's default effort for this model, when it says.
    #[serde(default)]
    pub default_effort: Option<String>,
}

/// Usage the CLI last reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Percent of the window used.
    pub used_pct: f64,
    /// The window's length in minutes.
    pub window_minutes: u64,
    /// When the window resets, ms since the epoch.
    pub resets_at: Option<u64>,
}

/// One agent CLI found on this computer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapability {
    /// Which agent.
    pub kind: AgentKind,
    /// Its version, `2.1.312`.
    pub version: String,
    /// The plan the CLI is signed in with, from its own credentials: `Max 20x`, `Pro`. None for API keys.
    pub plan: Option<String>,
    /// The models it takes.
    pub models: Vec<ModelOption>,
    /// Usage the CLI last reported, when it reports any: its main window.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Every usage window the CLI reports (Claude's 5-hour and weekly, Codex's primary and secondary), so the
    /// Schedule page can mark each reset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<Usage>,
}

/// What this computer can run, as sent to spew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCapabilities {
    /// The agent CLIs found.
    pub agents: Vec<AgentCapability>,
    /// The project keys this computer has a folder for.
    pub projects: Vec<String>,
    /// When this was put together, ms.
    pub reported_at: u64,
}

/// A run spew hands this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DueRun {
    /// spew's run id.
    pub id: String,
    /// The project key.
    pub project: String,
    /// The card, `spew#42`.
    pub issue: String,
    /// The card's title.
    pub title: String,
    /// Which agent runs it.
    pub agent: AgentKind,
    /// The model the CLI gets.
    pub model: String,
    /// The effort the CLI gets.
    pub effort: String,
    /// The full prompt.
    pub prompt: String,
    /// When it was scheduled, ms.
    pub run_at: u64,
    /// The run is missed if it hasn't started by this time, ms.
    pub deadline: u64,
}

/// How a run ended, as told to spew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FinishStatus {
    /// The CLI exited 0.
    Done,
    /// It failed, timed out or couldn't start.
    Failed,
}

impl FinishStatus {
    /// The status as written.
    pub fn as_str(self) -> &'static str {
        match self {
            FinishStatus::Done => "done",
            FinishStatus::Failed => "failed",
        }
    }
}

/// `POST /runs/:id/finish`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFinish {
    /// done or failed.
    pub status: FinishStatus,
    /// The last part of what the run printed, or why it failed.
    pub summary: String,
    /// The CLI's exit status, when it exited by itself.
    pub exit_code: Option<i32>,
}

/// Where a project's folder came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderSource {
    /// `[schedule.folders]` in settings.toml.
    Settings,
    /// An agent session worked there.
    Sessions,
    /// A `.spew.json` under the home folder names the project.
    Found,
}

/// A project and the folder its cards run in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFolderInfo {
    /// The project key.
    pub project: String,
    /// The folder.
    pub folder: String,
    /// Where it came from.
    pub source: FolderSource,
}

/// A due run the daemon saw at its last poll, and what it did about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenRun {
    /// spew's run id.
    pub id: String,
    /// The card.
    pub issue: String,
    /// The card's title.
    pub title: String,
    /// The agent.
    pub agent: String,
    /// The model.
    pub model: String,
    /// The effort.
    pub effort: String,
    /// When it was scheduled, ms.
    pub run_at: u64,
    /// Its deadline, ms.
    pub deadline: u64,
    /// What happened: started, or why it was left for later or another computer.
    pub note: String,
}

/// `schedule.status`: what `spew mesh schedule` and `doctor` show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleStatus {
    /// `[schedule] enabled`.
    pub enabled: bool,
    /// Signed in to spew.
    pub signed_in: bool,
    /// One line on the state: running, off, waiting for sign-in, or spew's last error.
    pub state: String,
    /// What was last detected, sent to spew or not.
    pub capabilities: Option<DeviceCapabilities>,
    /// When capabilities last reached spew, ms.
    pub reported_at: Option<u64>,
    /// When `/runs/due` was last read, ms.
    pub polled_at: Option<u64>,
    /// The last error talking to spew, if the last try failed.
    pub last_error: Option<String>,
    /// The folder for each project.
    pub folders: Vec<ProjectFolderInfo>,
    /// The due runs from the last poll.
    pub due: Vec<SeenRun>,
    /// Runs this device started, newest first.
    pub recent: Vec<ScheduleRunRecord>,
    /// The agents with a run going now.
    pub running: Vec<String>,
}
