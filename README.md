# codex-cost-tui

Rust terminal UI for browsing local Codex session logs and estimating API-equivalent cost.

The loader handles both Codex Desktop/CLI rollout JSONL token-count events and saved Codex exec/result usage records.

## Run

```bash
cargo run --release -- --sessions ~/.codex/sessions
```

The default sessions folder is `$CODEX_HOME/sessions` when `CODEX_HOME` is set, otherwise `~/.codex/sessions`.

## Controls

- Type to search across session id, path, model, cwd, first prompt, and raw session text.
- `Up` / `Down` or `j` / `k`: move selection.
- `Enter`: toggle detail view.
- `Tab`: switch list/detail focus.
- `r`: reload session files.
- `Esc`: clear search or return to list.
- `q`: quit.

## Pricing

Built-in pricing currently includes GPT-5.5 defaults:

- input: `$5.00 / 1M`
- cached input: `$0.50 / 1M`
- output: `$30.00 / 1M`
- web search: `$10.00 / 1K calls`

You can override pricing with JSON:

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

Then run:

```bash
cargo run --release -- --pricing pricing.json
```
