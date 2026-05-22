# codex-cost

Rust TUI for browsing local Codex session logs, searching past chats, and estimating API-equivalent cost.

https://github.com/user-attachments/assets/0869bcde-96be-4d98-9c07-c0586b0ea36a

## Install

```bash
brew install --cask peterxcli/tap/codex-cost
```

## Run

```bash
cargo run --release -- --sessions ~/.codex/sessions
```

Default session directory: `$CODEX_HOME/sessions`, or `~/.codex/sessions` when `CODEX_HOME` is unset.

## Features

- Fast persisted full-text search with an FST term index, prefix matching, match highlighting, and visible search cursor.
- Persisted Merkle tree and file watcher; startup reuses cached sessions and live changes only re-index changed session files.
- Search mode is explicit: `/` starts typing, `Enter` returns to browse, so query text can include browse shortcut keys.
- Sort by total cost, time, tokens, web searches, model, session id, or first prompt. Default is total cost descending.
- `index.lock` allows one cache writer. A second TUI prompts for read-only, quit, or force-write with an explicit corruption warning.
- Cache files are disposable. Codex session JSONL files are the source of truth; if cache format/corruption is detected, delete the cache folder shown in the TUI.

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
codex-cost [--sessions PATH] [--pricing PATH] [--no-web-cost] [--read-only-index] [--force-index]
```

- `--read-only-index`: open without writing the persisted search cache.
- `--force-index`: write without the lock. Use only after confirming no other TUI is running.

## Pricing

Built-in pricing includes GPT-5.5 token and web-search defaults. Override with `--pricing pricing.json`:

```json
{
  "web_search_per_1k": 10.0,
  "models": {
    "gpt-5.5": {
      "input_per_m": 5.0,
      "cached_input_per_m": 0.5,
      "output_per_m": 30.0,
      "long_context_threshold": 272000,
      "long_context_multiplier": 2.0
    }
  }
}
```

## Release

Releases are tag-driven. Update `Cargo.toml`, commit the change, then push a matching `vX.Y.Z` tag:

```bash
git tag v0.1.1
git push origin v0.1.1
```

The release workflow builds the macOS arm64 binary, publishes a GitHub release from `peterxcli/codex-cost`, publishes the Homebrew-downloadable tarball to `peterxcli/homebrew-tap`, and updates the tap cask.

Repository setup requirement: add a `HOMEBREW_TAP_TOKEN` secret with permission to push to `peterxcli/homebrew-tap`.
