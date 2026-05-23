use super::*;

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
        IndexWorker::run(
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
