# Modernize Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Modernize the Rust codebase without changing user-visible behavior by tightening module boundaries, moving crate-scope functions behind owner types, and adding RAII lifecycle wrappers.

**Architecture:** Keep `codex_cost::run()` and the CLI behavior stable. Refactor from broad module reexports and crate-scope functions toward focused owner structs: `CacheStore`, `SessionParser`, `IndexWorker`, `UiRenderer`, and RAII guards. Pure generic helpers may remain in `src/util.rs`; domain behavior should live on a type or inside its owning module.

**Tech Stack:** Rust 2021, anyhow, crossterm, ratatui, notify, fst, serde, existing `cargo test --locked` suite, clippy as structural feedback.

---

## Pre-Flight Notes

- The local checkout previously had read-only `.git` metadata. If commits are required, use a writable clone under `/private/tmp` or fix the local `.git` permissions first.
- Start implementation from a fresh `origin/main` checkout because the current local branch reports divergence.
- Do not include README, demo, release workflow, dependency upgrade, signing, notarization, or Homebrew release changes in this refactor batch.
- Keep `pub use cli::run;` as the only public crate API unless a later task explicitly changes that contract.

## File Structure Target

- `src/main.rs`: binary entrypoint only.
- `src/lib.rs`: module declarations plus the stable public `run` reexport. No wildcard crate reexports.
- `src/cli.rs`: `Args`, CLI parsing, default paths, index mode selection.
- `src/app.rs`: `App`, focus/input/sort state, key handling through small internal command helpers.
- `src/parser.rs`: `SessionParser` and parser state. Free wrapper functions only where required by current call sites/tests.
- `src/cache.rs`: temporary compatibility module during first pass. Later split into `src/cache/store.rs`, `src/cache/merkle.rs`, `src/cache/postings.rs`, and `src/cache/reconcile.rs`.
- `src/worker.rs`: `IndexWorker`, `IndexLock`, watcher loop, load progress/messages.
- `src/ui.rs`: `UiRenderer`, `TerminalGuard`, display formatting helpers.
- `src/search.rs`: `SearchIndex` methods and search token helpers owned by `SearchIndex` where practical.
- `src/pricing.rs`: `Pricing`, `CostEstimate`, and cost calculation.
- `src/util.rs`: generic filesystem/hash/time/atomic-write helpers only.
- `src/tests.rs`: compatibility test module for the first pass. Split in a later pass after production boundaries settle.

## Task 1: Create Refactor Invariants And Baseline

**Files:**
- Create: `docs/refactor-invariants.md`
- Read: `src/tests.rs`

- [ ] **Step 1: Write the invariants document**

Create `docs/refactor-invariants.md` with:

```markdown
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
```

- [ ] **Step 2: Run baseline tests**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
```

Expected: all existing tests pass.

- [ ] **Step 3: Run baseline structural lint**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo clippy --all-targets --locked -- -W dead_code -W clippy::wildcard_imports
```

Expected before this task's code changes: wildcard-import warnings from `src/lib.rs` are present; no dead-code warnings block the run.

- [ ] **Step 4: Commit**

```bash
git add docs/refactor-invariants.md docs/superpowers/plans/2026-05-23-modernize-refactor.md
git commit -m "docs: add refactor invariants and plan"
```

## Task 2: Remove Wildcard Reexports

**Files:**
- Modify: `src/lib.rs`
- Modify import lists in: `src/app.rs`, `src/cache.rs`, `src/cli.rs`, `src/parser.rs`, `src/pricing.rs`, `src/search.rs`, `src/ui.rs`, `src/util.rs`, `src/worker.rs`, `src/tests.rs`

- [ ] **Step 1: Replace wildcard reexports in `src/lib.rs`**

Change `src/lib.rs` from:

```rust
pub(crate) use app::*;
pub(crate) use cache::*;
pub(crate) use models::*;
pub(crate) use parser::*;
pub(crate) use pricing::*;
pub(crate) use search::*;
pub(crate) use ui::*;
pub(crate) use util::*;
pub(crate) use worker::*;
```

to:

```rust
pub use cli::run;
```

Keep the `mod` declarations and `#[cfg(test)] mod tests;`.

- [ ] **Step 2: Import concrete owner modules at call sites**

For example, in `src/cli.rs`, replace root imports like:

```rust
use crate::{cache_dir_for_sessions, index_lock_path, run_tui, App, IndexLock, IndexWorkerMode, Pricing};
```

with module-owned imports:

```rust
use crate::app::App;
use crate::cache::cache_dir_for_sessions;
use crate::pricing::Pricing;
use crate::ui::run_tui;
use crate::worker::{index_lock_path, IndexLock, IndexWorkerMode};
```

Apply the same pattern throughout the codebase. Use `crate::<module>::<item>` imports, not crate-root compatibility reexports.

- [ ] **Step 3: Run wildcard lint**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo clippy --all-targets --locked -- -W clippy::wildcard_imports
```

Expected: no wildcard-import warnings from production modules.

- [ ] **Step 4: Run behavior tests**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "refactor: remove crate wildcard reexports"
```

## Task 3: Introduce `SessionParser`

**Files:**
- Modify: `src/parser.rs`
- Modify: `src/cache.rs`
- Modify: `src/tests.rs` only if tests need direct associated-function access

- [ ] **Step 1: Add a parser owner type**

Add to `src/parser.rs`:

```rust
pub(crate) struct SessionParser;
```

- [ ] **Step 2: Move parser entrypoints into associated functions**

Move the current free parser entrypoints into `impl SessionParser`:

```rust
impl SessionParser {
    pub(crate) fn parse(path: &Path) -> Result<Session> {
        let (session, _fingerprint) = Self::parse_inner(path, None)?;
        Ok(session)
    }

    pub(crate) fn parse_with_fingerprint(
        path: &Path,
        relative_path: &str,
        metadata: FileMetadataParts,
    ) -> Result<ParsedSessionFile> {
        let (session, fingerprint) = Self::parse_inner(path, Some((relative_path, metadata)))?;
        Ok(ParsedSessionFile {
            session,
            fingerprint: fingerprint.expect("fingerprint requested"),
        })
    }
}
```

Keep temporary wrappers if needed:

```rust
pub(crate) fn parse_session(path: &Path) -> Result<Session> {
    SessionParser::parse(path)
}
```

Remove wrappers after all call sites use `SessionParser`.

- [ ] **Step 3: Move parser helper functions behind the owner**

Convert parser-only helpers to private associated functions, for example:

```rust
impl SessionParser {
    fn extract_message_text(payload: &Value) -> String {
        // existing body
    }
}
```

Call them with `Self::extract_message_text(...)`. Keep generic JSON helpers in `models.rs` or `util.rs` only if shared outside parser.

- [ ] **Step 4: Run parser-focused tests**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked parses_headless_exec_usage_records derives_last_usage_from_cumulative_token_count parses_first_human_prompt_after_environment_context indexes_event_user_message_text parse_session_with_fingerprint_matches_file_content_hash
```

Expected: all named tests pass.

- [ ] **Step 5: Run full tests and commit**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
git add src/parser.rs src/cache.rs src/tests.rs
git commit -m "refactor: move session parsing behind owner type"
```

## Task 4: Introduce `CacheStore`

**Files:**
- Modify: `src/cache.rs`
- Modify: `src/cli.rs`
- Modify: `src/app.rs`
- Modify: `src/worker.rs`
- Modify: `src/tests.rs`

- [ ] **Step 1: Add cache owner type**

Add to `src/cache.rs`:

```rust
#[derive(Clone, Debug)]
pub(crate) struct CacheStore {
    root: PathBuf,
    cache_dir: PathBuf,
}

impl CacheStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        let cache_dir = cache_dir_for_sessions(&root);
        Self { root, cache_dir }
    }

    pub(crate) fn with_cache_dir(root: PathBuf, cache_dir: PathBuf) -> Self {
        Self { root, cache_dir }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}
```

- [ ] **Step 2: Move cache operations into methods**

Add methods that delegate to current implementations first:

```rust
impl CacheStore {
    pub(crate) fn load_cached_index(&self) -> Result<Option<LoadResult>> {
        load_cached_index(&self.root, &self.cache_dir)
    }

    pub(crate) fn reconcile<F>(&self, progress: F) -> Result<LoadResult>
    where
        F: FnMut(LoadProgress),
    {
        reconcile_session_cache(&self.root, &self.cache_dir, progress)
    }

    pub(crate) fn reconcile_paths<F>(&self, paths: BTreeSet<PathBuf>, progress: F) -> Result<LoadResult>
    where
        F: FnMut(LoadProgress),
    {
        reconcile_session_cache_for_paths(&self.root, &self.cache_dir, paths, progress)
    }
}
```

After call sites move, make the old free functions private or delete them if not needed by tests.

- [ ] **Step 3: Update call sites**

Replace path pairs like:

```rust
load_cached_index(root, cache_dir)
reconcile_session_cache(root, cache_dir, progress)
```

with:

```rust
store.load_cached_index()
store.reconcile(progress)
```

Where a component already stores both `sessions_dir` and `cache_dir`, store `CacheStore` instead in a later pass if that change stays small. In this pass, method calls may construct `CacheStore::with_cache_dir(...)` locally.

- [ ] **Step 4: Run cache tests**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked persisted_index_reuses_cached_snapshot_and_fst targeted_reconcile_updates_changed_session_postings targeted_reconcile_adds_new_session_without_losing_old_postings stale_schema_cache_reports_delete_hint
```

Expected: all named tests pass.

- [ ] **Step 5: Run full tests and commit**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
git add src/cache.rs src/cli.rs src/app.rs src/worker.rs src/tests.rs
git commit -m "refactor: introduce cache store owner"
```

## Task 5: Introduce `IndexWorker`

**Files:**
- Modify: `src/worker.rs`
- Modify: `src/app.rs`
- Modify: `src/tests.rs`

- [ ] **Step 1: Add worker owner type**

Add:

```rust
pub(crate) struct IndexWorker {
    root: PathBuf,
    cache_dir: PathBuf,
    tx: Sender<LoadMessage>,
    mode: IndexWorkerMode,
}

impl IndexWorker {
    pub(crate) fn new(
        root: PathBuf,
        cache_dir: PathBuf,
        tx: Sender<LoadMessage>,
        mode: IndexWorkerMode,
    ) -> Self {
        Self { root, cache_dir, tx, mode }
    }

    pub(crate) fn run(self) {
        run_index_worker(self.root, self.cache_dir, self.tx, self.mode);
    }
}
```

- [ ] **Step 2: Update thread launch**

In `App::start_reload_with_mode`, replace:

```rust
run_index_worker(sessions_dir, cache_dir, tx.clone(), index_worker_mode);
```

with:

```rust
IndexWorker::new(sessions_dir, cache_dir, tx.clone(), index_worker_mode).run();
```

- [ ] **Step 3: Move worker helpers into methods where low-risk**

Keep free wrappers only if tests use them directly. Prefer private methods:

```rust
impl IndexWorker {
    fn run_writer(...)
    fn run_readonly(...)
}
```

- [ ] **Step 4: Run worker tests**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked index_lock_allows_only_one_writer index_lock_records_owner_pid writer_does_not_hide_incompatible_cache_error_with_watcher_status
```

Expected: all named tests pass.

- [ ] **Step 5: Run full tests and commit**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
git add src/worker.rs src/app.rs src/tests.rs
git commit -m "refactor: move index work behind worker type"
```

## Task 6: Add TUI RAII Guard

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Add `TerminalGuard`**

Add:

```rust
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
```

- [ ] **Step 2: Simplify `run_tui`**

Replace manual enter/exit cleanup with:

```rust
pub(crate) fn run_tui(mut app: App) -> Result<()> {
    let mut guard = TerminalGuard::enter()?;

    loop {
        app.poll_loader();
        guard.terminal_mut().draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key)? {
                    break;
                }
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Run TUI-adjacent tests**

Run:

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked slash_enters_search_mode_and_enter_returns_to_browse search_mode_treats_browse_shortcuts_as_query_text browse_mode_shortcuts_do_not_edit_search_query search_cursor_position_points_after_query_in_search_mode
```

Expected: all named tests pass.

- [ ] **Step 4: Run full tests and commit**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
git add src/ui.rs
git commit -m "refactor: guard terminal lifecycle with RAII"
```

## Task 7: Simplify App Key Handling

**Files:**
- Modify: `src/app.rs`
- Modify: `src/tests.rs` only for direct signature changes

- [ ] **Step 1: Extract directional movement helper**

Add:

```rust
impl App {
    fn move_focused(&mut self, delta: isize) {
        if self.focus == Focus::Detail {
            self.move_detail(delta);
        } else {
            self.move_selection(delta);
        }
    }
}
```

- [ ] **Step 2: Merge duplicate match arms**

Replace duplicated movement arms in `handle_key`:

```rust
KeyCode::Up | KeyCode::Char('k') => self.move_focused(-1),
KeyCode::Down | KeyCode::Char('j') => self.move_focused(1),
KeyCode::PageUp => self.move_focused(-10),
KeyCode::PageDown => self.move_focused(10),
```

Remove redundant empty `KeyCode::Char(_) => {}` if wildcard arm already handles it.

- [ ] **Step 3: Keep return type stable unless changing all call sites**

Keep `handle_key(&mut self, key: KeyEvent) -> Result<bool>` for this pass to minimize churn. A later pass may remove the unnecessary `Result`.

- [ ] **Step 4: Run key tests**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked slash_enters_search_mode_and_enter_returns_to_browse search_mode_treats_browse_shortcuts_as_query_text browse_mode_shortcuts_do_not_edit_search_query default_sort_is_total_cost_descending browse_mode_cycles_sort_key_with_s browse_mode_reverses_sort_direction_with_shift_s
```

Expected: all named tests pass.

- [ ] **Step 5: Run full tests and commit**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
git add src/app.rs src/tests.rs
git commit -m "refactor: simplify app key handling"
```

## Task 8: Split Tests By Domain

**Files:**
- Delete: `src/tests.rs`
- Create: `src/tests/mod.rs`
- Create: `src/tests/parser.rs`
- Create: `src/tests/cache.rs`
- Create: `src/tests/app.rs`
- Create: `src/tests/search.rs`
- Create: `src/tests/pricing.rs`
- Create: `src/tests/support.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Convert test module to directory module**

Keep in `src/lib.rs`:

```rust
#[cfg(test)]
mod tests;
```

Create `src/tests/mod.rs`:

```rust
mod app;
mod cache;
mod parser;
mod pricing;
mod search;
mod support;
```

- [ ] **Step 2: Move shared fixtures**

Move `temp_dir`, `session_for_search`, `app_for_key_tests`, `key_char`, `session_with_cost`, `app_with_sort_sessions`, `filtered_ids`, and `assert_cost_close` into `src/tests/support.rs` as appropriate. Use `pub(super)` visibility for shared helpers.

- [ ] **Step 3: Move tests by domain without changing assertions**

Move parser tests to `src/tests/parser.rs`, cache/worker tests to `src/tests/cache.rs`, key/sort tests to `src/tests/app.rs`, search tests to `src/tests/search.rs`, pricing tests to `src/tests/pricing.rs`.

- [ ] **Step 4: Run full tests**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
```

Expected: all tests pass with the same test names or clearly equivalent module-qualified names.

- [ ] **Step 5: Commit**

```bash
git add src/tests src/lib.rs
git rm src/tests.rs
git commit -m "refactor: split tests by domain"
```

## Task 9: Optional Clippy Cleanup Pass

**Files:**
- Modify as needed: `src/app.rs`, `src/cache.rs`, `src/parser.rs`, `src/pricing.rs`, `src/ui.rs`, `src/util.rs`, `src/worker.rs`

- [ ] **Step 1: Apply no-behavior clippy cleanups**

Allowed cleanups:

```rust
option.map(predicate).unwrap_or(false)
```

to:

```rust
option.is_some_and(predicate)
```

and:

```rust
option.map(format_tokens).unwrap_or_else(|| "-".to_string())
```

to:

```rust
option.map_or_else(|| "-".to_string(), format_tokens)
```

and duplicate match arms into merged patterns.

- [ ] **Step 2: Do not rename serialized fields**

Do not change `TokenUsage` field names despite `struct_field_names`; those names reflect external JSON/token terminology.

- [ ] **Step 3: Run clippy and tests**

```bash
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo clippy --all-targets --locked -- -W dead_code -W clippy::wildcard_imports
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
```

Expected: no dead-code or wildcard-import warnings; all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src
git commit -m "refactor: clean low-risk clippy findings"
```

## Out Of Scope Migration Tasks

Split these into separate migration tasks:

- Dependency upgrades.
- Public CLI behavior changes.
- Cache schema changes.
- Parser semantic changes for new event formats.
- Async runtime adoption.
- Release workflow, signing, notarization, or Homebrew tap changes.
- A full ratatui/crossterm version migration.

## Final Verification

Run:

```bash
cargo fmt -- --check
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo test --locked
HOME=/private/tmp/codex-cost-home RUSTUP_HOME=/Users/lixucheng/.rustup CARGO_HOME=/Users/lixucheng/.cargo cargo clippy --all-targets --locked -- -W dead_code -W clippy::wildcard_imports
```

Expected:

- Formatting check exits 0.
- Tests pass.
- No dead-code or wildcard-import warnings.
