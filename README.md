# ccost

Rust TUI for browsing local Codex and Claude Code session logs, searching past chats, and estimating API-equivalent cost.

https://github.com/user-attachments/assets/0869bcde-96be-4d98-9c07-c0586b0ea36a

## Install

```bash
brew install --cask peterxcli/ccost/ccost
```

## Run

```bash
ccost --sessions ~/.codex/sessions
```

For Claude Code transcripts:

```bash
ccost --sessions ~/.claude/projects
```

Default session directory: `$CODEX_HOME/sessions`, or `~/.codex/sessions` when `CODEX_HOME` is unset.

## Features

- Fast persisted full-text search with an FST term index, prefix matching, match highlighting, and visible search cursor.
- Persisted Merkle tree and file watcher; startup reuses cached sessions and live changes only re-index changed session files.
- Search mode is explicit: `/` starts typing, `Enter` returns to browse, so query text can include browse shortcut keys.
- Sort by total cost, time, tokens, web searches, model, session id, or first prompt. Default is total cost descending.
- `index.lock` allows one cache writer. A second TUI prompts for read-only, quit, or force-write with an explicit corruption warning.
- Cache files are disposable. Session JSONL files are the source of truth; if cache format/corruption is detected, delete the cache folder shown in the TUI.

## Controls

- `/`: search mode
- `Enter`: browse mode, or toggle detail while browsing
- `Up` / `Down` or `j` / `k`: move selection
- `Tab`: switch list/detail focus
- `s`: next sort key
- `S`: reverse sort direction
- `r`: reload
- `Esc`: clear search/back
- `q`: quit

## Options

```bash
ccost [--sessions PATH] [--pricing PATH] [--no-web-cost] [--read-only-index] [--force-index]
```

- `--read-only-index`: open without writing the persisted search cache.
- `--force-index`: write without the lock. Use only after confirming no other TUI is running.

## Pricing

Built-in pricing includes GPT-5.5, GPT-5.4, Claude Opus/Sonnet/Haiku model families, and web-search defaults. Override with `--pricing pricing.json`:

```json
{
  "web_search_per_1k": 10.0,
  "models": {
    "gpt-5.5": {
      "input_per_m": 5.0,
      "cache_creation_input_per_m": 0.0,
      "cached_input_per_m": 0.5,
      "output_per_m": 30.0,
      "long_context_threshold": 272000,
      "long_context_input_multiplier": 2.0,
      "long_context_output_multiplier": 1.5
    },
    "gpt-5.4": {
      "input_per_m": 2.5,
      "cache_creation_input_per_m": 0.0,
      "cached_input_per_m": 0.25,
      "output_per_m": 15.0,
      "long_context_threshold": 272000,
      "long_context_input_multiplier": 2.0,
      "long_context_output_multiplier": 1.5
    },
    "claude-sonnet-4-5": {
      "input_per_m": 3.0,
      "cache_creation_input_per_m": 3.75,
      "cached_input_per_m": 0.30,
      "output_per_m": 15.0
    }
  }
}
```

For older pricing overrides, `long_context_multiplier` is still accepted and applies to all token classes when the input/output-specific fields are omitted. `cache_creation_input_per_m` is optional and defaults to `0.0`.
