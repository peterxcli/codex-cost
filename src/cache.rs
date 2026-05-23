use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::models::{FileFingerprint, FileMetadataParts, Session};
use crate::parser::SessionParser;
use crate::search::{build_fst_bytes, session_terms, SearchIndex};
#[cfg(test)]
use crate::util::hash_file_fingerprint;
use crate::util::{
    file_metadata_parts, hash_hex, hash_text, read_json, relative_path_string, unix_seconds_now,
    write_bytes_atomic, write_json_atomic,
};
use crate::worker::{LoadPhase, LoadProgress, LoadResult};

pub(crate) const CACHE_SCHEMA_VERSION: u32 = 5;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct CacheManifest {
    pub(crate) schema_version: u32,
    pub(crate) generation: u64,
    pub(crate) sessions_root: String,
    pub(crate) merkle_root: String,
    pub(crate) updated_at_unix_seconds: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SessionsCache {
    pub(crate) schema_version: u32,
    pub(crate) docs: Vec<CachedSessionDoc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CachedSessionDoc {
    pub(crate) relative_path: String,
    pub(crate) fingerprint: FileFingerprint,
    pub(crate) session: Session,
    pub(crate) terms: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct PostingsCache {
    pub(crate) schema_version: u32,
    pub(crate) postings: BTreeMap<String, Vec<usize>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct MerkleCache {
    pub(crate) schema_version: u32,
    pub(crate) root: MerkleNode,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct MerkleSnapshot {
    pub(crate) root: MerkleNode,
    pub(crate) fingerprints: BTreeMap<String, FileFingerprint>,
    pub(crate) changed_paths: BTreeSet<String>,
    pub(crate) deleted_paths: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MerklePlan {
    pub(crate) files: Vec<MerkleFileState>,
    pub(crate) has_deleted_paths: bool,
    #[cfg(test)]
    pub(crate) deleted_paths: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct MerkleFileState {
    pub(crate) path: PathBuf,
    pub(crate) relative_path: String,
    pub(crate) metadata: FileMetadataParts,
    pub(crate) reused_fingerprint: Option<FileFingerprint>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct MerkleNode {
    pub(crate) name: String,
    pub(crate) relative_path: String,
    pub(crate) kind: MerkleNodeKind,
    pub(crate) hash: String,
    pub(crate) fingerprint: Option<FileFingerprint>,
    pub(crate) children: Vec<MerkleNode>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MerkleNodeKind {
    #[default]
    Directory,
    File,
}

#[derive(Default)]
pub(crate) struct MerkleBuilderNode {
    pub(crate) name: String,
    pub(crate) relative_path: String,
    pub(crate) fingerprint: Option<FileFingerprint>,
    pub(crate) children: BTreeMap<String, MerkleBuilderNode>,
}

pub(crate) fn cache_dir_for_sessions(root: &Path) -> PathBuf {
    let cache_root = dirs_next::cache_dir()
        .or_else(|| dirs_next::home_dir().map(|home| home.join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codex-cost")
        .join("index");
    cache_root.join(hash_hex(root.to_string_lossy().as_bytes()))
}

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

    pub(crate) fn load(&self) -> Result<Option<LoadResult>> {
        load_cached_index(self.root(), self.cache_dir())
    }

    pub(crate) fn reconcile<F>(&self, progress: F) -> Result<LoadResult>
    where
        F: FnMut(LoadProgress),
    {
        reconcile_session_cache(self.root(), self.cache_dir(), progress)
    }

    pub(crate) fn reconcile_paths<F>(
        &self,
        paths: BTreeSet<PathBuf>,
        progress: F,
    ) -> Result<LoadResult>
    where
        F: FnMut(LoadProgress),
    {
        reconcile_session_cache_for_paths(self.root(), self.cache_dir(), paths, progress)
    }
}

fn load_cached_index(root: &Path, cache_dir: &Path) -> Result<Option<LoadResult>> {
    let manifest_path = cache_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest = load_manifest(cache_dir)?
        .with_context(|| incompatible_cache_message(cache_dir, "manifest.json disappeared"))?;
    ensure_cache_root(root, cache_dir, &manifest)?;

    let sessions_cache = load_sessions_cache(cache_dir)?
        .with_context(|| incompatible_cache_message(cache_dir, "sessions.json is missing"))?;
    let postings_cache = load_postings_cache(cache_dir)?
        .with_context(|| incompatible_cache_message(cache_dir, "postings.json is missing"))?;
    let terms_path = cache_dir.join("terms.fst");
    if !terms_path.exists() {
        bail!(
            "{}",
            incompatible_cache_message(cache_dir, "terms.fst is missing")
        );
    }
    let terms_bytes = fs::read(&terms_path)
        .with_context(|| format!("failed to read {}", terms_path.display()))
        .with_context(|| cache_delete_hint(cache_dir))?;
    let sessions = sessions_cache
        .docs
        .iter()
        .map(|doc| doc.session.clone())
        .collect::<Vec<_>>();
    let search_index =
        SearchIndex::from_persisted(sessions.len(), postings_cache.postings, terms_bytes);

    Ok(Some(LoadResult {
        sessions,
        search_index,
        generation: manifest.generation,
    }))
}

fn reconcile_session_cache<F>(root: &Path, cache_dir: &Path, mut progress: F) -> Result<LoadResult>
where
    F: FnMut(LoadProgress),
{
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;

    progress(LoadProgress {
        phase: LoadPhase::Checking,
        current: 0,
        total: 0,
        path: None,
    });

    let previous_manifest = load_manifest(cache_dir)?;
    if let Some(manifest) = &previous_manifest {
        ensure_cache_root(root, cache_dir, manifest)?;
    }
    let previous_snapshot = load_merkle_snapshot(cache_dir)?;
    let merkle_plan = build_merkle_plan(root, previous_snapshot.as_ref(), &mut progress)?;
    if previous_manifest.is_some() && !merkle_plan.has_changes() {
        if let Some(result) = load_cached_index(root, cache_dir)? {
            return Ok(result);
        }
    }
    let previous_docs = if previous_manifest.is_some() {
        let cache = load_sessions_cache(cache_dir)?
            .with_context(|| incompatible_cache_message(cache_dir, "sessions.json is missing"))?;
        cache
            .docs
            .into_iter()
            .map(|doc| (doc.relative_path.clone(), doc))
            .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let total = merkle_plan.files.len();
    let parse_total = merkle_plan
        .files
        .iter()
        .filter(|file| {
            file.reused_fingerprint.is_none() || !previous_docs.contains_key(&file.relative_path)
        })
        .count();
    let mut parsed_count = 0;
    let mut docs = Vec::with_capacity(total);
    let mut fingerprints = BTreeMap::new();

    for file in merkle_plan.files.iter() {
        let parsed = if file.reused_fingerprint.is_none() {
            Some(SessionParser::parse_with_fingerprint_or_error(
                root,
                &file.path,
                &file.relative_path,
                file.metadata,
            )?)
        } else {
            None
        };
        let fingerprint = parsed
            .as_ref()
            .map(|parsed| parsed.fingerprint.clone())
            .or_else(|| file.reused_fingerprint.clone())
            .expect("fingerprint should be reused or parsed");
        fingerprints.insert(file.relative_path.clone(), fingerprint.clone());

        let relative_path = &file.relative_path;
        let cached = previous_docs.get(relative_path);
        let can_reuse = cached
            .map(|doc| doc.fingerprint == fingerprint)
            .unwrap_or(false);
        if can_reuse {
            docs.push(cached.unwrap().clone());
        } else if let Some(parsed) = parsed {
            let terms = session_terms(&parsed.session);
            docs.push(CachedSessionDoc {
                relative_path: relative_path.clone(),
                fingerprint,
                session: parsed.session,
                terms,
            });
            parsed_count += 1;
            progress(LoadProgress {
                phase: LoadPhase::Parsing,
                current: parsed_count,
                total: parse_total,
                path: Some(file.path.clone()),
            });
        } else {
            let session = SessionParser::parse_or_error(&file.path);
            let terms = session_terms(&session);
            docs.push(CachedSessionDoc {
                relative_path: relative_path.clone(),
                fingerprint: fingerprint.clone(),
                session,
                terms,
            });
            parsed_count += 1;
            progress(LoadProgress {
                phase: LoadPhase::Parsing,
                current: parsed_count,
                total: parse_total,
                path: Some(file.path.clone()),
            });
        }
    }

    let merkle_root = build_merkle_root(&fingerprints);
    docs.sort_by(|a, b| b.session.timestamp.cmp(&a.session.timestamp));
    let sessions = docs
        .iter()
        .map(|doc| doc.session.clone())
        .collect::<Vec<_>>();
    let postings = postings_from_docs(&docs);
    let search_index = SearchIndex::from_postings(sessions.len(), postings.clone());

    progress(LoadProgress {
        phase: LoadPhase::Indexing,
        current: sessions.len(),
        total: sessions.len(),
        path: None,
    });

    let previous_generation = load_manifest(cache_dir)?.map(|manifest| manifest.generation);
    let generation = previous_generation.unwrap_or_default() + 1;
    persist_cache(root, cache_dir, generation, &docs, &postings, &merkle_root)?;

    Ok(LoadResult {
        sessions,
        search_index,
        generation,
    })
}

fn reconcile_session_cache_for_paths<F>(
    root: &Path,
    cache_dir: &Path,
    paths: BTreeSet<PathBuf>,
    mut progress: F,
) -> Result<LoadResult>
where
    F: FnMut(LoadProgress),
{
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;

    if let Some(manifest) = load_manifest(cache_dir)? {
        ensure_cache_root(root, cache_dir, &manifest)?;
    }
    let Some(previous_snapshot) = load_merkle_snapshot(cache_dir)? else {
        return reconcile_session_cache(root, cache_dir, progress);
    };
    let Some(sessions_cache) = load_sessions_cache(cache_dir)? else {
        return reconcile_session_cache(root, cache_dir, progress);
    };

    let previous_docs = sessions_cache.docs;
    let mut docs_by_path = previous_docs
        .iter()
        .cloned()
        .map(|doc| (doc.relative_path.clone(), doc))
        .collect::<BTreeMap<_, _>>();
    let mut fingerprints = previous_snapshot.fingerprints;
    let mut changed_paths = BTreeSet::new();
    let mut deleted_any = false;
    let parse_total = paths
        .iter()
        .filter(|path| path.exists() && path.is_file())
        .count();
    let mut parsed_count = 0;

    for path in paths {
        let relative_path = relative_path_string(root, &path)?;
        if path.exists() && path.is_file() {
            let metadata = file_metadata_parts(&path)?;
            let can_reuse = fingerprints
                .get(&relative_path)
                .map(|previous| {
                    previous.size == metadata.size
                        && previous.modified_unix_nanos == metadata.modified_unix_nanos
                        && docs_by_path.contains_key(&relative_path)
                })
                .unwrap_or(false);
            if can_reuse {
                continue;
            }

            let parsed = SessionParser::parse_with_fingerprint_or_error(
                root,
                &path,
                &relative_path,
                metadata,
            )?;
            let terms = session_terms(&parsed.session);
            fingerprints.insert(relative_path.clone(), parsed.fingerprint.clone());
            docs_by_path.insert(
                relative_path.clone(),
                CachedSessionDoc {
                    relative_path: relative_path.clone(),
                    fingerprint: parsed.fingerprint,
                    session: parsed.session,
                    terms,
                },
            );
            changed_paths.insert(relative_path);
            parsed_count += 1;
            progress(LoadProgress {
                phase: LoadPhase::Parsing,
                current: parsed_count,
                total: parse_total,
                path: Some(path),
            });
        } else {
            deleted_any |= docs_by_path.remove(&relative_path).is_some();
            deleted_any |= fingerprints.remove(&relative_path).is_some();
        }
    }

    if changed_paths.is_empty() && !deleted_any {
        if let Some(result) = load_cached_index(root, cache_dir)? {
            return Ok(result);
        }
        return reconcile_session_cache(root, cache_dir, progress);
    }

    let mut docs = docs_by_path.into_values().collect::<Vec<_>>();
    docs.sort_by(|a, b| b.session.timestamp.cmp(&a.session.timestamp));
    let sessions = docs
        .iter()
        .map(|doc| doc.session.clone())
        .collect::<Vec<_>>();
    let postings = updated_or_rebuilt_postings(cache_dir, &previous_docs, &docs, &changed_paths)?;
    let search_index = SearchIndex::from_postings(sessions.len(), postings.clone());

    progress(LoadProgress {
        phase: LoadPhase::Indexing,
        current: changed_paths.len(),
        total: changed_paths.len().max(1),
        path: None,
    });

    let merkle_root = build_merkle_root(&fingerprints);
    let previous_generation = load_manifest(cache_dir)?.map(|manifest| manifest.generation);
    let generation = previous_generation.unwrap_or_default() + 1;
    persist_cache(root, cache_dir, generation, &docs, &postings, &merkle_root)?;

    Ok(LoadResult {
        sessions,
        search_index,
        generation,
    })
}

pub(crate) fn postings_from_docs(docs: &[CachedSessionDoc]) -> BTreeMap<String, Vec<usize>> {
    let mut postings: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, doc) in docs.iter().enumerate() {
        for term in &doc.terms {
            postings
                .entry(term.clone().to_string())
                .or_default()
                .push(idx);
        }
    }
    postings
}

pub(crate) fn updated_or_rebuilt_postings(
    cache_dir: &Path,
    previous_docs: &[CachedSessionDoc],
    docs: &[CachedSessionDoc],
    changed_paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, Vec<usize>>> {
    if let Some(postings_cache) = load_postings_cache(cache_dir)? {
        if same_doc_order(previous_docs, docs) {
            let mut postings = postings_cache.postings;
            for relative_path in changed_paths {
                let Some(idx) = docs
                    .iter()
                    .position(|doc| doc.relative_path == *relative_path)
                else {
                    return Ok(postings_from_docs(docs));
                };
                update_postings_for_doc(
                    &mut postings,
                    idx,
                    &previous_docs[idx].terms,
                    &docs[idx].terms,
                );
            }
            return Ok(postings);
        }
        if let Some(postings) = remap_postings_for_doc_order(
            postings_cache.postings,
            previous_docs,
            docs,
            changed_paths,
        ) {
            return Ok(postings);
        }
    }
    Ok(postings_from_docs(docs))
}

pub(crate) fn same_doc_order(left: &[CachedSessionDoc], right: &[CachedSessionDoc]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.relative_path == right.relative_path)
}

pub(crate) fn remap_postings_for_doc_order(
    previous_postings: BTreeMap<String, Vec<usize>>,
    previous_docs: &[CachedSessionDoc],
    docs: &[CachedSessionDoc],
    changed_paths: &BTreeSet<String>,
) -> Option<BTreeMap<String, Vec<usize>>> {
    let new_index_by_path = docs
        .iter()
        .enumerate()
        .map(|(idx, doc)| (doc.relative_path.as_str(), idx))
        .collect::<BTreeMap<_, _>>();
    let mut postings = BTreeMap::new();

    for (term, previous_doc_indexes) in previous_postings {
        let mut remapped = Vec::with_capacity(previous_doc_indexes.len());
        for previous_idx in previous_doc_indexes {
            let previous_doc = previous_docs.get(previous_idx)?;
            if changed_paths.contains(&previous_doc.relative_path) {
                continue;
            }
            if let Some(new_idx) = new_index_by_path.get(previous_doc.relative_path.as_str()) {
                remapped.push(*new_idx);
            }
        }
        remapped.sort_unstable();
        remapped.dedup();
        if !remapped.is_empty() {
            postings.insert(term, remapped);
        }
    }

    for relative_path in changed_paths {
        let Some((new_idx, doc)) = docs
            .iter()
            .enumerate()
            .find(|(_idx, doc)| doc.relative_path == *relative_path)
        else {
            continue;
        };
        for term in &doc.terms {
            let posting = postings.entry(term.clone()).or_insert_with(Vec::new);
            match posting.binary_search(&new_idx) {
                Ok(_) => {}
                Err(idx) => posting.insert(idx, new_idx),
            }
        }
    }

    Some(postings)
}

pub(crate) fn update_postings_for_doc(
    postings: &mut BTreeMap<String, Vec<usize>>,
    doc_idx: usize,
    old_terms: &[String],
    new_terms: &[String],
) {
    let old_terms = old_terms.iter().collect::<BTreeSet<_>>();
    let new_terms = new_terms.iter().collect::<BTreeSet<_>>();

    for term in old_terms.difference(&new_terms) {
        if let Some(posting) = postings.get_mut(*term) {
            posting.retain(|idx| *idx != doc_idx);
            if posting.is_empty() {
                postings.remove(*term);
            }
        }
    }

    for term in new_terms.difference(&old_terms) {
        let posting = postings.entry((*term).clone()).or_default();
        match posting.binary_search(&doc_idx) {
            Ok(_) => {}
            Err(idx) => posting.insert(idx, doc_idx),
        }
    }
}

pub(crate) fn persist_cache(
    root: &Path,
    cache_dir: &Path,
    generation: u64,
    docs: &[CachedSessionDoc],
    postings: &BTreeMap<String, Vec<usize>>,
    merkle_root: &MerkleNode,
) -> Result<()> {
    let terms_bytes = build_fst_bytes(postings.keys())?;
    let sessions_cache = SessionsCache {
        schema_version: CACHE_SCHEMA_VERSION,
        docs: compact_docs_for_cache(docs),
    };
    let postings_cache = PostingsCache {
        schema_version: CACHE_SCHEMA_VERSION,
        postings: postings.clone(),
    };
    let merkle_cache = MerkleCache {
        schema_version: CACHE_SCHEMA_VERSION,
        root: merkle_root.clone(),
    };
    let manifest = CacheManifest {
        schema_version: CACHE_SCHEMA_VERSION,
        generation,
        sessions_root: root.to_string_lossy().to_string(),
        merkle_root: merkle_root.hash.clone(),
        updated_at_unix_seconds: unix_seconds_now(),
    };

    write_json_atomic(&cache_dir.join("sessions.json"), &sessions_cache)?;
    write_json_atomic(&cache_dir.join("postings.json"), &postings_cache)?;
    write_bytes_atomic(&cache_dir.join("terms.fst"), &terms_bytes)?;
    write_json_atomic(&cache_dir.join("merkle.json"), &merkle_cache)?;
    write_json_atomic(&cache_dir.join("manifest.json"), &manifest)?;
    Ok(())
}

pub(crate) fn compact_docs_for_cache(docs: &[CachedSessionDoc]) -> Vec<CachedSessionDoc> {
    let mut cached_docs = docs.to_vec();
    for doc in &mut cached_docs {
        doc.session.compact_for_cache();
    }
    cached_docs
}

pub(crate) fn load_manifest(cache_dir: &Path) -> Result<Option<CacheManifest>> {
    let path = cache_dir.join("manifest.json");
    if !path.exists() {
        return Ok(None);
    }
    let manifest: CacheManifest = read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "manifest.json", manifest.schema_version)?;
    Ok(Some(manifest))
}

pub(crate) fn load_sessions_cache(cache_dir: &Path) -> Result<Option<SessionsCache>> {
    let path = cache_dir.join("sessions.json");
    if !path.exists() {
        return Ok(None);
    }
    let cache: SessionsCache = read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "sessions.json", cache.schema_version)?;
    Ok(Some(cache))
}

pub(crate) fn load_postings_cache(cache_dir: &Path) -> Result<Option<PostingsCache>> {
    let path = cache_dir.join("postings.json");
    if !path.exists() {
        return Ok(None);
    }
    let cache: PostingsCache = read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "postings.json", cache.schema_version)?;
    Ok(Some(cache))
}

pub(crate) fn load_merkle_snapshot(cache_dir: &Path) -> Result<Option<MerkleSnapshot>> {
    let path = cache_dir.join("merkle.json");
    if !path.exists() {
        return Ok(None);
    }
    let merkle_cache: MerkleCache =
        read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "merkle.json", merkle_cache.schema_version)?;
    let mut fingerprints = BTreeMap::new();
    collect_fingerprints(&merkle_cache.root, &mut fingerprints);
    Ok(Some(MerkleSnapshot {
        root: merkle_cache.root,
        fingerprints,
        changed_paths: BTreeSet::new(),
        deleted_paths: BTreeSet::new(),
    }))
}

pub(crate) fn ensure_cache_schema(cache_dir: &Path, file_name: &str, actual: u32) -> Result<()> {
    if actual != CACHE_SCHEMA_VERSION {
        bail!(
            "{}",
            incompatible_cache_message(
                cache_dir,
                &format!("{file_name} has schema {actual}, expected {CACHE_SCHEMA_VERSION}"),
            )
        );
    }
    Ok(())
}

pub(crate) fn ensure_cache_root(
    root: &Path,
    cache_dir: &Path,
    manifest: &CacheManifest,
) -> Result<()> {
    if manifest.sessions_root != root.to_string_lossy() {
        bail!(
            "{}",
            incompatible_cache_message(
                cache_dir,
                &format!(
                    "manifest.json belongs to {}, not {}",
                    manifest.sessions_root,
                    root.display()
                ),
            )
        );
    }
    Ok(())
}

pub(crate) fn incompatible_cache_message(cache_dir: &Path, detail: &str) -> String {
    format!(
        "search index cache is incompatible ({detail}); delete the cache folder and restart: {}",
        cache_dir.display()
    )
}

pub(crate) fn cache_delete_hint(cache_dir: &Path) -> String {
    format!(
        "delete the search index cache folder and restart: {}",
        cache_dir.display()
    )
}

#[cfg(test)]
pub(crate) fn build_merkle_snapshot(
    root: &Path,
    previous: Option<&MerkleSnapshot>,
) -> Result<MerkleSnapshot> {
    let plan = build_merkle_plan(root, previous, |_progress| {})?;
    let mut fingerprints = BTreeMap::new();
    let mut changed_paths = BTreeSet::new();

    for file in &plan.files {
        let fingerprint = match &file.reused_fingerprint {
            Some(fingerprint) => fingerprint.clone(),
            None => {
                changed_paths.insert(file.relative_path.clone());
                hash_file_fingerprint(&file.path, &file.relative_path, file.metadata)?
            }
        };
        fingerprints.insert(file.relative_path.clone(), fingerprint);
    }

    let root_node = build_merkle_root(&fingerprints);

    Ok(MerkleSnapshot {
        root: root_node,
        fingerprints,
        changed_paths,
        deleted_paths: plan.deleted_paths,
    })
}

pub(crate) fn build_merkle_plan<F>(
    root: &Path,
    previous: Option<&MerkleSnapshot>,
    mut progress: F,
) -> Result<MerklePlan>
where
    F: FnMut(LoadProgress),
{
    progress(LoadProgress {
        phase: LoadPhase::Discovering,
        current: 0,
        total: 0,
        path: None,
    });
    let paths = discover_session_paths(root);
    let total = paths.len();
    let previous_fingerprints = previous
        .map(|snapshot| &snapshot.fingerprints)
        .cloned()
        .unwrap_or_default();
    let mut files = Vec::with_capacity(total);
    let mut seen_paths = BTreeSet::new();

    for (idx, path) in paths.into_iter().enumerate() {
        let relative_path = relative_path_string(root, &path)?;
        let metadata = file_metadata_parts(&path)?;
        let reused_fingerprint = previous_fingerprints
            .get(&relative_path)
            .filter(|previous| {
                previous.size == metadata.size
                    && previous.modified_unix_nanos == metadata.modified_unix_nanos
            })
            .cloned();
        seen_paths.insert(relative_path.clone());
        progress(LoadProgress {
            phase: LoadPhase::Checking,
            current: idx + 1,
            total,
            path: Some(path.clone()),
        });
        files.push(MerkleFileState {
            path,
            relative_path,
            metadata,
            reused_fingerprint,
        });
    }

    let deleted_paths = previous_fingerprints
        .keys()
        .filter(|path| !seen_paths.contains(*path))
        .cloned()
        .collect::<BTreeSet<_>>();

    Ok(MerklePlan {
        files,
        has_deleted_paths: !deleted_paths.is_empty(),
        #[cfg(test)]
        deleted_paths,
    })
}

impl MerklePlan {
    pub(crate) fn has_changes(&self) -> bool {
        self.has_deleted_paths
            || self
                .files
                .iter()
                .any(|file| file.reused_fingerprint.is_none())
    }
}

pub(crate) fn collect_fingerprints(
    node: &MerkleNode,
    fingerprints: &mut BTreeMap<String, FileFingerprint>,
) {
    if let Some(fingerprint) = &node.fingerprint {
        fingerprints.insert(node.relative_path.clone(), fingerprint.clone());
    }
    for child in &node.children {
        collect_fingerprints(child, fingerprints);
    }
}

pub(crate) fn build_merkle_root(fingerprints: &BTreeMap<String, FileFingerprint>) -> MerkleNode {
    let mut root = MerkleBuilderNode {
        name: String::new(),
        relative_path: String::new(),
        fingerprint: None,
        children: BTreeMap::new(),
    };

    for (relative_path, fingerprint) in fingerprints {
        insert_merkle_leaf(&mut root, relative_path, fingerprint.clone());
    }

    finalize_merkle_node(root)
}

pub(crate) fn insert_merkle_leaf(
    root: &mut MerkleBuilderNode,
    relative_path: &str,
    fingerprint: FileFingerprint,
) {
    let parts = relative_path.split('/').collect::<Vec<_>>();
    let mut current = root;
    let mut accumulated = Vec::new();

    for (idx, part) in parts.iter().enumerate() {
        accumulated.push(*part);
        let child_relative_path = accumulated.join("/");
        let child = current
            .children
            .entry((*part).to_string())
            .or_insert_with(|| MerkleBuilderNode {
                name: (*part).to_string(),
                relative_path: child_relative_path,
                fingerprint: None,
                children: BTreeMap::new(),
            });
        if idx == parts.len() - 1 {
            child.fingerprint = Some(fingerprint.clone());
        }
        current = child;
    }
}

pub(crate) fn finalize_merkle_node(node: MerkleBuilderNode) -> MerkleNode {
    let mut children = node
        .children
        .into_values()
        .map(finalize_merkle_node)
        .collect::<Vec<_>>();
    children.sort_by(|a, b| a.name.cmp(&b.name));

    let kind = if node.fingerprint.is_some() {
        MerkleNodeKind::File
    } else {
        MerkleNodeKind::Directory
    };
    let hash = match &node.fingerprint {
        Some(fingerprint) => fingerprint.leaf_hash.clone(),
        None => directory_hash(&node.relative_path, &children),
    };

    MerkleNode {
        name: node.name,
        relative_path: node.relative_path,
        kind,
        hash,
        fingerprint: node.fingerprint,
        children,
    }
}
pub(crate) fn directory_hash(relative_path: &str, children: &[MerkleNode]) -> String {
    let mut text = format!("dir\0{relative_path}\0");
    for child in children {
        text.push_str(&child.name);
        text.push('\0');
        text.push_str(&child.hash);
        text.push('\0');
    }
    hash_text(&text)
}
#[cfg(test)]
pub(crate) fn load_sessions_with_progress<F>(root: &Path, mut progress: F) -> Result<LoadResult>
where
    F: FnMut(LoadProgress),
{
    CacheStore::new(root.to_path_buf()).reconcile(|load_progress| {
        progress(load_progress);
    })
}

pub(crate) fn discover_session_paths(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }

    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .map(|entry| entry.path().to_path_buf())
        .collect()
}
