use crate::app::{App, Focus, InputMode, SortDirection, SortKey};
use crate::cache::{
    build_merkle_snapshot, cache_dir_for_sessions, load_sessions_with_progress, CacheManifest,
    CacheStore, CACHE_SCHEMA_VERSION,
};
use crate::models::{GoalUsage, Session, TokenEvent, TokenUsage};
use crate::parser::SessionParser;
use crate::pricing::{estimate_cost, Pricing};
use crate::search::SearchIndex;
use crate::ui::{
    highlight_matches, match_highlight_style, search_cursor_position, session_table_empty_message,
};
use crate::util::{file_metadata_parts, hash_hex, relative_path_string, write_json_atomic};
use crate::worker::{
    index_lock_path, run_index_worker, IndexLaunchMode, IndexLock, IndexWorkerMode, LoadMessage,
    LoadPhase,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::{Rect, Style};
use ratatui::widgets::{ListState, TableState};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("codex-cost-{name}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_cost_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 0.000_001,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn parses_headless_exec_usage_records() {
    let dir = temp_dir("exec");
    let path = dir.join("run.jsonl");
    fs::write(
        &path,
        [
            json!({
                "type": "turn.completed",
                "timestamp": "2026-01-02T03:04:05.000Z",
                "model": "gpt-5.2-codex",
                "usage": {
                    "input_tokens": 120,
                    "cached_input_tokens": 20,
                    "output_tokens": 30,
                    "total_tokens": 150
                }
            })
            .to_string(),
            json!({
                "type": "result",
                "data": {
                    "timestamp": "2026-01-02T03:05:05.000Z",
                    "model_name": "gpt-5.2-codex",
                    "usage": {
                        "prompt_tokens": 50,
                        "cached_tokens": 5,
                        "completion_tokens": 12
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .unwrap();

    let session = SessionParser::parse(&path).unwrap();
    let final_usage = session.final_usage().unwrap();

    assert_eq!(session.token_events.len(), 2);
    assert_eq!(session.model.as_deref(), Some("gpt-5.2-codex"));
    assert_eq!(session.token_events[0].last.input_tokens, 120);
    assert_eq!(session.token_events[1].last.input_tokens, 50);
    assert_eq!(final_usage.input_tokens, 170);
    assert_eq!(final_usage.cached_input_tokens, 25);
    assert_eq!(final_usage.output_tokens, 42);
    assert_eq!(final_usage.total_tokens, 212);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn derives_last_usage_from_cumulative_token_count() {
    let dir = temp_dir("cumulative");
    let path = dir.join("rollout-test.jsonl");
    fs::write(
        &path,
        [
            json!({
                "timestamp": "2026-01-02T00:00:00.000Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5.5"}
            })
            .to_string(),
            json!({
                "timestamp": "2026-01-02T00:00:01.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 10,
                            "output_tokens": 20,
                            "reasoning_output_tokens": 5,
                            "total_tokens": 120
                        }
                    }
                }
            })
            .to_string(),
            json!({
                "timestamp": "2026-01-02T00:00:02.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 180,
                            "cached_input_tokens": 60,
                            "output_tokens": 30,
                            "reasoning_output_tokens": 8,
                            "total_tokens": 210
                        }
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .unwrap();

    let session = SessionParser::parse(&path).unwrap();

    assert_eq!(session.token_events.len(), 2);
    assert_eq!(session.token_events[1].last.input_tokens, 80);
    assert_eq!(session.token_events[1].last.cached_input_tokens, 50);
    assert_eq!(session.token_events[1].last.output_tokens, 10);
    assert_eq!(session.token_events[1].last.reasoning_output_tokens, 3);
    assert_eq!(session.final_usage().unwrap().total_tokens, 210);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parses_first_human_prompt_after_environment_context() {
    let dir = temp_dir("human-prompt");
    let path = dir.join("rollout-human.jsonl");
    fs::write(
            &path,
            [
                json!({
                    "timestamp": "2026-01-02T00:00:00.000Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "<environment_context>\n  <cwd>/tmp/project</cwd>\n</environment_context>"
                        }]
                    }
                })
                .to_string(),
                json!({
                    "timestamp": "2026-01-02T00:00:01.000Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello"}]
                    }
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

    let session = SessionParser::parse(&path).unwrap();
    let index = SearchIndex::build(std::slice::from_ref(&session), |_current, _total| {});

    assert_eq!(session.first_user_message.as_deref(), Some("hello"));
    assert_eq!(index.search("hello"), vec![0]);
    assert!(index.search("environment_context").is_empty());

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn indexes_event_user_message_text() {
    let dir = temp_dir("event-user-message");
    let path = dir.join("rollout-event-user.jsonl");
    fs::write(
        &path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "message": "hello from event stream",
                "images": [],
                "local_images": []
            }
        })
        .to_string(),
    )
    .unwrap();

    let session = SessionParser::parse(&path).unwrap();
    let index = SearchIndex::build(std::slice::from_ref(&session), |_current, _total| {});

    assert_eq!(
        session.first_user_message.as_deref(),
        Some("hello from event stream")
    );
    assert_eq!(index.search("hello event"), vec![0]);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn load_sessions_with_progress_reports_parsing_and_indexing() {
    let dir = temp_dir("load-progress");
    let first_path = dir.join("first.jsonl");
    let second_path = dir.join("second.jsonl");
    fs::write(
        &first_path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "alpha marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        &second_path,
        json!({
            "timestamp": "2026-01-03T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "beta marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();
    let mut updates = Vec::new();

    let result = load_sessions_with_progress(&dir, |progress| {
        updates.push((progress.phase, progress.current, progress.total));
    })
    .unwrap();

    assert_eq!(result.sessions.len(), 2);
    assert_eq!(result.search_index.search("alpha marker").len(), 1);
    assert!(updates.contains(&(LoadPhase::Parsing, 1, 2)));
    assert!(updates.contains(&(LoadPhase::Parsing, 2, 2)));
    assert_eq!(updates.last(), Some(&(LoadPhase::Indexing, 2, 2)));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn merkle_tree_detects_added_modified_and_deleted_jsonl_files() {
    let dir = temp_dir("merkle");
    let first_path = dir.join("first.jsonl");
    let second_path = dir.join("nested").join("second.jsonl");
    fs::create_dir_all(second_path.parent().unwrap()).unwrap();
    fs::write(&first_path, "alpha").unwrap();
    fs::write(&second_path, "beta").unwrap();

    let first = build_merkle_snapshot(&dir, None).unwrap();
    assert_eq!(first.fingerprints.len(), 2);
    assert!(first.changed_paths.contains("first.jsonl"));
    assert!(first.changed_paths.contains("nested/second.jsonl"));
    assert!(first.deleted_paths.is_empty());

    fs::write(&first_path, "alpha changed").unwrap();
    let added_path = dir.join("third.jsonl");
    fs::write(&added_path, "gamma").unwrap();
    fs::remove_file(&second_path).unwrap();

    let second = build_merkle_snapshot(&dir, Some(&first)).unwrap();
    assert_ne!(first.root.hash, second.root.hash);
    assert!(second.changed_paths.contains("first.jsonl"));
    assert!(second.changed_paths.contains("third.jsonl"));
    assert!(second.deleted_paths.contains("nested/second.jsonl"));
    assert!(!second.fingerprints.contains_key("nested/second.jsonl"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parse_session_with_fingerprint_matches_file_content_hash() {
    let dir = temp_dir("parse-fingerprint");
    let path = dir.join("session.jsonl");
    let content = [
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "single pass marker"}]
            }
        })
        .to_string(),
        json!({
            "timestamp": "2026-01-02T00:00:01.000Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 3,
                        "output_tokens": 4,
                        "total_tokens": 7
                    }
                }
            }
        })
        .to_string(),
    ]
    .join("\n");
    fs::write(&path, &content).unwrap();

    let parsed = SessionParser::parse_with_fingerprint(
        &path,
        "session.jsonl",
        file_metadata_parts(&path).unwrap(),
    )
    .unwrap();

    assert_eq!(
        parsed.session.first_user_message.as_deref(),
        Some("single pass marker")
    );
    assert_eq!(
        parsed.fingerprint.content_hash,
        hash_hex(content.as_bytes())
    );
    assert_eq!(parsed.fingerprint.size, content.len() as u64);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[cfg(unix)]
fn relative_path_accepts_canonical_watcher_path_under_symlink_root() {
    let dir = temp_dir("relative-symlink-root");
    let real_root = dir.join("real");
    let link_root = dir.join("link");
    let nested = real_root.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let file = nested.join("session.jsonl");
    fs::write(&file, "{}").unwrap();
    std::os::unix::fs::symlink(&real_root, &link_root).unwrap();
    let canonical_file = file.canonicalize().unwrap();

    let relative = relative_path_string(&link_root, &canonical_file).unwrap();

    assert_eq!(relative, "nested/session.jsonl");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn persisted_index_reuses_cached_snapshot_and_fst() {
    let dir = temp_dir("persisted-index");
    let cache_dir = dir.join(".cache");
    let sessions_dir = dir.join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let first_path = sessions_dir.join("first.jsonl");
    fs::write(
        &first_path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "alpha cache marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();

    let cache_store = CacheStore::with_cache_dir(sessions_dir.clone(), cache_dir.clone());
    let first = cache_store.reconcile(|_progress| {}).unwrap();
    let cached = cache_store.load().unwrap().expect("cache should load");

    assert_eq!(cached.sessions.len(), 1);
    assert_eq!(cached.search_index.search("alpha cache"), vec![0]);
    assert!(cache_dir.join("manifest.json").exists());
    assert!(cache_dir.join("merkle.json").exists());
    assert!(cache_dir.join("sessions.json").exists());
    assert!(cache_dir.join("postings.json").exists());
    assert!(cache_dir.join("terms.fst").exists());

    fs::write(
        &first_path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "updated cache marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();
    let second = cache_store.reconcile(|_progress| {}).unwrap();

    assert!(second.generation > first.generation);
    assert_eq!(second.sessions.len(), 1);
    assert_eq!(second.search_index.search("updated cache"), vec![0]);
    assert!(second.search_index.search("alpha cache").is_empty());

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn targeted_reconcile_updates_changed_session_postings() {
    let dir = temp_dir("targeted-index");
    let cache_dir = dir.join(".cache");
    let sessions_dir = dir.join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let first_path = sessions_dir.join("first.jsonl");
    let second_path = sessions_dir.join("second.jsonl");
    fs::write(
        &first_path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "alpha marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        &second_path,
        json!({
            "timestamp": "2026-01-03T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "beta marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();

    let cache_store = CacheStore::with_cache_dir(sessions_dir.clone(), cache_dir.clone());
    cache_store.reconcile(|_progress| {}).unwrap();
    fs::write(
        &second_path,
        json!({
            "timestamp": "2026-01-03T00:00:00.000Z",
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "message": "gamma event marker"
            }
        })
        .to_string(),
    )
    .unwrap();
    let mut progress = Vec::new();
    let result = cache_store
        .reconcile_paths(BTreeSet::from([second_path.clone()]), |update| {
            if update.phase == LoadPhase::Parsing {
                progress.push((update.current, update.total));
            }
        })
        .unwrap();

    assert_eq!(progress, vec![(1, 1)]);
    assert_eq!(result.sessions.len(), 2);
    assert_eq!(result.search_index.search("gamma event").len(), 1);
    assert!(result.search_index.search("beta marker").is_empty());
    assert_eq!(result.search_index.search("alpha marker").len(), 1);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn targeted_reconcile_adds_new_session_without_losing_old_postings() {
    let dir = temp_dir("targeted-index-add");
    let cache_dir = dir.join(".cache");
    let sessions_dir = dir.join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let first_path = sessions_dir.join("first.jsonl");
    let second_path = sessions_dir.join("second.jsonl");
    fs::write(
        &first_path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "alpha marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();

    let cache_store = CacheStore::with_cache_dir(sessions_dir.clone(), cache_dir.clone());
    cache_store.reconcile(|_progress| {}).unwrap();
    fs::write(
        &second_path,
        json!({
            "timestamp": "2026-01-03T00:00:00.000Z",
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "message": "delta new session marker"
            }
        })
        .to_string(),
    )
    .unwrap();

    let mut progress = Vec::new();
    let result = cache_store
        .reconcile_paths(BTreeSet::from([second_path]), |update| {
            if update.phase == LoadPhase::Parsing {
                progress.push((update.current, update.total));
            }
        })
        .unwrap();

    assert_eq!(progress, vec![(1, 1)]);
    assert_eq!(result.sessions.len(), 2);
    assert_eq!(result.search_index.search("delta new"), vec![0]);
    assert_eq!(result.search_index.search("alpha marker"), vec![1]);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn index_lock_allows_only_one_writer() {
    let dir = temp_dir("index-lock");

    let first = IndexLock::try_acquire(&dir).unwrap();
    let second = IndexLock::try_acquire(&dir).unwrap();
    assert!(first.is_some());
    assert!(second.is_none());

    drop(first);
    let third = IndexLock::try_acquire(&dir).unwrap();
    assert!(third.is_some());

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn index_lock_records_owner_pid() {
    let dir = temp_dir("index-lock-owner");

    let _lock = IndexLock::try_acquire(&dir).unwrap().unwrap();
    let owner = fs::read_to_string(index_lock_path(&dir)).unwrap();

    assert!(owner.contains(&format!("pid={}", std::process::id())));

    fs::remove_dir_all(dir).unwrap();
}

fn app_for_key_tests() -> App {
    let mut app = App {
        sessions_dir: PathBuf::from("/tmp/sessions"),
        cache_dir: PathBuf::from("/tmp/cache"),
        pricing: Pricing::default(),
        include_web_cost: true,
        sessions: vec![
            session_for_search("session-alpha", "alpha prompt", "alpha raw"),
            session_for_search("session-beta", "beta prompt", "beta raw"),
        ],
        search_index: SearchIndex::default(),
        filtered: Vec::new(),
        query: String::new(),
        list_state: ListState::default(),
        table_state: TableState::default(),
        focus: Focus::List,
        input_mode: InputMode::Browse,
        show_detail: false,
        status: String::new(),
        last_reload: Instant::now(),
        loading: None,
        loader: None,
        sort_key: SortKey::TotalCost,
        sort_direction: SortDirection::Descending,
        index_launch_mode: IndexLaunchMode::AcquireLock,
    };
    app.search_index = SearchIndex::build(&app.sessions, |_current, _total| {});
    app.apply_filter();
    app
}

fn key_char(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

#[test]
fn slash_enters_search_mode_and_enter_returns_to_browse() {
    let mut app = app_for_key_tests();

    app.handle_key(key_char('/')).unwrap();
    assert_eq!(app.input_mode, InputMode::Search);

    app.handle_key(key_char('a')).unwrap();
    assert_eq!(app.query, "a");

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert_eq!(app.input_mode, InputMode::Browse);
    assert_eq!(app.query, "a");
}

#[test]
fn search_mode_treats_browse_shortcuts_as_query_text() {
    let mut app = app_for_key_tests();
    app.handle_key(key_char('/')).unwrap();

    for c in ['j', 'k', 'r', 'q', '/'] {
        app.handle_key(key_char(c)).unwrap();
    }

    assert_eq!(app.query, "jkrq/");
    assert_eq!(app.input_mode, InputMode::Search);
}

#[test]
fn browse_mode_shortcuts_do_not_edit_search_query() {
    let mut app = app_for_key_tests();

    app.handle_key(key_char('j')).unwrap();
    app.handle_key(key_char('r')).unwrap();

    assert!(app.query.is_empty());
    assert_eq!(app.input_mode, InputMode::Browse);
}

#[test]
fn default_pricing_estimates_gpt_5_4_short_context() {
    let mut session = session_for_search("gpt-54-short", "pricing", "pricing");
    session.model = Some("gpt-5.4".to_string());
    session.cached_final_usage = Some(TokenUsage {
        input_tokens: 1_000_000,
        cached_input_tokens: 200_000,
        output_tokens: 1_000_000,
        total_tokens: 2_000_000,
        ..TokenUsage::default()
    });

    let estimate = estimate_cost(&session, &Pricing::default(), false);

    assert!(estimate.known_model_price);
    assert!(!estimate.long_context_applied);
    assert_cost_close(estimate.total_cost, 17.05);
}

#[test]
fn default_pricing_estimates_gpt_5_4_long_context_with_separate_output_rate() {
    let mut session = session_for_search("gpt-54-long", "pricing", "pricing");
    session.model = Some("gpt-5.4".to_string());
    session.max_request_input_tokens = 272_001;
    session.cached_final_usage = Some(TokenUsage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        total_tokens: 2_000_000,
        ..TokenUsage::default()
    });

    let estimate = estimate_cost(&session, &Pricing::default(), false);

    assert!(estimate.known_model_price);
    assert!(estimate.long_context_applied);
    assert_cost_close(estimate.total_cost, 27.50);
}

#[test]
fn initial_app_state_does_not_read_cached_snapshot() {
    let dir = temp_dir("bad-cache-launch");
    let cache_dir = cache_dir_for_sessions(&dir);
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(
        cache_dir.join("manifest.json"),
        "{this is not valid cache json",
    )
    .unwrap();

    let app = App::initial_state(
        dir.clone(),
        Pricing::default(),
        true,
        IndexLaunchMode::AcquireLock,
    )
    .unwrap();

    assert_eq!(app.sessions_dir, dir);
    assert!(app.sessions.is_empty());
    assert!(app.filtered.is_empty());

    fs::remove_dir_all(app.cache_dir).unwrap();
    fs::remove_dir_all(app.sessions_dir).unwrap();
}

#[test]
fn empty_session_table_surfaces_incompatible_cache_error() {
    let mut app = App::initial_state(
        PathBuf::from("/tmp/sessions"),
        Pricing::default(),
        true,
        IndexLaunchMode::AcquireLock,
    )
    .unwrap();
    app.status = "reload failed: search index cache is incompatible (manifest.json has schema 4, expected 5); delete the cache folder and restart: /tmp/cache".to_string();

    let message = session_table_empty_message(&app).expect("empty table should show error");

    assert!(message.contains("search index cache is incompatible"));
    assert!(message.contains("delete the cache folder and restart"));
}

#[test]
fn stale_schema_cache_reports_delete_hint() {
    let dir = temp_dir("stale-schema-cache");
    let sessions_dir = dir.join("sessions");
    let cache_dir = dir.join("cache");
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(
        sessions_dir.join("first.jsonl"),
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "fresh cache marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();
    write_json_atomic(
        &cache_dir.join("manifest.json"),
        &CacheManifest {
            schema_version: CACHE_SCHEMA_VERSION - 1,
            generation: 10,
            sessions_root: sessions_dir.to_string_lossy().to_string(),
            merkle_root: "stale".to_string(),
            updated_at_unix_seconds: 0,
        },
    )
    .unwrap();
    fs::write(cache_dir.join("sessions.json"), "{not valid json").unwrap();

    let cache_store = CacheStore::with_cache_dir(sessions_dir.clone(), cache_dir.clone());
    let err = cache_store
        .reconcile(|_progress| {})
        .expect_err("stale schema cache should be rejected");
    let message = format!("{err:#}");

    assert!(message.contains("search index cache is incompatible"));
    assert!(message.contains("delete the cache folder and restart"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn writer_does_not_hide_incompatible_cache_error_with_watcher_status() {
    let dir = temp_dir("stale-schema-worker");
    let sessions_dir = dir.join("sessions");
    let cache_dir = dir.join("cache");
    fs::create_dir_all(&sessions_dir).unwrap();
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(
        sessions_dir.join("first.jsonl"),
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "fresh cache marker"}]
            }
        })
        .to_string(),
    )
    .unwrap();
    write_json_atomic(
        &cache_dir.join("manifest.json"),
        &CacheManifest {
            schema_version: CACHE_SCHEMA_VERSION - 1,
            generation: 10,
            sessions_root: sessions_dir.to_string_lossy().to_string(),
            merkle_root: "stale".to_string(),
            updated_at_unix_seconds: 0,
        },
    )
    .unwrap();

    let (tx, rx) = mpsc::channel();
    let worker_sessions_dir = sessions_dir.clone();
    let worker_cache_dir = cache_dir.clone();
    thread::spawn(move || {
        run_index_worker(
            worker_sessions_dir,
            worker_cache_dir,
            tx,
            IndexWorkerMode::Force,
        );
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_incompatible_error = false;
    let mut saw_watcher_status = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(LoadMessage::Loaded(Err(err))) => {
                if err.contains("search index cache is incompatible") {
                    saw_incompatible_error = true;
                }
            }
            Ok(LoadMessage::Status(status)) => {
                if status.contains("watching sessions") {
                    saw_watcher_status = true;
                    break;
                }
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) if saw_incompatible_error => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    assert!(saw_incompatible_error);
    assert!(!saw_watcher_status);

    fs::remove_dir_all(dir).unwrap();
}

fn session_with_cost(id: &str, timestamp: &str, output_tokens: u64) -> Session {
    let mut session = session_for_search(id, &format!("{id} prompt"), &format!("{id} raw"));
    session.timestamp = timestamp.to_string();
    let usage = TokenUsage {
        input_tokens: 1,
        output_tokens,
        total_tokens: 1 + output_tokens,
        ..TokenUsage::default()
    };
    session.token_events = vec![TokenEvent {
        timestamp: timestamp.to_string(),
        total: usage.clone(),
        last: usage,
        context_window: None,
    }];
    session
}

fn app_with_sort_sessions() -> App {
    let mut app = App {
        sessions_dir: PathBuf::from("/tmp/sessions"),
        cache_dir: PathBuf::from("/tmp/cache"),
        pricing: Pricing::default(),
        include_web_cost: true,
        sessions: vec![
            session_with_cost("cheap-newer", "2026-01-03T00:00:00.000Z", 10),
            session_with_cost("expensive-older", "2026-01-02T00:00:00.000Z", 1_000),
        ],
        search_index: SearchIndex::default(),
        filtered: Vec::new(),
        query: String::new(),
        list_state: ListState::default(),
        table_state: TableState::default(),
        focus: Focus::List,
        input_mode: InputMode::Browse,
        show_detail: false,
        status: String::new(),
        last_reload: Instant::now(),
        loading: None,
        loader: None,
        sort_key: SortKey::TotalCost,
        sort_direction: SortDirection::Descending,
        index_launch_mode: IndexLaunchMode::AcquireLock,
    };
    app.search_index = SearchIndex::build(&app.sessions, |_current, _total| {});
    app.apply_filter();
    app
}

fn filtered_ids(app: &App) -> Vec<String> {
    app.filtered
        .iter()
        .map(|idx| app.sessions[*idx].id.clone())
        .collect()
}

#[test]
fn default_sort_is_total_cost_descending() {
    let app = app_with_sort_sessions();

    assert_eq!(app.sort_key, SortKey::TotalCost);
    assert_eq!(app.sort_direction, SortDirection::Descending);
    assert_eq!(filtered_ids(&app), vec!["expensive-older", "cheap-newer"]);
}

#[test]
fn browse_mode_cycles_sort_key_with_s() {
    let mut app = app_with_sort_sessions();

    app.handle_key(key_char('s')).unwrap();

    assert_eq!(app.sort_key, SortKey::Timestamp);
    assert_eq!(app.sort_direction, SortDirection::Descending);
    assert_eq!(filtered_ids(&app), vec!["cheap-newer", "expensive-older"]);
}

#[test]
fn browse_mode_reverses_sort_direction_with_shift_s() {
    let mut app = app_with_sort_sessions();

    app.handle_key(key_char('S')).unwrap();

    assert_eq!(app.sort_key, SortKey::TotalCost);
    assert_eq!(app.sort_direction, SortDirection::Ascending);
    assert_eq!(filtered_ids(&app), vec!["cheap-newer", "expensive-older"]);
}

#[test]
fn highlight_matches_marks_query_terms() {
    let line = highlight_matches(
        "alpha beta",
        "alp beta",
        Style::default(),
        match_highlight_style(),
    );
    let spans = line.spans;

    assert_eq!(spans[0].content.as_ref(), "alp");
    assert_eq!(spans[0].style, match_highlight_style());
    assert_eq!(spans[1].content.as_ref(), "ha ");
    assert_eq!(spans[2].content.as_ref(), "beta");
    assert_eq!(spans[2].style, match_highlight_style());
}

#[test]
fn search_cursor_position_points_after_query_in_search_mode() {
    let mut app = app_for_key_tests();
    app.input_mode = InputMode::Search;
    app.query = "abc".to_string();

    assert_eq!(
        search_cursor_position(&app, Rect::new(10, 20, 40, 3)),
        Some((22, 21))
    );

    app.input_mode = InputMode::Browse;
    assert_eq!(search_cursor_position(&app, Rect::new(10, 20, 40, 3)), None);
}

fn session_for_search(id: &str, first_user_message: &str, _raw_text: &str) -> Session {
    Session {
        id: id.to_string(),
        timestamp: "2026-01-02T00:00:00.000Z".to_string(),
        path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        cwd: Some(format!("/work/{id}")),
        model: Some("gpt-5.5".to_string()),
        model_provider: Some("openai".to_string()),
        first_user_message: Some(first_user_message.to_string()),
        final_assistant_message: None,
        token_events: Vec::new(),
        goal: GoalUsage::default(),
        web_search_calls: 0,
        line_count: 1,
        parse_errors: Vec::new(),
        search_messages: vec![first_user_message.to_string()],
        cached_final_usage: None,
        max_request_input_tokens: 0,
        token_event_count: 0,
    }
}

#[test]
fn prebuilt_search_index_matches_metadata_and_message_text() {
    let sessions = vec![
        {
            let mut session = session_for_search(
                "session-alpha",
                "Investigate indexing latency",
                "hidden raw detail",
            );
            session.final_assistant_message = Some("visible progress bar".to_string());
            session
        },
        session_for_search("session-beta", "Review pricing", "unrelated content"),
    ];

    let index = SearchIndex::build(&sessions, |_current, _total| {});

    assert_eq!(index.search("session-alpha"), vec![0]);
    assert_eq!(index.search("indexing latency"), vec![0]);
    assert_eq!(index.search("progress bar"), vec![0]);
    assert_eq!(index.search("pricing"), vec![1]);
    assert!(index.search("missing phrase").is_empty());
}

#[test]
fn prebuilt_search_index_does_not_index_raw_jsonl_noise() {
    let sessions = vec![session_for_search(
        "session-alpha",
        "small prompt",
        "raw-only-token-that-should-not-be-indexed",
    )];

    let index = SearchIndex::build(&sessions, |_current, _total| {});

    assert!(index.search("raw-only-token").is_empty());
    assert_eq!(index.search("small prompt"), vec![0]);
}

#[test]
fn prebuilt_search_index_reports_progress_while_building() {
    let sessions = vec![
        session_for_search("session-alpha", "first", "alpha raw"),
        session_for_search("session-beta", "second", "beta raw"),
    ];
    let mut updates = Vec::new();

    let _index = SearchIndex::build(&sessions, |current, total| {
        updates.push((current, total));
    });

    assert_eq!(updates, vec![(1, 2), (2, 2)]);
}
