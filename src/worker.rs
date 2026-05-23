use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use notify::{
    Config as NotifyConfig, Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher,
};

use crate::cache::{load_manifest, CacheStore};
use crate::models::Session;
use crate::search::SearchIndex;

pub(crate) const WATCH_DEBOUNCE_MS: u64 = 250;
pub(crate) struct IndexLock {
    pub(crate) _file: File,
}

pub(crate) enum IndexWorkerMode {
    AcquireLock,
    UseLock(IndexLock),
    ReadOnly,
    Force,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexLaunchMode {
    AcquireLock,
    ReadOnly,
    Force,
}

impl IndexWorkerMode {
    pub(crate) fn launch_mode(&self) -> IndexLaunchMode {
        match self {
            IndexWorkerMode::AcquireLock | IndexWorkerMode::UseLock(_) => {
                IndexLaunchMode::AcquireLock
            }
            IndexWorkerMode::ReadOnly => IndexLaunchMode::ReadOnly,
            IndexWorkerMode::Force => IndexLaunchMode::Force,
        }
    }
}

impl IndexLaunchMode {
    pub(crate) fn worker_mode(self) -> IndexWorkerMode {
        match self {
            IndexLaunchMode::AcquireLock => IndexWorkerMode::AcquireLock,
            IndexLaunchMode::ReadOnly => IndexWorkerMode::ReadOnly,
            IndexLaunchMode::Force => IndexWorkerMode::Force,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoadPhase {
    Discovering,
    Checking,
    Parsing,
    Indexing,
}

#[derive(Clone, Debug)]
pub(crate) struct LoadProgress {
    pub(crate) phase: LoadPhase,
    pub(crate) current: usize,
    pub(crate) total: usize,
    pub(crate) path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LoadResult {
    pub(crate) sessions: Vec<Session>,
    pub(crate) search_index: SearchIndex,
    pub(crate) generation: u64,
}

pub(crate) enum LoadMessage {
    Progress(LoadProgress),
    Loaded(std::result::Result<LoadResult, String>),
    Status(String),
    Finished,
}

impl LoadPhase {
    pub(crate) fn label(self) -> &'static str {
        match self {
            LoadPhase::Discovering => "Discovering sessions",
            LoadPhase::Checking => "Scanning session changes",
            LoadPhase::Parsing => "Parsing sessions",
            LoadPhase::Indexing => "Indexing search",
        }
    }
}
impl IndexLock {
    pub(crate) fn try_acquire(cache_dir: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(cache_dir)
            .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;
        let path = index_lock_path(cache_dir);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open lock file {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                file.set_len(0)
                    .with_context(|| format!("failed to clear {}", path.display()))?;
                writeln!(&file, "pid={}", std::process::id())
                    .with_context(|| format!("failed to write {}", path.display()))?;
                file.sync_all()
                    .with_context(|| format!("failed to sync {}", path.display()))?;
                Ok(Some(Self { _file: file }))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err).with_context(|| format!("failed to lock {}", path.display())),
        }
    }
}

pub(crate) fn index_lock_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("index.lock")
}

pub(crate) fn run_index_worker(
    root: PathBuf,
    cache_dir: PathBuf,
    tx: Sender<LoadMessage>,
    mode: IndexWorkerMode,
) {
    match mode {
        IndexWorkerMode::UseLock(lock) => {
            run_index_writer(root, cache_dir, tx, Some(lock), false);
        }
        IndexWorkerMode::Force => {
            run_index_writer(root, cache_dir, tx, None, true);
        }
        IndexWorkerMode::ReadOnly => {
            run_readonly_index_worker(root, cache_dir, tx, false);
        }
        IndexWorkerMode::AcquireLock => match IndexLock::try_acquire(&cache_dir) {
            Ok(Some(lock)) => run_index_writer(root, cache_dir, tx, Some(lock), false),
            Ok(None) => run_readonly_index_worker(root, cache_dir, tx, true),
            Err(err) => {
                let _ = tx.send(LoadMessage::Loaded(Err(format!("{err:#}"))));
            }
        },
    }
}

pub(crate) fn run_index_writer(
    root: PathBuf,
    cache_dir: PathBuf,
    tx: Sender<LoadMessage>,
    lock: Option<IndexLock>,
    forced: bool,
) {
    if forced {
        let _ = tx.send(LoadMessage::Status(format!(
            "force writing index cache at {}; lock ignored",
            cache_dir.display()
        )));
    } else {
        let _ = tx.send(LoadMessage::Status(format!(
            "index writer active at {}",
            cache_dir.display()
        )));
    }
    send_cached_snapshot(&root, &cache_dir, &tx);
    if !send_reconciled_cache(&root, &cache_dir, &tx) {
        return;
    }
    if let Err(err) = run_watch_loop(root, cache_dir, tx.clone(), lock) {
        let _ = tx.send(LoadMessage::Status(format!("watcher stopped: {err:#}")));
    }
}

pub(crate) fn run_readonly_index_worker(
    root: PathBuf,
    cache_dir: PathBuf,
    tx: Sender<LoadMessage>,
    locked: bool,
) {
    let mode = if locked {
        "index is locked"
    } else {
        "read-only index mode"
    };
    let _ = tx.send(LoadMessage::Status(format!(
        "{mode}; reading cached snapshots from {}",
        cache_dir.display()
    )));
    let cache_store = CacheStore::with_cache_dir(root.clone(), cache_dir.clone());
    match cache_store.load() {
        Ok(Some(result)) => {
            let _ = tx.send(LoadMessage::Loaded(Ok(result)));
        }
        Ok(None) => {
            let _ = tx.send(LoadMessage::Loaded(Err(format!(
                "{mode} and no cached snapshot exists yet"
            ))));
        }
        Err(err) => {
            let _ = tx.send(LoadMessage::Loaded(Err(format!("{err:#}"))));
        }
    }
    run_readonly_manifest_poll(root, cache_dir, tx);
}

pub(crate) fn send_cached_snapshot(root: &Path, cache_dir: &Path, tx: &Sender<LoadMessage>) {
    let cache_store = CacheStore::with_cache_dir(root.to_path_buf(), cache_dir.to_path_buf());
    match cache_store.load() {
        Ok(Some(result)) => {
            let _ = tx.send(LoadMessage::Loaded(Ok(result)));
        }
        Ok(None) => {}
        Err(err) => {
            let _ = tx.send(LoadMessage::Status(format!(
                "cached snapshot skipped: {err:#}"
            )));
        }
    }
}

pub(crate) fn send_reconciled_cache(
    root: &Path,
    cache_dir: &Path,
    tx: &Sender<LoadMessage>,
) -> bool {
    let cache_store = CacheStore::with_cache_dir(root.to_path_buf(), cache_dir.to_path_buf());
    match cache_store.reconcile(|progress| {
        let _ = tx.send(LoadMessage::Progress(progress));
    }) {
        Ok(result) => {
            let _ = tx.send(LoadMessage::Loaded(Ok(result)));
            true
        }
        Err(err) => {
            let _ = tx.send(LoadMessage::Loaded(Err(format!("{err:#}"))));
            false
        }
    }
}

pub(crate) fn send_reconciled_cache_for_paths(
    root: &Path,
    cache_dir: &Path,
    tx: &Sender<LoadMessage>,
    paths: BTreeSet<PathBuf>,
) {
    let cache_store = CacheStore::with_cache_dir(root.to_path_buf(), cache_dir.to_path_buf());
    let result = cache_store
        .reconcile_paths(paths, |progress| {
            let _ = tx.send(LoadMessage::Progress(progress));
        })
        .map_err(|err| format!("{err:#}"));
    let _ = tx.send(LoadMessage::Loaded(result));
}

pub(crate) fn run_watch_loop(
    root: PathBuf,
    cache_dir: PathBuf,
    tx: Sender<LoadMessage>,
    _lock: Option<IndexLock>,
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel::<notify::Result<NotifyEvent>>();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = event_tx.send(event);
        },
        NotifyConfig::default(),
    )?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", root.display()))?;
    let _ = tx.send(LoadMessage::Status(format!(
        "watching sessions under {}",
        root.display()
    )));

    loop {
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                if event_touches_sessions(&event) {
                    let mut events = vec![event];
                    events.extend(drain_notify_events(&event_rx));
                    match session_paths_from_events(&events) {
                        Some(paths) if !paths.is_empty() => {
                            send_reconciled_cache_for_paths(&root, &cache_dir, &tx, paths);
                        }
                        Some(_) => {}
                        None => {
                            send_reconciled_cache(&root, &cache_dir, &tx);
                        }
                    }
                }
            }
            Ok(Err(err)) => {
                if tx
                    .send(LoadMessage::Status(format!("watch event failed: {err:#}")))
                    .is_err()
                {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

pub(crate) fn run_readonly_manifest_poll(
    root: PathBuf,
    cache_dir: PathBuf,
    tx: Sender<LoadMessage>,
) {
    let mut seen_generation = load_manifest(&cache_dir)
        .ok()
        .flatten()
        .map(|manifest| manifest.generation)
        .unwrap_or_default();

    loop {
        thread::sleep(Duration::from_secs(1));
        let Ok(Some(manifest)) = load_manifest(&cache_dir) else {
            continue;
        };
        if manifest.generation <= seen_generation {
            continue;
        }
        seen_generation = manifest.generation;
        let cache_store = CacheStore::with_cache_dir(root.clone(), cache_dir.clone());
        match cache_store.load() {
            Ok(Some(result)) => {
                if tx.send(LoadMessage::Loaded(Ok(result))).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(err) => {
                if tx
                    .send(LoadMessage::Status(format!("cache reload failed: {err:#}")))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

pub(crate) fn event_touches_sessions(event: &NotifyEvent) -> bool {
    event.paths.iter().any(|path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            || path.is_dir()
            || path.extension().is_none()
    })
}

pub(crate) fn drain_notify_events(rx: &Receiver<notify::Result<NotifyEvent>>) -> Vec<NotifyEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.recv_timeout(Duration::from_millis(WATCH_DEBOUNCE_MS)) {
        if let Ok(event) = event {
            events.push(event);
        }
    }
    events
}

pub(crate) fn session_paths_from_events(events: &[NotifyEvent]) -> Option<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for event in events {
        for path in &event.paths {
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                paths.insert(path.clone());
            } else if path.is_dir() || path.extension().is_none() {
                return None;
            }
        }
    }
    Some(paths)
}
