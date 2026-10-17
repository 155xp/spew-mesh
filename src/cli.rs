//! The `spew mesh` command line (10). Each command talks to the daemon over the control socket, starting it
//! when needed (6.1), and acts as this device's `terminal` session (6.2).

mod output;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use clap::{Args, Parser, Subcommand};

use crate::agents::Client as Agent;
use crate::agents::runner::SystemRunner;
use crate::agents::setup::{ClientPaths, Options as SetupOptions, Setup, Wiring, closing_lines};
use crate::control::client::{Client, ClientError, ConnectOptions, wait_for_exit};
use crate::control::protocol::{
    AskParams, DirectoryQuery, FetchParams, LoginResult, ReplyParams, SendStatus, SessionOpen,
    TellParams,
};
use crate::daemon::DaemonOptions;
use crate::identity::DeviceIdentity;
use crate::paths::{Home, user_home_from_env};
use crate::settings::{Env, PROJECT_ENV, Settings};
use crate::spew_api::{Clock, OpenBrowser, SystemBrowser, SystemClock};
use crate::wire::SessionKind;

/// How long `daemon stop` waits for the daemon to exit.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The `spew` binary.
#[derive(Debug, Parser)]
#[command(name = "spew", version, about = "spew tools on this computer")]
pub struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Debug, Subcommand)]
enum Top {
    /// Let your coding agents and your team's ask each other questions, peer to peer.
    Mesh(MeshArgs),
}

#[derive(Debug, Args)]
struct MeshArgs {
    /// Use this home directory instead of ~/.spew/mesh (overrides SPEW_MESH_DIR).
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Wire your agents, start the daemon and sign in to spew.
    Setup {
        /// Only these agents: claude, codex, opencode (comma-separated).
        #[arg(long, value_name = "LIST")]
        agents: Option<String>,
        /// Name this device (default <person>-<platform>).
        #[arg(long, value_name = "NAME")]
        device_name: Option<String>,
        /// Don't sign in to spew.
        #[arg(long)]
        no_login: bool,
        /// Show what would change and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Revert setup and stop the daemon.
    Uninstall {
        /// Also delete the home directory.
        #[arg(long)]
        purge: bool,
    },
    /// Sign in to spew.
    Login,
    /// Sign out of spew and unregister this device.
    Logout,
    /// List the agent sessions you can reach.
    Who {
        /// Only sessions matching this text (address, client, repo, branch, card, activity).
        #[arg(value_name = "MATCH")]
        filter: Option<String>,
        /// Include offline devices.
        #[arg(long)]
        offline: bool,
        /// Show each device's path and round-trip time.
        #[arg(long)]
        links: bool,
        /// Print the listing as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ask a session a question and print the answer.
    Ask {
        /// Who to ask: a session name, person/name, person@device/name, a card ref or a person.
        to: String,
        /// The question; read from stdin when left out.
        text: Vec<String>,
        /// How long to wait for the answer, in seconds (default 100, at most 900).
        #[arg(long, value_name = "SECONDS")]
        wait: Option<u64>,
        /// A card ref the question is about, such as spew#42.
        #[arg(long, value_name = "REF")]
        card: Option<String>,
    },
    /// Send a message without waiting for an answer.
    Tell {
        /// Who to tell.
        to: String,
        /// The message; read from stdin when left out.
        text: Vec<String>,
        /// A card ref the message is about.
        #[arg(long, value_name = "REF")]
        card: Option<String>,
    },
    /// Answer a message the terminal session received.
    Reply {
        /// The message id.
        id: String,
        /// The answer; read from stdin when left out.
        text: Vec<String>,
    },
    /// Show the terminal session's messages.
    Messages {
        /// Wait this many seconds for a message when none is waiting.
        #[arg(long, value_name = "SECONDS")]
        wait: Option<u64>,
    },
    /// Answer questions with a headless agent (a responder).
    Serve(ServeArgs),
    /// List your devices from spew, or revoke one.
    Devices {
        #[command(subcommand)]
        command: Option<DevicesCommand>,
    },
    /// Check the setup and say what to fix.
    Doctor,
    /// Run, start, stop or check the daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Print this device's name and endpoint id.
    Identity,
    /// The MCP server agents run.
    #[command(hide = true)]
    McpServer,
    /// A hook agents run.
    #[command(hide = true)]
    Hook {
        /// claude, codex or opencode.
        client: String,
        /// The client's own event name.
        event: String,
    },
}

/// `spew mesh serve` (9).
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// claude, codex or opencode.
    pub client: String,
    /// The responder's session name (default: the client).
    #[arg(long, value_name = "N")]
    pub name: Option<String>,
    /// The folder it answers about (default: the current directory).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Allow file edits inside --cwd.
    #[arg(long)]
    pub allow_edits: bool,
    /// Per-run timeout in seconds.
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,
    /// The model the client runs.
    #[arg(long, value_name = "M")]
    pub model: Option<String>,
}

#[derive(Debug, Subcommand)]
enum DevicesCommand {
    /// Revoke a device you no longer control.
    Revoke {
        /// Its name, endpoint id or short id.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run in the foreground.
    Run,
    /// Start in the background.
    Start,
    /// Ask it to exit.
    Stop,
    /// Show its state.
    Status,
}

/// Entry point for the `spew` binary.
pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("spew mesh: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let Top::Mesh(mesh) = cli.command;
    // The hidden commands come first: they must work before anything else is set up.
    match &mesh.command {
        Some(Command::McpServer) => return Ok(mcp_server_not_built()),
        Some(Command::Hook { client, event }) => return Ok(hook_not_built(client, event)),
        Some(Command::Serve(args)) => return Ok(serve(mesh.home.as_deref(), args)),
        _ => {}
    }
    let home = Home::from_env(mesh.home.as_deref()).context("can't find the spew mesh home")?;
    match mesh.command {
        None => block_on(overview(&home)),
        Some(Command::Setup {
            agents,
            device_name,
            no_login,
            dry_run,
        }) => setup(
            &home,
            agents.as_deref(),
            device_name.as_deref(),
            no_login,
            dry_run,
        ),
        Some(Command::Uninstall { purge }) => uninstall(&home, purge),
        Some(Command::Login) => block_on(login(&home)),
        Some(Command::Logout) => block_on(logout(&home)),
        Some(Command::Who {
            filter,
            offline,
            links,
            json,
        }) => block_on(who(&home, filter, offline, links, json)),
        Some(Command::Ask {
            to,
            text,
            wait,
            card,
        }) => block_on(ask(&home, to, text_arg(text)?, wait, card)),
        Some(Command::Tell { to, text, card }) => block_on(tell(&home, to, text_arg(text)?, card)),
        Some(Command::Reply { id, text }) => block_on(reply(&home, id, text_arg(text)?)),
        Some(Command::Messages { wait }) => block_on(messages(&home, wait)),
        Some(Command::Devices { command }) => block_on(devices(&home, command)),
        Some(Command::Doctor) => block_on(doctor(&home)),
        Some(Command::Daemon { command }) => daemon(&home, command),
        Some(Command::Identity) => identity(&home),
        Some(Command::McpServer | Command::Hook { .. } | Command::Serve(_)) => {
            Ok(ExitCode::FAILURE)
        }
    }
}

/// `spew mesh mcp-server`: built by mesh#8 part 2.
pub fn mcp_server_not_built() -> ExitCode {
    eprintln!("spew mesh mcp-server: not built yet");
    ExitCode::FAILURE
}

/// `spew mesh hook <client> <event>`: built by mesh#8 part 2.
pub fn hook_not_built(client: &str, event: &str) -> ExitCode {
    eprintln!("spew mesh hook {client} {event}: not built yet");
    ExitCode::FAILURE
}

/// `spew mesh serve` (9).
pub fn serve(home: Option<&std::path::Path>, args: &ServeArgs) -> ExitCode {
    let opts = crate::responder::command::ServeOptions {
        client: args.client.clone(),
        name: args.name.clone(),
        cwd: args.cwd.clone(),
        allow_edits: args.allow_edits,
        timeout: args.timeout,
        model: args.model.clone(),
    };
    crate::responder::command::run(home, &opts)
}

fn block_on<T, F: Future<Output = anyhow::Result<T>>>(f: F) -> anyhow::Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("can't start the async runtime")?
        .block_on(f)
}

// The text of ask, tell and reply: the words given, else stdin when it isn't a terminal (10).
fn text_arg(words: Vec<String>) -> anyhow::Result<String> {
    if !words.is_empty() {
        return Ok(words.join(" "));
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        bail!("give the text after the address, or pipe it in");
    }
    let mut text = String::new();
    stdin
        .lock()
        .take(64 * 1024)
        .read_to_string(&mut text)
        .context("can't read the text from stdin")?;
    if text.trim().is_empty() {
        bail!("the text is empty");
    }
    Ok(text)
}

fn now_ms() -> u64 {
    SystemClock.now_ms()
}

async fn connect(home: &Home) -> anyhow::Result<Client> {
    Ok(Client::connect(home, &ConnectOptions::cli()).await?)
}

// Opens (or reuses) this person's terminal session and returns its token (6.2).
async fn terminal(client: &Client) -> anyhow::Result<String> {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|d| d.to_str().map(str::to_string))
        .unwrap_or_else(|| "/".to_string());
    let opened = client
        .session_open(&SessionOpen {
            client: "terminal".into(),
            client_session: None,
            client_pid: None,
            cwd,
            client_name: Some("spew-cli".into()),
            kind: SessionKind::Terminal,
            name: None,
            spew_project: std::env::var(PROJECT_ENV).ok().filter(|v| !v.is_empty()),
            serve: None,
        })
        .await?;
    Ok(opened.token)
}

async fn overview(home: &Home) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let status = client.status().await?;
    println!("{}", output::status_line(&status));
    let token = terminal(&client).await?;
    let dir = client
        .directory(&DirectoryQuery {
            token: Some(token),
            filter: None,
            include_offline: false,
        })
        .await?;
    print!("{}", output::who(&dir, now_ms(), false));
    Ok(ExitCode::SUCCESS)
}

fn wiring_setup<'a>(home: &Home, runner: &'a SystemRunner) -> anyhow::Result<Setup<'a>> {
    let user_home = user_home_from_env().context("HOME is not set")?;
    let exe = std::env::current_exe().context("can't find this program's path")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let home_arg = (!home.is_default(Some(&user_home))).then(|| home.root().to_path_buf());
    Ok(Setup {
        wiring: Wiring::new(exe, home_arg)?,
        paths: ClientPaths::from_env(&user_home, &|k| std::env::var_os(k))?,
        manifest_path: home.manifest_file(),
        runner,
    })
}

fn print_login(r: &LoginResult) {
    let mut text = format!("Signed in to spew as {}.", r.username);
    if !r.projects.is_empty() {
        text.push_str(&format!(" Projects: {}.", r.projects.join(", ")));
    }
    if r.registered {
        text.push_str(&format!(" This device is registered as {}.", r.device));
    }
    println!("{text}");
    if let Some(note) = &r.note {
        println!("{note}");
    }
}

async fn sign_in(client: &Client) -> Result<LoginResult, ClientError> {
    client.login(|url| SystemBrowser.open(url)).await
}

// Stops a running daemon and waits for it to exit; false when none was running.
async fn stop_daemon(home: &Home) -> anyhow::Result<bool> {
    let Ok(client) = Client::connect(home, &ConnectOptions::existing()).await else {
        return Ok(false);
    };
    client.shutdown().await?;
    if !wait_for_exit(home, Instant::now() + STOP_WAIT).await {
        bail!("the daemon didn't exit within {} s", STOP_WAIT.as_secs());
    }
    Ok(true)
}

fn setup(
    home: &Home,
    agents: Option<&str>,
    device_name: Option<&str>,
    no_login: bool,
    dry_run: bool,
) -> anyhow::Result<ExitCode> {
    let agents = agents.map(Agent::parse_list).transpose()?;
    let mut renamed = false;
    if let Some(name) = device_name {
        if dry_run {
            println!("Would name this device {name}.");
        } else {
            home.ensure()?;
            let identity = match DeviceIdentity::rename(home, name)? {
                Some(identity) => identity,
                None => DeviceIdentity::load_or_create(home, name, now_ms())?.0,
            };
            println!("This device is named {}.", identity.name());
            renamed = true;
        }
    }
    let runner = SystemRunner::from_process_env();
    let wiring = wiring_setup(home, &runner)?;
    let report = wiring.setup(&SetupOptions { agents, dry_run })?;
    for line in &report.lines {
        println!("{line}");
    }
    if dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    block_on(async {
        // A new name takes effect when the daemon starts again.
        if renamed {
            stop_daemon(home).await?;
        }
        let client = connect(home).await?;
        let status = client.status().await?;
        println!(
            "The spew mesh daemon is running on {} ({}).",
            status.device.name, status.device.short_id
        );
        if !no_login {
            match sign_in(&client).await {
                Ok(r) => print_login(&r),
                Err(e) => println!(
                    "Sign-in didn't work ({e}); everything else is set up. Run `spew mesh login` later."
                ),
            }
        }
        Ok(ExitCode::SUCCESS)
    })?;
    for line in closing_lines(&report.wired) {
        println!("{line}");
    }
    Ok(if report.problems > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn uninstall(home: &Home, purge: bool) -> anyhow::Result<ExitCode> {
    let runner = SystemRunner::from_process_env();
    let wiring = wiring_setup(home, &runner)?;
    let report = wiring.uninstall()?;
    for line in &report.lines {
        println!("{line}");
    }
    if block_on(stop_daemon(home))? {
        println!("Stopped the spew mesh daemon.");
    }
    if purge {
        purge_home(home)?;
        println!("Deleted {}.", home.root().display());
    }
    Ok(if report.problems > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

// Deletes the home directory, refusing paths that can't be a spew mesh home.
fn purge_home(home: &Home) -> anyhow::Result<()> {
    let root = home.root();
    let user_home = user_home_from_env();
    if root.parent().is_none() || user_home.as_deref() == Some(root) {
        bail!("refusing to delete {}", root.display());
    }
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("can't delete {}", root.display())),
    }
}

async fn login(home: &Home) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    print_login(&sign_in(&client).await?);
    Ok(ExitCode::SUCCESS)
}

async fn logout(home: &Home) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let r = client.logout().await?;
    if !r.was_signed_in {
        println!("This device wasn't signed in to spew.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("Signed out of spew.");
    if !r.device_unregistered {
        println!(
            "spew couldn't be told to unregister this device; `spew mesh devices revoke` from another device removes it."
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn who(
    home: &Home,
    filter: Option<String>,
    offline: bool,
    links: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let token = terminal(&client).await?;
    let dir = client
        .directory(&DirectoryQuery {
            token: Some(token),
            filter,
            include_offline: offline,
        })
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&dir)?);
    } else {
        print!("{}", output::who(&dir, now_ms(), links));
    }
    Ok(ExitCode::SUCCESS)
}

fn send_code(status: SendStatus) -> ExitCode {
    match status {
        SendStatus::Refused | SendStatus::Expired => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

async fn ask(
    home: &Home,
    to: String,
    text: String,
    wait: Option<u64>,
    card: Option<String>,
) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let token = terminal(&client).await?;
    let r = client
        .ask(&AskParams {
            token,
            to,
            text,
            card,
            wait_seconds: wait,
        })
        .await?;
    print!("{}", output::send_result(&r));
    Ok(send_code(r.status))
}

async fn tell(
    home: &Home,
    to: String,
    text: String,
    card: Option<String>,
) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let token = terminal(&client).await?;
    let r = client
        .tell(&TellParams {
            token,
            to,
            text,
            card,
        })
        .await?;
    print!("{}", output::send_result(&r));
    Ok(send_code(r.status))
}

async fn reply(home: &Home, id: String, text: String) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let token = terminal(&client).await?;
    let r = client
        .reply(&ReplyParams {
            token,
            id,
            text,
            wait_seconds: None,
        })
        .await?;
    print!("{}", output::send_result(&r));
    Ok(send_code(r.status))
}

async fn messages(home: &Home, wait: Option<u64>) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    let token = terminal(&client).await?;
    let f = client
        .fetch(&FetchParams {
            token,
            wait_seconds: wait,
            keep_unread: false,
        })
        .await?;
    print!("{}", output::messages(&f));
    Ok(ExitCode::SUCCESS)
}

async fn devices(home: &Home, command: Option<DevicesCommand>) -> anyhow::Result<ExitCode> {
    let client = connect(home).await?;
    match command {
        None => {
            let list = client.devices().await?;
            print!("{}", output::devices(&list.devices, now_ms()));
        }
        Some(DevicesCommand::Revoke { name }) => {
            let r = client.revoke(&name).await?;
            println!(
                "Revoked {} ({}); it can no longer reach your devices or teammates.",
                r.name,
                r.endpoint_id.get(..10).unwrap_or(&r.endpoint_id)
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn doctor(home: &Home) -> anyhow::Result<ExitCode> {
    let mut fixes = 0usize;
    println!("spew mesh {}", crate::control::client::own_version());
    println!("home: {}", home.root().display());
    let user_home = user_home_from_env();
    match Settings::load(home, user_home.as_deref()) {
        Ok(_) => println!("settings: {}", home.settings_file().display()),
        Err(e) => {
            fixes += 1;
            println!("fix: {e}");
        }
    }
    match DeviceIdentity::load(home) {
        Ok(Some(id)) => println!(
            "identity: {} ({}), {}",
            id.name(),
            id.short_id(),
            home.device_file().display()
        ),
        Ok(None) => println!("identity: none yet; the daemon creates it on its first start"),
        Err(e) => {
            fixes += 1;
            println!("fix: {e}");
        }
    }
    match Client::connect(home, &ConnectOptions::existing()).await {
        Ok(client) => {
            let status = client.status().await?;
            print!("{}", output::status_details(&status, now_ms()));
            if status.version != crate::control::client::own_version() {
                fixes += 1;
                println!(
                    "fix: the daemon runs version {} and this is {}; `spew mesh daemon stop` and any command starts the new one",
                    status.version,
                    crate::control::client::own_version()
                );
            }
            if status.roster.problem.is_some() {
                fixes += 1;
            }
        }
        Err(ClientError::NotRunning) => {
            println!("daemon: not running; any spew mesh command or agent session starts it");
        }
        Err(e) => {
            fixes += 1;
            println!("fix: {e}");
        }
    }
    let runner = SystemRunner::from_process_env();
    match wiring_setup(home, &runner).and_then(|s| s.doctor()) {
        Ok(checks) => {
            for check in checks {
                let mark = if check.ok { "ok" } else { "fix" };
                if !check.ok {
                    fixes += 1;
                }
                println!("{} {mark}: {}", check.client.display(), check.text);
            }
        }
        Err(e) => {
            fixes += 1;
            println!("fix: can't check the agents: {e:#}");
        }
    }
    if fixes == 0 {
        println!("Nothing to fix.");
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

fn daemon(home: &Home, command: DaemonCommand) -> anyhow::Result<ExitCode> {
    match command {
        DaemonCommand::Run => daemon_run(home),
        DaemonCommand::Start => block_on(async {
            let client = connect(home).await?;
            let status = client.status().await?;
            println!("{}", output::status_line(&status));
            Ok(ExitCode::SUCCESS)
        }),
        DaemonCommand::Stop => block_on(async {
            if stop_daemon(home).await? {
                println!("Stopped the spew mesh daemon.");
            } else {
                println!("The spew mesh daemon isn't running.");
            }
            Ok(ExitCode::SUCCESS)
        }),
        DaemonCommand::Status => block_on(async {
            match Client::connect(home, &ConnectOptions::existing()).await {
                Ok(client) => {
                    let status = client.status().await?;
                    print!("{}", output::status_details(&status, now_ms()));
                    Ok(ExitCode::SUCCESS)
                }
                Err(ClientError::NotRunning) => {
                    println!("The spew mesh daemon isn't running.");
                    Ok(ExitCode::from(3))
                }
                Err(e) => Err(e.into()),
            }
        }),
    }
}

fn init_logging(env: &Env) {
    let filter = tracing_subscriber::EnvFilter::try_new(env.log.as_deref().unwrap_or("info"))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn daemon_run(home: &Home) -> anyhow::Result<ExitCode> {
    let env = Env::from_process()?;
    init_logging(&env);
    let opts = DaemonOptions {
        home: home.clone(),
        user_home: user_home_from_env(),
        env,
        runtime_dirs: crate::control::place::runtime_dirs_from_env(),
        clock: Arc::new(SystemClock),
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("can't start the async runtime")?
        .block_on(crate::daemon::run(opts))?;
    Ok(ExitCode::SUCCESS)
}

fn identity(home: &Home) -> anyhow::Result<ExitCode> {
    let identity = match DeviceIdentity::load(home)? {
        Some(identity) => identity,
        None => {
            // The daemon creates the identity on its first start.
            block_on(connect(home))?;
            DeviceIdentity::load(home)?.context("the daemon started but wrote no identity")?
        }
    };
    println!("{}", identity.name());
    println!("endpoint id: {}", identity.endpoint_id_hex());
    println!("short id: {}", identity.short_id());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_command_line_parses() {
        Cli::command_check();
        let cli = Cli::try_parse_from([
            "spew",
            "mesh",
            "--home",
            "/srv/mesh",
            "ask",
            "maya",
            "how",
            "now",
        ])
        .unwrap();
        let Top::Mesh(m) = cli.command;
        assert_eq!(m.home.as_deref(), Some(Path::new("/srv/mesh")));
        assert!(
            matches!(m.command, Some(Command::Ask { ref to, ref text, .. }) if to == "maya" && text.len() == 2)
        );
        let cli = Cli::try_parse_from([
            "spew",
            "mesh",
            "serve",
            "codex",
            "--name",
            "web-help",
            "--allow-edits",
        ])
        .unwrap();
        let Top::Mesh(m) = cli.command;
        assert!(
            matches!(m.command, Some(Command::Serve(ref s)) if s.name.as_deref() == Some("web-help") && s.allow_edits)
        );
        assert!(Cli::try_parse_from(["spew", "mesh", "hook", "claude", "SessionStart"]).is_ok());
        assert!(
            Cli::try_parse_from(["spew", "mesh", "daemon", "status", "--home", "/srv/mesh"])
                .is_ok()
        );
    }

    impl Cli {
        fn command_check() {
            use clap::CommandFactory;
            Cli::command().debug_assert();
        }
    }
}
