//! Codex serve runs (8.3): `codex exec` under a deny-by-default permission profile defined on the command
//! line, plus the checks that keep the person's and the repo's config from weakening it.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::Value as Json;
use toml_edit::{DocumentMut, InlineTable, Item, Value};

use super::config::validate_model;
use super::process::{Exit, ProcessRunner, RunOutput, RunSpec};
use super::{RunFailure, RunResult, ServeError, env::HostEnv, is_safe_id, one_line, random_hex};

/// Variables Codex sets for the commands its model runs; dropped from the child environment.
pub const SCRUB: &[&str] = &["CODEX_THREAD_ID", "CODEX_SESSION_ID", "CODEX_TOOL_CALL_ID"];

/// The permission profile's name, and the thread source tag.
pub const PROFILE: &str = "spew-mesh-serve";

/// The positional prompt for follow-ups when Codex rejects `resume <id> -`; the message goes on stdin.
pub const RESUME_PROMPT: &str = "Answer the question in the stdin block.";

/// Project-config keys that make serve refuse to start.
pub const PROJECT_KEYS: &[&str] = &["sandbox_mode", "mcp_servers", "hooks", "notify", "features"];

/// The only keys copied from the person's `[model_providers.<id>]` table.
pub const PROVIDER_KEYS: &[&str] = &["name", "base_url", "env_key", "wire_api", "query_params"];

/// Paths in the workspace that stay out of reach, read-only or with `--allow-edits`.
pub const WORKSPACE_DENY: &[&str] = &[
    "**/.env",
    "**/*.env",
    "**/.env.*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
    ".git/config",
    ".git/hooks",
];

/// The permission profile, with `<codex dir>` filled in. `--allow-edits` makes the workspace writable;
/// the deny entries stay.
pub fn profile(codex_dir: &Path, allow_edits: bool) -> Result<String, ServeError> {
    let dir = codex_dir.to_str().ok_or_else(|| {
        ServeError::Invalid(format!(
            "the Codex install path {} is not UTF-8",
            codex_dir.display()
        ))
    })?;
    let workspace = if allow_edits { "write" } else { "read" };
    let deny: String = WORKSPACE_DENY
        .iter()
        .map(|p| format!(", {}=\"deny\"", toml_string(p)))
        .collect();
    Ok(format!(
        "{{extends=\":read-only\", filesystem={{\":root\"=\"deny\", \":minimal\"=\"read\", {dir}=\"read\", \
         \":workspace_roots\"={{\".\"=\"{workspace}\"{deny}}}}}, network={{enabled=false}}}}",
        dir = toml_string(dir),
    ))
}

/// A TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home(host: &HostEnv) -> Result<PathBuf, ServeError> {
    if let Some(home) = host.path_var("CODEX_HOME") {
        return Ok(home);
    }
    host.home()
        .map(|h| h.join(".codex"))
        .ok_or_else(|| ServeError::Invalid("neither CODEX_HOME nor HOME is set".into()))
}

/// The folder Codex needs to read inside its sandbox to re-run itself, from the real path of `codex`.
pub fn codex_dir(real_binary: &Path, codex_home: &Path) -> PathBuf {
    let parts: Vec<Component<'_>> = real_binary.components().collect();
    for i in 0..parts.len().saturating_sub(2) {
        let is = |j: usize, name: &str| matches!(parts[j], Component::Normal(p) if p == name);
        if is(i, "node_modules") && is(i + 1, "@openai") && is(i + 2, "codex") {
            let package: PathBuf = parts[..=i + 2].iter().collect();
            return hoisted_platform_package(&package).unwrap_or(package);
        }
    }
    // `current` is usually a symlink to a versioned folder, and the binary's real path goes through
    // the target, so compare against both forms.
    let standalone = codex_home
        .join("packages")
        .join("standalone")
        .join("current");
    let resolved = std::fs::canonicalize(&standalone).ok();
    for candidate in [Some(standalone), resolved].into_iter().flatten() {
        if real_binary.starts_with(&candidate) {
            return candidate;
        }
    }
    real_binary
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| real_binary.to_path_buf())
}

/// The npm package that holds the native Codex binary when it sits outside the `@openai/codex` wrapper
/// package. A global npm install nests it inside the wrapper (the spec's case), but a local or `--prefix`
/// install hoists it next to the wrapper, where the sandbox can't see it and every command fails with
/// `bwrap: execvp ... No such file or directory`. The wrapper finds it through Node's module lookup, so this
/// looks in the same places: `<dir>/node_modules` for each ancestor `<dir>` that isn't a `node_modules` folder.
fn hoisted_platform_package(package: &Path) -> Option<PathBuf> {
    let name = platform_package()?;
    if package
        .join("node_modules")
        .join("@openai")
        .join(name)
        .is_dir()
    {
        return None;
    }
    package
        .ancestors()
        .skip(1)
        .filter(|dir| dir.file_name().is_none_or(|n| n != "node_modules"))
        .map(|dir| dir.join("node_modules").join("@openai").join(name))
        .find(|candidate| candidate.is_dir())
        .and_then(|found| std::fs::canonicalize(found).ok())
}

/// The platform package the npm wrapper runs on this computer (`@openai/<name>`).
fn platform_package() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("codex-linux-x64"),
        ("linux", "aarch64") => Some("codex-linux-arm64"),
        ("macos", "x86_64") => Some("codex-darwin-x64"),
        ("macos", "aarch64") => Some("codex-darwin-arm64"),
        _ => None,
    }
}

/// The model and provider a run uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelChoice {
    /// `-m`: `--model`, else `model` from config.toml.
    pub model: Option<String>,
    /// `model_provider` from config.toml, with its copied table.
    pub provider: Option<Provider>,
}

/// A model provider passed with `-c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// The provider id (`model_provider`).
    pub id: String,
    /// The copied `[model_providers.<id>]` table as an inline table; none for a built-in provider.
    pub table: Option<String>,
}

/// Reads `model` and `model_provider` from the person's config.toml. A missing file means none.
pub fn read_user_config(path: &Path, model_flag: Option<&str>) -> Result<ModelChoice, ServeError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ServeError::Config {
                path: path.to_path_buf(),
                reason: e.to_string(),
            });
        }
    };
    model_choice(&text, path, model_flag)
}

/// [`read_user_config`] on text already read; `path` names the file in errors.
pub fn model_choice(
    text: &str,
    path: &Path,
    model_flag: Option<&str>,
) -> Result<ModelChoice, ServeError> {
    let bad = |reason: String| ServeError::Config {
        path: path.to_path_buf(),
        reason,
    };
    let doc: DocumentMut = text.parse().map_err(|e| bad(format!("{e}")))?;

    let model = match model_flag {
        Some(m) => Some(m.to_string()),
        None => match doc.get("model") {
            None => None,
            Some(item) => Some(
                item.as_str()
                    .ok_or_else(|| bad("model is not a string".into()))?
                    .to_string(),
            ),
        },
    };
    if let Some(m) = &model {
        validate_model(m)?;
    }

    let provider = match doc.get("model_provider") {
        None => None,
        Some(item) => {
            let id = item
                .as_str()
                .ok_or_else(|| bad("model_provider is not a string".into()))?;
            if !is_bare_key(id) {
                return Err(bad(format!("model_provider {id:?} is not a plain id")));
            }
            let tables = match doc.get("model_providers") {
                None => None,
                Some(item) => Some(
                    item.as_table_like()
                        .ok_or_else(|| bad("model_providers is not a table".into()))?,
                ),
            };
            let table = match tables.and_then(|t| t.get(id)) {
                None => None,
                Some(item) => Some(copy_provider(id, item, path)?),
            };
            Some(Provider {
                id: id.to_string(),
                table,
            })
        }
    };
    Ok(ModelChoice { model, provider })
}

fn copy_provider(id: &str, item: &Item, path: &Path) -> Result<String, ServeError> {
    let bad = |reason: String| ServeError::Config {
        path: path.to_path_buf(),
        reason,
    };
    let table = item
        .as_table_like()
        .ok_or_else(|| bad(format!("model_providers.{id} is not a table")))?;
    // Refuse before copying anything: a provider that needs headers or tokens won't work without them,
    // and they can't go on a command line.
    for (key, _) in table.iter() {
        if is_secret_key(key) {
            return Err(ServeError::ProviderSecret {
                provider: id.to_string(),
                key: key.to_string(),
            });
        }
    }
    let mut out = InlineTable::new();
    for &key in PROVIDER_KEYS {
        let Some(value) = table.get(key) else {
            continue;
        };
        if key == "query_params" {
            let params = value
                .as_table_like()
                .ok_or_else(|| bad(format!("model_providers.{id}.query_params is not a table")))?;
            let mut copied = InlineTable::new();
            for (name, v) in params.iter() {
                if is_secret_param(name) {
                    return Err(ServeError::ProviderSecret {
                        provider: id.to_string(),
                        key: format!("query_params.{name}"),
                    });
                }
                let s = v.as_str().ok_or_else(|| {
                    bad(format!(
                        "model_providers.{id}.query_params.{name} is not a string"
                    ))
                })?;
                copied.insert(name, Value::from(s));
            }
            out.insert(key, Value::InlineTable(copied));
        } else {
            let s = value
                .as_str()
                .ok_or_else(|| bad(format!("model_providers.{id}.{key} is not a string")))?;
            out.insert(key, Value::from(s));
        }
    }
    out.fmt();
    Ok(out.to_string().trim().to_string())
}

/// Header and token keys, and any other key that holds a credential. `env_key` and
/// `env_key_instructions` only name a variable.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "header",
        "token",
        "secret",
        "password",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|w| k.contains(w))
        || (k.contains("key") && !k.starts_with("env_key"))
}

/// Query parameters that carry a credential (`api-key`, `sig`, ...). Others (`api-version`) are copied.
fn is_secret_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["key", "token", "secret", "password", "sig", "auth"]
        .iter()
        .any(|w| n.contains(w))
}

fn is_bare_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Refuses when a project config from the repo root down to `cwd` sets a key in [`PROJECT_KEYS`]. A
/// project config that doesn't parse is refused too.
pub fn check_project_configs(cwd: &Path) -> Result<(), ServeError> {
    for dir in project_dirs(cwd) {
        let path = dir.join(".codex").join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(ServeError::Config {
                    path,
                    reason: e.to_string(),
                });
            }
        };
        let doc: DocumentMut = text.parse().map_err(|e| ServeError::Config {
            path: path.clone(),
            reason: format!("{e}"),
        })?;
        if let Some(key) = PROJECT_KEYS.iter().find(|k| doc.contains_key(k)) {
            return Err(ServeError::ProjectConfig {
                path,
                key: key.to_string(),
            });
        }
    }
    Ok(())
}

/// The folders whose `.codex/config.toml` Codex may load: the repo root (nearest folder holding `.git`)
/// down to `cwd`, or only `cwd` outside a repo. Checking the folders in between too is stricter than
/// the spec's root-and-cwd and costs nothing.
fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let Some(root) = cwd.ancestors().find(|d| d.join(".git").exists()) else {
        return vec![cwd.to_path_buf()];
    };
    let mut dirs: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|d| *d != root)
        .map(Path::to_path_buf)
        .collect();
    dirs.push(root.to_path_buf());
    dirs.reverse();
    dirs
}

/// How a follow-up passes its prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeStyle {
    /// `resume <thread id> -`, the prompt on stdin.
    Dash,
    /// `resume <thread id> "Answer the question in the stdin block."`, the message on stdin.
    Positional,
}

/// What varies between `codex exec` runs.
#[derive(Clone, Debug)]
pub struct ExecArgs<'a> {
    /// `-C`.
    pub cwd: &'a Path,
    /// The permission profile from [`profile`].
    pub profile: &'a str,
    /// `-m` and the provider.
    pub model: &'a ModelChoice,
    /// `-o`.
    pub run_file: &'a Path,
    /// A follow-up's thread id and prompt style.
    pub resume: Option<(&'a str, ResumeStyle)>,
}

/// The arguments after `codex`. Every flag goes before `resume`; Codex rejects `-C` after it.
pub fn exec_args(a: &ExecArgs<'_>) -> Vec<OsString> {
    let mut v: Vec<OsString> = Vec::new();
    let mut push = |s: &str| v.push(OsString::from(s));
    push("exec");
    push("--json");
    push("--skip-git-repo-check");
    push("-C");
    v.push(a.cwd.as_os_str().to_owned());
    // `--ignore-user-config` drops the person's opt-outs too. Codex 0.163.0 then clones its plugin marketplace
    // from github.com and sends usage metrics to ab.chatgpt.com on every run. A serve run needs neither and
    // should reach nothing but the model.
    for s in [
        "--ignore-user-config",
        "--disable",
        "hooks",
        "--disable",
        "plugins",
        "-c",
        "analytics.enabled=false",
        "--thread-source",
        PROFILE,
        "-c",
    ] {
        v.push(s.into());
    }
    v.push(format!("default_permissions={}", toml_string(PROFILE)).into());
    v.push("-c".into());
    v.push(format!("permissions.{PROFILE}={}", a.profile).into());
    if let Some(model) = &a.model.model {
        v.push("-m".into());
        v.push(model.into());
    }
    if let Some(p) = &a.model.provider {
        v.push("-c".into());
        v.push(format!("model_provider={}", toml_string(&p.id)).into());
        if let Some(table) = &p.table {
            v.push("-c".into());
            v.push(format!("model_providers.{}={table}", p.id).into());
        }
    }
    v.push("-o".into());
    v.push(a.run_file.as_os_str().to_owned());
    match a.resume {
        None => v.push("-".into()),
        Some((thread, style)) => {
            v.push("resume".into());
            v.push(thread.into());
            v.push(
                match style {
                    ResumeStyle::Dash => "-",
                    ResumeStyle::Positional => RESUME_PROMPT,
                }
                .into(),
            );
        }
    }
    v
}

/// `codex sandbox` reading one file under the serve profile, for the self-check.
pub fn sandbox_args(cwd: &Path, profile: &str, file: &Path) -> Vec<OsString> {
    vec![
        "sandbox".into(),
        "-C".into(),
        cwd.as_os_str().to_owned(),
        "-c".into(),
        format!("permissions.{PROFILE}={profile}").into(),
        "-P".into(),
        PROFILE.into(),
        "--".into(),
        "cat".into(),
        file.as_os_str().to_owned(),
    ]
}

/// True when a follow-up died on the command line itself (exit 2, Codex's usage-error status, before any
/// event): the sign that this Codex rejects `-` after `resume`.
pub fn rejected_resume_dash(out: &RunOutput) -> bool {
    out.exit == Exit::Code(2) && !out.stdout_text().contains("thread.started")
}

/// Reads a run: the answer is the run file, else the last `agent_message`; exit status 1 (or any other
/// non-zero status) or a `turn.failed` event fails it.
pub fn parse(out: &RunOutput, run_file: Option<&str>) -> Result<RunResult, RunFailure> {
    let mut thread_id: Option<String> = None;
    let mut last_message: Option<String> = None;
    let mut turn_failed: Option<String> = None;
    let mut last_error: Option<String> = None;
    for line in out.stdout_text().lines() {
        let Ok(event) = serde_json::from_str::<Json>(line.trim()) else {
            continue;
        };
        match event.get("type").and_then(Json::as_str) {
            Some("thread.started") if thread_id.is_none() => {
                thread_id = event
                    .get("thread_id")
                    .and_then(Json::as_str)
                    .filter(|id| is_safe_id(id))
                    .map(str::to_string);
            }
            Some("item.completed") => {
                let item = event.get("item");
                if item.and_then(|i| i.get("type")).and_then(Json::as_str) == Some("agent_message")
                    && let Some(text) = item.and_then(|i| i.get("text")).and_then(Json::as_str)
                {
                    last_message = Some(text.to_string());
                }
            }
            Some("turn.failed") => {
                let msg = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Json::as_str)
                    .unwrap_or("no reason given");
                turn_failed = Some(one_line(msg, 200));
            }
            Some("error") => {
                if let Some(msg) = event.get("message").and_then(Json::as_str) {
                    last_error = Some(one_line(msg, 200));
                }
            }
            _ => {}
        }
    }
    let fail = |reason: String| RunFailure {
        reason: one_line(&reason, 300),
        session_id: thread_id.clone(),
        cost_usd: None,
    };
    if let Some(msg) = turn_failed {
        return Err(fail(format!("the Codex turn failed: {msg}")));
    }
    if !out.success() {
        let base = format!("codex {}", out.exit.describe());
        return Err(fail(match last_error {
            Some(e) => format!("{base}: {e}"),
            None => base,
        }));
    }
    let answer = run_file
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            last_message
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    match answer {
        Some(answer) => Ok(RunResult {
            answer,
            session_id: thread_id,
            cost_usd: None,
        }),
        None => Err(fail("codex gave no answer".into())),
    }
}

/// Inputs for [`canary_check`].
#[derive(Clone, Debug)]
pub struct Canary<'a> {
    /// The `codex` executable.
    pub program: &'a Path,
    /// `--cwd`, canonical.
    pub cwd: &'a Path,
    /// The permission profile.
    pub profile: &'a str,
    /// The child environment.
    pub env: &'a [(OsString, OsString)],
    /// A folder outside `cwd` where the canary file goes (`<home>/run/serve`), canonical.
    pub outside_dir: &'a Path,
    /// Per-check timeout.
    pub timeout: Duration,
}

/// The startup self-check (8.3): through `codex sandbox` with the serve profile, reading a file inside
/// `--cwd` must work, and reading a canary file outside it or shallow and deep `.env` canaries inside it must not.
pub async fn canary_check<R: ProcessRunner>(runner: &R, c: &Canary<'_>) -> Result<(), ServeError> {
    if c.outside_dir.starts_with(c.cwd) {
        return Err(ServeError::Canary(format!(
            "the canary folder {} is inside --cwd, so it can't show that reads outside are blocked",
            c.outside_dir.display()
        )));
    }
    let inside_token = random_hex(16)?;
    let outside_token = random_hex(16)?;
    let env_token = random_hex(16)?;
    let inside = probe_file(c.cwd, ".spew-mesh-canary-", &inside_token)?;
    let outside = probe_file(c.outside_dir, "canary-", &outside_token)?;
    // A `.env` of its own in a fresh folder, so an existing `.env` in the repo is never touched.
    let env_dir = tempfile::Builder::new()
        .prefix(".spew-mesh-canary-")
        .tempdir_in(c.cwd)
        .map_err(|e| {
            ServeError::io(
                format!("can't write a canary file in {}", c.cwd.display()),
                e,
            )
        })?;
    let env_file = env_dir.path().join(".env");
    std::fs::write(&env_file, &env_token).map_err(|e| {
        ServeError::io(
            format!("can't write a canary file in {}", c.cwd.display()),
            e,
        )
    })?;
    // A shallow probe misses a depth-limited expansion of the secret-file globs.
    let deep_dir = env_dir.path().join("nested/one/two");
    std::fs::create_dir_all(&deep_dir)
        .and_then(|()| std::fs::write(deep_dir.join(".env"), &env_token))
        .map_err(|e| ServeError::io("can't write a nested canary file", e))?;
    let read = |file: &Path| RunSpec {
        program: c.program.to_path_buf(),
        args: sandbox_args(c.cwd, c.profile, file),
        cwd: c.cwd.to_path_buf(),
        env: c.env.to_vec(),
        stdin: Vec::new(),
        timeout: c.timeout,
        stdout_cap: 64 * 1024,
    };
    let run = |file: &Path| {
        let spec = read(file);
        async move {
            runner
                .run(spec)
                .await
                .map_err(|e| ServeError::io("can't run codex sandbox", e))
        }
    };
    let leaked = |out: &RunOutput, token: &str| {
        out.stdout_text().contains(token) || out.stderr_text().contains(token)
    };

    let out = run(inside.path()).await?;
    if !(out.success() && out.stdout_text().contains(&inside_token)) {
        let detail = one_line(&out.stderr_text(), 200);
        return Err(ServeError::Canary(format!(
            "reading a file inside --cwd through the serve profile failed (codex {}){}",
            out.exit.describe(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )));
    }

    let out = run(outside.path()).await?;
    if leaked(&out, &outside_token) {
        return Err(ServeError::Canary(
            "a sandboxed command read a file outside --cwd, so Codex doesn't enforce the serve \
             profile here"
                .into(),
        ));
    }

    let out = run(&env_file).await?;
    if leaked(&out, &env_token) {
        return Err(ServeError::Canary(
            "a sandboxed command read a .env file inside --cwd, so Codex doesn't enforce the serve \
             profile's deny rules here"
                .into(),
        ));
    }
    let out = run(&deep_dir.join(".env")).await?;
    if leaked(&out, &env_token) {
        return Err(ServeError::Canary(
            "a sandboxed command read a deeply nested .env file inside --cwd, so Codex doesn't enforce the \
             serve profile's deny rules at every depth here"
                .into(),
        ));
    }
    Ok(())
}

fn probe_file(
    dir: &Path,
    prefix: &str,
    token: &str,
) -> Result<tempfile::NamedTempFile, ServeError> {
    use std::io::Write;
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".txt")
        .tempfile_in(dir)
        .map_err(|e| {
            ServeError::io(format!("can't write a canary file in {}", dir.display()), e)
        })?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| {
            ServeError::io(format!("can't write a canary file in {}", dir.display()), e)
        })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const DIR: &str = "/opt/codex/lib/node_modules/@openai/codex";

    fn output(code: i32, stdout: &str) -> RunOutput {
        RunOutput {
            exit: Exit::Code(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
            elapsed: Duration::from_secs(1),
        }
    }

    #[test]
    fn profile_is_the_spec_text() {
        assert_eq!(
            profile(Path::new(DIR), false).unwrap(),
            "{extends=\":read-only\", filesystem={\":root\"=\"deny\", \":minimal\"=\"read\", \
             \"/opt/codex/lib/node_modules/@openai/codex\"=\"read\", \
             \":workspace_roots\"={\".\"=\"read\", \"**/.env\"=\"deny\", \"**/*.env\"=\"deny\", \
             \"**/.env.*\"=\"deny\", \"**/*.pem\"=\"deny\", \"**/*.key\"=\"deny\", \"**/id_rsa*\"=\"deny\", \
             \"**/id_ed25519*\"=\"deny\", \".git/config\"=\"deny\", \".git/hooks\"=\"deny\"}}, \
             network={enabled=false}}"
        );
    }

    #[test]
    fn allow_edits_keeps_every_deny_entry() {
        let read = profile(Path::new(DIR), false).unwrap();
        let edits = profile(Path::new(DIR), true).unwrap();
        assert_eq!(edits, read.replace("\".\"=\"read\"", "\".\"=\"write\""));
        let value: Value = edits.parse().unwrap();
        let fs = value.as_inline_table().unwrap().get("filesystem").unwrap();
        let roots = fs
            .as_inline_table()
            .unwrap()
            .get(":workspace_roots")
            .unwrap()
            .as_inline_table()
            .unwrap();
        assert_eq!(roots.get(".").unwrap().as_str(), Some("write"));
        for entry in WORKSPACE_DENY {
            assert_eq!(roots.get(entry).unwrap().as_str(), Some("deny"), "{entry}");
        }
        assert_eq!(roots.len(), 1 + WORKSPACE_DENY.len());
    }

    #[test]
    fn profile_parses_as_a_toml_inline_table() {
        let odd = Path::new("/opt/we\"ird\\path/codex");
        for p in [Path::new(DIR), odd] {
            let text = profile(p, false).unwrap();
            let value: Value = text.parse().unwrap();
            let table = value.as_inline_table().unwrap();
            assert_eq!(table.get("extends").unwrap().as_str(), Some(":read-only"));
            let fs = table.get("filesystem").unwrap().as_inline_table().unwrap();
            assert_eq!(fs.get(p.to_str().unwrap()).unwrap().as_str(), Some("read"));
            assert_eq!(fs.get(":root").unwrap().as_str(), Some("deny"));
            let net = table.get("network").unwrap().as_inline_table().unwrap();
            assert_eq!(net.get("enabled").unwrap().as_bool(), Some(false));
        }
    }

    #[test]
    fn codex_dir_rules() {
        let home = Path::new("/home/sam/.codex");
        assert_eq!(
            codex_dir(
                Path::new("/opt/codex/lib/node_modules/@openai/codex/bin/codex.js"),
                home
            ),
            PathBuf::from(DIR)
        );
        assert_eq!(
            codex_dir(
                Path::new(
                    "/opt/n/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/codex"
                ),
                home
            ),
            PathBuf::from("/opt/n/node_modules/@openai/codex")
        );
        assert_eq!(
            codex_dir(
                Path::new("/home/sam/.codex/packages/standalone/current/bin/codex"),
                home
            ),
            PathBuf::from("/home/sam/.codex/packages/standalone/current")
        );
        assert_eq!(
            codex_dir(Path::new("/usr/local/bin/codex"), home),
            PathBuf::from("/usr/local/bin")
        );
    }

    // A local npm install (`npm install --prefix <dir> @openai/codex`) hoists the platform package next to
    // the wrapper; the sandbox must be allowed to read that package, where the native binary is.
    #[test]
    fn codex_dir_finds_a_hoisted_platform_package() {
        let Some(name) = platform_package() else {
            eprintln!(
                "codex_dir_finds_a_hoisted_platform_package: skipped, no Codex package for this platform"
            );
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let modules = tmp.path().join("tools").join("node_modules");
        let wrapper = modules.join("@openai").join("codex");
        std::fs::create_dir_all(wrapper.join("bin")).unwrap();
        let script = wrapper.join("bin").join("codex.js");
        std::fs::write(&script, "").unwrap();
        let home = tmp.path().join("codex-home");

        // No platform package anywhere: the wrapper package, as the spec says.
        assert_eq!(codex_dir(&script, &home), wrapper);

        let hoisted = modules.join("@openai").join(name);
        std::fs::create_dir_all(hoisted.join("vendor")).unwrap();
        assert_eq!(
            codex_dir(&script, &home),
            std::fs::canonicalize(&hoisted).unwrap()
        );

        // A nested one (a global install) wins, and the wrapper package covers it.
        std::fs::create_dir_all(wrapper.join("node_modules").join("@openai").join(name)).unwrap();
        assert_eq!(codex_dir(&script, &home), wrapper);
    }

    #[test]
    fn codex_dir_finds_a_platform_package_further_up() {
        let Some(name) = platform_package() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("node_modules");
        let wrapper = outer
            .join("tool")
            .join("node_modules")
            .join("@openai")
            .join("codex");
        std::fs::create_dir_all(&wrapper).unwrap();
        let platform = outer.join("@openai").join(name);
        std::fs::create_dir_all(&platform).unwrap();
        assert_eq!(
            codex_dir(&wrapper.join("bin").join("codex.js"), tmp.path()),
            std::fs::canonicalize(&platform).unwrap()
        );
    }

    #[test]
    fn codex_dir_follows_a_symlinked_current() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        let standalone = home.join("packages/standalone");
        let versioned = standalone.join("0.163.0");
        std::fs::create_dir_all(versioned.join("bin")).unwrap();
        std::os::unix::fs::symlink(&versioned, standalone.join("current")).unwrap();
        let real = std::fs::canonicalize(versioned.join("bin"))
            .unwrap()
            .join("codex");
        assert_eq!(
            codex_dir(&real, &home),
            std::fs::canonicalize(&versioned).unwrap()
        );
    }

    #[test]
    fn codex_home_prefers_the_variable() {
        let host = HostEnv::new([("CODEX_HOME", "/srv/codex"), ("HOME", "/home/sam")]);
        assert_eq!(codex_home(&host).unwrap(), PathBuf::from("/srv/codex"));
        let host = HostEnv::new([("HOME", "/home/sam")]);
        assert_eq!(
            codex_home(&host).unwrap(),
            PathBuf::from("/home/sam/.codex")
        );
        assert!(codex_home(&HostEnv::default()).is_err());
    }

    fn choice(text: &str, flag: Option<&str>) -> Result<ModelChoice, ServeError> {
        model_choice(text, Path::new("config.toml"), flag)
    }

    #[test]
    fn model_comes_from_config_and_the_flag_wins() {
        let text = "model = \"gpt-6\"\n";
        assert_eq!(
            choice(text, None).unwrap(),
            ModelChoice {
                model: Some("gpt-6".into()),
                provider: None
            }
        );
        assert_eq!(
            choice(text, Some("gpt-6-mini")).unwrap().model.as_deref(),
            Some("gpt-6-mini")
        );
        assert_eq!(choice("", None).unwrap(), ModelChoice::default());
        assert!(choice("model = 3\n", None).is_err());
        assert!(choice("model = \"--yolo\"\n", None).is_err());
        assert!(choice("not toml [", None).is_err());
    }

    #[test]
    fn provider_table_copies_only_the_listed_keys() {
        let text = r#"
model = "m1"
model_provider = "acme"
sandbox_mode = "danger-full-access"

[model_providers.acme]
name = "Acme"
base_url = "https://llm.example.com/v1"
env_key = "ACME_API_KEY"
env_key_instructions = "Set ACME_API_KEY"
wire_api = "responses"
request_max_retries = 4
query_params = { api-version = "2026-01-01" }
"#;
        let c = choice(text, None).unwrap();
        let p = c.provider.unwrap();
        assert_eq!(p.id, "acme");
        let table = p.table.unwrap();
        assert_eq!(
            table,
            "{ name = \"Acme\", base_url = \"https://llm.example.com/v1\", env_key = \"ACME_API_KEY\", \
             wire_api = \"responses\", query_params = { api-version = \"2026-01-01\" } }"
        );
        let parsed: Value = table.parse().unwrap();
        assert_eq!(parsed.as_inline_table().unwrap().len(), 5);
    }

    #[test]
    fn built_in_provider_has_no_table() {
        let c = choice("model_provider = \"oss\"\n", None).unwrap();
        assert_eq!(
            c.provider,
            Some(Provider {
                id: "oss".into(),
                table: None
            })
        );
    }

    #[test]
    fn header_and_token_keys_are_refused() {
        for key in [
            "http_headers = { X-Api = \"k\" }",
            "env_http_headers = { X-Api = \"ACME\" }",
            "bearer_token_env_var = \"ACME\"",
            "experimental_bearer_token = \"t\"",
            "api_key = \"k\"",
            "query_params = { api-key = \"k\" }",
        ] {
            let text = format!(
                "model_provider = \"acme\"\n[model_providers.acme]\nname = \"Acme\"\n{key}\n"
            );
            match choice(&text, None) {
                Err(ServeError::ProviderSecret { provider, .. }) => assert_eq!(provider, "acme"),
                other => panic!("{key}: {other:?}"),
            }
        }
    }

    #[test]
    fn odd_provider_ids_and_values_are_refused() {
        assert!(choice("model_provider = \"a.b\"\n", None).is_err());
        assert!(
            choice(
                "model_provider = \"acme\"\n[model_providers.acme]\nbase_url = 1\n",
                None
            )
            .is_err()
        );
    }

    #[test]
    fn project_config_keys_are_refused() {
        for key in PROJECT_KEYS {
            let repo = tempfile::tempdir().unwrap();
            std::fs::create_dir(repo.path().join(".git")).unwrap();
            let cwd = repo.path().join("sub");
            std::fs::create_dir_all(&cwd).unwrap();
            assert!(check_project_configs(&cwd).is_ok());
            std::fs::create_dir(repo.path().join(".codex")).unwrap();
            let value = if *key == "sandbox_mode" {
                format!("{key} = \"workspace-write\"\n")
            } else {
                format!("[{key}]\nx = 1\n")
            };
            std::fs::write(repo.path().join(".codex/config.toml"), value).unwrap();
            match check_project_configs(&cwd) {
                Err(ServeError::ProjectConfig { key: k, .. }) => assert_eq!(k, *key),
                other => panic!("{key}: {other:?}"),
            }
        }
    }

    #[test]
    fn project_config_in_cwd_and_harmless_keys() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::create_dir_all(repo.path().join(".codex")).unwrap();
        std::fs::write(repo.path().join(".codex/config.toml"), "model = \"m\"\n").unwrap();
        let cwd = repo.path().join("pkg");
        std::fs::create_dir_all(cwd.join(".codex")).unwrap();
        assert!(check_project_configs(&cwd).is_ok());
        std::fs::write(cwd.join(".codex/config.toml"), "notify = [\"x\"]\n").unwrap();
        assert!(matches!(
            check_project_configs(&cwd),
            Err(ServeError::ProjectConfig { .. })
        ));
        std::fs::write(cwd.join(".codex/config.toml"), "broken = [\n").unwrap();
        assert!(matches!(
            check_project_configs(&cwd),
            Err(ServeError::Config { .. })
        ));
    }

    #[test]
    fn outside_a_repo_only_cwd_is_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(project_dirs(&cwd), vec![cwd.clone()]);
    }

    fn strings(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn exec_argv_is_exact() {
        let model = ModelChoice::default();
        let prof = profile(Path::new(DIR), false).unwrap();
        let args = exec_args(&ExecArgs {
            cwd: Path::new("/work/api"),
            profile: &prof,
            model: &model,
            run_file: Path::new("/home/sam/.spew/mesh/run/serve/01J.txt"),
            resume: None,
        });
        let perm = format!("permissions.spew-mesh-serve={prof}");
        assert_eq!(
            strings(&args),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "/work/api",
                "--ignore-user-config",
                "--disable",
                "hooks",
                "--disable",
                "plugins",
                "-c",
                "analytics.enabled=false",
                "--thread-source",
                "spew-mesh-serve",
                "-c",
                "default_permissions=\"spew-mesh-serve\"",
                "-c",
                perm.as_str(),
                "-o",
                "/home/sam/.spew/mesh/run/serve/01J.txt",
                "-",
            ]
        );
    }

    #[test]
    fn exec_argv_with_model_provider_and_resume() {
        let model = ModelChoice {
            model: Some("gpt-6".into()),
            provider: Some(Provider {
                id: "acme".into(),
                table: Some("{ name = \"Acme\" }".into()),
            }),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        let base = ExecArgs {
            cwd: Path::new("/work/api"),
            profile: &prof,
            model: &model,
            run_file: Path::new("/r/01J.txt"),
            resume: Some(("0199a3e1-0000-7000-8000-000000000001", ResumeStyle::Dash)),
        };
        let args = strings(&exec_args(&base));
        assert_eq!(
            args[18..],
            [
                "-m",
                "gpt-6",
                "-c",
                "model_provider=\"acme\"",
                "-c",
                "model_providers.acme={ name = \"Acme\" }",
                "-o",
                "/r/01J.txt",
                "resume",
                "0199a3e1-0000-7000-8000-000000000001",
                "-",
            ]
        );
        let positional = strings(&exec_args(&ExecArgs {
            resume: Some((
                "0199a3e1-0000-7000-8000-000000000001",
                ResumeStyle::Positional,
            )),
            ..base
        }));
        assert_eq!(
            positional.last().map(String::as_str),
            Some("Answer the question in the stdin block.")
        );
        assert_eq!(positional[positional.len() - 3], "resume");
    }

    #[test]
    fn sandbox_argv_is_exact() {
        let prof = profile(Path::new(DIR), false).unwrap();
        let perm = format!("permissions.spew-mesh-serve={prof}");
        assert_eq!(
            strings(&sandbox_args(
                Path::new("/work/api"),
                &prof,
                Path::new("/work/api/f.txt")
            )),
            [
                "sandbox",
                "-C",
                "/work/api",
                "-c",
                perm.as_str(),
                "-P",
                "spew-mesh-serve",
                "--",
                "cat",
                "/work/api/f.txt",
            ]
        );
    }

    const EVENTS: &str = concat!(
        r#"{"type":"thread.started","thread_id":"0199a3e1-0000-7000-8000-000000000001"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Looking."}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"rg x","aggregated_output":"","exit_code":0,"status":"completed"}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"It is in api/x.rs."}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}"#,
        "\n",
    );

    #[test]
    fn answer_is_the_run_file_else_the_last_agent_message() {
        let out = output(0, EVENTS);
        let r = parse(&out, Some("From the file.\n")).unwrap();
        assert_eq!(r.answer, "From the file.");
        assert_eq!(
            r.session_id.as_deref(),
            Some("0199a3e1-0000-7000-8000-000000000001")
        );
        assert_eq!(parse(&out, None).unwrap().answer, "It is in api/x.rs.");
        assert_eq!(
            parse(&out, Some("  ")).unwrap().answer,
            "It is in api/x.rs."
        );
    }

    #[test]
    fn failed_codex_runs() {
        let failed = format!(
            "{EVENTS}{}\n",
            r#"{"type":"turn.failed","error":{"message":"stream disconnected"}}"#
        );
        assert_eq!(
            parse(&output(1, &failed), None).unwrap_err().reason,
            "the Codex turn failed: stream disconnected"
        );
        let errored = r#"{"type":"error","message":"model not found"}"#;
        assert_eq!(
            parse(&output(1, errored), None).unwrap_err().reason,
            "codex exited with status 1: model not found"
        );
        assert_eq!(
            parse(&output(0, ""), None).unwrap_err().reason,
            "codex gave no answer"
        );
        let mut timed_out = output(0, EVENTS);
        timed_out.exit = Exit::TimedOut;
        assert!(parse(&timed_out, None).is_err());
    }

    #[test]
    fn resume_dash_rejection_is_exit_two_without_events() {
        assert!(rejected_resume_dash(&output(2, "")));
        assert!(!rejected_resume_dash(&output(2, EVENTS)));
        assert!(!rejected_resume_dash(&output(1, "")));
    }

    /// Plays `codex sandbox`: answers each read from a closure and records the specs.
    struct FakeSandbox<F: Fn(&Path) -> RunOutput + Send + Sync> {
        answer: F,
        calls: Mutex<Vec<RunSpec>>,
    }

    impl<F: Fn(&Path) -> RunOutput + Send + Sync> ProcessRunner for FakeSandbox<F> {
        async fn run(&self, spec: RunSpec) -> std::io::Result<RunOutput> {
            let file = PathBuf::from(spec.args.last().unwrap());
            self.calls.lock().unwrap().push(spec);
            Ok((self.answer)(&file))
        }
    }

    fn cat(file: &Path) -> RunOutput {
        output(0, &std::fs::read_to_string(file).unwrap())
    }

    fn is_env(f: &Path) -> bool {
        let name = f.file_name().unwrap().to_string_lossy();
        name == ".env" || name.ends_with(".env") || name.starts_with(".env.")
    }

    /// What an enforcing sandbox does: the repo is readable except its `.env` files, nothing else is.
    fn enforced(cwd: PathBuf) -> impl Fn(&Path) -> RunOutput + Send + Sync {
        move |f: &Path| {
            if f.starts_with(&cwd) && !is_env(f) {
                cat(f)
            } else {
                denied(f)
            }
        }
    }

    fn denied(_: &Path) -> RunOutput {
        RunOutput {
            exit: Exit::Code(1),
            stdout: vec![],
            stderr: b"cat: No such file or directory\n".to_vec(),
            elapsed: Duration::from_millis(5),
        }
    }

    struct CanaryDirs {
        _tmp: tempfile::TempDir,
        cwd: PathBuf,
        outside: PathBuf,
    }

    fn canary_dirs() -> CanaryDirs {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let cwd = root.join("repo");
        let outside = root.join("home/run/serve");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        CanaryDirs {
            _tmp: tmp,
            cwd,
            outside,
        }
    }

    fn canary<'a>(d: &'a CanaryDirs, profile: &'a str) -> Canary<'a> {
        Canary {
            program: Path::new("/usr/local/bin/codex"),
            cwd: &d.cwd,
            profile,
            env: &[],
            outside_dir: &d.outside,
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn canary_passes_when_only_the_repo_is_readable() {
        let d = canary_dirs();
        let runner = FakeSandbox {
            answer: enforced(d.cwd.clone()),
            calls: Mutex::new(vec![]),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        canary_check(&runner, &canary(&d, &prof)).await.unwrap();
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].args[0], "sandbox");
        assert!(PathBuf::from(calls[0].args.last().unwrap()).starts_with(&d.cwd));
        assert!(PathBuf::from(calls[1].args.last().unwrap()).starts_with(&d.outside));
        let env = PathBuf::from(calls[2].args.last().unwrap());
        assert!(env.starts_with(&d.cwd));
        assert_eq!(env.file_name().unwrap(), ".env");
        let deep_env = PathBuf::from(calls[3].args.last().unwrap());
        assert!(deep_env.starts_with(env.parent().unwrap()));
        assert!(deep_env.strip_prefix(&d.cwd).unwrap().components().count() > 4);
        drop(calls);
        // Both probe files are gone afterwards.
        assert_eq!(std::fs::read_dir(&d.cwd).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&d.outside).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn canary_fails_when_outside_reads_work() {
        let d = canary_dirs();
        let runner = FakeSandbox {
            answer: cat,
            calls: Mutex::new(vec![]),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        let err = canary_check(&runner, &canary(&d, &prof)).await.unwrap_err();
        assert!(matches!(err, ServeError::Canary(_)), "{err}");
        assert!(err.to_string().contains("outside --cwd"));
    }

    #[tokio::test]
    async fn canary_fails_when_a_dot_env_inside_cwd_is_readable() {
        let d = canary_dirs();
        let cwd = d.cwd.clone();
        let runner = FakeSandbox {
            answer: move |f: &Path| {
                if f.starts_with(&cwd) {
                    cat(f)
                } else {
                    denied(f)
                }
            },
            calls: Mutex::new(vec![]),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        let err = canary_check(&runner, &canary(&d, &prof)).await.unwrap_err();
        assert!(err.to_string().contains(".env file inside --cwd"), "{err}");
        assert_eq!(runner.calls.lock().unwrap().len(), 3);
        assert_eq!(std::fs::read_dir(&d.cwd).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn canary_fails_when_only_deep_secret_reads_work() {
        let d = canary_dirs();
        let cwd = d.cwd.clone();
        let runner = FakeSandbox {
            answer: move |f: &Path| {
                if !f.starts_with(&cwd)
                    || (f.file_name().is_some_and(|name| name == ".env")
                        && f.strip_prefix(&cwd).unwrap().components().count() <= 3)
                {
                    denied(f)
                } else {
                    cat(f)
                }
            },
            calls: Mutex::new(vec![]),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        let err = canary_check(&runner, &canary(&d, &prof)).await.unwrap_err();
        assert!(err.to_string().contains("deeply nested .env"), "{err}");
        assert_eq!(runner.calls.lock().unwrap().len(), 4);
        assert_eq!(std::fs::read_dir(&d.cwd).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&d.outside).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn canary_fails_when_inside_reads_fail() {
        let d = canary_dirs();
        let runner = FakeSandbox {
            answer: denied,
            calls: Mutex::new(vec![]),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        let err = canary_check(&runner, &canary(&d, &prof)).await.unwrap_err();
        assert!(err.to_string().contains("inside --cwd"), "{err}");
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn canary_refuses_a_canary_folder_inside_cwd() {
        let d = canary_dirs();
        let inner = d.cwd.join("run");
        std::fs::create_dir_all(&inner).unwrap();
        let runner = FakeSandbox {
            answer: cat,
            calls: Mutex::new(vec![]),
        };
        let prof = profile(Path::new(DIR), false).unwrap();
        let c = Canary {
            outside_dir: &inner,
            ..canary(&d, &prof)
        };
        assert!(canary_check(&runner, &c).await.is_err());
        assert!(runner.calls.lock().unwrap().is_empty());
    }
}
