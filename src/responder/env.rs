//! The serve process's environment and the child environment for each client (8.2 to 8.4).
//!
//! The environment is captured once and passed in, so command builders and tests never read globals.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{ServeClient, claude, codex, opencode};

/// A snapshot of an environment: the serve process's own, or a test's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEnv {
    vars: Vec<(OsString, OsString)>,
}

impl HostEnv {
    /// An environment from name and value pairs; a later duplicate name wins.
    pub fn new<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut out: Vec<(OsString, OsString)> = Vec::new();
        for (k, v) in vars {
            let k = k.into();
            out.retain(|(name, _)| *name != k);
            out.push((k, v.into()));
        }
        HostEnv { vars: out }
    }

    /// The current process's environment.
    pub fn from_process() -> Self {
        HostEnv::new(std::env::vars_os())
    }

    /// The value of one variable.
    pub fn get(&self, key: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_os_str())
    }

    /// An absolute path from a variable, ignoring empty or relative values.
    pub fn path_var(&self, key: &str) -> Option<PathBuf> {
        let value = self.get(key)?;
        let path = PathBuf::from(value);
        path.is_absolute().then_some(path)
    }

    /// The person's home folder (`HOME`).
    pub fn home(&self) -> Option<PathBuf> {
        self.path_var("HOME")
    }

    /// Finds an executable on PATH. Empty and relative PATH entries are skipped: they resolve against the
    /// serve folder, where a repo could plant its own `claude`.
    pub fn find_program(&self, name: &str) -> Option<PathBuf> {
        let path = self.get("PATH")?;
        std::env::split_paths(path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    }

    /// The child environment for a client: this environment minus the client's scrub list, with `adds`
    /// set (replacing any inherited value of the same name).
    pub fn child_env(
        &self,
        client: ServeClient,
        adds: &[(&str, String)],
    ) -> Vec<(OsString, OsString)> {
        let mut out: Vec<(OsString, OsString)> = self
            .vars
            .iter()
            .filter(|(k, _)| !is_scrubbed(client, k))
            .filter(|(k, _)| !adds.iter().any(|(name, _)| k == name))
            .cloned()
            .collect();
        out.extend(
            adds.iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        out
    }
}

/// True when a variable describes the parent agent session rather than the person's setup, and so must
/// not reach a serve run of this client.
pub fn is_scrubbed(client: ServeClient, key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    match client {
        ServeClient::Claude => {
            claude::SCRUB.contains(&key) || key.starts_with(claude::SCRUB_PREFIX)
        }
        ServeClient::Codex => codex::SCRUB.contains(&key),
        ServeClient::Opencode => opencode::SCRUB.contains(&key),
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn keys(env: &[(OsString, OsString)]) -> Vec<String> {
        env.iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn claude_scrub_drops_exactly_the_listed_names() {
        let mut vars: Vec<(String, String)> = claude::SCRUB
            .iter()
            .map(|k| (k.to_string(), "x".to_string()))
            .collect();
        vars.push(("CLAUDE_PLUGIN_ROOT".into(), "x".into()));
        vars.push(("CLAUDE_PLUGIN_OPTION_TOKEN".into(), "x".into()));
        let kept = [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "HOME",
            "PATH",
            "CODEX_THREAD_ID",
        ];
        for k in kept {
            vars.push((k.into(), "y".into()));
        }
        let host = HostEnv::new(vars);
        let env = host.child_env(ServeClient::Claude, &[]);
        assert_eq!(keys(&env), kept);
    }

    #[test]
    fn codex_scrub_drops_its_ids() {
        let host = HostEnv::new([
            ("CODEX_THREAD_ID", "t"),
            ("CODEX_SESSION_ID", "s"),
            ("CODEX_TOOL_CALL_ID", "c"),
            ("CODEX_HOME", "/home/sam/.codex"),
            ("CLAUDECODE", "1"),
            ("OPENAI_API_KEY", "k"),
        ]);
        let env = host.child_env(ServeClient::Codex, &[]);
        assert_eq!(keys(&env), ["CODEX_HOME", "CLAUDECODE", "OPENAI_API_KEY"]);
    }

    #[test]
    fn opencode_scrub_drops_its_parent_and_permission_variables() {
        let host = HostEnv::new([
            ("OPENCODE_PERMISSION", "{\"*\":\"allow\"}"),
            ("OPENCODE", "1"),
            ("OPENCODE_PID", "4242"),
            ("OPENCODE_CONFIG_DIR", "/home/sam/oc"),
            ("XDG_CONFIG_HOME", "/home/sam/.config"),
            ("CODEX_THREAD_ID", "t"),
        ]);
        let env = host.child_env(
            ServeClient::Opencode,
            &[("OPENCODE_DISABLE_PROJECT_CONFIG", "1".to_string())],
        );
        assert_eq!(
            keys(&env),
            [
                "OPENCODE_CONFIG_DIR",
                "XDG_CONFIG_HOME",
                "CODEX_THREAD_ID",
                "OPENCODE_DISABLE_PROJECT_CONFIG"
            ]
        );
    }

    #[test]
    fn adds_replace_inherited_values() {
        let host = HostEnv::new([
            ("OPENCODE_CONFIG_CONTENT", "{\"permission\":\"allow\"}"),
            ("HOME", "/home/sam"),
        ]);
        let env = host.child_env(
            ServeClient::Opencode,
            &[("OPENCODE_CONFIG_CONTENT", "{}".to_string())],
        );
        assert_eq!(
            env,
            vec![
                (OsString::from("HOME"), OsString::from("/home/sam")),
                (
                    OsString::from("OPENCODE_CONFIG_CONTENT"),
                    OsString::from("{}")
                ),
            ]
        );
    }

    #[test]
    fn later_duplicates_win() {
        let host = HostEnv::new([("A", "1"), ("A", "2")]);
        assert_eq!(host.get("A"), Some(OsStr::new("2")));
    }

    #[test]
    fn find_program_skips_relative_entries_and_non_executables() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let plain = other.join("claude");
        std::fs::write(&plain, "not executable").unwrap();
        let exe = bin.join("claude");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = format!("relative/bin::{}:{}", other.display(), bin.display());
        let host = HostEnv::new([("PATH", path)]);
        assert_eq!(host.find_program("claude"), Some(exe));
        assert_eq!(host.find_program("codex"), None);
        assert_eq!(HostEnv::default().find_program("claude"), None);
    }

    #[test]
    fn home_must_be_absolute() {
        assert_eq!(HostEnv::new([("HOME", "relative")]).home(), None);
        assert_eq!(
            HostEnv::new([("HOME", "/home/sam")]).home(),
            Some(PathBuf::from("/home/sam"))
        );
    }
}
