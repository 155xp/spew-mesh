//! End-to-end tests of setup, uninstall and doctor against config fixtures in a temporary home, with fake
//! `claude`, `codex` and `opencode` commands. Nothing here touches the real home or real agent configs.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::Client;
use super::codex::TRUST_LINE;
use super::fsx::backup_path;
use super::json::{Json, Parsed, Style, parse_config};
use super::manifest::{Manifest, manifest_path};
use super::runner::SystemRunner;
use super::runner::testing::{calls, fake_command};
use super::setup::{ClientPaths, Options, Report, Setup, Wiring};
use super::skill::SKILL_MD;

const CLAUDE_SETTINGS: &str = r#"{
  "model": "opus",
  "permissions": {
    "allow": [
      "Bash(npm test:*)"
    ],
    "deny": [
      "Read(./.env)"
    ]
  },
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "npx prettier --write \"$FILE\""
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "notify-send done"
          }
        ]
      }
    ]
  }
}
"#;

const CLAUDE_JSON: &str = r#"{
  "numStartups": 12,
  "projects": {
    "/home/sam/api": {
      "hasTrustDialogAccepted": true
    }
  },
  "mcpServers": {
    "github": {
      "type": "stdio",
      "command": "github-mcp",
      "env": {
        "GITHUB_TOKEN": "placeholder"
      }
    }
  }
}"#;

const CODEX_CONFIG: &str = r#"# Codex settings for sam
model = "some-model"
approval_policy = "on-request"

[mcp_servers.github]
command = "github-mcp" # the GitHub server
args = ["--read-only"]

[features]
hooks = true
"#;

const CODEX_HOOKS: &str = r#"{
  "description": "sam's hooks",
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo hello",
            "timeout": 3
          }
        ]
      }
    ]
  }
}
"#;

const OPENCODE_JSON: &str = r#"{
	"$schema": "https://opencode.ai/config.json",
	"theme": "tokyonight",
	"mcp": {
		"docs": {
			"type": "remote",
			"url": "https://docs.example.com/mcp"
		}
	},
	"permission": {
		"bash": "ask"
	}
}
"#;

struct World {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    mesh: PathBuf,
    paths: ClientPaths,
    runner: SystemRunner,
    wiring: Wiring,
}

impl World {
    /// A home with configs for all three clients and the given fake commands on PATH (name, `--version` output).
    fn new(commands: &[(&str, &str)]) -> World {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home/sam");
        let bin = root.path().join("bin");
        let mesh = root.path().join("mesh");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let paths = ClientPaths::from_env(&home, &|_| None).unwrap();
        for (name, version) in commands {
            fake_command(
                &bin,
                name,
                &format!("[ \"$1\" = --version ] && echo '{version}'\nexit 0"),
            );
        }
        let runner =
            SystemRunner::new(bin.as_os_str(), Duration::from_secs(20)).with_env("HOME", &home);
        let wiring = Wiring::new(PathBuf::from("/opt/spew/bin/spew"), None).unwrap();
        World {
            _root: root,
            home,
            bin,
            mesh,
            paths,
            runner,
            wiring,
        }
    }

    /// Every client on PATH with a supported version, except Claude Code (so `.claude.json` is edited directly).
    fn standard() -> World {
        let w = World::new(&[("codex", "codex-cli 0.163.0"), ("opencode", "1.19.3")]);
        w.write(&w.paths.claude_dir.join("settings.json"), CLAUDE_SETTINGS);
        w.write(&w.paths.claude_json, CLAUDE_JSON);
        w.write(&w.paths.codex_dir.join("config.toml"), CODEX_CONFIG);
        w.write(&w.paths.codex_dir.join("hooks.json"), CODEX_HOOKS);
        w.write(&w.paths.opencode_dir.join("opencode.json"), OPENCODE_JSON);
        w
    }

    fn write(&self, path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn read(&self, path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn json(&self, path: &Path) -> Json {
        match parse_config(&self.read(path)) {
            Parsed::Plain(v) => v,
            other => panic!("{}: {other:?}", path.display()),
        }
    }

    fn setup(&self) -> Setup<'_> {
        Setup {
            wiring: self.wiring.clone(),
            paths: self.paths.clone(),
            manifest_path: manifest_path(&self.mesh),
            runner: &self.runner,
        }
    }

    fn run(&self) -> Report {
        self.setup().setup(&Options::default()).unwrap()
    }

    /// Every file and folder under the home and the mesh home: path -> (content, mode, mtime).
    fn snapshot(&self) -> Snapshot {
        let mut out = self.home_snapshot();
        walk(&self.mesh, &mut out);
        out
    }

    /// The same for the person's home only (the mesh home belongs to the CLI, which creates and purges it).
    fn home_snapshot(&self) -> Snapshot {
        let mut out = BTreeMap::new();
        walk(&self.home, &mut out);
        out
    }
}

/// path -> (content, mode, mtime seconds, mtime nanoseconds).
type Snapshot = BTreeMap<PathBuf, (Option<Vec<u8>>, u32, i64, i64)>;

fn walk(dir: &Path, out: &mut Snapshot) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries {
        let p = e.unwrap().path();
        let m = fs::symlink_metadata(&p).unwrap();
        let content = if m.is_file() {
            Some(fs::read(&p).unwrap())
        } else {
            None
        };
        out.insert(
            p.clone(),
            (content, m.mode() & 0o7777, m.mtime(), m.mtime_nsec()),
        );
        if m.is_dir() {
            walk(&p, out);
        }
    }
}

fn contents(s: &Snapshot) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    s.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect()
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().mode() & 0o777
}

fn hooks_of<'a>(doc: &'a Json, event: &str) -> &'a Vec<Json> {
    doc.at(&["hooks".into(), event.into()])
        .and_then(Json::as_array)
        .unwrap()
}

#[test]
fn fixtures_are_in_the_canonical_style() {
    // Uninstall restores byte for byte only for files whose layout a rewrite reproduces; make sure ours do.
    for text in [CLAUDE_SETTINGS, CLAUDE_JSON, CODEX_HOOKS, OPENCODE_JSON] {
        let Parsed::Plain(v) = parse_config(text) else {
            panic!()
        };
        assert_eq!(Style::detect(text).render(&v), text);
    }
}

#[test]
fn setup_wires_all_three_and_keeps_the_persons_entries() {
    let w = World::standard();
    let report = w.run();
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    assert_eq!(
        report.wired,
        vec![Client::Claude, Client::Codex, Client::Opencode]
    );

    // Claude Code settings: the person's hooks first and untouched, ours appended; other keys kept.
    let settings = w.json(&w.paths.claude_dir.join("settings.json"));
    assert_eq!(settings.get("model"), Some(&Json::str("opus")));
    let post = hooks_of(&settings, "PostToolUse");
    assert_eq!(post.len(), 2);
    assert_eq!(post[0].get("matcher"), Some(&Json::str("Edit|Write")));
    assert_eq!(post[1].get("matcher"), Some(&Json::str("*")));
    let stop = hooks_of(&settings, "Stop");
    assert_eq!(
        stop[0].to_compact(),
        r#"{"hooks":[{"type":"command","command":"notify-send done"}]}"#
    );
    assert_eq!(
        stop[1].to_compact(),
        r#"{"hooks":[{"type":"command","command":"/opt/spew/bin/spew","args":["mesh","hook","claude","Stop"],"timeout":8}]}"#
    );
    for event in ["SessionStart", "UserPromptSubmit", "SessionEnd"] {
        assert_eq!(hooks_of(&settings, event).len(), 1, "{event}");
    }
    let allow = settings
        .at(&["permissions".into(), "allow".into()])
        .unwrap();
    assert_eq!(
        allow,
        &Json::strings(&["Bash(npm test:*)", "mcp__mesh__who", "mcp__mesh__messages"])
    );
    let text = w.read(&w.paths.claude_dir.join("settings.json"));
    assert!(
        !text.contains("mcp__mesh__ask")
            && !text.contains("mcp__mesh__tell")
            && !text.contains("mcp__mesh__reply")
    );

    // .claude.json: our server next to the person's, everything else kept.
    let cj = w.json(&w.paths.claude_json);
    assert_eq!(cj.get("numStartups"), Some(&Json::int(12)));
    assert!(cj.at(&["mcpServers".into(), "github".into()]).is_some());
    assert_eq!(
        cj.at(&["mcpServers".into(), "mesh".into()])
            .unwrap()
            .to_compact(),
        r#"{"type":"stdio","command":"/opt/spew/bin/spew","args":["mesh","mcp-server"],"env":{"SPEW_MESH_CLIENT":"claude"},"alwaysLoad":true}"#
    );
    let skill = w.paths.claude_dir.join("skills/spew-mesh/SKILL.md");
    assert_eq!(w.read(&skill), SKILL_MD);
    assert_eq!(mode(&skill), 0o600);

    // Codex: comments and layout kept, our table next to the person's servers; hook groups after theirs.
    let toml = w.read(&w.paths.codex_dir.join("config.toml"));
    let block = "[mcp_servers.mesh]\ncommand = \"/opt/spew/bin/spew\"\nargs = [\"mesh\", \"mcp-server\"]\n\
                 startup_timeout_sec = 10\ntool_timeout_sec = 960\nrequired = false\n\n\
                 [mcp_servers.mesh.env]\nSPEW_MESH_CLIENT = \"codex\"\n\n";
    assert!(toml.contains(block), "{toml}");
    assert_eq!(
        toml.replacen(block, "", 1),
        CODEX_CONFIG,
        "nothing else changed"
    );
    let ch = w.json(&w.paths.codex_dir.join("hooks.json"));
    assert_eq!(ch.get("description"), Some(&Json::str("sam's hooks")));
    let start = hooks_of(&ch, "SessionStart");
    assert_eq!(
        start[0].to_compact(),
        r#"{"hooks":[{"type":"command","command":"echo hello","timeout":3}]}"#
    );
    assert_eq!(
        start[1].to_compact(),
        r#"{"hooks":[{"type":"command","command":"'/opt/spew/bin/spew' mesh hook codex SessionStart","timeout":10,"additionalContextLimit":0}]}"#
    );
    assert_eq!(
        hooks_of(&ch, "PostToolUse")[0].get("matcher"),
        Some(&Json::str("^mcp__spew__"))
    );

    // OpenCode: tabs kept, entry and approvals added, plugin written; Claude Code's skill covers OpenCode.
    let oc_text = w.read(&w.paths.opencode_dir.join("opencode.json"));
    assert!(
        oc_text.contains("\n\t\"theme\": \"tokyonight\",\n"),
        "{oc_text}"
    );
    let oc = w.json(&w.paths.opencode_dir.join("opencode.json"));
    assert!(oc.at(&["mcp".into(), "docs".into()]).is_some());
    assert_eq!(
        oc.at(&["mcp".into(), "mesh".into(), "timeout".into()]),
        Some(&Json::int(960_000))
    );
    assert_eq!(
        oc.get("permission").unwrap().to_compact(),
        r#"{"bash":"ask","mesh_ask":"ask","mesh_tell":"ask","mesh_reply":"ask"}"#
    );
    let plugin = w.read(&w.paths.opencode_dir.join("plugins/spew-mesh.js"));
    assert!(plugin.contains("const SPEW = \"/opt/spew/bin/spew\";"));
    assert!(!w.paths.opencode_dir.join("skills").exists());

    // One-time private backups of every file that existed, and a private manifest.
    for (file, original) in [
        (w.paths.claude_dir.join("settings.json"), CLAUDE_SETTINGS),
        (w.paths.claude_json.clone(), CLAUDE_JSON),
        (w.paths.codex_dir.join("config.toml"), CODEX_CONFIG),
        (w.paths.codex_dir.join("hooks.json"), CODEX_HOOKS),
        (w.paths.opencode_dir.join("opencode.json"), OPENCODE_JSON),
    ] {
        let b = backup_path(&file);
        assert_eq!(w.read(&b), original, "{}", b.display());
        assert_eq!(mode(&b), 0o600);
        assert_eq!(mode(&file), 0o644, "the original's mode is kept");
    }
    let m = manifest_path(&w.mesh);
    assert_eq!(mode(&m), 0o600);
    let manifest = Manifest::load(&m).unwrap();
    assert_eq!(
        manifest.entries.len(),
        7 + 1 + 1 + 1 + 5 + 4 + 1,
        "{:#?}",
        manifest.entries
    );
}

#[test]
fn running_setup_twice_changes_nothing() {
    let w = World::standard();
    w.run();
    let before = w.snapshot();
    let report = w.run();
    assert_eq!(
        w.snapshot(),
        before,
        "no file is rewritten, not even with the same bytes"
    );
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    assert_eq!(
        report
            .lines
            .iter()
            .filter(|l| l.contains("already wired"))
            .count(),
        3,
        "{:#?}",
        report.lines
    );
}

#[test]
fn uninstall_restores_the_original_files() {
    let w = World::standard();
    let before = contents(&w.home_snapshot());
    w.run();
    let report = w.setup().uninstall().unwrap();
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    assert!(!manifest_path(&w.mesh).exists());
    let after = contents(&w.home_snapshot());
    assert_eq!(
        after, before,
        "same files, same bytes; backups, skill, plugin and manifest gone"
    );
    let again = w.setup().uninstall().unwrap();
    assert_eq!(again.lines, vec!["No agent changes to revert."]);
}

#[test]
fn files_setup_created_are_removed_by_uninstall() {
    let w = World::new(&[
        ("claude", "2.1.302 (Claude Code)"),
        ("codex", "codex-cli 0.170.2"),
        ("opencode", "1.20.0"),
    ]);
    // Claude Code through its CLI; nothing else exists yet.
    let report = w.run();
    assert_eq!(
        report.problems, 1,
        "the fake claude doesn't store the server: {:#?}",
        report.lines
    );
    let oc = w.read(&w.paths.opencode_dir.join("opencode.json"));
    assert!(
        oc.starts_with("{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\""),
        "{oc}"
    );
    assert!(w.paths.codex_dir.join("hooks.json").exists());
    let report = w.setup().uninstall().unwrap();
    assert!(
        report.lines.iter().any(|l| l.starts_with("removed ")),
        "{:#?}",
        report.lines
    );
    let left: Vec<PathBuf> = w.home_snapshot().into_keys().collect();
    assert_eq!(
        left,
        Vec::<PathBuf>::new(),
        "every file and folder setup made is gone"
    );
}

#[test]
fn jsonc_is_never_rewritten_and_the_block_is_printed() {
    let w = World::standard();
    let jsonc = "{\n  // my OpenCode config\n  \"theme\": \"tokyonight\",\n}\n";
    let file = w.paths.opencode_dir.join("opencode.json");
    w.write(&file, jsonc);
    let report = w.run();
    assert_eq!(w.read(&file), jsonc, "byte for byte");
    let note = report
        .lines
        .iter()
        .find(|l| l.contains("add this to it yourself"))
        .expect("a note");
    assert!(
        note.contains("\"mesh\": {\n      \"type\": \"local\""),
        "{note}"
    );
    assert!(note.contains("\"mesh_ask\": \"ask\""), "{note}");
    assert!(!backup_path(&file).exists());
    assert!(
        w.paths.opencode_dir.join("plugins/spew-mesh.js").exists(),
        "the plugin is still written"
    );

    let doctor = w.setup().doctor().unwrap();
    let check = doctor
        .iter()
        .find(|c| c.text.starts_with(&file.display().to_string()))
        .unwrap();
    assert!(
        !check.ok && check.text.contains("add this to it yourself"),
        "{check:?}"
    );

    // The person pastes the block, keeping their comment: doctor is satisfied and setup leaves it be.
    let pasted = format!(
        "{{\n  // my OpenCode config\n  \"theme\": \"tokyonight\",\n  \"mcp\": {{\"mesh\": {}}},\n  \"permission\": {{\"mesh_ask\": \"ask\", \"mesh_tell\": \"ask\", \"mesh_reply\": \"ask\"}},\n}}\n",
        super::opencode::mcp_entry(&w.wiring).to_compact()
    );
    w.write(&file, &pasted);
    let doctor = w.setup().doctor().unwrap();
    assert!(
        doctor
            .iter()
            .any(|c| c.ok && c.text == format!("{}: wired", file.display())),
        "{doctor:#?}"
    );
    let report = w.run();
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    assert_eq!(w.read(&file), pasted);
}

#[test]
fn an_opencode_jsonc_file_is_only_ever_printed() {
    let w = World::new(&[("opencode", "1.19.3")]);
    let file = w.paths.opencode_dir.join("opencode.jsonc");
    w.write(&file, "{\"theme\": \"x\"}\n");
    let report = w.run();
    assert_eq!(w.read(&file), "{\"theme\": \"x\"}\n");
    assert!(!w.paths.opencode_dir.join("opencode.json").exists());
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("opencode.jsonc may hold comments")),
        "{:#?}",
        report.lines
    );
    assert_eq!(
        w.read(&w.paths.opencode_dir.join("skills/spew-mesh/SKILL.md")),
        SKILL_MD,
        "without Claude Code, OpenCode gets its own skill"
    );
}

#[test]
fn a_file_changed_between_read_and_rename_is_read_again() {
    let w = World::standard();
    let settings = w.paths.claude_dir.join("settings.json");
    let mut seen = 0;
    let report = w
        .setup()
        .setup_with(&Options::default(), &mut |p, attempt| {
            if p == settings && attempt == 0 {
                seen += 1;
                // A running Claude Code writes its own change meanwhile.
                let text = fs::read_to_string(p).unwrap().replacen(
                    "\"model\": \"opus\"",
                    "\"model\": \"sonnet\"",
                    1,
                );
                fs::write(p, text).unwrap();
            }
        })
        .unwrap();
    assert_eq!(seen, 1);
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    let doc = w.json(&settings);
    assert_eq!(
        doc.get("model"),
        Some(&Json::str("sonnet")),
        "the concurrent change survives"
    );
    assert_eq!(
        hooks_of(&doc, "Stop").len(),
        2,
        "and our edit is applied on top"
    );
}

#[test]
fn setup_gives_up_on_a_file_that_keeps_changing() {
    let w = World::standard();
    let settings = w.paths.claude_dir.join("settings.json");
    let mut n = 0;
    let report = w
        .setup()
        .setup_with(&Options::default(), &mut |p, _| {
            if p == settings {
                n += 1;
                fs::write(p, CLAUDE_SETTINGS.replace("opus", &"o".repeat(n))).unwrap();
            }
        })
        .unwrap();
    assert_eq!(n, 4);
    assert!(
        report.lines.iter().any(|l| l.contains("kept changing")),
        "{:#?}",
        report.lines
    );
    assert_eq!(
        w.read(&settings),
        CLAUDE_SETTINGS.replace("opus", "oooo"),
        "never overwritten"
    );
    assert!(
        !backup_path(&settings).exists(),
        "no backup of an edit that never happened"
    );
    let manifest = Manifest::load(&manifest_path(&w.mesh)).unwrap();
    assert!(manifest.entries.iter().all(|e| e.file != settings));
    assert!(
        manifest.entries.iter().any(|e| e.client == Client::Codex),
        "the other clients are wired"
    );
}

#[test]
fn old_versions_are_skipped_with_the_version_needed() {
    let w = World::new(&[
        ("claude", "2.1.200 (Claude Code)"),
        ("codex", "codex-cli 0.162.9"),
        ("opencode", "1.19.3"),
    ]);
    fs::create_dir_all(&w.paths.claude_dir).unwrap();
    let report = w.run();
    assert!(report.lines.contains(&"Claude Code 2.1.200 is too old: spew mesh needs 2.1.259 or newer. Skipped; update Claude Code and run setup again.".to_string()), "{:#?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("Codex 0.162.9 is too old: spew mesh needs 0.163.0 or newer"))
    );
    assert_eq!(report.wired, vec![Client::Opencode]);
    assert!(!w.paths.claude_dir.join("settings.json").exists());
    assert!(!w.paths.codex_dir.exists());
    assert!(
        calls(&w.bin, "claude").iter().all(|c| c == "--version"),
        "an old claude is only asked its version"
    );
    let doctor = w.setup().doctor().unwrap();
    assert!(
        doctor
            .iter()
            .any(|c| !c.ok && c.text.contains("needs 2.1.259 or newer"))
    );
}

#[test]
fn claude_code_gets_its_server_through_its_cli() {
    let w = World::new(&[("claude", "2.1.302 (Claude Code)")]);
    w.write(&w.paths.claude_json, CLAUDE_JSON);
    let report = w.run();
    let def = super::claude::mcp_definition(&w.wiring).to_compact();
    assert_eq!(
        calls(&w.bin, "claude"),
        vec![
            "--version".to_string(),
            format!("mcp add-json --scope user mesh {def}")
        ]
    );
    assert_eq!(w.read(&backup_path(&w.paths.claude_json)), CLAUDE_JSON);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("ran claude mcp add-json")),
        "{:#?}",
        report.lines
    );

    // What the real CLI would have stored: setup then sees it and leaves it.
    let mut doc = w.json(&w.paths.claude_json);
    doc.at_mut(&["mcpServers".into()]).unwrap().insert(
        "mesh",
        Json::from_value(&serde_json::from_str(&def).unwrap()),
    );
    w.write(&w.paths.claude_json, &Style::default().render(&doc));
    w.run();
    assert_eq!(calls(&w.bin, "claude").len(), 3, "only another --version");

    let report = w.setup().uninstall().unwrap();
    assert_eq!(
        calls(&w.bin, "claude").last().unwrap(),
        "mcp remove --scope user mesh"
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("ran claude mcp remove")),
        "{:#?}",
        report.lines
    );
}

#[test]
fn dry_run_prints_the_plan_and_changes_nothing() {
    let w = World::standard();
    fake_command(
        &w.bin,
        "claude",
        "[ \"$1\" = --version ] && echo '2.1.302 (Claude Code)'\nexit 0",
    );
    let before = w.snapshot();
    let report = w
        .setup()
        .setup(&Options {
            agents: None,
            dry_run: true,
        })
        .unwrap();
    assert_eq!(w.snapshot(), before);
    assert!(!manifest_path(&w.mesh).exists());
    let settings = format!(
        "  would change {}: add the SessionStart hook",
        w.paths.claude_dir.join("settings.json").display()
    );
    assert!(
        report.lines.iter().any(|l| l.starts_with(&settings)),
        "{:#?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("  would run: claude mcp add-json --scope user mesh '{")),
        "{:#?}",
        report.lines
    );
    assert_eq!(calls(&w.bin, "claude"), vec!["--version"]);
}

#[test]
fn uninstall_leaves_what_the_person_changed_and_says_so() {
    let w = World::standard();
    w.run();
    let settings = w.paths.claude_dir.join("settings.json");
    let edited = w
        .read(&settings)
        .replace("\"timeout\": 8\n", "\"timeout\": 30\n");
    w.write(&settings, &edited);
    let skill = w.paths.claude_dir.join("skills/spew-mesh/SKILL.md");
    fs::write(&skill, "my own notes\n").unwrap();

    let report = w.setup().uninstall().unwrap();
    let left: Vec<&String> = report
        .lines
        .iter()
        .filter(|l| l.starts_with("left "))
        .collect();
    assert_eq!(
        left.len(),
        4,
        "UserPromptSubmit, PostToolUse, Stop and the skill: {:#?}",
        report.lines
    );
    assert!(left.iter().any(|l| l.contains("claude.skill")));
    let doc = w.json(&settings);
    assert_eq!(hooks_of(&doc, "Stop").len(), 2, "the changed hook stays");
    assert!(
        doc.at(&["hooks".into(), "SessionStart".into()]).is_none(),
        "the unchanged ones go"
    );
    assert_eq!(w.read(&skill), "my own notes\n");
    assert!(
        backup_path(&settings).exists(),
        "kept while something of setup's is left"
    );
    assert!(!backup_path(&w.paths.codex_dir.join("config.toml")).exists());
    assert!(!manifest_path(&w.mesh).exists());
}

#[test]
fn a_moved_binary_updates_setups_entries_in_place() {
    let mut w = World::standard();
    w.run();
    w.wiring = Wiring::new(PathBuf::from("/usr/local/bin/spew"), None).unwrap();
    let report = w.run();
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("updated the Stop hook")),
        "{:#?}",
        report.lines
    );
    let settings = w.json(&w.paths.claude_dir.join("settings.json"));
    let stop = hooks_of(&settings, "Stop");
    assert_eq!(stop.len(), 2, "replaced, not added again");
    assert!(stop[1].to_compact().contains("/usr/local/bin/spew"));
    let start = hooks_of(
        &w.json(&w.paths.codex_dir.join("hooks.json")),
        "SessionStart",
    )
    .clone();
    assert_eq!(start.len(), 2);
    assert!(
        start[1]
            .to_compact()
            .contains("'/usr/local/bin/spew' mesh hook codex SessionStart")
    );
    assert!(
        w.read(&w.paths.codex_dir.join("config.toml"))
            .contains("command = \"/usr/local/bin/spew\"")
    );
    // Uninstall still restores the original.
    w.setup().uninstall().unwrap();
    assert_eq!(
        w.read(&w.paths.claude_dir.join("settings.json")),
        CLAUDE_SETTINGS
    );
    assert_eq!(w.read(&w.paths.codex_dir.join("config.toml")), CODEX_CONFIG);
}

#[test]
fn unparseable_files_and_foreign_values_are_left_alone() {
    let w = World::standard();
    let settings = w.paths.claude_dir.join("settings.json");
    w.write(&settings, "{ \"model\": ");
    w.write(
        &w.paths.opencode_dir.join("opencode.json"),
        "{\"permission\": \"allow\", \"mcp\": {\"mesh\": {\"type\": \"remote\"}}}",
    );
    let report = w.run();
    assert_eq!(w.read(&settings), "{ \"model\": ");
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("settings.json doesn't parse")),
        "{:#?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("mcp.mesh is already set to something else")),
        "{:#?}",
        report.lines
    );
    let single = report
        .lines
        .iter()
        .filter(|l| l.contains("\"permission\" is a single value"))
        .count();
    assert_eq!(single, 1, "said once: {:#?}", report.lines);
    assert!(w.paths.codex_dir.join("config.toml").exists());
    assert_eq!(report.problems, 3);
    let doctor = w.setup().doctor().unwrap();
    assert!(
        doctor
            .iter()
            .any(|c| !c.ok && c.text.contains("\"permission\" is a single value")),
        "{doctor:#?}"
    );
}

#[test]
fn doctor_reports_versions_wiring_and_fixes() {
    let w = World::standard();
    let before = w.setup().doctor().unwrap();
    assert!(
        before
            .iter()
            .any(|c| !c.ok && c.text.contains("missing the SessionStart hook")),
        "{before:#?}"
    );
    w.run();
    let after = w.setup().doctor().unwrap();
    let bad: Vec<_> = after.iter().filter(|c| !c.ok).collect();
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(
        after
            .iter()
            .any(|c| c.client == Client::Codex && c.text.starts_with("Codex 0.163.0 ("))
    );
    assert!(after.iter().any(|c| c.text == TRUST_LINE));
    assert!(
        after
            .iter()
            .any(|c| c.client == Client::Claude && c.text.contains("`claude` isn't on PATH"))
    );

    let settings = w.paths.claude_dir.join("settings.json");
    let mut doc = w.json(&settings);
    doc.insert("disableAllHooks", Json::Bool(true));
    w.write(&settings, &Style::default().render(&doc));
    let checks = w.setup().doctor().unwrap();
    assert!(
        checks
            .iter()
            .any(|c| !c.ok && c.text.contains("disableAllHooks"))
    );
}

#[test]
fn agents_limits_setup_and_missing_clients_are_skipped() {
    let w = World::new(&[]);
    let report = w.run();
    assert_eq!(report.wired, Vec::<Client>::new());
    assert_eq!(
        report
            .lines
            .iter()
            .filter(|l| l.ends_with("not found; skipped."))
            .count(),
        3
    );
    assert!(w.snapshot().is_empty(), "nothing written");

    let w = World::standard();
    let report = w
        .setup()
        .setup(&Options {
            agents: Some(vec![Client::Codex]),
            dry_run: false,
        })
        .unwrap();
    assert_eq!(report.wired, vec![Client::Codex]);
    assert_eq!(
        w.read(&w.paths.claude_dir.join("settings.json")),
        CLAUDE_SETTINGS
    );
    assert_eq!(
        w.read(&w.paths.opencode_dir.join("opencode.json")),
        OPENCODE_JSON
    );
}

#[test]
fn a_codex_hook_command_survives_the_shell_with_a_quote_in_the_path() {
    let mut w = World::new(&[("codex", "codex-cli 0.163.0")]);
    let odd = w.home.join("it's a \"dir\"");
    fs::create_dir_all(&odd).unwrap();
    let log = odd.join("args.log");
    let script = odd.join("spew");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s|' \"$@\" > \"$(dirname \"$0\")/args.log\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    w.wiring = Wiring::new(odd.join("spew"), Some(w.home.join("mesh home"))).unwrap();
    w.run();
    let doc = w.json(&w.paths.codex_dir.join("hooks.json"));
    let cmd = hooks_of(&doc, "Stop")[0]
        .get("hooks")
        .and_then(Json::as_array)
        .unwrap()[0]
        .get("command")
        .and_then(Json::as_str)
        .unwrap()
        .to_string();
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .unwrap();
    assert!(status.success(), "{cmd}");
    let home = w.home.join("mesh home");
    assert_eq!(
        w.read(&log),
        format!("mesh|--home|{}|hook|codex|Stop|", home.display())
    );
}

#[test]
fn linked_configs_are_edited_at_their_target_and_the_links_survive() {
    let w = World::standard();
    let dotfiles = w.home.join("dotfiles");
    let settings = w.paths.claude_dir.join("settings.json");
    let config = w.paths.codex_dir.join("config.toml");
    let links = [
        (
            settings.clone(),
            dotfiles.join("claude-settings.json"),
            CLAUDE_SETTINGS,
        ),
        (
            config.clone(),
            dotfiles.join("codex-config.toml"),
            CODEX_CONFIG,
        ),
    ];
    for (link, target, text) in &links {
        w.write(target, text);
        fs::remove_file(link).unwrap();
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    let before = contents(&w.home_snapshot());

    let report = w.run();
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    for (link, target, _) in &links {
        assert_eq!(&fs::read_link(link).unwrap(), target, "still the same link");
        assert!(
            !backup_path(link).exists(),
            "nothing written next to the link"
        );
        assert!(
            backup_path(target).exists(),
            "the backup is next to the target"
        );
        assert_eq!(mode(target), 0o644);
    }
    assert_eq!(hooks_of(&w.json(&settings), "Stop").len(), 2);
    assert!(w.read(&config).contains("[mcp_servers.mesh]"));
    let manifest = Manifest::load(&manifest_path(&w.mesh)).unwrap();
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.id == "codex.mcp")
        .unwrap();
    assert_eq!(entry.file, config, "the path setup looked at");
    assert_eq!(
        entry.target,
        Some(fs::canonicalize(&links[1].1).unwrap()),
        "and the file it edited"
    );

    let snapshot = w.snapshot();
    let again = w.run();
    assert_eq!(w.snapshot(), snapshot, "a second run changes nothing");
    assert_eq!(again.problems, 0, "{:#?}", again.lines);

    let report = w.setup().uninstall().unwrap();
    assert_eq!(report.problems, 0, "{:#?}", report.lines);
    assert!(
        report.lines.iter().any(|l| l.contains("(linked to ")),
        "{:#?}",
        report.lines
    );
    for (link, target, _) in &links {
        assert_eq!(
            &fs::read_link(link).unwrap(),
            target,
            "uninstall keeps the link"
        );
    }
    assert_eq!(
        contents(&w.home_snapshot()),
        before,
        "the targets are restored byte for byte, their backups gone"
    );
}

#[test]
fn opencode_gets_its_own_skill_when_claude_code_keeps_its_config_elsewhere() {
    let mut w = World::new(&[("opencode", "1.19.3")]);
    let claude_dir = w.home.join("work/claude");
    let env = |k: &str| (k == "CLAUDE_CONFIG_DIR").then(|| OsString::from(&claude_dir));
    w.paths = ClientPaths::from_env(&w.home, &env).unwrap();
    assert!(!w.paths.claude_dir_is_default);
    fs::create_dir_all(&claude_dir).unwrap();
    let report = w.run();
    assert_eq!(report.wired, vec![Client::Claude, Client::Opencode]);
    assert_eq!(
        w.read(&claude_dir.join("skills/spew-mesh/SKILL.md")),
        SKILL_MD
    );
    assert_eq!(
        w.read(&w.paths.opencode_dir.join("skills/spew-mesh/SKILL.md")),
        SKILL_MD,
        "OpenCode reads Claude Code's skills only from ~/.claude"
    );
    w.setup().uninstall().unwrap();
    assert!(!w.paths.opencode_dir.join("skills").exists());
}

#[test]
fn nothing_changes_when_the_manifest_cannot_be_saved() {
    let w = World::standard();
    let before = contents(&w.home_snapshot());
    let m = manifest_path(&w.mesh);
    let report = w
        .setup()
        .setup_with(&Options::default(), &mut |_, _| {
            // A folder where the manifest goes: saving it fails.
            fs::create_dir_all(&m).unwrap();
        })
        .unwrap();
    assert!(
        report.lines.iter().any(|l| l.contains("recording it in")),
        "{:#?}",
        report.lines
    );
    assert_eq!(
        contents(&w.home_snapshot()),
        before,
        "no config changed, and no backup, file or folder is left"
    );
}
