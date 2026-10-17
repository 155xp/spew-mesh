//! The responder: one question in, one answer or failure out (9).

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::codex::{self, ModelChoice, ResumeStyle};
use super::env::HostEnv;
use super::limits::Clock;
use super::memory::{ConversationKey, ConversationStore, StoredSession};
use super::process::{Exit, ProcessRunner, RunOutput, RunSpec};
use super::prompt::{self, PromptContext};
use super::{
    Question, RunFailure, RunResult, ServeClient, ServeConfig, ServeError, claude, is_safe_id,
    opencode,
};

/// The largest answer a reply can carry (7.1); longer answers are cut at a line break.
pub const MAX_ANSWER_BYTES: usize = 32 * 1024;

const CUT_NOTE: &str = "\n(cut: the answer was longer than 32 KiB)";
const CANARY_TIMEOUT: Duration = Duration::from_secs(60);
const LOGGED_OUTPUT: usize = 16 * 1024;

/// A run's answer, ready for `message.reply`.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// The answer text, at most [`MAX_ANSWER_BYTES`].
    pub text: String,
    /// The cost the client reported, if any.
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug)]
struct CodexSetup {
    profile: String,
    model: ModelChoice,
    resume: ResumeStyle,
}

/// Answers questions with headless runs of one client in one folder.
pub struct Responder<R, M, C> {
    cfg: ServeConfig,
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
    codex: Option<CodexSetup>,
    runner: R,
    conversations: M,
    clock: C,
}

impl<R, M, C> Responder<R, M, C>
where
    R: ProcessRunner,
    M: ConversationStore,
    C: Clock,
{
    /// Checks everything a run needs and refuses to start when anything is off: the client on PATH, the
    /// folder, and for Codex the configs and the sandbox self-check (8.3).
    pub async fn start(
        mut cfg: ServeConfig,
        host: &HostEnv,
        runner: R,
        conversations: M,
        clock: C,
    ) -> Result<Self, ServeError> {
        cfg.validate()?;
        cfg.cwd = canonical_dir(&cfg.cwd)?;
        let name = cfg.client.program();
        let program = host
            .find_program(name)
            .ok_or(ServeError::ProgramNotFound(name))?;
        let adds = match cfg.client {
            ServeClient::Opencode => opencode::env_adds(cfg.allow_edits),
            ServeClient::Claude | ServeClient::Codex => Vec::new(),
        };
        let env = host.child_env(cfg.client, &adds);
        let codex = match cfg.client {
            ServeClient::Codex => {
                Some(prepare_codex(&mut cfg, host, &program, &env, &runner).await?)
            }
            ServeClient::Claude | ServeClient::Opencode => None,
        };
        Ok(Responder {
            cfg,
            program,
            env,
            codex,
            runner,
            conversations,
            clock,
        })
    }

    /// The configuration in effect (with the folder canonicalized).
    pub fn config(&self) -> &ServeConfig {
        &self.cfg
    }

    /// The client executable.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The conversation map.
    pub fn conversations(&self) -> &M {
        &self.conversations
    }

    /// Runs one question. A failure's [`RunFailure::reply_text`] is what goes back to the asker.
    pub async fn answer(&mut self, q: &Question) -> Result<Answer, RunFailure> {
        let key = ConversationKey::of(q);
        let client = self.cfg.client;
        let known = match self.conversations.get(&key) {
            Ok(Some(stored)) if stored.client == client => Some(stored.session_id),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "can't read the serve conversation map; starting a new conversation");
                None
            }
        };
        let place = self
            .cfg
            .cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let text = prompt::build(
            q,
            &PromptContext {
                allow_edits: self.cfg.allow_edits,
                place: &place,
            },
        );
        let (result, forget) = match client {
            ServeClient::Claude => self.run_claude(known.as_deref(), text).await,
            ServeClient::Codex => (self.run_codex(known.as_deref(), text).await, false),
            ServeClient::Opencode => (
                self.run_opencode(known.as_deref(), &q.from.address(), text)
                    .await,
                false,
            ),
        };
        self.remember(&key, known.as_deref(), &result, forget);
        match result {
            Ok(r) => {
                tracing::info!(
                    %client,
                    question = %q.id,
                    conversation = %q.conversation,
                    cost_usd = ?r.cost_usd,
                    "serve run answered"
                );
                Ok(Answer {
                    text: fit_answer(&r.answer),
                    cost_usd: r.cost_usd,
                })
            }
            Err(f) => {
                tracing::warn!(
                    %client,
                    question = %q.id,
                    conversation = %q.conversation,
                    reason = %f.reason,
                    cost_usd = ?f.cost_usd,
                    "serve run failed"
                );
                Err(f)
            }
        }
    }

    fn remember(
        &mut self,
        key: &ConversationKey,
        known: Option<&str>,
        result: &Result<RunResult, RunFailure>,
        forget: bool,
    ) {
        let reported = match result {
            Ok(r) => r.session_id.as_deref(),
            Err(f) => f.session_id.as_deref(),
        };
        if forget {
            if let Err(e) = self.conversations.remove(key) {
                tracing::warn!(error = %e, "can't update the serve conversation map");
            }
            return;
        }
        let Some(id) = reported else {
            return;
        };
        if known.is_some_and(|k| k != id) {
            // Codex starts a new thread when it can't find the old one; the conversation starts over.
            tracing::info!(client = %self.cfg.client, "the follow-up ran in a new session; the conversation starts over");
        }
        let stored = StoredSession {
            client: self.cfg.client,
            session_id: id.to_string(),
            last_used_ms: self.clock.now_ms(),
        };
        if let Err(e) = self.conversations.put(key.clone(), stored) {
            tracing::warn!(error = %e, "can't update the serve conversation map");
        }
    }

    fn spec(&self, args: Vec<OsString>, stdin: Vec<u8>) -> RunSpec {
        RunSpec {
            program: self.program.clone(),
            args,
            cwd: self.cfg.cwd.clone(),
            env: self.env.clone(),
            stdin,
            timeout: self.cfg.timeout,
            stdout_cap: self.cfg.stdout_cap,
        }
    }

    /// Returns the result and whether to forget the conversation (Claude Code no longer has it).
    async fn run_claude(
        &mut self,
        known: Option<&str>,
        prompt: String,
    ) -> (Result<RunResult, RunFailure>, bool) {
        let (session, resumed) = match known.filter(|id| claude::is_uuid(id)) {
            Some(id) => (id.to_string(), true),
            None => match claude::new_session_id() {
                Ok(id) => (id, false),
                Err(e) => return (Err(RunFailure::new(e.to_string())), false),
            },
        };
        let opts = claude::Options {
            allow_edits: self.cfg.allow_edits,
            grep: self.cfg.claude_grep,
            max_turns: self.cfg.max_turns,
            max_budget_usd: self.cfg.max_budget_usd,
            model: self.cfg.model.as_deref(),
        };
        let which = if resumed {
            claude::Session::Resume(&session)
        } else {
            claude::Session::New(&session)
        };
        let args = claude::args(which, &opts)
            .into_iter()
            .map(OsString::from)
            .collect();
        let spec = self.spec(args, prompt.into_bytes());
        let out = match execute(&self.runner, spec, self.cfg.client).await {
            Ok(out) => out,
            Err(f) => return (Err(f), false),
        };
        let forget = resumed && claude::session_missing(&out);
        let mut result = claude::parse(&out);
        match &mut result {
            // The session we named exists once a run answered, even if the result didn't repeat its id.
            Ok(r) if r.session_id.is_none() => r.session_id = Some(session),
            Ok(_) => {}
            Err(f) => log_output(self.cfg.client, &out, f),
        }
        (result, forget)
    }

    async fn run_codex(
        &mut self,
        known: Option<&str>,
        prompt: String,
    ) -> Result<RunResult, RunFailure> {
        let Some(setup) = self.codex.clone() else {
            return Err(RunFailure::new("the Codex responder wasn't prepared"));
        };
        let thread = known.filter(|id| is_safe_id(id));
        let run_file = RunFile::new(&self.cfg.run_dir);
        let mut style = setup.resume;
        loop {
            let args = codex::exec_args(&codex::ExecArgs {
                cwd: &self.cfg.cwd,
                profile: &setup.profile,
                model: &setup.model,
                run_file: run_file.path(),
                resume: thread.map(|id| (id, style)),
            });
            let spec = self.spec(args, prompt.clone().into_bytes());
            let out = execute(&self.runner, spec, self.cfg.client).await?;
            if thread.is_some() && style == ResumeStyle::Dash && codex::rejected_resume_dash(&out) {
                tracing::warn!(
                    "Codex rejected `resume <thread id> -`; follow-ups pass a positional prompt from now on"
                );
                style = ResumeStyle::Positional;
                if let Some(c) = self.codex.as_mut() {
                    c.resume = style;
                }
                continue;
            }
            let file_text = run_file.read(self.cfg.stdout_cap);
            let result = codex::parse(&out, file_text.as_deref());
            if let Err(f) = &result {
                log_output(self.cfg.client, &out, f);
            }
            return result;
        }
    }

    async fn run_opencode(
        &mut self,
        known: Option<&str>,
        asker: &str,
        prompt: String,
    ) -> Result<RunResult, RunFailure> {
        let session = known.filter(|id| is_safe_id(id));
        let title: String = asker
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
            .take(120)
            .collect();
        let args = opencode::args(session, self.cfg.model.as_deref(), &title, &self.cfg.cwd);
        let spec = self.spec(args, prompt.into_bytes());
        let out = execute(&self.runner, spec, self.cfg.client).await?;
        let result = opencode::parse(&out);
        if let Err(f) = &result {
            log_output(self.cfg.client, &out, f);
        }
        result
    }
}

/// Runs one spec and turns a start error, a timeout or too much output into a failure.
async fn execute<R: ProcessRunner>(
    runner: &R,
    spec: RunSpec,
    client: ServeClient,
) -> Result<RunOutput, RunFailure> {
    let timeout = spec.timeout;
    let cap = spec.stdout_cap;
    let out = runner
        .run(spec)
        .await
        .map_err(|e| RunFailure::new(format!("{client} could not be started: {e}")))?;
    let failure = match out.exit {
        Exit::TimedOut => RunFailure::new(format!(
            "it ran longer than {} and was stopped",
            describe_duration(timeout)
        )),
        Exit::OutputTooLarge => {
            RunFailure::new(format!("it printed more than {cap} bytes and was stopped"))
        }
        Exit::Code(_) | Exit::Signal(_) => return Ok(out),
    };
    log_output(client, &out, &failure);
    Err(failure)
}

/// The raw output of a failed run stays in the log (9).
fn log_output(client: ServeClient, out: &RunOutput, failure: &RunFailure) {
    let cut = |bytes: &[u8]| {
        String::from_utf8_lossy(&bytes[..bytes.len().min(LOGGED_OUTPUT)]).into_owned()
    };
    tracing::warn!(
        %client,
        exit = ?out.exit,
        elapsed_ms = u64::try_from(out.elapsed.as_millis()).unwrap_or(u64::MAX),
        cost_usd = ?failure.cost_usd,
        stdout = %cut(&out.stdout),
        stderr = %cut(&out.stderr),
        "serve run output"
    );
}

fn describe_duration(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        let s = d.as_secs();
        if s == 1 {
            "1 second".to_string()
        } else {
            format!("{s} seconds")
        }
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Cuts an answer longer than a reply can carry at a line break, with a note.
pub(crate) fn fit_answer(text: &str) -> String {
    if text.len() <= MAX_ANSWER_BYTES {
        return text.to_string();
    }
    let mut end = MAX_ANSWER_BYTES - CUT_NOTE.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let head = match head.rfind('\n') {
        Some(i) if i >= end / 2 => &head[..i],
        _ => head,
    };
    format!("{head}{CUT_NOTE}")
}

fn canonical_dir(dir: &Path) -> Result<PathBuf, ServeError> {
    let real = std::fs::canonicalize(dir)
        .map_err(|e| ServeError::io(format!("can't use --cwd {}", dir.display()), e))?;
    if !real.is_dir() {
        return Err(ServeError::Invalid(format!(
            "--cwd {} is not a folder",
            dir.display()
        )));
    }
    Ok(real)
}

async fn prepare_codex<R: ProcessRunner>(
    cfg: &mut ServeConfig,
    host: &HostEnv,
    program: &Path,
    env: &[(OsString, OsString)],
    runner: &R,
) -> Result<CodexSetup, ServeError> {
    codex::check_project_configs(&cfg.cwd)?;
    let home = codex::codex_home(host)?;
    let model = codex::read_user_config(&home.join("config.toml"), cfg.model.as_deref())?;
    let real = std::fs::canonicalize(program)
        .map_err(|e| ServeError::io(format!("can't resolve {}", program.display()), e))?;
    let profile = codex::profile(&codex::codex_dir(&real, &home), cfg.allow_edits)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&cfg.run_dir)
        .map_err(|e| ServeError::io(format!("can't create {}", cfg.run_dir.display()), e))?;
    cfg.run_dir = std::fs::canonicalize(&cfg.run_dir)
        .map_err(|e| ServeError::io(format!("can't use {}", cfg.run_dir.display()), e))?;
    codex::canary_check(
        runner,
        &codex::Canary {
            program,
            cwd: &cfg.cwd,
            profile: &profile,
            env,
            outside_dir: &cfg.run_dir,
            timeout: CANARY_TIMEOUT,
        },
    )
    .await?;
    Ok(CodexSetup {
        profile,
        model,
        resume: ResumeStyle::Dash,
    })
}

/// `<home>/run/serve/<ulid>.txt`, which Codex writes with `-o`; deleted when dropped.
struct RunFile {
    path: PathBuf,
}

impl RunFile {
    fn new(dir: &Path) -> Self {
        RunFile {
            path: dir.join(format!("{}.txt", ulid::Ulid::new())),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self, cap: usize) -> Option<String> {
        let file = std::fs::File::open(&self.path).ok()?;
        let mut buf = Vec::new();
        file.take(u64::try_from(cap).unwrap_or(u64::MAX))
            .read_to_end(&mut buf)
            .ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

impl Drop for RunFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::super::limits::ManualClock;
    use super::*;

    #[test]
    fn short_answers_pass_untouched() {
        assert_eq!(fit_answer("hi"), "hi");
        let exact = "x".repeat(MAX_ANSWER_BYTES);
        assert_eq!(fit_answer(&exact), exact);
    }

    #[test]
    fn long_answers_are_cut_at_a_line_break() {
        let line = format!("{}\n", "y".repeat(99));
        let long = line.repeat(1000);
        let cut = fit_answer(&long);
        assert!(cut.len() <= MAX_ANSWER_BYTES);
        assert!(cut.ends_with(CUT_NOTE));
        let head = cut.strip_suffix(CUT_NOTE).unwrap();
        assert!(head.ends_with('y'));
        assert_eq!(head.len() % 100, 99);
    }

    #[test]
    fn long_answers_without_breaks_cut_on_a_char_boundary() {
        let long = "é".repeat(MAX_ANSWER_BYTES);
        let cut = fit_answer(&long);
        assert!(cut.len() <= MAX_ANSWER_BYTES);
        assert!(cut.ends_with(CUT_NOTE));
    }

    #[test]
    fn durations() {
        assert_eq!(describe_duration(Duration::from_secs(300)), "300 seconds");
        assert_eq!(describe_duration(Duration::from_secs(1)), "1 second");
        assert_eq!(describe_duration(Duration::from_millis(1500)), "1500 ms");
    }

    #[test]
    fn run_file_is_deleted_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let f = RunFile::new(dir.path());
            std::fs::write(f.path(), "answer").unwrap();
            assert_eq!(f.read(3).as_deref(), Some("ans"));
            f.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    /// A store that is Send but not Sync, like one holding a SQLite connection.
    struct CellStore(std::cell::Cell<u8>);

    impl ConversationStore for CellStore {
        fn get(&self, _: &ConversationKey) -> anyhow::Result<Option<StoredSession>> {
            self.0.set(self.0.get().wrapping_add(1));
            Ok(None)
        }
        fn put(&mut self, _: ConversationKey, _: StoredSession) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove(&mut self, _: &ConversationKey) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn assert_send<T: Send>(_: &T) {}

    #[allow(dead_code)]
    async fn answer_future_is_send(
        mut r: Responder<super::super::process::SystemRunner, CellStore, ManualClock>,
        q: Question,
    ) {
        let fut = r.answer(&q);
        assert_send(&fut);
        let _ = fut.await;
    }
}
