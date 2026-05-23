# Refactor Invariants

The modernization refactor must preserve these behaviors:

- `codex_cost::run()` remains the only public library API.
- The binary still starts through `src/main.rs` and calls `codex_cost::run()`.
- CLI flags and errors remain stable: `--sessions`, `--pricing`, `--no-web-cost`, `--read-only-index`, `--force-index`, `--help`, `--version`.
- Cache schema version and persisted cache file names remain stable unless a dedicated migration task changes them.
- Search semantics remain prefix-based and continue indexing session metadata plus searchable user/assistant/goal/error fields.
- Parser behavior remains compatible with existing JSONL event shapes covered by tests.
- TUI key bindings remain stable.
- Index locking remains one-writer with read-only/force fallback.

