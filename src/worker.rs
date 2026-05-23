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

pub(crate) struct IndexWorker {
    cache_store: CacheStore,
    tx: Sender<LoadMessage>,
}

impl IndexWorker {
    fn new(root: PathBuf, cache_dir: PathBuf, tx: Sender<LoadMessage>) -> Self {
        Self {
            cache_store: CacheStore::with_cache_dir(root, cache_dir),
            tx,
        }
    }

    pub(crate) fn run(
        root: PathBuf,
        cache_dir: PathBuf,
        tx: Sender<LoadMessage>,
        mode: IndexWorkerMode,
    ) {
        Self::new(root, cache_dir, tx).run_with_mode(mode);
    }

    fn run_with_mode(self, mode: IndexWorkerMode) {
        match mode {
            IndexWorkerMode::UseLock(lock) => {
                self.run_writer(Some(lock), false);
            }
            IndexWorkerMode::Force => {
                self.run_writer(None, true);
            }
            IndexWorkerMode::ReadOnly => {
                self.run_readonly(false);
            }
            IndexWorkerMode::AcquireLock => {
                match IndexLock::try_acquire(self.cache_store.cache_dir()) {
                    Ok(Some(lock)) => self.run_writer(Some(lock), false),
                    Ok(None) => self.run_readonly(true),
                    Err(err) => {
                        let _ = self.tx.send(LoadMessage::Loaded(Err(format!("{err:#}"))));
                    }
                }
            }
        }
    }

    fn run_writer(self, lock: Option<IndexLock>, forced: bool) {
        if forced {
            let _ = self.tx.send(LoadMessage::Status(format!(
                "force writing index cache at {}; lock ignored",
                self.cache_store.cache_dir().display()
            )));
        } else {
            let _ = self.tx.send(LoadMessage::Status(format!(
                "index writer active at {}",
                self.cache_store.cache_dir().display()
            )));
        }
        self.send_cached_snapshot();
        if !self.send_reconciled_cache() {
            return;
        }

        let tx = self.tx.clone();
        if let Err(err) = self.watch_loop(lock) {
            let _ = tx.send(LoadMessage::Status(format!("watcher stopped: {err:#}")));
        }
    }

    fn run_readonly(self, locked: bool) {
        let mode = if locked {
            "index is locked"
        } else {
            "read-only index mode"
        };
        let _ = self.tx.send(LoadMessage::Status(format!(
            "{mode}; reading cached snapshots from {}",
            self.cache_store.cache_dir().display()
        )));
        match self.cache_store.load() {
            Ok(Some(result)) => {
                let _ = self.tx.send(LoadMessage::Loaded(Ok(result)));
            }
            Ok(None) => {
                let _ = self.tx.send(LoadMessage::Loaded(Err(format!(
                    "{mode} and no cached snapshot exists yet"
                ))));
            }
            Err(err) => {
                let _ = self.tx.send(LoadMessage::Loaded(Err(format!("{err:#}"))));
            }
        }
        self.readonly_manifest_poll();
    }

    fn send_cached_snapshot(&self) {
        match self.cache_store.load() {
            Ok(Some(result)) => {
                let _ = self.tx.send(LoadMessage::Loaded(Ok(result)));
            }
            Ok(None) => {}
            Err(err) => {
                let _ = self.tx.send(LoadMessage::Status(format!(
                    "cached snapshot skipped: {err:#}"
                )));
            }
        }
    }

    fn send_reconciled_cache(&self) -> bool {
        match self.cache_store.reconcile(|progress| {
            let _ = self.tx.send(LoadMessage::Progress(progress));
        }) {
            Ok(result) => {
                let _ = self.tx.send(LoadMessage::Loaded(Ok(result)));
                true
            }
            Err(err) => {
                let _ = self.tx.send(LoadMessage::Loaded(Err(format!("{err:#}"))));
                false
            }
        }
    }

    fn send_reconciled_cache_for_paths(&self, paths: BTreeSet<PathBuf>) {
        let result = self
            .cache_store
            .reconcile_paths(paths, |progress| {
                let _ = self.tx.send(LoadMessage::Progress(progress));
            })
            .map_err(|err| format!("{err:#}"));
        let _ = self.tx.send(LoadMessage::Loaded(result));
    }

    fn watch_loop(self, _lock: Option<IndexLock>) -> Result<()> {
        let (event_tx, event_rx) = mpsc::channel::<notify::Result<NotifyEvent>>();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = event_tx.send(event);
            },
            NotifyConfig::default(),
        )?;
        watcher
            .watch(self.cache_store.root(), RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", self.cache_store.root().display()))?;
        let _ = self.tx.send(LoadMessage::Status(format!(
            "watching sessions under {}",
            self.cache_store.root().display()
        )));

        loop {
            match event_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    if Self::event_touches_sessions(&event) {
                        let mut events = vec![event];
                        events.extend(Self::drain_notify_events(&event_rx));
                        match Self::session_paths_from_events(&events) {
                            Some(paths) if !paths.is_empty() => {
                                self.send_reconciled_cache_for_paths(paths);
                            }
                            Some(_) => {}
                            None => {
                                self.send_reconciled_cache();
                            }
                        }
                    }
                }
                Ok(Err(err)) => {
                    if self
                        .tx
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

    fn readonly_manifest_poll(self) {
        let mut seen_generation = load_manifest(self.cache_store.cache_dir())
            .ok()
            .flatten()
            .map(|manifest| manifest.generation)
            .unwrap_or_default();

        loop {
            thread::sleep(Duration::from_secs(1));
            let Ok(Some(manifest)) = load_manifest(self.cache_store.cache_dir()) else {
                continue;
            };
            if manifest.generation <= seen_generation {
                continue;
            }
            seen_generation = manifest.generation;
            match self.cache_store.load() {
                Ok(Some(result)) => {
                    if self.tx.send(LoadMessage::Loaded(Ok(result))).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    if self
                        .tx
                        .send(LoadMessage::Status(format!("cache reload failed: {err:#}")))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }

    fn event_touches_sessions(event: &NotifyEvent) -> bool {
        event.paths.iter().any(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                || path.is_dir()
                || path.extension().is_none()
        })
    }

    fn drain_notify_events(rx: &Receiver<notify::Result<NotifyEvent>>) -> Vec<NotifyEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_millis(WATCH_DEBOUNCE_MS)) {
            if let Ok(event) = event {
                events.push(event);
            }
        }
        events
    }

    fn session_paths_from_events(events: &[NotifyEvent]) -> Option<BTreeSet<PathBuf>> {
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
}
