use super::*;

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
