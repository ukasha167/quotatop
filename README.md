# quotatop

A terminal dashboard for three AI coding subscriptions at once: Claude (Claude Code), ChatGPT (Codex) and Google (Antigravity, `agy`). It shows each subscription's 5-hour and weekly limits, every running session's context use and auto-compact point, and each Claude session's prompt cache state.

It spends no tokens and makes no paid API calls. It never reads stored credentials (Keychain, OAuth tokens, cookies, `auth.json`). Each number comes from a CLI's own `/usage` or from the logs the CLIs write locally.

```
 AI subscriptions                           Sun 18 Oct 00:42  r refresh  q quit
 Most room now: ✓ agy Gemini 0% · ✓ agy Claude+GPT 0% · ✓ Codex 43%
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
| `--interval N` | Seconds between limit refreshes. Default 60, minimum 30. |
| `--idle-backoff` | Refresh every 5 minutes while no local session has been active for 10 minutes. Off by default, because claude.ai use on another device also counts against your limits. |
| `--once [--width N]` | Fetch everything once, print a plain-text screen and exit. |

Set `NO_COLOR=1` to turn colours off. Every colour on screen also has a word or symbol, so nothing is lost.

## Where each number comes from

| Shown | Source | Cost per refresh |
|---|---|---|
| Claude limits | `claude -p /usage --output-format json --no-session-persistence`, run in the temp folder. quotatop parses the same text `/usage` prints. | about 4 s, 2.4 s CPU, 400 MB peak |
| Codex limits and free resets | `codex app-server`, method `account/rateLimits/read` | about 3 s, 0.3 s CPU, 150 MB peak |
| Antigravity limits, both pools | `agy -p=/usage --output-format json`. The percentages come from the table agy prints. | 7 to 13 s, 1 s CPU, 150 MB peak |
| Claude sessions | `~/.claude/sessions/<pid>.json` lists live sessions with their title and busy/idle state. The end of each transcript in `~/.claude/projects` gives context and cache. | file reads only |
| Codex sessions | Rollouts in `~/.codex/sessions/YYYY/MM/DD` written in the last 6 hours | file reads only |

The three CLIs run 20 s apart so their memory peaks don't overlap. quotatop itself uses 8 to 9 MB of memory and about 0.13% of one CPU core, measured over 150 s on an M1 MacBook Air. Almost all of the cost is the CLIs.

## How each value is worked out

**Context** is the input tokens of the session's last API request (for Claude, input plus cache reads plus cache writes). For Codex it is `last_token_usage.total_tokens`.

**Context window.** Claude: from Claude Code's model docs. Fable 5+, Sonnet 5+, Haiku 5.5+ and Opus 4.7+ have 1M by default; older models have 200K unless the ID ends in `[1m]`. `CLAUDE_CODE_DISABLE_1M_CONTEXT` and `CLAUDE_CODE_MAX_CONTEXT_TOKENS` are honoured. Codex: `model_context_window` from the log, else `context_window × effective_context_window_percent` from `~/.codex/models_cache.json`.

**Auto-compact point.** Claude, highest precedence first: `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `modelSettings.<model>.autoCompactWindow`, then `autoCompactWindow`, read from the session folder's `.claude/settings.local.json` and `.claude/settings.json`, then `~/.claude/settings.json`. The value is capped at the window. With none set, a 1M model compacts at "about 967K" according to the docs, so quotatop shows `≈967k`. Codex: `model_auto_compact_token_limit` from `config.toml`, capped at the window. If it is not set, quotatop shows the point as unknown instead of guessing Codex's default.

**Claude cache.** The transcript records, for each response, how many tokens were read from and written to the cache and whether the write used the 1 h or 5 m TTL. The cache expires one TTL after the last request was sent. quotatop takes the send time from the last user entry before that response's first block, because a long response can be written minutes after its request. These values carry `≈`. "If it goes cold" is the size of the prefix cached now. The next request re-sends at least that much, so the screen says "at least".

**"Not started".** A window with 0% used whose reset time is exactly one window length after the reading has not begun. Antigravity reports unused windows this way, and the reset time moves forward with every reading.

**"Most room now"** lists each pool by the fuller of its 5-hour and weekly windows, emptiest first. Pools with stale readings are left out.

**Staleness.** Each section shows the age of its reading. After 2 minutes without a good reading (or two refresh intervals, if longer) it says "⚠ stale" and gives the last error. A missing value shows `?`, never a default.

## Exact cache numbers for terminal sessions

Claude Code sends exact `prompt_cache` and `context_window` data to the status line. It only does this for `claude` running in a terminal; Claude desktop app sessions do not run the status line. To use that data, make `quotatop tap` your status line command. It saves the JSON to `~/.cache/quotatop/tap/<session>.json` (on Windows, `%LOCALAPPDATA%\quotatop\tap`), passes the same JSON to your own status line command, and prints that command's output:

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
- **Auto-compact turned off in `/config`.** That setting lives in `~/.claude.json`, which also holds account details, so quotatop does not read it. `DISABLE_AUTO_COMPACT` is honoured.
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

The parsers are tested against real `/usage` output captured on macOS, with account IDs removed.
