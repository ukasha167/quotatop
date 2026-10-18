# quotatop

A terminal dashboard for three AI coding subscriptions at once: Claude (Claude Code), ChatGPT (Codex) and Google (Antigravity, `agy`). It shows each subscription's 5-hour and weekly limits, every running session's context use and auto-compact point, and each Claude session's prompt cache state.

It spends no tokens and makes no paid API calls. It never reads stored credentials (Keychain, OAuth tokens, cookies, `auth.json`). Each number comes from a CLI's own `/usage` or from the logs the CLIs write locally.

```
 AI subscriptions                           Sun 18 Oct 00:42  r refresh  q quit
 Most room now: ✓ agy Gemini 0% · ✓ agy Claude+GPT 0% · ✓ Codex 43% · +1 more
╭ Claude ───────────────────────────────────────────────────── updated 5 s ago ╮
│ 5-hour  █████████▎░░░░░░░░░░  46% ✓ ok    resets 01:40 (in 57 m)             │
│ Weekly  █████▍░░░░░░░░░░░░░░  27% ✓ ok    resets Wed 20:00 · week 46% gone   │
╰──────────────────────────────────────────────────────────────────────────────╯
╭ Codex · Plus ─────────────────────────────────────────────── updated 8 s ago ╮
│ 5-hour  ████████▋░░░░░░░░░░░  43% ✓ ok    resets 04:39 (in 3 h 57 m)         │
│ Weekly  ████▊░░░░░░░░░░░░░░░  24% ✓ ok    resets Thu 22:39 · week 30% gone   │
│ 2 free full resets available, first one expires 29 Oct                       │
╰──────────────────────────────────────────────────────────────────────────────╯
╭ Sessions ─────────────────────────────────────────────────── updated 0 s ago ╮
│ Claude  AI coding subscriptions dashboard            Opus 5.5     ▶ working  │
│   context █████▌░░░░┃░░░░░░░░░ 277k of 1M   ✓  auto-compacts at 500k         │
│   cache   ● warm, ≈59 m left   277k cached   if it goes cold: at least 277k  │
│ Claude  FYP Phase 1 Agile SRS document               Opus 5.5     ‖ idle     │
│   context ██████▉░░░┃░░░░░░░░░ 344k of 1M   ✓  auto-compacts at 500k         │
│   cache   ○ cold since ≈22:34   next message re-caches at least 344k         │
╰──────────────────────────────────────────────────────────────────────────────╯
```

## Install

You need Rust 1.88 or later (install it from https://rustup.rs).

```bash
cargo build --release
```

The binary is `target/release/quotatop`, about 1 MB. Copy it somewhere on your PATH, such as `~/.local/bin`.

## Use

```bash
quotatop
```

Keys: `q` or `Esc` quits, `r` refreshes everything now, the arrow keys and `j`/`k` scroll the session list.

| Option | Effect |
|---|---|
| `--interval N` | Seconds between limit refreshes, 30 to 86400. Default 60. |
| `--idle-backoff` | Refresh every 5 minutes while no local session has been active for 10 minutes. Off by default, because claude.ai use on another device also counts against your limits. |
| `--once [--width N]` | Fetch everything once, print a plain-text screen and exit. `N` is 60 to 300. |

Set `NO_COLOR=1` to turn colours off. Every colour on screen also has a word or symbol, so nothing is lost.

## Where each number comes from

| Shown | Source | Cost per refresh |
|---|---|---|
| Claude limits | `claude -p /usage --output-format json --no-session-persistence --setting-sources user --strict-mcp-config`. quotatop parses the same text `/usage` prints. | about 4 s, 2.4 s CPU, 400 MB peak |
| Codex limits and free resets | `codex app-server`, method `account/rateLimits/read` | about 3 s, 0.3 s CPU, 150 MB peak |
| Antigravity limits, both pools | `agy -p=/usage --output-format json`. The percentages come from the table agy prints. | 7 to 13 s, 1 s CPU, 150 MB peak |
| Claude sessions | `~/.claude/sessions/<pid>.json` lists live sessions with their title and busy/idle state. The end of each transcript in `~/.claude/projects` gives context and cache. | file reads only |
| Codex sessions | Rollouts in any `~/.codex/sessions/YYYY/MM/DD` folder written in the last 6 hours. Guardian reviews and other sub-agent threads, and threads imported from another tool, are left out. The 6 newest are shown and the rest counted. | file reads only |

The CLIs run in `~/.cache/quotatop/run`, a folder only you can read (mode 0700). `claude -p` skips the workspace trust prompt, so in a shared folder such as `/tmp` it would run hooks and MCP servers that another user left there; the private folder and `--setting-sources user --strict-mcp-config` stop that. agy's log, which holds your account email, also goes there and is deleted after each run.

**Schedule.** Claude, Codex and agy each have a fixed slot, 20 s apart (0, 20 and 40 s after launch, then every interval), so their memory peaks don't overlap. A slow fetch, a failure or a press of `r` never moves a CLI off its slot. `r` fetches all three at once, and is ignored for a CLI that started a fetch less than 10 s ago. After a failure the wait doubles (2, 4, 8 intervals) up to 10 minutes. A CLI installed after launch is found within a minute, or on `r`.

quotatop itself uses about 8 MB of memory with no sessions and 18 to 23 MB with 5 Claude and 6 Codex sessions, and 0.10 to 0.25% of one CPU core, measured on an M1 MacBook Air. Almost all of the cost is the CLIs.

## How each value is worked out

**Context** is the input tokens of the session's last API request (for Claude, input plus cache reads plus cache writes). For Codex it is `last_token_usage.total_tokens`.

**Context window.** Claude: from Claude Code's model docs. Fable 5+, Sonnet 5+, Haiku 5.5+ and Opus 4.7+ have 1M by default; older models have 200K unless the session runs a `[1m]` model (in the transcript, the `model` setting or `ANTHROPIC_MODEL`). Bedrock and Vertex IDs (`us.anthropic.claude-opus-5-5-v1:0`, `claude-haiku-5-5@20261001`) are read as the plain model. `CLAUDE_CODE_DISABLE_1M_CONTEXT` is honoured. `CLAUDE_CODE_MAX_CONTEXT_TOKENS` sets the window of a model Claude Code does not know; for a known model it counts only together with `DISABLE_COMPACT`, as the model docs say. Codex: `model_context_window` from the log, else `context_window × effective_context_window_percent` from `~/.codex/models_cache.json`.

**Auto-compact point.** Claude: `CLAUDE_CODE_AUTO_COMPACT_WINDOW` first. Then the settings files in precedence order: the session folder's `.claude/settings.local.json`, its `.claude/settings.json`, then `~/.claude/settings.json`. The first file that sets `modelSettings.<model>.autoCompactWindow` or `autoCompactWindow` decides, the per-model entry first within a file. Values can be numbers or text such as `500k`; `"auto"` means the default and hides lower files. Model IDs with a date (`claude-haiku-4-5-20251001`) match the plain entry. The value is capped at the window. With none set, a 1M model compacts at "about 967K" according to the docs, so quotatop shows `≈967k`. Auto-compact shows as off when `/config` turned it off (`autoCompactEnabled: false`), or `DISABLE_AUTO_COMPACT` or `DISABLE_COMPACT` is set.

Environment variables come from a settings file's `env` block, else from the environment the session was started with. On macOS and Linux quotatop reads that from the session's process (only these variables, nothing else); on Windows it uses its own environment.

Codex: `model_auto_compact_token_limit` from the active profile in `config.toml`, else from its top level, else the model's `auto_compact_token_limit` in `models_cache.json`, capped at the window. If none is set, quotatop shows the point as unknown instead of guessing Codex's default.

**Claude cache.** The transcript records, for each response, how many tokens were read from and written to the cache and whether the write used the 1 h or 5 m TTL. The cache expires one TTL after the last request was sent. quotatop takes the send time from the last user entry before that response's first block, because a long response can be written minutes after its request. These values carry `≈`. A response logged before the 1 h/5 m breakdown existed counts as a 5 m write. "If it goes cold" is the size of the prefix cached now. The next request re-sends at least that much, so the screen says "at least". The exception is a session on the 1 h TTL with more than 200k cached: Claude Code may compact it while idle, so the figure is shown as `≈` instead.

**"Not started".** A window with 0% used whose reset time is exactly one window length after the reading has not begun. Antigravity reports unused windows this way, and the reset time moves forward with every reading.

**"Most room now"** lists each pool by the fuller of its 5-hour and weekly windows, emptiest first. Per-model limits such as Claude's "Weekly Opus" are not counted, since they bind one model only. Pools with stale readings are left out, and pools that do not fit the width are counted as "+N more".

**Staleness.** Each section shows the age of its reading. After 2 minutes without a good reading (or two refresh intervals, if longer, counting the 5-minute `--idle-backoff` interval) it says "⚠ stale" and gives the last error. A missing value shows `?`, never a default. Percentages are never rounded up to 70, 90 or 100%, so 69.96% shows as 69.9% next to "✓ ok".

## Exact cache numbers for terminal sessions

Claude Code sends exact `prompt_cache` and `context_window` data to the status line. It only does this for `claude` running in a terminal; Claude desktop app sessions do not run the status line. To use that data, make `quotatop tap` your status line command. It saves the JSON to `~/.cache/quotatop/tap/<session>.json` (on Windows, `%LOCALAPPDATA%\quotatop\tap`), with the folder at mode 0700 and each file at 0600 as Claude Code does, passes the same JSON to your own status line command, and prints that command's output:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/full/path/to/quotatop tap --then '~/.claude/statusline-cache.sh'"
  }
}
```

Without `--then`, it prints the model name and context percentage. When a snapshot is at least as new as the session's last request, quotatop uses it and drops the `≈`.

## What it cannot show

- **Antigravity sessions.** agy stores conversations as SQLite files with undocumented protobuf data, so quotatop lists none rather than guess at their contents.
- **Codex cache expiry.** Codex does not report it.
- **A `--autocompact` flag** passed when a Claude session was launched. Only the settings files and environment are read. `/autocompact` saves to settings, so that one is picked up.
- **A session's environment on Windows.** quotatop uses its own environment there, so a variable set only in the shell that started `claude` is missed.
- **Claude's plan name** (Pro or Max). `/usage` does not print it.

## Environment

| Variable | Use |
|---|---|
| `QUOTATOP_CLAUDE`, `QUOTATOP_CODEX`, `QUOTATOP_AGY` | Full path to a CLI that is not on PATH |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | Read the same way the CLIs read them |
| `NO_COLOR` | Turn colours off |

## Tests

```bash
cargo test
```

The parsers are tested against real `/usage` output captured on macOS, with account IDs removed. The session collectors run against folders the tests build in the temp folder: a dead PID, a Codex session in an old day folder, guardian and imported threads, and a transcript whose usage line sits outside the first 2 MB read.
