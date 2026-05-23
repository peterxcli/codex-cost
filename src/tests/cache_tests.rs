use super::*;

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
