//! `spew mesh setup`, `uninstall` and the agent part of `doctor` (8, 10, 11).
//!
//! Every config edit follows the safety rules of 8: one-time 0600 backups, atomic writes with fsync, a re-check
//! against concurrent writers, unparseable files left alone, and a manifest of exactly what was changed.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Client;
use super::claude::{self, MCP_ID};
use super::codex::{self, TRUST_LINE};
use super::fsx::{self, BeforeRename, Written, read_stamped, sh_quote};
use super::json::{Json, Parsed, parse_config};
use super::manifest::{Entry, Manifest, Op};
use super::opencode;
use super::plan::{self, FilePlan, Format};
use super::runner::{Found, Runner, detect, min_version, too_old_line};
use super::skill::SKILL_MD;

/// What setup writes into agent configs to run spew: the binary's absolute path, and `--home` when the home
/// isn't the default (10), because agents don't always pass the environment on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wiring {
    spew: String,
    home: Option<String>,
}

impl Wiring {
    /// `spew` must be absolute (configs name the binary by its absolute path, 8); so must `home`.
    pub fn new(spew: PathBuf, home: Option<PathBuf>) -> Result<Wiring> {
        let text = |p: PathBuf, what: &str| -> Result<String> {
            if !p.is_absolute() {
                bail!("the {what} path {} isn't absolute", p.display());
            }
            match p.into_os_string().into_string() {
                Ok(s) if !s.contains(['\n', '\r', '\0']) => Ok(s),
                _ => bail!(
                    "the {what} path can't be written into agent configs (not plain UTF-8 text)"
                ),
            }
        };
        Ok(Wiring {
            spew: text(spew, "spew binary")?,
            home: home.map(|h| text(h, "home")).transpose()?,
        })
    }

    /// The binary's path.
    pub fn spew_str(&self) -> String {
        self.spew.clone()
    }

    /// `mesh [--home DIR] <tail...>`.
    pub fn args(&self, tail: &[&str]) -> Vec<String> {
        let mut v = vec!["mesh".to_string()];
        if let Some(h) = &self.home {
            v.push("--home".into());
            v.push(h.clone());
        }
        v.extend(tail.iter().map(|s| s.to_string()));
        v
    }

    /// The same as one shell command, with the paths single-quoted (8).
    pub fn shell_command(&self, tail: &[&str]) -> String {
        let mut s = sh_quote(&self.spew);
        s.push_str(" mesh");
        if let Some(h) = &self.home {
            s.push_str(" --home ");
            s.push_str(&sh_quote(h));
        }
        for t in tail {
            s.push(' ');
            s.push_str(t);
        }
        s
    }
}

/// Where each client keeps its config (8.2 to 8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientPaths {
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_dir: PathBuf,
    /// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
    pub claude_json: PathBuf,
    /// `$CODEX_HOME`, else `~/.codex`.
    pub codex_dir: PathBuf,
    /// `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`.
    pub opencode_dir: PathBuf,
    /// `claude_dir` is `~/.claude`, the only place OpenCode reads Claude Code's skills from (8.4).
    pub claude_dir_is_default: bool,
}

impl ClientPaths {
    /// Computes the folders from the person's home folder and an environment lookup.
    pub fn from_env(
        user_home: &Path,
        var: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<ClientPaths> {
        let set = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let absolute = |k: &str| -> Result<Option<PathBuf>> {
            match set(k) {
                Some(p) if !p.is_absolute() => bail!(
                    "{k} is set to a relative path ({}); use an absolute one",
                    p.display()
                ),
                other => Ok(other),
            }
        };
        let (claude_dir, claude_json) = match absolute("CLAUDE_CONFIG_DIR")? {
            Some(d) => (d.clone(), d.join(".claude.json")),
            None => (user_home.join(".claude"), user_home.join(".claude.json")),
        };
        let claude_dir_is_default = claude_dir == user_home.join(".claude");
        let codex_dir = absolute("CODEX_HOME")?.unwrap_or_else(|| user_home.join(".codex"));
        // The XDG spec says to ignore a relative value.
        let config = set("XDG_CONFIG_HOME")
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| user_home.join(".config"));
        Ok(ClientPaths {
            claude_dir,
            claude_json,
            codex_dir,
            opencode_dir: config.join("opencode"),
            claude_dir_is_default,
        })
    }

    /// The client's config folder.
    pub fn dir(&self, client: Client) -> &Path {
        match client {
            Client::Claude => &self.claude_dir,
            Client::Codex => &self.codex_dir,
            Client::Opencode => &self.opencode_dir,
        }
    }
}

/// Options of `spew mesh setup` that concern the agents.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `--agents`; `None` means every client found.
    pub agents: Option<Vec<Client>>,
    /// `--dry-run`: print the plan, change nothing.
    pub dry_run: bool,
}

/// What a command did, as lines for the person.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub lines: Vec<String>,
    /// Clients that were wired (or would be, in a dry run).
    pub wired: Vec<Client>,
    /// How many things need the person's attention (also among the lines).
    pub problems: usize,
}

impl Report {
    fn line(&mut self, s: impl Into<String>) {
        self.lines.push(s.into());
    }

    fn problem(&mut self, s: impl Into<String>) {
        self.problems += 1;
        self.lines.push(s.into());
    }
}

/// One doctor finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub client: Client,
    pub ok: bool,
    pub text: String,
}

/// The lines setup ends with (10), after starting the daemon and signing in.
pub fn closing_lines(wired: &[Client]) -> Vec<String> {
    let mut v = vec![
        "Restart any open Claude Code, Codex or OpenCode session to pick this up.".to_string(),
    ];
    if wired.contains(&Client::Codex) {
        v.push(TRUST_LINE.to_string());
    }
    v
}

enum Step {
    File(FilePlan),
    ClaudeCli(PathBuf),
}

/// Runs setup, uninstall and doctor against one set of folders.
pub struct Setup<'a> {
    pub wiring: Wiring,
    pub paths: ClientPaths,
    /// `<home>/agents/manifest.json`.
    pub manifest_path: PathBuf,
    pub runner: &'a dyn Runner,
}

impl Setup<'_> {
    /// Wires each client it finds (its command on PATH or its config folder present), limited by `--agents`.
    pub fn setup(&self, opts: &Options) -> Result<Report> {
        self.setup_with(opts, &mut |_, _| {})
    }

    /// [`Setup::setup`] with a hook called before each rename (tests play a concurrent writer with it).
    pub fn setup_with(&self, opts: &Options, before_rename: BeforeRename) -> Result<Report> {
        let mut manifest = Manifest::load(&self.manifest_path)?;
        let mut report = Report::default();
        let clients: Vec<Client> = Client::ALL
            .into_iter()
            .filter(|c| opts.agents.as_ref().is_none_or(|a| a.contains(c)))
            .collect();
        for client in clients {
            let found = detect(self.runner, client);
            let folder = self.paths.dir(client).is_dir();
            match &found {
                Found::Ok { version, .. } => report.line(format!("{client} {version}")),
                Found::TooOld { version, .. } => {
                    report.problem(too_old_line(client, *version));
                    continue;
                }
                Found::Unreadable { why, .. } => {
                    report.problem(format!("{client}: couldn't read its version ({why}); skipped."));
                    continue;
                }
                Found::Missing if folder => report.line(format!(
                    "{client} (`{}` isn't on PATH, so its version wasn't checked; it needs {} or newer)",
                    client.slug(),
                    min_version(client)
                )),
                Found::Missing => {
                    report.line(format!("{client}: not found; skipped."));
                    continue;
                }
            }
            report.wired.push(client);
            let claude_skill =
                report.wired.contains(&Client::Claude) || self.claude_skill_present();
            let before = report.lines.len();
            for step in self.steps(client, &found, claude_skill) {
                match step {
                    Step::File(p) => {
                        self.run_file(&p, &mut manifest, opts.dry_run, before_rename, &mut report)
                    }
                    Step::ClaudeCli(cmd) => {
                        self.run_claude_cli(&cmd, &mut manifest, opts.dry_run, &mut report)
                    }
                }
            }
            if report.lines.len() == before {
                report.line("  already wired; nothing to change");
            }
        }
        Ok(report)
    }

    fn claude_skill_present(&self) -> bool {
        let file = claude::skill_plan(&self.paths).file;
        fs::read_to_string(file).is_ok_and(|t| t == SKILL_MD)
    }

    fn steps(&self, client: Client, found: &Found, claude_skill: bool) -> Vec<Step> {
        let (w, p) = (&self.wiring, &self.paths);
        match client {
            Client::Claude => vec![
                Step::File(claude::settings_plan(w, p)),
                match found.path() {
                    Some(cmd) => Step::ClaudeCli(cmd.to_path_buf()),
                    None => Step::File(claude::mcp_plan(w, p)),
                },
                Step::File(claude::skill_plan(p)),
            ],
            Client::Codex => vec![
                Step::File(codex::config_plan(w, p)),
                Step::File(codex::hooks_plan(w, p)),
            ],
            Client::Opencode => {
                let mut v = vec![
                    Step::File(opencode::config_plan(w, p)),
                    Step::File(opencode::plugin_plan(w, p)),
                ];
                // OpenCode also reads Claude Code's skills, but only from ~/.claude (8.4).
                if !(claude_skill && p.claude_dir_is_default) {
                    v.push(Step::File(opencode::skill_plan(p)));
                }
                v
            }
        }
    }

    fn run_file(
        &self,
        plan: &FilePlan,
        manifest: &mut Manifest,
        dry_run: bool,
        before_rename: BeforeRename,
        report: &mut Report,
    ) {
        let shown = plan.file.display().to_string();
        if dry_run {
            let res = read_stamped(&plan.file)
                .and_then(|(cur, _)| plan::apply(plan, manifest, cur.as_deref()));
            match res {
                Ok((text, out)) => {
                    if text.is_some() && !out.changes.is_empty() {
                        let what: Vec<String> = out.changes.iter().map(|c| c.planned()).collect();
                        report.line(format!("  would change {shown}: {}", what.join(", ")));
                    }
                    for n in out.notes {
                        report.problem(format!("  {n}"));
                    }
                }
                Err(e) => report.problem(format!("  can't read {shown}: {e:#}")),
            }
            return;
        }
        let snapshot = manifest.clone();
        let mut recorded = false;
        let res = fsx::edit_file(
            &plan.file,
            true,
            &mut |cur| plan::apply(plan, &snapshot, cur),
            // Recorded right before the rename, so a change is never on disk without its entry. When the rename
            // then doesn't happen, the entry's value is simply absent, which uninstall skips.
            &mut |out: &plan::Outcome, written: &Written| {
                let mut next = snapshot.clone();
                let mut entries = out.entries.clone();
                plan::finish_entries(&mut entries, written);
                for e in entries {
                    next.upsert(e);
                }
                next.save(&self.manifest_path)
                    .with_context(|| format!("recording it in {}", self.manifest_path.display()))?;
                recorded = true;
                *manifest = next;
                Ok(())
            },
            before_rename,
        );
        match res {
            Err(e) => {
                if recorded {
                    // Best effort: drop the entries of the edit that didn't happen.
                    *manifest = snapshot.clone();
                    let _ = manifest.save(&self.manifest_path);
                }
                report.problem(format!("  couldn't change {shown}: {e:#}"));
            }
            Ok((out, written)) => {
                if written.is_some() {
                    let what: Vec<String> = out.changes.iter().map(|c| c.past()).collect();
                    report.line(format!("  changed {shown}: {}", what.join(", ")));
                }
                for n in out.notes {
                    report.problem(format!("  {n}"));
                }
            }
        }
    }

    /// Adds the MCP server with `claude mcp add-json` (8.2) unless `.claude.json` already has it.
    fn run_claude_cli(
        &self,
        claude_cmd: &Path,
        manifest: &mut Manifest,
        dry_run: bool,
        report: &mut Report,
    ) {
        let file = &self.paths.claude_json;
        let shown = file.display();
        let path = vec!["mcpServers".to_string(), "mesh".to_string()];
        let def = claude::mcp_definition(&self.wiring);
        let doc = match read_stamped(file) {
            Err(e) => return report.problem(format!("  can't read {shown}: {e:#}")),
            Ok((None, _)) => Json::object(),
            Ok((Some(text), _)) => match parse_config(&text) {
                Parsed::Plain(v) | Parsed::Jsonc(v) => v,
                Parsed::Invalid(e) => {
                    return report.problem(format!("  {shown} doesn't parse ({e}); left it alone"));
                }
            },
        };
        let recorded = manifest
            .find(MCP_ID, file)
            .map(|e| Json::from_value(&e.value));
        let update = match doc.at(&path) {
            Some(cur) if def.is_subset_of(cur) => return,
            Some(cur) if recorded.as_ref() == Some(cur) => true,
            Some(_) => {
                return report.problem(format!(
                    "  {shown} already has an MCP server named mesh that setup didn't add; left it alone"
                ));
            }
            None => false,
        };
        let add = claude::cli_add_args(&self.wiring);
        if dry_run {
            let mut shell: Vec<String> = add.iter().map(|a| quote_if_needed(a)).collect();
            shell.insert(0, "claude".into());
            let prefix = if update {
                "claude mcp remove --scope user mesh && "
            } else {
                ""
            };
            report.line(format!("  would run: {prefix}{}", shell.join(" ")));
            return;
        }
        let backup = match fsx::backup_once(file) {
            Ok(b) => b,
            Err(e) => return report.problem(format!("  couldn't back up {shown}: {e:#}")),
        };
        let entry = |value: &Json| Entry {
            id: MCP_ID.to_string(),
            client: Client::Claude,
            file: file.clone(),
            target: fsx::edit_target(file).ok().filter(|t| t != file),
            op: Op::ClaudeCli,
            path: path.clone(),
            value: value.to_value(),
            created: Vec::new(),
            created_file: false,
            created_dirs: Vec::new(),
            backup: backup.as_ref().map(|b| b.path.clone()),
        };
        // Recorded before the CLI runs, as with file edits, then updated with what the CLI stored.
        let snapshot = manifest.clone();
        manifest.upsert(entry(&def));
        if let Err(e) = manifest.save(&self.manifest_path) {
            *manifest = snapshot;
            backup.iter().for_each(fsx::Backup::discard);
            return report.problem(format!(
                "  couldn't record the change in {}, so setup didn't run claude mcp add-json: {e:#}",
                self.manifest_path.display()
            ));
        }
        let failed = |manifest: &mut Manifest, report: &mut Report, what: String| {
            *manifest = snapshot.clone();
            let _ = manifest.save(&self.manifest_path);
            backup.iter().for_each(fsx::Backup::discard);
            report.problem(what);
        };
        if update {
            match self.runner.run(claude_cmd, &claude::cli_remove_args()) {
                Ok(out) if out.success() => {}
                Ok(out) => {
                    let why = out.first_error_line();
                    return failed(
                        manifest,
                        report,
                        format!("  claude mcp remove failed: {why}"),
                    );
                }
                Err(e) => {
                    return failed(
                        manifest,
                        report,
                        format!("  claude mcp remove failed: {e:#}"),
                    );
                }
            }
        }
        match self.runner.run(claude_cmd, &add) {
            Ok(out) if out.success() => {}
            Ok(out) => {
                let why = out.first_error_line();
                return failed(
                    manifest,
                    report,
                    format!("  claude mcp add-json failed: {why}"),
                );
            }
            Err(e) => {
                return failed(
                    manifest,
                    report,
                    format!("  claude mcp add-json failed: {e:#}"),
                );
            }
        }
        // Record what Claude Code actually stored, so uninstall compares against that.
        let stored = read_stamped(file)
            .ok()
            .and_then(|(t, _)| t)
            .and_then(|t| match parse_config(&t) {
                Parsed::Plain(v) | Parsed::Jsonc(v) => v.at(&path).cloned(),
                Parsed::Invalid(_) => None,
            });
        let verb = if update { "updated" } else { "added" };
        report.line(format!(
            "  ran claude mcp add-json: {verb} the mesh MCP server in {shown}"
        ));
        match stored {
            None => report.problem(format!(
                "  claude mcp add-json succeeded, but {shown} doesn't show the server; doctor checks it"
            )),
            Some(v) if v != def => {
                manifest.upsert(entry(&v));
                if let Err(e) = manifest.save(&self.manifest_path) {
                    report.problem(format!(
                        "  couldn't record the change in {}: {e:#}",
                        self.manifest_path.display()
                    ));
                }
            }
            Some(_) => {}
        }
    }

    /// Reverts exactly the recorded changes whose current value still matches what setup wrote, and lists the
    /// ones it left alone (8).
    pub fn uninstall(&self) -> Result<Report> {
        self.uninstall_with(&mut |_, _| {})
    }

    /// [`Setup::uninstall`] with a hook called before each rename.
    pub fn uninstall_with(&self, before_rename: BeforeRename) -> Result<Report> {
        let mut manifest = Manifest::load(&self.manifest_path)?;
        let mut report = Report::default();
        if manifest.entries.is_empty() {
            report.line("No agent changes to revert.");
            return Ok(report);
        }
        // A linked file is reverted at the target setup edited, which leaves the link in place (8).
        let edited = |e: &Entry| e.target.clone().unwrap_or_else(|| e.file.clone());
        let mut files: Vec<PathBuf> = Vec::new();
        for e in &manifest.entries {
            if !files.contains(&edited(e)) {
                files.push(edited(e));
            }
        }
        let mut keep: Vec<Entry> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for file in files {
            let entries: Vec<Entry> = manifest
                .entries
                .iter()
                .filter(|e| edited(e) == file)
                .cloned()
                .collect();
            let mut rest = Vec::new();
            let mut left = 0;
            for e in entries.iter().cloned() {
                if e.op != Op::ClaudeCli {
                    rest.push(e);
                    continue;
                }
                match self.runner.find(Client::Claude.slug()) {
                    Some(cmd) => match self.uninstall_claude_cli(&cmd, &e, &mut report) {
                        Ok(true) => {}
                        Ok(false) => left += 1,
                        Err(()) => keep.push(e),
                    },
                    None => rest.push(e),
                }
            }
            if !rest.is_empty() {
                match self.revert_file(&file, &rest, before_rename, &mut dirs, &mut report) {
                    Ok(n) => left += n,
                    Err(()) => {
                        keep.extend(rest);
                        continue;
                    }
                }
            }
            if left == 0 {
                let backups: BTreeSet<&PathBuf> =
                    entries.iter().filter_map(|e| e.backup.as_ref()).collect();
                for b in backups {
                    let _ = fs::remove_file(b);
                }
            }
        }
        // Folders last, once every file in them is gone.
        fsx::remove_empty_dirs(&dirs);
        manifest.entries = keep;
        manifest.save(&self.manifest_path)?;
        if report.lines.is_empty() {
            report.line("Nothing left to revert.");
        }
        Ok(report)
    }

    /// Reverts one file's entries; returns how many it left alone, or `Err` when the file couldn't be handled.
    fn revert_file(
        &self,
        file: &Path,
        entries: &[Entry],
        before_rename: BeforeRename,
        dirs: &mut Vec<PathBuf>,
        report: &mut Report,
    ) -> Result<usize, ()> {
        let shown = match &entries[0].target {
            Some(t) => format!("{} (linked to {})", entries[0].file.display(), t.display()),
            None => file.display().to_string(),
        };
        let format = match entries[0].op {
            Op::TomlSet => Format::Toml,
            Op::File => Format::Text,
            _ => Format::Json,
        };
        let refs: Vec<&Entry> = entries.iter().collect();
        let res = fsx::edit_file(
            file,
            false,
            &mut |cur| Ok(plan::revert(format, &refs, cur)),
            &mut |_, _| Ok(()),
            before_rename,
        );
        let (rev, written) = match res {
            Ok(r) => r,
            Err(e) => {
                report.problem(format!("couldn't revert {shown}: {e:#}"));
                return Err(());
            }
        };
        let mut removed = false;
        if rev.delete && fsx::is_link(file) {
            report.problem(format!(
                "left {shown} alone: it is a link now, so uninstall deletes neither it nor the file it points to"
            ));
        } else if rev.delete {
            // Re-read: delete only what is still exactly the reverted (or setup-written) text.
            if let Ok((Some(text), _)) = read_stamped(file)
                && plan::revert(format, &refs, Some(&text)).1.delete
            {
                match fsx::remove_if_unchanged(file, &text) {
                    Ok(true) => removed = true,
                    Ok(false) => {}
                    Err(e) => report.problem(format!("couldn't remove {shown}: {e:#}")),
                }
            }
            if removed {
                dirs.extend(entries.iter().flat_map(|e| e.created_dirs.iter().cloned()));
            }
        }
        if removed {
            report.line(format!("removed {shown}"));
        } else if written.is_some() {
            report.line(format!("reverted {shown}"));
        }
        for (id, why) in &rev.left {
            report.problem(format!("left {id} in {shown} alone: {why}"));
        }
        Ok(rev.left.len())
    }

    /// `claude mcp remove --scope user mesh`, when the server is still what setup added. `Ok(false)`: left alone.
    fn uninstall_claude_cli(&self, cmd: &Path, e: &Entry, report: &mut Report) -> Result<bool, ()> {
        let shown = e.file.display();
        let current = match read_stamped(&e.file) {
            Ok((Some(t), _)) => match parse_config(&t) {
                Parsed::Plain(v) | Parsed::Jsonc(v) => v.at(&e.path).cloned(),
                Parsed::Invalid(_) => {
                    report.problem(format!(
                        "left {} in {shown} alone: the file doesn't parse",
                        e.id
                    ));
                    return Ok(false);
                }
            },
            Ok((None, _)) => None,
            Err(err) => {
                report.problem(format!("couldn't read {shown}: {err:#}"));
                return Err(());
            }
        };
        match current {
            None => Ok(true),
            Some(cur) if cur == Json::from_value(&e.value) => {
                match self.runner.run(cmd, &claude::cli_remove_args()) {
                    Ok(out) if out.success() => {
                        report.line(format!(
                            "ran claude mcp remove: removed the mesh MCP server from {shown}"
                        ));
                        Ok(true)
                    }
                    Ok(out) => {
                        report.problem(format!(
                            "claude mcp remove failed: {}",
                            out.first_error_line()
                        ));
                        Err(())
                    }
                    Err(err) => {
                        report.problem(format!("claude mcp remove failed: {err:#}"));
                        Err(())
                    }
                }
            }
            Some(_) => {
                report.problem(format!(
                    "left {} in {shown} alone: it changed since setup added it",
                    e.id
                ));
                Ok(false)
            }
        }
    }

    /// The agent part of `spew mesh doctor`: versions, what is wired, and what to fix.
    pub fn doctor(&self) -> Result<Vec<Check>> {
        let manifest = Manifest::load(&self.manifest_path)?;
        let mut checks = Vec::new();
        let claude_skill = self.claude_skill_present();
        for client in Client::ALL {
            let mut add = |ok: bool, text: String| checks.push(Check { client, ok, text });
            let found = detect(self.runner, client);
            let folder = self.paths.dir(client).is_dir();
            match &found {
                Found::Ok { version, path } => {
                    add(true, format!("{client} {version} ({})", path.display()))
                }
                Found::TooOld { version, .. } => {
                    add(false, too_old_line(client, *version));
                    continue;
                }
                Found::Unreadable { why, .. } => {
                    add(
                        false,
                        format!("{client}: couldn't read its version ({why})"),
                    );
                    continue;
                }
                Found::Missing if folder => add(
                    true,
                    format!(
                        "{client}: `{}` isn't on PATH; it needs {} or newer",
                        client.slug(),
                        min_version(client)
                    ),
                ),
                Found::Missing => {
                    add(true, format!("{client}: not found"));
                    continue;
                }
            }
            let mut plans: Vec<FilePlan> = Vec::new();
            for step in self.steps(client, &found, claude_skill) {
                match step {
                    Step::File(p) => plans.push(p),
                    Step::ClaudeCli(_) => {
                        let mut p = claude::mcp_plan(&self.wiring, &self.paths);
                        // The manifest records which .claude.json setup used (8.2).
                        if let Some(e) =
                            manifest.for_client(Client::Claude).find(|e| e.id == MCP_ID)
                        {
                            p.file = e.file.clone();
                        }
                        plans.push(p);
                    }
                }
            }
            for p in &plans {
                let shown = p.file.display();
                let current = match read_stamped(&p.file) {
                    Ok((t, _)) => t,
                    Err(e) => {
                        add(false, format!("can't read {shown}: {e:#}"));
                        continue;
                    }
                };
                match plan::missing(p, current.as_deref()) {
                    Err(e) => add(false, format!("{e}; setup leaves it alone until it parses")),
                    Ok(m) if m.is_empty() => add(true, format!("{shown}: wired")),
                    Ok(m) => {
                        let labels: Vec<&str> = m.iter().map(|w| w.label.as_str()).collect();
                        let mut text = format!("{shown}: missing {}", labels.join(", "));
                        if p.never_write
                            || current
                                .as_deref()
                                .is_some_and(|t| matches!(parse_config(t), Parsed::Jsonc(_)))
                        {
                            text.push_str(&format!(
                                "; add this to it yourself:\n{}",
                                plan::block_for(&m)
                            ));
                        } else {
                            text.push_str("; run spew mesh setup");
                        }
                        add(false, text);
                    }
                }
                if let Some(t) = current.as_deref() {
                    for note in config_warnings(client, p, t) {
                        add(false, note);
                    }
                }
            }
            if client == Client::Codex {
                add(true, TRUST_LINE.to_string());
            }
        }
        Ok(checks)
    }
}

/// Settings that keep the wiring from working even when it is all there.
fn config_warnings(client: Client, plan: &FilePlan, text: &str) -> Vec<String> {
    let (Parsed::Plain(doc) | Parsed::Jsonc(doc)) = parse_config(text) else {
        return Vec::new();
    };
    let shown = plan.file.display();
    let mut out = Vec::new();
    match client {
        Client::Claude if plan.file.ends_with("settings.json") => {
            if doc.get("disableAllHooks") == Some(&Json::Bool(true)) {
                out.push(format!(
                    "{shown} sets disableAllHooks, so the spew mesh hooks don't run"
                ));
            }
        }
        Client::Opencode if plan.wants.iter().any(|w| w.id == "opencode.mcp") => {
            if let Some(p) = doc.get("permission")
                && !p.is_object()
            {
                out.push(format!(
                    "{shown}: \"permission\" is a single value, so OpenCode won't ask before mesh_ask, mesh_tell \
                     or mesh_reply send. Make it an object to have it ask."
                ));
            }
        }
        _ => {}
    }
    out
}

fn quote_if_needed(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@".contains(&b))
    {
        s.to_string()
    } else {
        sh_quote(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_paths_follow_the_environment() {
        let home = Path::new("/home/sam");
        let none = |_: &str| None;
        let p = ClientPaths::from_env(home, &none).unwrap();
        assert_eq!(p.claude_dir, Path::new("/home/sam/.claude"));
        assert_eq!(p.claude_json, Path::new("/home/sam/.claude.json"));
        assert_eq!(p.codex_dir, Path::new("/home/sam/.codex"));
        assert_eq!(p.opencode_dir, Path::new("/home/sam/.config/opencode"));

        let env = |k: &str| match k {
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/work/claude")),
            "CODEX_HOME" => Some(OsString::from("/work/codex")),
            "XDG_CONFIG_HOME" => Some(OsString::from("relative")),
            _ => None,
        };
        let p = ClientPaths::from_env(home, &env).unwrap();
        assert_eq!(
            p.claude_json,
            Path::new("/work/claude/.claude.json"),
            "follows CLAUDE_CONFIG_DIR"
        );
        assert_eq!(p.codex_dir, Path::new("/work/codex"));
        assert_eq!(p.opencode_dir, Path::new("/home/sam/.config/opencode"));

        let bad = |k: &str| (k == "CODEX_HOME").then(|| OsString::from("codex"));
        assert!(ClientPaths::from_env(home, &bad).is_err());
    }

    #[test]
    fn wiring_quotes_and_validates() {
        assert!(Wiring::new(PathBuf::from("spew"), None).is_err());
        assert!(Wiring::new(PathBuf::from("/a/spew"), Some(PathBuf::from("rel"))).is_err());
        let w = Wiring::new(PathBuf::from("/a b/spew"), Some(PathBuf::from("/h/it's"))).unwrap();
        assert_eq!(
            w.args(&["mcp-server"]),
            vec!["mesh", "--home", "/h/it's", "mcp-server"]
        );
        assert_eq!(
            w.shell_command(&["hook", "codex", "Stop"]),
            "'/a b/spew' mesh --home '/h/it'\\''s' hook codex Stop"
        );
        assert_eq!(closing_lines(&[Client::Claude]).len(), 1);
        assert_eq!(closing_lines(&[Client::Codex])[1], TRUST_LINE);
        assert_eq!(quote_if_needed("add-json"), "add-json");
        assert_eq!(quote_if_needed("{\"a\":1}"), "'{\"a\":1}'");
    }
}
