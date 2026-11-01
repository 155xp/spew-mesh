//! What this computer can run: each agent CLI's version, models, effort levels, plan and usage.
//!
//! Everything comes from the CLIs themselves or their own local files, run with a timeout; a CLI that is missing
//! or doesn't answer is just absent. Credential files are read for their plan fields alone, and no token is ever
//! kept, logged or sent.
//!
//! - Claude Code: `claude --version`, the `--effort` choices in `claude --help`, the model aliases (`fable`,
//!   `opus`, `sonnet`, `haiku`) named after the models they point at in Claude Code's model catalog cache, the plan
//!   from `.credentials.json` (`subscriptionType`, `rateLimitTier`), and the 5-hour usage Claude Code cached in
//!   `~/.claude.json` when it is recent.
//! - Codex: `codex --version`, `codex debug models` (listed models with their reasoning levels), the plan from the
//!   `id_token` claims in `auth.json`, and the usage from the newest session log.
//! - OpenCode: `opencode --version` and `opencode models`. It reports no plan, usage or effort levels.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::Value;

use super::command::{base_env, is_effort, is_model_id};
use super::{AgentCapability, AgentKind, ModelOption, Usage};
use crate::responder::env::HostEnv;
use crate::responder::process::{ProcessRunner, RunSpec};
use crate::spew_api::Clock;

/// How long one detection command may take.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The effort levels `claude --help` listed when this was written, used when the help can't be read.
pub const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// The aliases Claude Code takes for the latest model of each family.
pub const CLAUDE_ALIASES: &[&str] = &["fable", "opus", "sonnet", "haiku"];
/// Cached Claude usage older than this isn't reported.
pub const CLAUDE_USAGE_MAX_AGE_MS: u64 = 60 * 60 * 1000;

const MAX_OUTPUT: usize = 16 << 20;
const MAX_FILE: u64 = 16 << 20;
// How much of the end of a Codex session log is searched for its rate limits.
const ROLLOUT_TAIL: u64 = 2 << 20;
// How many of the newest Codex session logs are searched.
const ROLLOUT_FILES: usize = 5;
const MAX_LABEL: usize = 80;

/// Finds what this computer can run. [`SystemDetect`] is the real one; tests substitute their own.
pub trait Detect: Send + Sync + 'static {
    /// The agent CLIs found now.
    fn detect(&self) -> impl Future<Output = Vec<AgentCapability>> + Send;
}

/// Detection with real processes and files.
pub struct SystemDetect<R> {
    env: HostEnv,
    runner: R,
    clock: Arc<dyn Clock>,
}

impl<R: ProcessRunner> SystemDetect<R> {
    /// Detection from `process` (the daemon's environment, trimmed with [`base_env`]).
    pub fn new(process: &HostEnv, runner: R, clock: Arc<dyn Clock>) -> Self {
        SystemDetect {
            env: base_env(process),
            runner,
            clock,
        }
    }

    async fn output(&self, program: &Path, args: &[&str]) -> Option<String> {
        let cwd = self.env.home().unwrap_or_else(|| PathBuf::from("/"));
        let spec = RunSpec {
            program: program.to_path_buf(),
            args: args.iter().map(Into::into).collect(),
            cwd,
            env: self.env.child_env(AgentKind::Claude.serve_client(), &[]),
            stdin: Vec::new(),
            timeout: DETECT_TIMEOUT,
            stdout_cap: MAX_OUTPUT,
            keep_tail: false,
        };
        match self.runner.run(spec).await {
            Ok(out) if out.success() => Some(out.stdout_text()),
            Ok(out) => {
                tracing::debug!(program = %program.display(), ?args, exit = %out.exit.describe(), "detection command failed");
                None
            }
            Err(e) => {
                tracing::debug!(program = %program.display(), error = %e, "detection command didn't start");
                None
            }
        }
    }

    async fn version(&self, program: &Path) -> Option<String> {
        parse_version(&self.output(program, &["--version"]).await?)
    }

    async fn claude(&self) -> Option<AgentCapability> {
        let program = self.env.find_program("claude")?;
        let version = self.version(&program).await?;
        let help = self.output(&program, &["--help"]).await;
        let efforts = help
            .as_deref()
            .and_then(parse_claude_efforts)
            .unwrap_or_else(|| CLAUDE_EFFORTS.iter().map(|s| s.to_string()).collect());
        let config = claude_config_dir(&self.env);
        let catalog = config
            .as_deref()
            .and_then(|dir| newest_file(&dir.join("cache").join("model-catalog"), ".json"))
            .and_then(|path| read_small(&path))
            .map(|text| parse_claude_catalog(&text))
            .unwrap_or_default();
        let plan = config
            .as_deref()
            .and_then(|dir| read_small(&dir.join(".credentials.json")))
            .and_then(|text| parse_claude_plan(&text));
        let limits = claude_state_file(&self.env)
            .and_then(|path| read_small(&path))
            .map(|text| parse_claude_limits(&text, self.clock.now_ms()))
            .unwrap_or_default();
        Some(AgentCapability {
            kind: AgentKind::Claude,
            version,
            plan,
            models: claude_models(&efforts, &catalog),
            usage: limits.iter().find(|u| u.window_minutes == 300).cloned(),
            limits,
        })
    }

    async fn codex(&self) -> Option<AgentCapability> {
        let program = self.env.find_program("codex")?;
        let version = self.version(&program).await?;
        let models = match self.output(&program, &["debug", "models"]).await {
            Some(text) => parse_codex_models(&text).unwrap_or_else(|e| {
                tracing::info!(error = %e, "can't read `codex debug models`");
                Vec::new()
            }),
            None => Vec::new(),
        };
        let home = codex_home(&self.env);
        let plan = home
            .as_deref()
            .and_then(|dir| read_small(&dir.join("auth.json")))
            .and_then(|text| parse_codex_plan(&text));
        let now = self.clock.now_ms();
        let limits = home
            .as_deref()
            .map(|dir| codex_limits(&dir.join("sessions"), now))
            .unwrap_or_default();
        Some(AgentCapability {
            kind: AgentKind::Codex,
            version,
            plan,
            models,
            usage: limits.first().cloned(),
            limits,
        })
    }

    async fn opencode(&self) -> Option<AgentCapability> {
        let program = self.env.find_program("opencode")?;
        let version = self.version(&program).await?;
        let models = self
            .output(&program, &["models"])
            .await
            .map(|text| parse_opencode_models(&text))
            .unwrap_or_default();
        Some(AgentCapability {
            kind: AgentKind::Opencode,
            version,
            plan: None,
            models,
            usage: None,
            limits: Vec::new(),
        })
    }
}

impl<R: ProcessRunner + 'static> Detect for SystemDetect<R> {
    async fn detect(&self) -> Vec<AgentCapability> {
        let (claude, codex, opencode) = tokio::join!(self.claude(), self.codex(), self.opencode());
        [claude, codex, opencode].into_iter().flatten().collect()
    }
}

fn read_small(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_FILE {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_FILE).read_to_string(&mut text).ok()?;
    Some(text)
}

// The newest file in `dir` whose name ends with `suffix`.
fn newest_file(dir: &Path, suffix: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(suffix)))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max()
        .map(|(_, path)| path)
}

/// Claude Code's config folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_config_dir(env: &HostEnv) -> Option<PathBuf> {
    env.path_var("CLAUDE_CONFIG_DIR")
        .or_else(|| env.home().map(|h| h.join(".claude")))
}

/// Claude Code's state file: `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
pub fn claude_state_file(env: &HostEnv) -> Option<PathBuf> {
    match env.path_var("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(dir.join(".claude.json")),
        None => env.home().map(|h| h.join(".claude.json")),
    }
}

/// Codex's home: `CODEX_HOME`, else `~/.codex`.
pub fn codex_home(env: &HostEnv) -> Option<PathBuf> {
    env.path_var("CODEX_HOME")
        .or_else(|| env.home().map(|h| h.join(".codex")))
}

// Server- and CLI-provided text kept to one short printable line.
fn label(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL)
        .collect::<String>()
        .trim()
        .to_string()
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The version in a `--version` line: `2.1.312 (Claude Code)`, `codex-cli 0.165.0`, `v2.2.5`.
pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let found = line
        .split_whitespace()
        .map(|t| t.trim_start_matches('v'))
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()) && t.contains('.'));
    Some(label(found.unwrap_or(line))).filter(|v| !v.is_empty())
}

/// The choices of `--effort` in `claude --help`: the parenthesized list on its line or the lines that continue it.
pub fn parse_claude_efforts(help: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = help.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim_start().starts_with("--effort"))?;
    let mut text = lines[at].to_string();
    for line in &lines[at + 1..] {
        let t = line.trim();
        if t.is_empty() || t.starts_with('-') {
            break;
        }
        text.push(' ');
        text.push_str(t);
    }
    let open = text.find('(')?;
    let close = open + text[open..].find(')')?;
    let inner = text[open + 1..close].trim();
    let inner = inner.strip_prefix("choices:").unwrap_or(inner);
    let list: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| is_effort(s))
        .collect();
    (!list.is_empty()).then_some(list)
}

/// A model in Claude Code's catalog cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogModel {
    /// `Opus 5.5`.
    pub name: String,
    /// Its effort levels, when the catalog lists them.
    pub efforts: Option<Vec<String>>,
    /// Its default effort.
    pub default_effort: Option<String>,
}

/// Claude Code's model catalog cache (`~/.claude/cache/model-catalog/*.json`): each alias and the model it points
/// at. Empty when the file doesn't have the expected shape.
pub fn parse_claude_catalog(text: &str) -> BTreeMap<String, CatalogModel> {
    let mut out = BTreeMap::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    let config = &v["catalog"]["config"];
    let models: Vec<&Value> = config["models"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let Some(targets) = config["provider_alias_targets"].as_object() else {
        return out;
    };
    for (alias, target) in targets {
        let Some(id) = target["default"].as_str() else {
            continue;
        };
        let Some(model) = models.iter().find(|m| m["id"].as_str() == Some(id)) else {
            continue;
        };
        let name = label(model["name"].as_str().unwrap_or(""));
        if name.is_empty() || !is_model_id(alias) {
            continue;
        }
        let runtime = &model["runtime"];
        let efforts = runtime["effort_levels"].as_array().map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|e| is_effort(e))
                .map(str::to_string)
                .collect()
        });
        let default_effort = runtime["default_effort"]
            .as_str()
            .filter(|e| is_effort(e))
            .map(str::to_string);
        out.insert(
            alias.clone(),
            CatalogModel {
                name,
                efforts,
                default_effort,
            },
        );
    }
    out
}

/// Claude Code's models: the four aliases (and any other alias the catalog knows), named after the model each one
/// points at when the catalog says, else `Opus (latest)`. Efforts are the `--effort` choices, narrowed to what the
/// catalog lists for the model.
pub fn claude_models(
    efforts: &[String],
    catalog: &BTreeMap<String, CatalogModel>,
) -> Vec<ModelOption> {
    let mut aliases: Vec<String> = CLAUDE_ALIASES.iter().map(|s| s.to_string()).collect();
    let extra: Vec<String> = catalog
        .keys()
        .filter(|k| !CLAUDE_ALIASES.contains(&k.as_str()))
        .cloned()
        .collect();
    aliases.extend(extra);
    aliases
        .into_iter()
        .map(|alias| match catalog.get(&alias) {
            Some(m) => {
                let list: Vec<String> = match &m.efforts {
                    Some(own) => efforts
                        .iter()
                        .filter(|e| own.contains(e))
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                };
                let default_effort = m.default_effort.clone().filter(|d| list.contains(d));
                ModelOption {
                    id: alias,
                    label: m.name.clone(),
                    efforts: list,
                    default_effort,
                }
            }
            None => ModelOption {
                label: format!("{} (latest)", capitalize(&alias)),
                id: alias,
                efforts: efforts.to_vec(),
                default_effort: None,
            },
        })
        .collect()
}

#[derive(Deserialize)]
struct ClaudeCredentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<ClaudePlanFields>,
}

// Only the plan fields; serde skips the tokens next to them without keeping them.
#[derive(Deserialize)]
struct ClaudePlanFields {
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

/// The plan in Claude Code's `.credentials.json`: `Max 20x`, `Max 5x`, `Pro`. None without a claude.ai sign-in
/// (an API key, or a file with no plan).
pub fn parse_claude_plan(text: &str) -> Option<String> {
    let creds: ClaudeCredentials = serde_json::from_str(text).ok()?;
    let fields = creds.oauth?;
    claude_plan_label(
        fields.subscription_type.as_deref()?,
        fields.rate_limit_tier.as_deref().unwrap_or(""),
    )
}

/// `max` with a `..._max_20x` tier is `Max 20x`, and so on.
pub fn claude_plan_label(subscription: &str, tier: &str) -> Option<String> {
    let sub = subscription.trim().to_ascii_lowercase();
    let tier = tier.to_ascii_lowercase();
    let text = match sub.as_str() {
        "" => return None,
        "max" if tier.contains("20x") => "Max 20x".to_string(),
        "max" if tier.contains("5x") => "Max 5x".to_string(),
        other => capitalize(other),
    };
    Some(label(&text)).filter(|t| !t.is_empty())
}

/// Claude's weekly windows in `cachedUsageUtilization`, each reported with its own length.
const CLAUDE_WEEKLY: [&str; 3] = ["seven_day", "seven_day_opus", "seven_day_sonnet"];
/// A cached weekly window older than this isn't reported (its reset time stays true longer than its percent).
pub const CLAUDE_WEEKLY_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Every usage window Claude Code cached in `~/.claude.json`: the 5-hour one (fresh within the last hour) and the
/// weekly ones (fresh within a day), each only while its window hasn't reset. Empty when there's nothing to trust.
pub fn parse_claude_limits(text: &str, now_ms: u64) -> Vec<Usage> {
    let mut out: Vec<Usage> = parse_claude_usage(text, now_ms).into_iter().collect();
    let Some(cached) = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("cachedUsageUtilization").cloned())
    else {
        return out;
    };
    let Some(fetched) = cached.get("fetchedAtMs").and_then(Value::as_u64) else {
        return out;
    };
    if now_ms.saturating_sub(fetched) > CLAUDE_WEEKLY_MAX_AGE_MS {
        return out;
    }
    for key in CLAUDE_WEEKLY {
        let Some(w) = cached.get("utilization").and_then(|u| u.get(key)) else {
            continue;
        };
        let (Some(used), Some(resets_at)) = (
            w.get("utilization").and_then(Value::as_f64),
            w.get("resets_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_ms),
        ) else {
            continue;
        };
        if resets_at > now_ms
            && !out
                .iter()
                .any(|u| u.resets_at == Some(resets_at) && u.window_minutes == 10080)
        {
            out.push(Usage {
                used_pct: used.clamp(0.0, 100.0),
                window_minutes: 10080,
                resets_at: Some(resets_at),
            });
        }
    }
    out
}

/// The 5-hour usage Claude Code cached in `~/.claude.json` (`cachedUsageUtilization`), when it was fetched within
/// the last hour and its window hasn't reset since. None otherwise: no guess.
pub fn parse_claude_usage(text: &str, now_ms: u64) -> Option<Usage> {
    let v: Value = serde_json::from_str(text).ok()?;
    let cached = v.get("cachedUsageUtilization")?;
    let fetched = cached.get("fetchedAtMs")?.as_u64()?;
    if now_ms.saturating_sub(fetched) > CLAUDE_USAGE_MAX_AGE_MS {
        return None;
    }
    let five = cached.get("utilization")?.get("five_hour")?;
    let used = five.get("utilization")?.as_f64()?;
    let resets_at = five
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms);
    if resets_at.is_some_and(|r| r <= now_ms) {
        return None;
    }
    Some(Usage {
        used_pct: used.clamp(0.0, 100.0),
        window_minutes: 300,
        resets_at,
    })
}

/// Milliseconds since the epoch for an RFC 3339 time such as `2026-11-07T03:59:59.528712+00:00`.
pub fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { text.get(r)?.parse().ok() };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if b[13] != b':' || b[16] != b':' || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let ms: String = frac[..digits]
            .chars()
            .chain("000".chars())
            .take(3)
            .collect();
        millis = ms.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset_secs = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    u64::try_from(secs * 1000 + millis).ok()
}

/// Codex's listed models from `codex debug models`, with their reasoning levels.
pub fn parse_codex_models(text: &str) -> Result<Vec<ModelOption>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let models = v["models"].as_array().ok_or("no models list")?;
    Ok(models
        .iter()
        .filter(|m| m["visibility"].as_str() == Some("list"))
        .filter_map(|m| {
            let id = m["slug"].as_str().filter(|s| is_model_id(s))?;
            let efforts: Vec<String> = m["supported_reasoning_levels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l["effort"].as_str())
                        .filter(|e| is_effort(e))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let default_effort = m["default_reasoning_level"]
                .as_str()
                .filter(|d| efforts.iter().any(|e| e == d))
                .map(str::to_string);
            let name = label(m["display_name"].as_str().unwrap_or(id));
            Some(ModelOption {
                id: id.to_string(),
                label: if name.is_empty() {
                    id.to_string()
                } else {
                    name
                },
                efforts,
                default_effort,
            })
        })
        .collect())
}

/// The ChatGPT plan in Codex's `auth.json`: the `chatgpt_plan_type` claim of its `id_token`, decoded without
/// verification (it is only shown). None for an API key.
pub fn parse_codex_plan(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Auth {
        auth_mode: Option<String>,
        tokens: Option<Tokens>,
    }
    #[derive(Deserialize)]
    struct Tokens {
        id_token: Option<String>,
    }
    let auth: Auth = serde_json::from_str(text).ok()?;
    if auth
        .auth_mode
        .as_deref()
        .is_some_and(|m| m.eq_ignore_ascii_case("apikey"))
    {
        return None;
    }
    let token = auth.tokens?.id_token?;
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let plan = claims["https://api.openai.com/auth"]["chatgpt_plan_type"].as_str()?;
    let plan = label(plan);
    (!plan.is_empty() && plan.len() <= 32).then(|| capitalize(&plan))
}

/// The newest `rate_limits` in Codex session log lines: `primary` then `secondary`, each while its window hasn't
/// reset yet.
pub fn parse_codex_limits(text: &str, now_ms: u64) -> Vec<Usage> {
    let Some(limits) = newest_rate_limits(text) else {
        return Vec::new();
    };
    ["primary", "secondary"]
        .iter()
        .filter_map(|key| codex_window(limits.get(*key)?, now_ms))
        .collect()
}

fn codex_window(w: &Value, now_ms: u64) -> Option<Usage> {
    let used = w.get("used_percent")?.as_f64()?;
    let window = w.get("window_minutes")?.as_u64()?;
    let resets_at = w
        .get("resets_at")
        .and_then(Value::as_u64)
        .map(|s| s.saturating_mul(1000));
    if resets_at.is_some_and(|r| r <= now_ms) {
        return None;
    }
    Some(Usage {
        used_pct: used.clamp(0.0, 100.0),
        window_minutes: window,
        resets_at,
    })
}

fn newest_rate_limits(text: &str) -> Option<Value> {
    text.lines().rev().find_map(|line| {
        if !line.contains("\"rate_limits\"") {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let limits = v
            .pointer("/payload/rate_limits")
            .or_else(|| v.get("rate_limits"))?;
        limits.is_object().then(|| limits.clone())
    })
}

/// The newest `rate_limits.primary` in Codex session log lines, when its window hasn't reset yet.
pub fn parse_codex_usage(text: &str, now_ms: u64) -> Option<Usage> {
    codex_window(newest_rate_limits(text)?.get("primary")?, now_ms)
}

// Folders of `dir` with numeric names (the year, month and day folders of Codex's session logs), newest first.
fn dated_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<(u32, PathBuf)> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path())))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    dirs.into_iter().map(|(_, p)| p).collect()
}

// The newest Codex session logs: `sessions/YYYY/MM/DD/rollout-*.jsonl`.
fn newest_rollouts(sessions: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for year in dated_dirs(sessions) {
        for month in dated_dirs(&year) {
            for day in dated_dirs(&month) {
                let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&day)
                    .map(|rd| {
                        rd.filter_map(Result::ok)
                            .filter(|e| {
                                e.file_name().to_str().is_some_and(|n| {
                                    n.starts_with("rollout-") && n.ends_with(".jsonl")
                                })
                            })
                            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                            .collect()
                    })
                    .unwrap_or_default();
                files.sort_by_key(|f| std::cmp::Reverse(f.0));
                for (_, path) in files {
                    out.push(path);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
    }
    out
}

fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // A cut in the middle leaves a partial first line.
    Some(if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest.to_string())?
    } else {
        text
    })
}

/// Every Codex usage window from the newest session logs that report rate limits.
pub fn codex_limits(sessions: &Path, now_ms: u64) -> Vec<Usage> {
    newest_rollouts(sessions, ROLLOUT_FILES)
        .iter()
        .filter_map(|path| read_tail(path, ROLLOUT_TAIL))
        .find(|text| text.contains("\"rate_limits\""))
        .map(|text| parse_codex_limits(&text, now_ms))
        .unwrap_or_default()
}

/// OpenCode's models from `opencode models`: one `provider/model` per line.
pub fn parse_opencode_models(text: &str) -> Vec<ModelOption> {
    text.lines()
        .map(str::trim)
        .filter(|l| l.contains('/') && is_model_id(l))
        .map(|id| ModelOption {
            id: id.to_string(),
            label: id.to_string(),
            efforts: Vec::new(),
            default_effort: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_HELP: &str = "Usage: claude [options] [command] [prompt]

Options:
  --disallowedTools, --disallowed-tools <tools...>
      Comma or space-separated list of tool names to deny (e.g. \"Bash(git *)
      Edit\")
  --effort <level>                      Effort level for the current session
                                        (low, medium, high, xhigh, max)
  --environment <environment_id>        Create a new cloud session that runs on
                                        the given self-hosted environment
  --model <model>                       Model for the current session. Provide
                                        an alias for the latest model (e.g.
                                        'fable', 'opus', or 'sonnet') or a
                                        model's full name.
";

    #[test]
    fn versions() {
        assert_eq!(
            parse_version("2.1.312 (Claude Code)\n").as_deref(),
            Some("2.1.312")
        );
        assert_eq!(
            parse_version("codex-cli 0.165.0\n").as_deref(),
            Some("0.165.0")
        );
        assert_eq!(parse_version("\nv2.2.5\n").as_deref(), Some("2.2.5"));
        assert_eq!(parse_version("").as_deref(), None);
    }

    #[test]
    fn claude_efforts_come_from_the_help() {
        assert_eq!(
            parse_claude_efforts(CLAUDE_HELP).unwrap(),
            ["low", "medium", "high", "xhigh", "max"]
        );
        let one_line = "  --effort <level>  Effort (choices: \"low\", \"high\")\n  --x\n";
        assert_eq!(parse_claude_efforts(one_line).unwrap(), ["low", "high"]);
        assert_eq!(parse_claude_efforts("  --model <m>  Model\n"), None);
    }

    fn catalog_fixture() -> String {
        serde_json::json!({
            "version": 2,
            "catalog": { "surface": "cc", "config": {
                "id": "cc",
                "models": [
                    { "id": "claude-opus-5-5", "name": "Opus 5.5",
                      "runtime": { "effort_levels": ["low","medium","high","xhigh","max"], "default_effort": "medium" } },
                    { "id": "claude-fable-5-1", "name": "Fable 5.1",
                      "runtime": { "effort_levels": ["low","medium","high","xhigh","max"], "default_effort": "high" } },
                    { "id": "claude-sonnet-5-5", "name": "Sonnet 5.5",
                      "runtime": { "effort_levels": ["low","medium","high","max"], "default_effort": "xhigh" } },
                    { "id": "claude-haiku-4-5", "name": "Haiku 4.5", "runtime": {} }
                ],
                "provider_alias_targets": {
                    "fable": { "default": "claude-fable-5-1", "per_provider": { "gateway": "claude-fable-5" } },
                    "opus": { "default": "claude-opus-5-5" },
                    "sonnet": { "default": "claude-sonnet-5-5" },
                    "haiku": { "default": "claude-haiku-4-5" }
                }
            } }
        })
        .to_string()
    }

    #[test]
    fn claude_models_are_named_from_the_catalog() {
        let efforts: Vec<String> = CLAUDE_EFFORTS.iter().map(|s| s.to_string()).collect();
        let models = claude_models(&efforts, &parse_claude_catalog(&catalog_fixture()));
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["fable", "opus", "sonnet", "haiku"]);
        assert_eq!(models[0].label, "Fable 5.1");
        assert_eq!(models[0].default_effort.as_deref(), Some("high"));
        assert_eq!(models[1].label, "Opus 5.5");
        assert_eq!(models[1].efforts, efforts);
        // Narrowed to the model's own levels; a default it doesn't list is dropped.
        assert_eq!(models[2].efforts, ["low", "medium", "high", "max"]);
        assert_eq!(models[2].default_effort, None);
        // A model without effort levels takes none.
        assert_eq!(models[3].label, "Haiku 4.5");
        assert!(models[3].efforts.is_empty());
    }

    #[test]
    fn claude_models_without_a_catalog_say_latest() {
        let efforts = vec!["low".to_string(), "high".to_string()];
        let models = claude_models(&efforts, &parse_claude_catalog("not json"));
        assert_eq!(models.len(), 4);
        assert_eq!(models[1].id, "opus");
        assert_eq!(models[1].label, "Opus (latest)");
        assert_eq!(models[1].efforts, efforts);
        assert_eq!(models[1].default_effort, None);
    }

    #[test]
    fn claude_plans() {
        let creds = |sub: &str, tier: &str| {
            serde_json::json!({ "claudeAiOauth": {
                "accessToken": "sk-ant-oat-canary", "refreshToken": "sk-ant-ort-canary",
                "expiresAt": 1, "scopes": ["user:inference"],
                "subscriptionType": sub, "rateLimitTier": tier,
            } })
            .to_string()
        };
        assert_eq!(
            parse_claude_plan(&creds("max", "default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            parse_claude_plan(&creds("max", "default_claude_max_5x")).as_deref(),
            Some("Max 5x")
        );
        assert_eq!(
            parse_claude_plan(&creds("pro", "default_claude_ai")).as_deref(),
            Some("Pro")
        );
        assert_eq!(parse_claude_plan(&creds("", "")), None);
        assert_eq!(
            parse_claude_plan(r#"{"primaryApiKey":"sk-ant-api-x"}"#),
            None
        );
        assert_eq!(
            parse_claude_plan(r#"{"claudeAiOauth":{"accessToken":"x"}}"#),
            None
        );
        assert_eq!(parse_claude_plan("garbage"), None);
    }

    #[test]
    fn claude_usage_only_when_fresh() {
        let now = parse_rfc3339_ms("2026-10-31T22:00:00Z").unwrap();
        let file = |fetched: u64, five: Value| {
            serde_json::json!({ "cachedUsageUtilization": {
                "fetchedAtMs": fetched, "utilization": { "five_hour": five, "seven_day": { "utilization": 16 } }
            } })
            .to_string()
        };
        let window =
            serde_json::json!({ "utilization": 42.5, "resets_at": "2026-11-01T01:30:00.5+00:00" });
        let usage = parse_claude_usage(&file(now - 60_000, window.clone()), now).unwrap();
        assert_eq!(usage.used_pct, 42.5);
        assert_eq!(usage.window_minutes, 300);
        assert_eq!(
            usage.resets_at,
            Some(parse_rfc3339_ms("2026-11-01T01:30:00.500Z").unwrap())
        );
        assert_eq!(
            parse_claude_usage(&file(now - 2 * 3_600_000, window), now),
            None
        );
        assert_eq!(parse_claude_usage(&file(now, Value::Null), now), None);
        let reset = serde_json::json!({ "utilization": 90, "resets_at": "2026-10-31T21:00:00Z" });
        assert_eq!(parse_claude_usage(&file(now, reset), now), None);
    }

    #[test]
    fn claude_limits_include_the_weekly_windows() {
        let now = parse_rfc3339_ms("2026-11-01T00:00:00Z").unwrap();
        let text = |fetched: u64| {
            serde_json::json!({ "cachedUsageUtilization": { "fetchedAtMs": fetched, "utilization": {
                "five_hour": { "utilization": 30, "resets_at": "2026-11-01T02:00:00Z" },
                "seven_day": { "utilization": 18, "resets_at": "2026-11-07T04:00:00.417802+00:00" },
                "seven_day_opus": null
            } } })
            .to_string()
        };
        let limits = parse_claude_limits(&text(now - 60_000), now);
        assert_eq!(
            limits.iter().map(|u| u.window_minutes).collect::<Vec<_>>(),
            vec![300, 10080]
        );
        assert_eq!(limits[1].used_pct, 18.0);
        // three hours old: the 5-hour percent is stale, the weekly reset still counts
        let older = parse_claude_limits(&text(now - 3 * 3_600_000), now);
        assert_eq!(
            older.iter().map(|u| u.window_minutes).collect::<Vec<_>>(),
            vec![10080]
        );
        assert!(parse_claude_limits(&text(now - 2 * 24 * 3_600_000), now).is_empty());
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-11-07T03:59:59.528712+00:00"),
            Some(1_794_023_999_528)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-07T05:59:59.528+02:00"),
            Some(1_794_023_999_528)
        );
        assert_eq!(parse_rfc3339_ms("yesterday"), None);
    }

    const CODEX_MODELS: &str = r#"{"models":[
        {"slug":"gpt-6.1-sol","display_name":"GPT-6.1-Sol","default_reasoning_level":"low",
         "supported_reasoning_levels":[{"effort":"low","description":"x"},{"effort":"medium"},{"effort":"high"},
            {"effort":"xhigh"},{"effort":"max"},{"effort":"ultra"}],
         "visibility":"list","model_messages":{"persistent_instructions":"long text"}},
        {"slug":"gpt-reserve","display_name":"GPT-Reserve","default_reasoning_level":"medium",
         "supported_reasoning_levels":[{"effort":"low"}],"visibility":"hide"},
        {"slug":"gpt-6-luna","display_name":"GPT-6-Luna","default_reasoning_level":"medium",
         "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"},{"effort":"xhigh"},
            {"effort":"max"}],"visibility":"list"},
        {"slug":"codex-auto-review","display_name":"Codex Auto Review","visibility":"hide",
         "supported_reasoning_levels":[]}
    ]}"#;

    #[test]
    fn codex_models_are_the_listed_ones() {
        let models = parse_codex_models(CODEX_MODELS).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-6.1-sol");
        assert_eq!(models[0].label, "GPT-6.1-Sol");
        assert_eq!(
            models[0].efforts,
            ["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(models[0].default_effort.as_deref(), Some("low"));
        assert_eq!(models[1].id, "gpt-6-luna");
        assert_eq!(models[1].default_effort.as_deref(), Some("medium"));
        assert!(parse_codex_models("nope").is_err());
    }

    fn jwt(claims: Value) -> String {
        let enc = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
        format!(
            "{}.{}.sig-canary",
            enc(&serde_json::json!({"alg":"RS256"})),
            enc(&claims)
        )
    }

    #[test]
    fn codex_plan_from_the_id_token() {
        let auth = |mode: &str, plan: &str| {
            serde_json::json!({
                "auth_mode": mode, "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": jwt(serde_json::json!({
                        "email": "sam@example.com",
                        "https://api.openai.com/auth": { "chatgpt_plan_type": plan, "chatgpt_user_id": "u" }
                    })),
                    "access_token": "access-canary", "refresh_token": "refresh-canary", "account_id": "a"
                },
                "last_refresh": "2026-10-31T00:00:00Z"
            })
            .to_string()
        };
        assert_eq!(
            parse_codex_plan(&auth("chatgpt", "pro")).as_deref(),
            Some("Pro")
        );
        assert_eq!(
            parse_codex_plan(&auth("chatgpt", "plus")).as_deref(),
            Some("Plus")
        );
        assert_eq!(parse_codex_plan(&auth("apikey", "pro")), None);
        assert_eq!(
            parse_codex_plan(r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#),
            None
        );
        assert_eq!(
            parse_codex_plan(r#"{"tokens":{"id_token":"not-a-jwt"}}"#),
            None
        );
    }

    #[test]
    fn codex_usage_from_the_newest_session_log() {
        let now = 1_793_500_000_000;
        let line = |used: f64, resets: u64| {
            serde_json::json!({ "timestamp": "t", "type": "event_msg", "payload": {
                "type": "token_count", "info": null,
                "rate_limits": { "limit_id": "codex", "primary": {
                    "used_percent": used, "window_minutes": 10080, "resets_at": resets
                }, "secondary": null, "plan_type": "pro" }
            } })
            .to_string()
        };
        let text = format!(
            "{}\n{}\n{}\n",
            line(10.0, 1_793_900_677),
            r#"{"type":"response_item","payload":{"type":"message"}}"#,
            line(40.0, 1_793_900_677)
        );
        let usage = parse_codex_usage(&text, now).unwrap();
        assert_eq!(usage.used_pct, 40.0);
        assert_eq!(usage.window_minutes, 10080);
        assert_eq!(usage.resets_at, Some(1_793_900_677_000));
        assert_eq!(parse_codex_usage(&line(40.0, 1_000), now), None);
        assert_eq!(parse_codex_usage("{}\n", now), None);

        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("10").join("31");
        fs::create_dir_all(&day).unwrap();
        fs::create_dir_all(dir.path().join("2025").join("12").join("01")).unwrap();
        fs::write(day.join("rollout-a.jsonl"), &text).unwrap();
        fs::write(day.join("notes.txt"), "x").unwrap();
        let limits = codex_limits(dir.path(), now);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].used_pct, 40.0);
        assert!(codex_limits(&dir.path().join("missing"), now).is_empty());
    }

    #[test]
    fn codex_limits_primary_then_secondary() {
        let now = 1_793_500_000_000;
        let line = serde_json::json!({ "payload": { "rate_limits": {
            "primary": { "used_percent": 12.0, "window_minutes": 300, "resets_at": 1_793_510_000 },
            "secondary": { "used_percent": 40.0, "window_minutes": 10080, "resets_at": 1_793_900_677 }
        } } })
        .to_string();
        let limits = parse_codex_limits(&line, now);
        assert_eq!(
            limits.iter().map(|u| u.window_minutes).collect::<Vec<_>>(),
            vec![300, 10080]
        );
        // a window that already reset is left out
        assert_eq!(parse_codex_limits(&line, 1_793_600_000_000).len(), 1);
    }

    #[test]
    fn opencode_models_one_per_line() {
        let models = parse_opencode_models(
            "opencode-go/kimi-k3\nopencode-go/glm-5.3\n\nnot a model\nWARN something\n",
        );
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["opencode-go/kimi-k3", "opencode-go/glm-5.3"]);
        assert!(models.iter().all(|m| m.efforts.is_empty()));
    }
}
