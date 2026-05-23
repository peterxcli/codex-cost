use super::*;

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
