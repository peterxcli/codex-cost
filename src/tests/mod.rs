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
    index_lock_path, IndexLaunchMode, IndexLock, IndexWorker, IndexWorkerMode, LoadMessage,
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

mod app_tests;
mod cache_tests;
mod parser_tests;
mod pricing_tests;
mod search_tests;
mod ui_tests;
mod worker_tests;
