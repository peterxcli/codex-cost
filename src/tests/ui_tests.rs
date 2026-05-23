use super::*;

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
