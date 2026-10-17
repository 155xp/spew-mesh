//! The per-responder configuration built from the `[serve]` settings (11) and the serve flags (9).

use std::path::PathBuf;
use std::time::Duration;

use super::{ServeClient, ServeError};
use crate::settings::{ServeFrom, ServeSettings};

/// The `[serve]` defaults from 11, the values `Settings::defaults` uses too (a test keeps them equal).
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
    fn defaults_match_the_settings_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::Home::at(tmp.path()).unwrap();
        let settings = crate::settings::Settings::defaults(&home);
        assert_eq!(ServeSettings::default(), settings.serve);
        let cfg = ServeConfig::new(ServeClient::Claude, "/w", "/r", &settings.serve);
        assert_eq!(cfg.timeout, Duration::from_secs(300));
        assert_eq!(cfg.max_turns, 20);
        assert_eq!(cfg.max_budget_usd, 0.50);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn bad_values_on_a_command_line_are_refused() {
        let base = ServeConfig::new(ServeClient::Claude, "/w", "/r", &ServeSettings::default());
        let bad = [
            ServeConfig {
                timeout: Duration::ZERO,
                ..base.clone()
            },
            ServeConfig {
                max_turns: 0,
                ..base.clone()
            },
            ServeConfig {
                max_budget_usd: f64::NAN,
                ..base.clone()
            },
            ServeConfig {
                max_budget_usd: 0.0,
                ..base.clone()
            },
            ServeConfig {
                model: Some("--yolo".into()),
                ..base.clone()
            },
        ];
        for cfg in bad {
            assert!(cfg.validate().is_err(), "{cfg:?}");
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
