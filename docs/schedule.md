# Scheduled runs

You can run a spew card at a set time on your own computer. On the app's Schedule page, drop a card into a 5-hour
session and pick the agent (Claude Code, Codex or OpenCode), the model and the effort. While your computer is on,
the spew mesh daemon runs that card headless at that time. There is no desktop app: the daemon is the runner.

Scheduling is on by default once you sign in to spew. Turn it off or on with:

```sh
spew mesh schedule off
spew mesh schedule on
```

## What you need

- spew mesh signed in to spew (`spew mesh login`).
- The agent's CLI on `PATH` and signed in, the same way you use it by hand.
- A folder for the card's project. The daemon finds it on its own, two ways:
  - every 10 minutes it looks up to three levels under your home folder (`~/Code/spew`, `~/Code/org/web`) for a
    `.spew.json` naming a project, skipping hidden folders, `node_modules`, build output and media folders;
  - any agent session that works in a folder whose `.spew.json` names the project teaches it that folder.

  A folder an agent worked in wins over one that was only found, and the most recently used wins among those.
  To pick the folder yourself, run this in it (or pass the path); it wins over both:

```console
$ cd ~/Code/spew && spew mesh schedule folder spew
Scheduled spew cards run in /Users/sam/Code/spew on this computer.
```

That writes `[schedule.folders]` in `settings.toml`, which you can also edit by hand:

```toml
[schedule.folders]
spew = "~/Code/spew"
```

## See what this computer can run

```console
$ spew mesh schedule
Scheduled runs are on. While this computer is on, it runs the cards you schedule for it on the app's Schedule page.

Agents:
  Claude Code 2.1.312, Max 20x plan
    usage: not reported
    fable (Fable 5.1): low, medium, high, xhigh, max (default high)
    opus (Opus 5.5): low, medium, high, xhigh, max (default medium)
    ...
  Codex 0.165.0, Pro plan
    usage: 40% of the 7-day window, resets in 4d
    gpt-6.1-sol (GPT-6.1-Sol): low, medium, high, xhigh, max, ultra (default low)
    ...

Project folders:
  spew  /Users/sam/Code/spew  (from agent sessions)

Recent runs here:
  2h ago  spew#42 "Fix the login redirect"  claude opus high: done (exit 0)
```

`spew mesh schedule --json` prints the same as JSON, with every model. `spew mesh doctor` shows one line on it.

## How it works

1. The daemon finds the agent CLIs and asks each one what it can do. It tells spew at start, every 10 minutes and
   whenever something changes, so the app always shows this computer's real model list.
2. Every minute it asks spew for due runs: runs you created, whose time has come, still inside their session, and
   meant for this computer or for any of your computers.
3. For each due run whose agent is installed here and whose project has a folder here, it takes the run. If
   another of your computers took it first, spew says so and this one skips it.
4. It runs the agent in the project folder. Different projects run side by side, up to three runs at once; a
   project's own cards run one at a time, so two runs never edit the same folder together.
5. When the run ends, it tells spew how: done or failed, the exit status, and the last 2000 characters of what the
   agent said. If spew can't be reached then, it tries again every minute.

A run that hasn't started by the end of its session is not started. spew marks it missed. A run that goes past the
timeout (2 hours by default) is stopped with its whole process group:

```toml
[schedule]
enabled = true        # `spew mesh schedule on|off` sets this
timeout_minutes = 120
```

Each run is logged in the daemon log (`~/.spew/mesh/logs/mesh.log`) and kept in the local history for 30 days.
Stopping the daemon stops a run in progress. The next daemon reports it as failed.

## The exact commands

The prompt goes on stdin, so it never shows in the process list. The run starts in the project folder.

| Agent | Command |
|---|---|
| Claude Code | `claude -p --model <model> --effort <effort> --output-format json --permission-mode auto --permission-prompts none --allowedTools mcp__spew,mcp__claude_ai_spew,mcp__plugin_spew_spew` |
| Codex | `codex exec --json --skip-git-repo-check --model <model> -c model_reasoning_effort="<effort>" -c approval_policy="never" --sandbox workspace-write -C <folder> -` |
| OpenCode | `opencode run --model <model>` |

Claude Code takes the aliases `fable`, `opus`, `sonnet` and `haiku`, which always mean the latest model of each
family. OpenCode reports no effort levels, so its runs get none.

## Permissions

A scheduled run is your own request: spew only hands a computer the runs its own person created. So it runs with
your normal agent settings, MCP servers and sign-in, unlike `spew mesh serve`, which answers other people
read-only.

- Claude Code runs in `auto` mode: its safety checks decide each action, so it can edit, run tests and use git.
  spew's MCP tools are allowed so it can claim the card and move it to review. Anything that would still need your
  approval is denied at once, because nobody is there to answer. Your own allow and deny rules still apply.
- Codex runs in its `workspace-write` sandbox and never stops to ask for approval.
- OpenCode runs with your OpenCode config.
- Nothing skips permission checks. No run uses `--dangerously-skip-permissions` or a bypass flag.

The run's environment is the daemon's trimmed one (`HOME`, `PATH`, `USER`, locale, proxy and `XDG_` variables),
plus `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `OPENCODE_CONFIG` and `OPENCODE_CONFIG_DIR` when set, so each CLI finds its
own sign-in. `SPEW_PROJECT` is set to the card's project.

## What goes to spew

Only lists and names:

- each agent's kind and version, its model ids and labels, and the effort levels each model takes;
- the plan name each CLI is signed in with, such as `Max 20x` or `Pro`;
- every usage window the CLI last reported (Claude's 5-hour and weekly, Codex's primary and secondary), each as a
  percent, a window length and a reset time, so the Schedule page can mark each reset;
- the project keys this computer has a folder for (not the folder paths);
- for each run: done or failed, the exit status, and the end of the agent's output.

Tokens never leave the computer and are never logged. To find the plan, the daemon reads only these fields:

- Claude Code: `subscriptionType` and `rateLimitTier` in `~/.claude/.credentials.json`. With an API key there is
  no plan.
- Codex: the `chatgpt_plan_type` claim inside the `id_token` in `~/.codex/auth.json`. It is decoded, not verified,
  and only the plan name is kept.

Where the rest comes from:

- Claude Code: `claude --version`, the effort levels on the `--effort` line of `claude --help`, model names from
  Claude Code's model catalog cache (`~/.claude/cache/model-catalog/`), and the usage Claude Code cached in
  `~/.claude.json`: the 5-hour window when it was fetched in the last hour, the weekly windows when fetched in the
  last day.
- Codex: `codex --version`, `codex debug models` (models marked to list), and `rate_limits` (`primary` and `secondary`) in the
  newest session log under `~/.codex/sessions/`.
- OpenCode: `opencode --version` and `opencode models`.

Each command has a 30-second timeout. A CLI that is missing or doesn't answer is left out.
