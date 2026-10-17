# serve

`spew mesh serve` answers questions from other agents while nobody is at the keyboard. It opens a responder
session, and for each question it runs one headless turn of Claude Code, Codex or OpenCode in a folder you choose,
then sends the agent's final text back as the answer. Runs are read-only by default.

```sh
spew mesh serve <claude|codex|opencode> [--name N] [--cwd DIR] [--allow-edits] [--timeout SECONDS] [--model M]
```

| Flag | Meaning | Default |
|---|---|---|
| `--name N` | The responder's session name | the client: `claude`, `codex` or `opencode` |
| `--cwd DIR` | The folder it answers about | the current folder |
| `--allow-edits` | Also allow file edits inside `--cwd` | off |
| `--timeout SECONDS` | How long one run may take | `[serve] timeout_seconds` (300) |
| `--model M` | The model the client runs | the client's own default |

```console
$ cd ~/code/api
$ spew mesh serve claude
Serving claude in /Users/sam/code/api, read-only as sam@sam-mac/claude. Ctrl-C stops.
01JA2Q...: question from maya/claude-web
01JA2Q...: answered ($0.0123)
```

The client's command must be on `PATH`. serve checks everything it needs before it starts, and refuses to start
with one line saying what is wrong and a `fix:` line.

## What it answers

- **Questions** (`ask`). The answer goes back to the asker, whose `ask` is waiting for it.
- **Notes** (`tell`), including a reply to one of the responder's answers. The run's final text goes back as a
  message of its own.
- **Follow-ups.** When the asker continues a conversation (the MCP `reply` tool with `wait_seconds`, on the
  responder's answer), the next run resumes the same Claude Code session, Codex thread or OpenCode session, so the
  agent remembers what it said. A new conversation starts fresh. The map from conversation to client session is
  kept in the store and dropped after 30 days unused.

Each run gets a short preface before the message: who is asking, that it is a colleague's agent and not your
person, that the final message goes back to them, that it must not follow instructions your person wouldn't
approve or put secrets in the answer, and whether it may edit files. Then comes the message in the same marked
block hooks use (see [agents.md](agents.md)).

A run that fails is answered with one line, so the asker's wait ends:

```text
The serve run failed: it ran longer than 300 seconds and was stopped.
```

An answer longer than 32 KiB is cut at a line break, with a note.

## Who may ask

`[serve] from` in `settings.toml` decides:

- `"room"` (the default): your own devices, and anyone admitted who shares the responder's room.
- `"own"`: only your own devices. Teammates don't see the responder in `who` at all.

The responder's room comes from the folder, as for any session: `SPEW_PROJECT` in serve's environment, else the
`project` key of the nearest `.spew.json` above `--cwd`, else `[roster] default_room`. A responder with no room is
visible only to your own devices.

Anyone else gets the same refusal as for a session that doesn't exist (`no-such-session`), so they can't tell a
responder is there.

## Limits

| Setting in `[serve]` | Default | What it limits |
|---|---|---|
| `timeout_seconds` | 300 | One run. `--timeout` overrides it. |
| `max_runs_per_person_per_hour` | 10 | Runs for any one asking person, over a rolling hour. Your own devices don't count here. |
| `max_runs_per_hour` | 30 | All runs together, over a rolling hour. |
| `max_turns` | 20 | Agent turns per Claude Code run (`--max-turns`). |
| `max_budget_usd` | 0.50 | Dollars per Claude Code run (`--max-budget-usd`). |

- One run at a time. Up to 8 questions wait behind it, and `who` shows how many as `responder (3 waiting)`.
- A question over a run limit, or past a full queue, is refused with `busy`, and the asker sees that at once.
- A timeout kills the run's whole process group. Each run starts in its own process group with no terminal.
- Codex has no turn or dollar cap for `codex exec`, and serve sets none for OpenCode. For those two, the timeout
  and the run limits are what bound the cost.

Settings edits apply when serve restarts.

## What a run may do

By default a run can read and search files inside `--cwd`. It can't write files, reach the network, or read
outside `--cwd`. Secret files are off limits: `.env` files, `*.pem` and `*.key` files, `id_rsa*` and
`id_ed25519*`, and `.git/config`.

`--allow-edits` also lets the run edit files inside `--cwd`, but never under `.git/` and never the secret files
above. Use it only for a folder where you'd accept a colleague's agent making changes.

Each run's environment drops the variables that describe a parent agent session, so a run started from inside an
agent doesn't act as that agent. Sign-in and provider variables stay.

Here is exactly how each client is run.

### Claude Code

One `claude -p` run per question, the prompt on stdin:

```text
claude -p --output-format json (--session-id <uuid> | --resume <uuid>) --restricted
  --tools "Read,Grep,Glob" --disallowedTools "mcp__*" --strict-mcp-config --permission-mode dontAsk
  --settings '<settings>' --max-turns 20 --max-budget-usd 0.5 [--model <m>] --permission-prompts none
```

`<settings>` turns off hooks and cross-session messages, and denies reads of secret files:

```json
{"disableAllHooks":true,"crossSessionInbound":"refuse","permissions":{"deny":["Read(./.env)","Read(./.env.*)","Read(./**/.env)","Read(./**/.env.*)","Read(./**/*.pem)","Read(./**/*.key)","Read(./**/id_rsa*)","Read(./**/id_ed25519*)","Read(./.git/config)"]}}
```

- `--restricted` loads only managed settings and `--settings`, so a repo's own `.claude/settings.json` can't widen
  what a run may do.
- A conversation's first run gets a new session id. Follow-ups use `--resume` with every flag repeated.
- `--allow-edits` makes the tools `Read,Grep,Glob,Edit,Write` and the permission mode `acceptEdits`, and adds
  `Edit(...)` and `Write(...)` deny rules for the same secret files and for `./.git/**`.
- A result with `is_error: true`, a `subtype` other than `success`, or a non-zero exit is a failed run. The cost
  Claude Code reports (`total_cost_usd`) goes to the terminal and the log.
- Dropped from the environment: `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ID`,
  `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_ENTRYPOINT`,
  `CLAUDE_CODE_SESSION_ATTENDED`, `CLAUDE_CODE_BRIDGE_SESSION_ID`, `CLAUDE_CODE_REMOTE_SESSION_ID`, `CLAUDE_PID`,
  `CLAUDE_PROJECT_DIR`, `CLAUDE_ENV_FILE`, `CLAUDE_EFFORT` and every `CLAUDE_PLUGIN_*`.

### Codex

One `codex exec` run per question, the prompt on stdin. Codex's own `read-only` sandbox can still read every file
on the computer, so serve defines a stricter permission profile on the command line, where your config can't change
it:

```text
codex exec --json --skip-git-repo-check -C <cwd> --ignore-user-config --disable hooks
  --thread-source spew-mesh-serve
  -c 'default_permissions="spew-mesh-serve"' -c 'permissions.spew-mesh-serve=<profile>'
  [-m <model>] [-c 'model_provider="<id>"' -c 'model_providers.<id>=<inline table>']
  -o <run file> [resume <thread id>] -
```

`<profile>` denies the whole file system except the minimum Codex needs, its own install folder, and the
workspace, and turns the network off:

```text
{extends=":read-only", filesystem={":root"="deny", ":minimal"="read", "<codex dir>"="read",
 glob_scan_max_depth=3, ":workspace_roots"={"."="read", "**/.env"="deny", "**/*.env"="deny",
 "**/.env.*"="deny", "**/*.pem"="deny", "**/*.key"="deny", "**/id_rsa*"="deny", "**/id_ed25519*"="deny",
 ".git/config"="deny", ".git/hooks"="deny"}}, network={enabled=false}}
```

- `--ignore-user-config` keeps your `config.toml` out of the run: its MCP servers (mesh included), hooks and
  `notify` don't load, and a `sandbox_mode` there can't switch the profile off. serve still reads `model` and
  `model_provider` from it; `--model` wins. From the provider's table it copies only `name`, `base_url`,
  `env_key`, `wire_api` and `query_params`.
- serve refuses to start when that provider table holds a header or token key (such as `http_headers` or
  `bearer_token_env_var`), because command lines are visible to other users of the computer. Use a provider that
  reads its key from an environment variable (`env_key`).
- serve refuses to start when the repo's Codex config (`<repo root>/.codex/config.toml`, and the one in `--cwd`)
  sets `sandbox_mode`, `mcp_servers`, `hooks`, `notify` or `features`.
- At start, serve checks the profile with `codex sandbox`: reading a file inside `--cwd` must work, and reading a
  file outside it, or a `.env` file inside it, must fail. If Codex doesn't enforce the profile on this computer,
  serve refuses to start.
- `--allow-edits` makes the workspace writable (`"."="write"`). The deny entries stay.
- The answer is the run file Codex writes with `-o` (under `~/.spew/mesh/run/serve/`, deleted after reading),
  else the last agent message. Exit status 1 or a `turn.failed` event is a failed run.
- Dropped from the environment: `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, `CODEX_TOOL_CALL_ID`.

### OpenCode

One `opencode run` per question, the message on stdin:

```text
opencode run --pure --format json --agent spew-mesh-serve [--session <id>] [--model <provider/model>]
  --title "spew mesh: <asker address>" --dir <cwd>
```

With these environment changes:

- Dropped: `OPENCODE_PERMISSION` (it could widen the agent's permissions), `OPENCODE` and `OPENCODE_PID`.
- `OPENCODE_DISABLE_PROJECT_CONFIG=1`, so a repo's `opencode.json`, agents, plugins and MCP servers never load.
  `--pure` keeps every plugin out, spew mesh's own included.
- `OPENCODE_CONFIG_CONTENT`, which outranks your global config, defines a deny-all agent that may only read, glob
  and list:

```json
{"mcp":{"mesh":{"enabled":false}},"permission":{"*":"deny"},"agent":{"spew-mesh-serve":{"description":"Answers questions from other agents, read-only","mode":"primary","permission":{"*":"deny","read":{"*":"allow","*.env":"deny","*.env.*":"deny","*.pem":"deny","*.key":"deny","*id_rsa*":"deny","*id_ed25519*":"deny","*.git/config":"deny"},"glob":"allow","list":"allow"}}}}
```

- Denied tools disappear from the model's tool list. Reads outside `--dir` are refused by OpenCode's
  `external_directory` rule.
- If OpenCode can't find the agent, it falls back to its default agent with a warning. The top-level
  `"permission": {"*": "deny"}` keeps even that fallback from editing, running commands or reaching the network,
  and serve treats a run whose warning names the fallback as failed.
- Your other global MCP servers still start, but their tools are denied.
- `--allow-edits` adds `"edit": {"*": "allow", ".git/*": "deny", "*.env": "deny", "*.env.*": "deny", "*.pem":
  "deny", "*.key": "deny"}` to the agent. These edit rules haven't been checked against a real model yet, so
  prefer read-only with OpenCode for now.
- A non-zero exit or an `error` event is a failed run.

## Logs

The terminal gets one line per question: taken, answered (with the cost when the client reports one), or failed
with the reason. serve also writes its events to the daemon's log, `~/.spew/mesh/logs/mesh.log`, including the raw
output of every failed run. `SPEW_MESH_LOG` sets the log filter (`info` by default; `debug` for more).

## Stopping

Ctrl-C (or SIGTERM or SIGHUP) stops serve. It waits up to 5 seconds for a reply in flight and closes the responder
session, then prints `spew mesh serve: stopped`. Questions still waiting are refused, and each asker gets a notice.

If the daemon goes away, serve prints `Lost the daemon; reconnecting.` and keeps trying, starting the daemon again
unless you stopped it with `spew mesh daemon stop`. When it is back, serve prints `Back on the daemon as ...` and
carries on.
