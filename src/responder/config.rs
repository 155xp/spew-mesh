//! The `[serve]` settings (11) and the per-responder configuration built from them and the serve flags (9).

use std::path::PathBuf;
use std::time::Duration;

use super::{ServeClient, ServeError};

/// Who may ask a responder (`[serve] from`, 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ServeFrom {
    /// Anyone admitted who shares the responder's room.
    #[default]
    Room,
    /// Only the person's own devices.
    Own,
}

/// The `[serve]` table of settings.toml, with the defaults from 11.
#[derive(Clone, Debug, PartialEq)]
pub struct ServeSettings {
    /// Per-run timeout when `--timeout` isn't given.
    pub timeout_seconds: u64,
    /// `--max-turns` for Claude Code runs.
    pub max_turns: u32,
    /// Who may ask.
    pub from: ServeFrom,
    /// Runs per asking person per rolling hour.
    pub max_runs_per_person_per_hour: u32,
    /// Runs per rolling hour in all.
    pub max_runs_per_hour: u32,
    /// `--max-budget-usd` for Claude Code runs.
    pub max_budget_usd: f64,
}

impl Default for ServeSettings {
    fn default() -> Self {
        ServeSettings {
            timeout_seconds: 300,
            max_turns: 20,
            from: ServeFrom::Room,
            max_runs_per_person_per_hour: 10,
            max_runs_per_hour: 30,
            max_budget_usd: 0.50,
        }
    }
}

impl ServeSettings {
    /// Rejects values that would remove a limit or can't be passed to a client.
    pub fn validate(&self) -> Result<(), ServeError> {
        if self.timeout_seconds == 0 {
            return Err(ServeError::Invalid(
                "timeout_seconds must be at least 1".into(),
            ));
        }
        if self.max_turns == 0 {
            return Err(ServeError::Invalid("max_turns must be at least 1".into()));
        }
        if self.max_runs_per_person_per_hour == 0 || self.max_runs_per_hour == 0 {
            return Err(ServeError::Invalid(
                "max_runs_per_person_per_hour and max_runs_per_hour must be at least 1".into(),
            ));
        }
        validate_budget(self.max_budget_usd)
    }
}

pub(crate) fn validate_budget(usd: f64) -> Result<(), ServeError> {
    if usd.is_finite() && usd > 0.0 {
        Ok(())
    } else {
        Err(ServeError::Invalid(format!(
            "max_budget_usd must be a positive number, not {usd}"
        )))
    }
}

/// Everything one responder needs to build and run its commands.
#[derive(Clone, Debug, PartialEq)]
pub struct ServeConfig {
    /// The client to drive.
    pub client: ServeClient,
    /// `--cwd`: the folder the runs may read (canonicalized at start).
    pub cwd: PathBuf,
    /// `--allow-edits`.
    pub allow_edits: bool,
    /// `--model`.
    pub model: Option<String>,
    /// `--timeout`, else `[serve] timeout_seconds`.
    pub timeout: Duration,
    /// `[serve] max_turns`.
    pub max_turns: u32,
    /// `[serve] max_budget_usd`.
    pub max_budget_usd: f64,
    /// `<home>/run/serve`: Codex run files and the sandbox canary live here.
    pub run_dir: PathBuf,
    /// Largest stdout a run may print before it is stopped.
    pub stdout_cap: usize,
    /// Whether Claude Code runs get Grep. Turned off when Grep is found not to honor the deny rules (8.2).
    pub claude_grep: bool,
}

/// Default stdout cap: Codex and OpenCode print every tool result in their JSON streams.
pub const DEFAULT_STDOUT_CAP: usize = 8 * 1024 * 1024;

impl ServeConfig {
    /// A configuration with the settings' limits and no serve flags beyond the client and folder.
    pub fn new(
        client: ServeClient,
        cwd: impl Into<PathBuf>,
        run_dir: impl Into<PathBuf>,
        settings: &ServeSettings,
    ) -> Self {
        ServeConfig {
            client,
            cwd: cwd.into(),
            allow_edits: false,
            model: None,
            timeout: Duration::from_secs(settings.timeout_seconds),
            max_turns: settings.max_turns,
            max_budget_usd: settings.max_budget_usd,
            run_dir: run_dir.into(),
            stdout_cap: DEFAULT_STDOUT_CAP,
            claude_grep: true,
        }
    }

    /// Checks the values that end up on a command line.
    pub fn validate(&self) -> Result<(), ServeError> {
        if self.timeout.is_zero() {
            return Err(ServeError::Invalid("the timeout must be positive".into()));
        }
        if self.max_turns == 0 {
            return Err(ServeError::Invalid("max_turns must be at least 1".into()));
        }
        if self.stdout_cap == 0 {
            return Err(ServeError::Invalid(
                "the output cap must be positive".into(),
            ));
        }
        validate_budget(self.max_budget_usd)?;
        if let Some(model) = &self.model {
            validate_model(model)?;
        }
        Ok(())
    }
}

/// A model name goes into argv as its own element; one that starts with `-` would read as a flag.
pub(crate) fn validate_model(model: &str) -> Result<(), ServeError> {
    let ok = !model.is_empty()
        && model.len() <= 200
        && !model.starts_with('-')
        && model.chars().all(|c| c.is_ascii_graphic());
    if ok {
        Ok(())
    } else {
        Err(ServeError::Invalid(format!(
            "{model:?} is not a model name"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let s = ServeSettings::default();
        assert_eq!(s.timeout_seconds, 300);
        assert_eq!(s.max_turns, 20);
        assert_eq!(s.from, ServeFrom::Room);
        assert_eq!(s.max_runs_per_person_per_hour, 10);
        assert_eq!(s.max_runs_per_hour, 30);
        assert_eq!(s.max_budget_usd, 0.50);
        assert!(s.validate().is_ok());
    }

    #[test]
    fn zero_or_bad_limits_are_refused() {
        let bad = [
            ServeSettings {
                timeout_seconds: 0,
                ..Default::default()
            },
            ServeSettings {
                max_turns: 0,
                ..Default::default()
            },
            ServeSettings {
                max_runs_per_hour: 0,
                ..Default::default()
            },
            ServeSettings {
                max_budget_usd: 0.0,
                ..Default::default()
            },
            ServeSettings {
                max_budget_usd: f64::NAN,
                ..Default::default()
            },
            ServeSettings {
                max_budget_usd: f64::INFINITY,
                ..Default::default()
            },
        ];
        for s in bad {
            assert!(s.validate().is_err(), "{s:?}");
        }
    }

    #[test]
    fn model_names() {
        assert!(validate_model("claude-sonnet-5").is_ok());
        assert!(validate_model("openai/gpt-6").is_ok());
        assert!(validate_model("--dangerously-skip-permissions").is_err());
        assert!(validate_model("two words").is_err());
        assert!(validate_model("").is_err());
    }
}
