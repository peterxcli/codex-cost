use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use fs2::FileExt;
use fst::automaton::Str;
use fst::{Automaton, IntoStreamer, Set, SetBuilder, Streamer};
use notify::{
    Config as NotifyConfig, Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const CACHE_SCHEMA_VERSION: u32 = 5;
const WATCH_DEBOUNCE_MS: u64 = 250;
const MAX_CACHED_TOKEN_EVENTS: usize = 20;

#[derive(Debug, Default)]
struct Args {
    sessions: Option<PathBuf>,
    pricing: Option<PathBuf>,
    no_web_cost: bool,
    read_only_index: bool,
    force_index: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct TokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TokenEvent {
    timestamp: String,
    total: TokenUsage,
    last: TokenUsage,
    context_window: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct GoalUsage {
    objective: Option<String>,
    status: Option<String>,
    tokens_used: Option<u64>,
    time_used_seconds: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Session {
    id: String,
    timestamp: String,
    path: PathBuf,
    cwd: Option<String>,
    model: Option<String>,
    model_provider: Option<String>,
    first_user_message: Option<String>,
    final_assistant_message: Option<String>,
    token_events: Vec<TokenEvent>,
    goal: GoalUsage,
    web_search_calls: u64,
    line_count: usize,
    parse_errors: Vec<String>,
    #[serde(default, skip)]
    search_messages: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_final_usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    max_request_input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    token_event_count: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct PricingFile {
    #[serde(default)]
    web_search_per_1k: Option<f64>,
    #[serde(default)]
    models: HashMap<String, ModelPrice>,
}

#[derive(Clone, Debug, Deserialize)]
struct ModelPrice {
    input_per_m: f64,
    cached_input_per_m: f64,
    output_per_m: f64,
    #[serde(default)]
    long_context_threshold: Option<u64>,
    #[serde(default)]
    long_context_multiplier: Option<f64>,
}

#[derive(Clone, Debug)]
struct Pricing {
    web_search_per_1k: f64,
    models: HashMap<String, ModelPrice>,
}

#[derive(Clone, Debug, Default)]
struct CostEstimate {
    token_cost: f64,
    web_search_cost: f64,
    total_cost: f64,
    uncached_input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    long_context_applied: bool,
    known_model_price: bool,
}

#[derive(Clone, Debug, Default)]
struct SearchIndex {
    doc_count: usize,
    terms: Option<Set<Vec<u8>>>,
    postings: BTreeMap<String, Vec<usize>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CacheManifest {
    schema_version: u32,
    generation: u64,
    sessions_root: String,
    merkle_root: String,
    updated_at_unix_seconds: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SessionsCache {
    schema_version: u32,
    docs: Vec<CachedSessionDoc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedSessionDoc {
    relative_path: String,
    fingerprint: FileFingerprint,
    session: Session,
    terms: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PostingsCache {
    schema_version: u32,
    postings: BTreeMap<String, Vec<usize>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct MerkleCache {
    schema_version: u32,
    root: MerkleNode,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct MerkleSnapshot {
    root: MerkleNode,
    fingerprints: BTreeMap<String, FileFingerprint>,
    changed_paths: BTreeSet<String>,
    deleted_paths: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
struct MerklePlan {
    files: Vec<MerkleFileState>,
    has_deleted_paths: bool,
    #[cfg(test)]
    deleted_paths: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct MerkleFileState {
    path: PathBuf,
    relative_path: String,
    metadata: FileMetadataParts,
    reused_fingerprint: Option<FileFingerprint>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct MerkleNode {
    name: String,
    relative_path: String,
    kind: MerkleNodeKind,
    hash: String,
    fingerprint: Option<FileFingerprint>,
    children: Vec<MerkleNode>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum MerkleNodeKind {
    #[default]
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileFingerprint {
    size: u64,
    modified_unix_nanos: u64,
    content_hash: String,
    leaf_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileMetadataParts {
    size: u64,
    modified_unix_nanos: u64,
}

#[derive(Clone, Debug)]
struct ParsedSessionFile {
    session: Session,
    fingerprint: FileFingerprint,
}

#[derive(Default)]
struct MerkleBuilderNode {
    name: String,
    relative_path: String,
    fingerprint: Option<FileFingerprint>,
    children: BTreeMap<String, MerkleBuilderNode>,
}

struct IndexLock {
    _file: File,
}

enum IndexWorkerMode {
    AcquireLock,
    UseLock(IndexLock),
    ReadOnly,
    Force,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexLaunchMode {
    AcquireLock,
    ReadOnly,
    Force,
}

impl IndexWorkerMode {
    fn launch_mode(&self) -> IndexLaunchMode {
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
    fn worker_mode(self) -> IndexWorkerMode {
        match self {
            IndexLaunchMode::AcquireLock => IndexWorkerMode::AcquireLock,
            IndexLaunchMode::ReadOnly => IndexWorkerMode::ReadOnly,
            IndexLaunchMode::Force => IndexWorkerMode::Force,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadPhase {
    Discovering,
    Checking,
    Parsing,
    Indexing,
}

#[derive(Clone, Debug)]
struct LoadProgress {
    phase: LoadPhase,
    current: usize,
    total: usize,
    path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
struct LoadResult {
    sessions: Vec<Session>,
    search_index: SearchIndex,
    generation: u64,
}

enum LoadMessage {
    Progress(LoadProgress),
    Loaded(std::result::Result<LoadResult, String>),
    Status(String),
    Finished,
}

impl LoadPhase {
    fn label(self) -> &'static str {
        match self {
            LoadPhase::Discovering => "Discovering sessions",
            LoadPhase::Checking => "Scanning session changes",
            LoadPhase::Parsing => "Parsing sessions",
            LoadPhase::Indexing => "Indexing search",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    List,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
    Browse,
    Search,
}

impl InputMode {
    fn label(self) -> &'static str {
        match self {
            InputMode::Browse => "browse",
            InputMode::Search => "search",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortKey {
    TotalCost,
    Timestamp,
    Tokens,
    WebSearches,
    Model,
    Session,
    FirstPrompt,
}

impl SortKey {
    fn next(self) -> Self {
        match self {
            SortKey::TotalCost => SortKey::Timestamp,
            SortKey::Timestamp => SortKey::Tokens,
            SortKey::Tokens => SortKey::WebSearches,
            SortKey::WebSearches => SortKey::Model,
            SortKey::Model => SortKey::Session,
            SortKey::Session => SortKey::FirstPrompt,
            SortKey::FirstPrompt => SortKey::TotalCost,
        }
    }

    fn default_direction(self) -> SortDirection {
        match self {
            SortKey::Model | SortKey::Session | SortKey::FirstPrompt => SortDirection::Ascending,
            SortKey::TotalCost | SortKey::Timestamp | SortKey::Tokens | SortKey::WebSearches => {
                SortDirection::Descending
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortKey::TotalCost => "cost",
            SortKey::Timestamp => "time",
            SortKey::Tokens => "tokens",
            SortKey::WebSearches => "web",
            SortKey::Model => "model",
            SortKey::Session => "session",
            SortKey::FirstPrompt => "prompt",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    fn reverse(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortDirection::Ascending => "asc",
            SortDirection::Descending => "desc",
        }
    }
}

struct App {
    sessions_dir: PathBuf,
    cache_dir: PathBuf,
    pricing: Pricing,
    include_web_cost: bool,
    sessions: Vec<Session>,
    search_index: SearchIndex,
    filtered: Vec<usize>,
    query: String,
    list_state: ListState,
    table_state: TableState,
    focus: Focus,
    input_mode: InputMode,
    show_detail: bool,
    status: String,
    last_reload: Instant,
    loading: Option<LoadProgress>,
    loader: Option<Receiver<LoadMessage>>,
    sort_key: SortKey,
    sort_direction: SortDirection,
    index_launch_mode: IndexLaunchMode,
}

impl TokenUsage {
    fn from_value(value: &Value) -> Self {
        Self {
            input_tokens: json_u64(value.get("input_tokens")).unwrap_or_default(),
            cached_input_tokens: json_u64(value.get("cached_input_tokens"))
                .or_else(|| json_u64(value.get("cache_read_input_tokens")))
                .unwrap_or_default(),
            output_tokens: json_u64(value.get("output_tokens")).unwrap_or_default(),
            reasoning_output_tokens: json_u64(value.get("reasoning_output_tokens"))
                .or_else(|| json_u64(value.get("reasoning_tokens")))
                .unwrap_or_default(),
            total_tokens: json_u64(value.get("total_tokens")).unwrap_or_default(),
        }
    }

    fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
            && self.total_tokens == 0
    }

    fn normalize_total(mut self) -> Self {
        self.cached_input_tokens = self.cached_input_tokens.min(self.input_tokens);
        if self.total_tokens == 0 {
            self.total_tokens =
                self.input_tokens + self.output_tokens + self.reasoning_output_tokens;
        }
        self
    }

    fn saturating_sub(&self, previous: Option<&TokenUsage>) -> Self {
        let previous = previous.cloned().unwrap_or_default();
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(previous.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
        }
    }

    fn saturating_add(&self, other: &TokenUsage) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_add(other.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
        .normalize_total()
    }
}

impl Pricing {
    fn load(path: Option<&Path>) -> Result<Self> {
        let mut pricing = Self::default();
        if let Some(path) = path {
            let file = File::open(path)
                .with_context(|| format!("failed to open pricing file {}", path.display()))?;
            let override_pricing: PricingFile = serde_json::from_reader(file)
                .with_context(|| format!("failed to parse pricing file {}", path.display()))?;
            if let Some(web) = override_pricing.web_search_per_1k {
                pricing.web_search_per_1k = web;
            }
            for (model, price) in override_pricing.models {
                pricing.models.insert(model, price);
            }
        }
        Ok(pricing)
    }
}

impl Default for Pricing {
    fn default() -> Self {
        let mut models = HashMap::new();
        models.insert(
            "gpt-5.5".to_string(),
            ModelPrice {
                input_per_m: 5.0,
                cached_input_per_m: 0.5,
                output_per_m: 30.0,
                long_context_threshold: Some(272_000),
                long_context_multiplier: Some(2.0),
            },
        );
        Self {
            web_search_per_1k: 10.0,
            models,
        }
    }
}

impl Session {
    fn final_usage(&self) -> Option<&TokenUsage> {
        self.token_events
            .last()
            .map(|event| &event.total)
            .or(self.cached_final_usage.as_ref())
    }

    fn max_request_input(&self) -> u64 {
        self.token_events
            .iter()
            .map(|event| event.last.input_tokens)
            .max()
            .unwrap_or_default()
            .max(self.max_request_input_tokens)
    }

    fn token_event_count(&self) -> usize {
        self.token_event_count.max(self.token_events.len())
    }

    fn token_events_are_truncated(&self) -> bool {
        self.token_event_count() > self.token_events.len()
    }

    fn compact_for_cache(&mut self) {
        self.cached_final_usage = self.final_usage().cloned();
        self.max_request_input_tokens = self.max_request_input();
        self.token_event_count = self.token_events.len();
        if self.token_events.len() > MAX_CACHED_TOKEN_EVENTS {
            let drop_count = self.token_events.len() - MAX_CACHED_TOKEN_EVENTS;
            self.token_events.drain(0..drop_count);
        }
    }
}

impl SearchIndex {
    #[cfg(test)]
    fn build<F>(sessions: &[Session], mut progress: F) -> Self
    where
        F: FnMut(usize, usize),
    {
        let total = sessions.len();
        let mut postings: BTreeMap<String, Vec<usize>> = BTreeMap::new();

        for (idx, session) in sessions.iter().enumerate() {
            for token in session_terms(session) {
                postings.entry(token).or_default().push(idx);
            }
            progress(idx + 1, total);
        }

        Self::from_postings(total, postings)
    }

    fn from_postings(doc_count: usize, postings: BTreeMap<String, Vec<usize>>) -> Self {
        let terms = build_fst_bytes(postings.keys()).ok().and_then(|bytes| {
            if bytes.is_empty() {
                None
            } else {
                Set::new(bytes).ok()
            }
        });
        Self {
            doc_count,
            terms,
            postings,
        }
    }

    fn from_persisted(
        doc_count: usize,
        postings: BTreeMap<String, Vec<usize>>,
        fst_bytes: Vec<u8>,
    ) -> Self {
        let terms = if fst_bytes.is_empty() {
            None
        } else {
            Set::new(fst_bytes).ok()
        };
        Self {
            doc_count,
            terms,
            postings,
        }
    }

    fn search(&self, query: &str) -> Vec<usize> {
        let terms = unique_search_terms(&query.trim().to_lowercase());
        if terms.is_empty() {
            return (0..self.doc_count).collect();
        }

        let mut posting_lists = Vec::with_capacity(terms.len());
        for term in &terms {
            let postings = self.postings_for_prefix(term);
            if postings.is_empty() {
                return Vec::new();
            }
            posting_lists.push(postings);
        }

        posting_lists.sort_by_key(Vec::len);
        let mut candidates = posting_lists.remove(0);
        for postings in posting_lists {
            candidates = intersect_sorted(&candidates, &postings);
            if candidates.is_empty() {
                return Vec::new();
            }
        }
        candidates
    }

    fn postings_for_prefix(&self, prefix: &str) -> Vec<usize> {
        let mut docs = BTreeSet::new();

        if let Some(terms) = &self.terms {
            let matcher = Str::new(prefix).starts_with();
            let mut stream = terms.search(matcher).into_stream();
            while let Some(key) = stream.next() {
                if let Ok(token) = std::str::from_utf8(key) {
                    if let Some(postings) = self.postings.get(token) {
                        docs.extend(postings.iter().copied());
                    }
                }
            }
            return docs.into_iter().collect();
        }

        for (token, postings) in self.postings.range(prefix.to_string()..) {
            if !token.starts_with(prefix) {
                break;
            }
            docs.extend(postings.iter().copied());
        }
        docs.into_iter().collect()
    }
}

fn build_fst_bytes<'a, I>(terms: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = &'a String>,
{
    let mut builder = SetBuilder::memory();
    for term in terms {
        builder.insert(term.as_str())?;
    }
    Ok(builder.into_inner()?)
}

fn indexed_text_lower(session: &Session) -> String {
    let mut text = String::with_capacity(
        session.id.len()
            + session.path.to_string_lossy().len()
            + session
                .first_user_message
                .as_deref()
                .map(str::len)
                .unwrap_or_default()
            + session
                .final_assistant_message
                .as_deref()
                .map(str::len)
                .unwrap_or_default()
            + session
                .search_messages
                .iter()
                .map(String::len)
                .sum::<usize>()
            + session
                .goal
                .objective
                .as_deref()
                .map(str::len)
                .unwrap_or_default()
            + 128,
    );
    push_lower_field(&mut text, &session.id);
    push_lower_field(&mut text, &session.path.display().to_string());
    push_optional_lower_field(&mut text, session.model.as_deref());
    push_optional_lower_field(&mut text, session.model_provider.as_deref());
    push_optional_lower_field(&mut text, session.cwd.as_deref());
    push_optional_lower_field(&mut text, session.first_user_message.as_deref());
    push_optional_lower_field(&mut text, session.final_assistant_message.as_deref());
    for message in &session.search_messages {
        push_lower_field(&mut text, message);
    }
    push_optional_lower_field(&mut text, session.goal.objective.as_deref());
    push_optional_lower_field(&mut text, session.goal.status.as_deref());
    for error in &session.parse_errors {
        push_lower_field(&mut text, error);
    }
    text
}

fn session_terms(session: &Session) -> Vec<String> {
    indexed_text_lower(session)
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn push_optional_lower_field(out: &mut String, value: Option<&str>) {
    if let Some(value) = value {
        push_lower_field(out, value);
    }
}

fn push_lower_field(out: &mut String, value: &str) {
    out.push(' ');
    out.push_str(&value.to_lowercase());
}

fn unique_search_terms(text: &str) -> Vec<String> {
    search_terms(text)
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn search_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        terms.push(current);
    }

    terms
}

fn intersect_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut left_idx = 0;
    let mut right_idx = 0;

    while left_idx < left.len() && right_idx < right.len() {
        match left[left_idx].cmp(&right[right_idx]) {
            std::cmp::Ordering::Less => left_idx += 1,
            std::cmp::Ordering::Greater => right_idx += 1,
            std::cmp::Ordering::Equal => {
                out.push(left[left_idx]);
                left_idx += 1;
                right_idx += 1;
            }
        }
    }

    out
}

fn compare_sessions_for_sort(
    left: &Session,
    right: &Session,
    pricing: &Pricing,
    include_web_cost: bool,
    key: SortKey,
) -> Ordering {
    match key {
        SortKey::TotalCost => {
            let left_cost = estimate_cost(left, pricing, include_web_cost).total_cost;
            let right_cost = estimate_cost(right, pricing, include_web_cost).total_cost;
            left_cost
                .partial_cmp(&right_cost)
                .unwrap_or(Ordering::Equal)
        }
        SortKey::Timestamp => left.timestamp.cmp(&right.timestamp),
        SortKey::Tokens => left
            .final_usage()
            .map(|usage| usage.total_tokens)
            .unwrap_or_default()
            .cmp(
                &right
                    .final_usage()
                    .map(|usage| usage.total_tokens)
                    .unwrap_or_default(),
            ),
        SortKey::WebSearches => left.web_search_calls.cmp(&right.web_search_calls),
        SortKey::Model => left
            .model
            .as_deref()
            .unwrap_or("")
            .cmp(right.model.as_deref().unwrap_or("")),
        SortKey::Session => left.id.cmp(&right.id),
        SortKey::FirstPrompt => left
            .first_user_message
            .as_deref()
            .unwrap_or("")
            .cmp(right.first_user_message.as_deref().unwrap_or("")),
    }
}

fn match_highlight_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn highlight_matches(
    text: &str,
    query: &str,
    base_style: Style,
    highlight_style: Style,
) -> Line<'static> {
    let terms = unique_search_terms(&query.trim().to_lowercase());
    if terms.is_empty() || text.is_empty() {
        return Line::from(Span::styled(text.to_string(), base_style));
    }

    let lower = text.to_lowercase();
    let mut ranges = Vec::new();
    for term in terms {
        let mut offset = 0;
        while let Some(relative_start) = lower[offset..].find(&term) {
            let start = offset + relative_start;
            let end = start + term.len();
            if text.is_char_boundary(start) && text.is_char_boundary(end) {
                ranges.push(start..end);
            }
            offset = end;
            if offset >= lower.len() {
                break;
            }
        }
    }

    if ranges.is_empty() {
        return Line::from(Span::styled(text.to_string(), base_style));
    }

    ranges.sort_by(|left, right| left.start.cmp(&right.start).then(left.end.cmp(&right.end)));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }

    let mut spans = Vec::new();
    let mut cursor = 0;
    for range in merged {
        if cursor < range.start {
            spans.push(Span::styled(
                text[cursor..range.start].to_string(),
                base_style,
            ));
        }
        spans.push(Span::styled(
            text[range.start..range.end].to_string(),
            highlight_style,
        ));
        cursor = range.end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base_style));
    }

    Line::from(spans)
}

fn search_cursor_position(app: &App, area: Rect) -> Option<(u16, u16)> {
    if app.input_mode != InputMode::Search || area.width < 3 || area.height < 3 {
        return None;
    }
    let input_offset = "Search: ".chars().count() as u16;
    let query_width = app.query.chars().count() as u16;
    let max_x = area.x.saturating_add(area.width.saturating_sub(2));
    let x = area
        .x
        .saturating_add(1)
        .saturating_add(input_offset)
        .saturating_add(query_width)
        .min(max_x);
    Some((x, area.y.saturating_add(1)))
}

impl App {
    fn new(
        sessions_dir: PathBuf,
        pricing: Pricing,
        include_web_cost: bool,
        index_worker_mode: IndexWorkerMode,
    ) -> Result<Self> {
        let index_launch_mode = index_worker_mode.launch_mode();
        let mut app =
            Self::initial_state(sessions_dir, pricing, include_web_cost, index_launch_mode)?;
        app.start_reload_with_mode(index_worker_mode);
        Ok(app)
    }

    fn initial_state(
        sessions_dir: PathBuf,
        pricing: Pricing,
        include_web_cost: bool,
        index_launch_mode: IndexLaunchMode,
    ) -> Result<Self> {
        let cache_dir = cache_dir_for_sessions(&sessions_dir);
        Ok(Self {
            sessions_dir,
            cache_dir,
            pricing,
            include_web_cost,
            sessions: Vec::new(),
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
            index_launch_mode,
        })
    }

    fn start_reload(&mut self) {
        self.start_reload_with_mode(self.index_launch_mode.worker_mode());
    }

    fn start_reload_with_mode(&mut self, index_worker_mode: IndexWorkerMode) {
        if self.loader.is_some() {
            self.status = "index worker already running".to_string();
            return;
        }

        let sessions_dir = self.sessions_dir.clone();
        let cache_dir = self.cache_dir.clone();
        let (tx, rx) = mpsc::channel();
        self.loader = Some(rx);
        self.loading = Some(LoadProgress {
            phase: LoadPhase::Discovering,
            current: 0,
            total: 0,
            path: None,
        });
        self.status = format!("loading sessions from {}", self.sessions_dir.display());

        thread::spawn(move || {
            run_index_worker(sessions_dir, cache_dir, tx.clone(), index_worker_mode);
            let _ = tx.send(LoadMessage::Finished);
        });
    }

    fn poll_loader(&mut self) {
        let mut clear_loader = false;

        loop {
            let Some(loader) = self.loader.as_ref() else {
                break;
            };
            match loader.try_recv() {
                Ok(LoadMessage::Progress(progress)) => {
                    self.loading = Some(progress);
                }
                Ok(LoadMessage::Loaded(result)) => {
                    self.loading = None;
                    match result {
                        Ok(result) => {
                            self.sessions = result.sessions;
                            self.search_index = result.search_index;
                            self.apply_filter();
                            self.status = format!(
                                "loaded {} sessions from {} (generation {})",
                                self.sessions.len(),
                                self.sessions_dir.display(),
                                result.generation
                            );
                            self.last_reload = Instant::now();
                        }
                        Err(err) => {
                            self.status = format!("reload failed: {err}");
                        }
                    }
                    break;
                }
                Ok(LoadMessage::Status(status)) => {
                    self.status = status;
                }
                Ok(LoadMessage::Finished) => {
                    clear_loader = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.loading = None;
                    self.status = "reload failed: loader disconnected".to_string();
                    clear_loader = true;
                    break;
                }
            }
        }

        if clear_loader {
            self.loader = None;
        }
    }

    fn apply_filter(&mut self) {
        self.filtered = self.search_index.search(&self.query);
        self.sort_filtered();
        if self.filtered.is_empty() {
            self.list_state.select(None);
        } else {
            let selected = self
                .list_state
                .selected()
                .unwrap_or_default()
                .min(self.filtered.len() - 1);
            self.list_state.select(Some(selected));
        }
        self.table_state.select(Some(0));
    }

    fn sort_filtered(&mut self) {
        let pricing = &self.pricing;
        let include_web_cost = self.include_web_cost;
        let key = self.sort_key;
        let direction = self.sort_direction;
        self.filtered.sort_by(|left, right| {
            let left_session = &self.sessions[*left];
            let right_session = &self.sessions[*right];
            let ordering = compare_sessions_for_sort(
                left_session,
                right_session,
                pricing,
                include_web_cost,
                key,
            );
            match direction {
                SortDirection::Ascending => ordering,
                SortDirection::Descending => ordering.reverse(),
            }
            .then_with(|| right_session.timestamp.cmp(&left_session.timestamp))
            .then_with(|| left_session.id.cmp(&right_session.id))
        });
    }

    fn cycle_sort_key(&mut self) {
        self.sort_key = self.sort_key.next();
        self.sort_direction = self.sort_key.default_direction();
        self.apply_filter();
    }

    fn reverse_sort_direction(&mut self) {
        self.sort_direction = self.sort_direction.reverse();
        self.apply_filter();
    }

    fn selected_session(&self) -> Option<&Session> {
        let selected = self.list_state.selected()?;
        let idx = *self.filtered.get(selected)?;
        self.sessions.get(idx)
    }

    fn selected_cost(&self) -> CostEstimate {
        self.selected_session()
            .map(|session| estimate_cost(session, &self.pricing, self.include_web_cost))
            .unwrap_or_default()
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.list_state.select(None);
            return;
        }
        let len = self.filtered.len() as isize;
        let current = self.list_state.selected().unwrap_or_default() as isize;
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.list_state.select(Some(next));
        self.table_state.select(Some(0));
    }

    fn move_detail(&mut self, delta: isize) {
        let Some(session) = self.selected_session() else {
            return;
        };
        if session.token_events.is_empty() {
            self.table_state.select(None);
            return;
        }
        let len = session.token_events.len() as isize;
        let current = self.table_state.selected().unwrap_or_default() as isize;
        let next = (current + delta).clamp(0, len - 1) as usize;
        self.table_state.select(Some(next));
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }

        if self.input_mode == InputMode::Search {
            match key.code {
                KeyCode::Enter => {
                    self.input_mode = InputMode::Browse;
                }
                KeyCode::Esc => {
                    self.input_mode = InputMode::Browse;
                    if !self.query.is_empty() {
                        self.query.clear();
                        self.apply_filter();
                    }
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.apply_filter();
                }
                KeyCode::Char(c) => {
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                    {
                        self.query.push(c);
                        self.apply_filter();
                    }
                }
                _ => {}
            }
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Esc => {
                if self.show_detail {
                    self.show_detail = false;
                    self.focus = Focus::List;
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.apply_filter();
                }
            }
            KeyCode::Char('/') => {
                self.input_mode = InputMode::Search;
            }
            KeyCode::Enter => {
                self.show_detail = !self.show_detail;
                self.focus = if self.show_detail {
                    Focus::Detail
                } else {
                    Focus::List
                };
            }
            KeyCode::Tab => {
                if self.show_detail {
                    self.focus = match self.focus {
                        Focus::List => Focus::Detail,
                        Focus::Detail => Focus::List,
                    };
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.apply_filter();
            }
            KeyCode::Up => {
                if self.focus == Focus::Detail {
                    self.move_detail(-1);
                } else {
                    self.move_selection(-1);
                }
            }
            KeyCode::Down => {
                if self.focus == Focus::Detail {
                    self.move_detail(1);
                } else {
                    self.move_selection(1);
                }
            }
            KeyCode::PageUp => {
                if self.focus == Focus::Detail {
                    self.move_detail(-10);
                } else {
                    self.move_selection(-10);
                }
            }
            KeyCode::PageDown => {
                if self.focus == Focus::Detail {
                    self.move_detail(10);
                } else {
                    self.move_selection(10);
                }
            }
            KeyCode::Char('j') => {
                if self.focus == Focus::Detail {
                    self.move_detail(1);
                } else {
                    self.move_selection(1);
                }
            }
            KeyCode::Char('k') => {
                if self.focus == Focus::Detail {
                    self.move_detail(-1);
                } else {
                    self.move_selection(-1);
                }
            }
            KeyCode::Char('r') => {
                self.start_reload();
            }
            KeyCode::Char('s') => {
                self.cycle_sort_key();
            }
            KeyCode::Char('S') => {
                self.reverse_sort_direction();
            }
            KeyCode::Char(_) => {}
            _ => {}
        }
        Ok(false)
    }
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    let sessions_dir = args.sessions.clone().unwrap_or_else(default_sessions_dir);
    let cache_dir = cache_dir_for_sessions(&sessions_dir);
    let index_worker_mode = choose_index_worker_mode(&args, &cache_dir)?;
    let pricing = Pricing::load(args.pricing.as_deref())?;
    let app = App::new(sessions_dir, pricing, !args.no_web_cost, index_worker_mode)?;
    run_tui(app)
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Args::default();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("codex-cost {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--sessions" => {
                    let Some(value) = args.next() else {
                        bail!("--sessions requires a path");
                    };
                    parsed.sessions = Some(expand_tilde(&value));
                }
                "--pricing" => {
                    let Some(value) = args.next() else {
                        bail!("--pricing requires a path");
                    };
                    parsed.pricing = Some(expand_tilde(&value));
                }
                "--no-web-cost" => {
                    parsed.no_web_cost = true;
                }
                "--read-only-index" => {
                    parsed.read_only_index = true;
                }
                "--force-index" => {
                    parsed.force_index = true;
                }
                other if other.starts_with("--sessions=") => {
                    parsed.sessions = Some(expand_tilde(&other["--sessions=".len()..]));
                }
                other if other.starts_with("--pricing=") => {
                    parsed.pricing = Some(expand_tilde(&other["--pricing=".len()..]));
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        Ok(parsed)
    }
}

fn print_help() {
    println!(
        "codex-cost {}\n\nUSAGE:\n    codex-cost [--sessions PATH] [--pricing PATH] [--no-web-cost]\n\nOPTIONS:\n    --sessions PATH      Codex session directory containing rollout JSONL files\n    --pricing PATH       Optional pricing JSON override\n    --no-web-cost        Disable web-search call cost in estimates\n    --read-only-index    Open without writing the persisted search cache\n    --force-index        Write the cache even when index.lock is held; can corrupt cache data\n    -h, --help           Print help\n    -V, --version        Print version",
        env!("CARGO_PKG_VERSION")
    );
}

fn choose_index_worker_mode(args: &Args, cache_dir: &Path) -> Result<IndexWorkerMode> {
    if args.read_only_index && args.force_index {
        bail!("--read-only-index and --force-index cannot be used together");
    }
    if args.read_only_index {
        return Ok(IndexWorkerMode::ReadOnly);
    }
    if args.force_index {
        return Ok(IndexWorkerMode::Force);
    }

    match IndexLock::try_acquire(cache_dir)? {
        Some(lock) => Ok(IndexWorkerMode::UseLock(lock)),
        None => prompt_locked_index_mode(cache_dir),
    }
}

fn prompt_locked_index_mode(cache_dir: &Path) -> Result<IndexWorkerMode> {
    let lock_path = index_lock_path(cache_dir);
    eprintln!(
        "\nAnother codex-cost instance is already holding the search index lock:\n  {}",
        lock_path.display()
    );
    if let Ok(owner) = fs::read_to_string(&lock_path) {
        let owner = owner.trim();
        if !owner.is_empty() {
            eprintln!("Lock owner: {owner}");
        }
    }
    eprintln!(
        "Use read-only mode to browse the current cached index. Force writing only if you have verified that no other codex-cost instance is running; forcing while another writer is active can corrupt the persisted cache."
    );

    loop {
        eprint!("Choose [r]ead-only, [f]orce write, or [q]uit: ");
        io::stderr().flush()?;
        let mut choice = String::new();
        if io::stdin().read_line(&mut choice)? == 0 {
            eprintln!("No input received; opening read-only.");
            return Ok(IndexWorkerMode::ReadOnly);
        }
        match choice.trim().to_ascii_lowercase().as_str() {
            "" | "r" | "read-only" | "readonly" => return Ok(IndexWorkerMode::ReadOnly),
            "q" | "quit" => std::process::exit(0),
            "f" | "force" => {
                eprint!("Type FORCE to confirm cache writes without the lock: ");
                io::stderr().flush()?;
                let mut confirm = String::new();
                if io::stdin().read_line(&mut confirm)? == 0 {
                    return Ok(IndexWorkerMode::ReadOnly);
                }
                if confirm.trim() == "FORCE" {
                    return Ok(IndexWorkerMode::Force);
                }
                eprintln!("Confirmation did not match; choose again.");
            }
            _ => eprintln!("Please enter r, f, or q."),
        }
    }
}

fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return dirs_next::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

fn default_sessions_dir() -> PathBuf {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(codex_home).join("sessions");
    }
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

fn run_tui(mut app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = loop {
        app.poll_loader();
        terminal.draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key)? {
                    break Ok(());
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn draw(frame: &mut Frame, app: &mut App) {
    let footer_height = if app.loading.is_some() { 3 } else { 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(footer_height),
        ])
        .split(frame.size());

    draw_search(frame, app, chunks[0]);
    if app.show_detail {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(chunks[1]);
        draw_session_list(frame, app, body[0]);
        draw_detail(frame, app, body[1]);
    } else {
        draw_session_table(frame, app, chunks[1]);
    }
    draw_status(frame, app, chunks[2]);
}

fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " Codex Cost TUI | {} sessions | {} matches | {} | sort {} {} ",
        app.sessions.len(),
        app.filtered.len(),
        app.input_mode.label(),
        app.sort_key.label(),
        app.sort_direction.label()
    );
    let help = match app.input_mode {
        InputMode::Browse => {
            "/ search  s sort  S reverse  Enter detail  Tab focus  r reload  Esc clear/back  q quit"
        }
        InputMode::Search => "typing edits search  Enter browse  Esc clear/back",
    };
    let text = Line::from(vec![
        Span::styled("Search: ", Style::default().fg(Color::Yellow)),
        Span::raw(app.query.as_str()),
        Span::styled("  ", Style::default()),
        Span::styled(help, Style::default().fg(Color::DarkGray)),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray));
    frame.render_widget(Paragraph::new(text).block(block), area);
    if let Some((x, y)) = search_cursor_position(app, area) {
        frame.set_cursor(x, y);
    }
}

fn draw_session_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.filtered.iter().map(|idx| {
        let session = &app.sessions[*idx];
        let cost = estimate_cost(session, &app.pricing, app.include_web_cost);
        let usage = session.final_usage().cloned().unwrap_or_default();
        let time = short_timestamp(&session.timestamp);
        let id = short_id(&session.id);
        let model = session.model.clone().unwrap_or_else(|| "-".to_string());
        let cost_text = format!("${:.2}", cost.total_cost);
        let tokens = format_tokens(usage.total_tokens);
        let prompt = one_line(session.first_user_message.as_deref().unwrap_or("-"), 80);
        Row::new(vec![
            Cell::from(highlight_matches(
                &time,
                &app.query,
                Style::default(),
                match_highlight_style(),
            )),
            Cell::from(highlight_matches(
                &id,
                &app.query,
                Style::default().fg(Color::Cyan),
                match_highlight_style(),
            )),
            Cell::from(highlight_matches(
                &model,
                &app.query,
                Style::default(),
                match_highlight_style(),
            )),
            Cell::from(highlight_matches(
                &cost_text,
                &app.query,
                Style::default().fg(Color::Green),
                match_highlight_style(),
            )),
            Cell::from(highlight_matches(
                &tokens,
                &app.query,
                Style::default(),
                match_highlight_style(),
            )),
            Cell::from(highlight_matches(
                &prompt,
                &app.query,
                Style::default(),
                match_highlight_style(),
            )),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(17),
            Constraint::Length(13),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec![
            "time",
            "session",
            "model",
            "cost",
            "tokens",
            "first prompt",
        ])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .title(" Sessions ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray)),
    )
    .highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let mut table_state = TableState::default();
    table_state.select(app.list_state.selected());
    frame.render_stateful_widget(table, area, &mut table_state);

    if let Some(message) = session_table_empty_message(app) {
        let message_area = Rect {
            x: area.x.saturating_add(2),
            y: area.y.saturating_add(3),
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(4),
        };
        let paragraph = Paragraph::new(message)
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, message_area);
    }
}

fn draw_session_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|idx| {
            let session = &app.sessions[*idx];
            let cost = estimate_cost(session, &app.pricing, app.include_web_cost);
            let mut spans = Vec::new();
            spans.extend(
                highlight_matches(
                    &short_id(&session.id),
                    &app.query,
                    Style::default().fg(Color::Cyan),
                    match_highlight_style(),
                )
                .spans,
            );
            spans.push(Span::raw(" "));
            spans.extend(
                highlight_matches(
                    session.model.as_deref().unwrap_or("-"),
                    &app.query,
                    Style::default(),
                    match_highlight_style(),
                )
                .spans,
            );
            spans.push(Span::raw(" "));
            spans.extend(
                highlight_matches(
                    &format!("${:.2}", cost.total_cost),
                    &app.query,
                    Style::default().fg(Color::Green),
                    match_highlight_style(),
                )
                .spans,
            );
            spans.push(Span::raw(" "));
            spans.extend(
                highlight_matches(
                    &one_line(session.first_user_message.as_deref().unwrap_or("-"), 50),
                    &app.query,
                    Style::default(),
                    match_highlight_style(),
                )
                .spans,
            );
            let line = Line::from(spans);
            ListItem::new(line)
        })
        .collect();

    let title = if app.focus == Focus::List {
        " Sessions (focused) "
    } else {
        " Sessions "
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(focus_style(app.focus == Focus::List)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn session_table_empty_message(app: &App) -> Option<String> {
    if app.loading.is_some() {
        return None;
    }
    if app.sessions.is_empty() {
        if app.status.contains("search index cache is incompatible") {
            return Some(app.status.clone());
        }
        return Some(format!(
            "No sessions loaded from {}",
            app.sessions_dir.display()
        ));
    }
    if app.filtered.is_empty() && !app.query.is_empty() {
        return Some(format!("No matches for \"{}\"", app.query));
    }
    None
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(session) = app.selected_session().cloned() else {
        let block = Block::default().title(" Detail ").borders(Borders::ALL);
        frame.render_widget(Paragraph::new("No session selected").block(block), area);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Min(8),
        ])
        .split(area);

    draw_detail_summary(frame, app, &session, chunks[0]);
    draw_detail_text(frame, &session, chunks[1]);
    draw_token_events(frame, app, &session, chunks[2]);
}

fn draw_detail_summary(frame: &mut Frame, app: &App, session: &Session, area: Rect) {
    let cost = estimate_cost(session, &app.pricing, app.include_web_cost);
    let usage = session.final_usage().cloned().unwrap_or_default();
    let goal_tokens = session
        .goal
        .tokens_used
        .map(format_tokens)
        .unwrap_or_else(|| "-".to_string());
    let goal_time = session
        .goal
        .time_used_seconds
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());
    let warning = if cost.known_model_price {
        ""
    } else {
        "missing model price; token cost shown as $0"
    };
    let long_context = if cost.long_context_applied {
        "yes"
    } else {
        "no"
    };

    let file_display = session.path.to_string_lossy().to_string();
    let cost_text = format!(
        "${:.4}  tokens=${:.4}  web=${:.4}  {}",
        cost.total_cost, cost.token_cost, cost.web_search_cost, warning
    );
    let input_text = format!(
        "{} uncached + {} cached",
        format_tokens(cost.uncached_input_tokens),
        format_tokens(cost.cached_input_tokens)
    );
    let output_text = format!(
        "{} total, {} reasoning",
        format_tokens(cost.output_tokens),
        format_tokens(usage.reasoning_output_tokens)
    );
    let total_text = format!(
        "{} raw tokens, {} goal tokens, {} elapsed",
        format_tokens(usage.total_tokens),
        goal_tokens,
        goal_time
    );
    let extras_text = format!(
        "{} token events, {} web searches, {} parse errors, max request input {}, long context {}",
        session.token_event_count(),
        session.web_search_calls,
        session.parse_errors.len(),
        format_tokens(session.max_request_input()),
        long_context
    );

    let rows = vec![
        Row::new(vec![Cell::from("session"), Cell::from(session.id.clone())]),
        Row::new(vec![Cell::from("file"), Cell::from(file_display)]),
        Row::new(vec![
            Cell::from("model"),
            Cell::from(session.model.clone().unwrap_or_else(|| "-".to_string())),
        ]),
        Row::new(vec![
            Cell::from("provider"),
            Cell::from(
                session
                    .model_provider
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            ),
        ]),
        Row::new(vec![
            Cell::from("cwd"),
            Cell::from(session.cwd.clone().unwrap_or_else(|| "-".to_string())),
        ]),
        Row::new(vec![Cell::from("cost"), Cell::from(cost_text)]),
        Row::new(vec![Cell::from("input"), Cell::from(input_text)]),
        Row::new(vec![Cell::from("output"), Cell::from(output_text)]),
        Row::new(vec![Cell::from("total"), Cell::from(total_text)]),
        Row::new(vec![Cell::from("extras"), Cell::from(extras_text)]),
    ];

    let title = if app.focus == Focus::Detail {
        " Detail (focused) "
    } else {
        " Detail "
    };
    let table = Table::new(rows, [Constraint::Length(10), Constraint::Min(20)])
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(focus_style(app.focus == Focus::Detail)),
        )
        .column_spacing(1);
    frame.render_widget(table, area);
}

fn draw_detail_text(frame: &mut Frame, session: &Session, area: Rect) {
    let text = vec![
        Line::from(vec![
            Span::styled("first user: ", Style::default().fg(Color::Yellow)),
            Span::raw(one_line(
                session.first_user_message.as_deref().unwrap_or("-"),
                240,
            )),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("final assistant: ", Style::default().fg(Color::Yellow)),
            Span::raw(one_line(
                session.final_assistant_message.as_deref().unwrap_or("-"),
                240,
            )),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("goal: ", Style::default().fg(Color::Yellow)),
            Span::raw(one_line(
                session.goal.objective.as_deref().unwrap_or("-"),
                240,
            )),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(" Text ").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_token_events(frame: &mut Frame, app: &mut App, session: &Session, area: Rect) {
    let rows = session.token_events.iter().map(|event| {
        Row::new(vec![
            Cell::from(short_timestamp(&event.timestamp)),
            Cell::from(format_tokens(event.total.input_tokens)),
            Cell::from(format_tokens(event.total.cached_input_tokens)),
            Cell::from(format_tokens(event.total.output_tokens)),
            Cell::from(format_tokens(event.total.total_tokens)),
            Cell::from(format_tokens(event.last.input_tokens)),
            Cell::from(
                event
                    .context_window
                    .map(format_tokens)
                    .unwrap_or_else(|| "-".to_string()),
            ),
        ])
    });

    let title = if session.token_events_are_truncated() {
        format!(" Token Events (last {}) ", session.token_events.len())
    } else {
        " Token Events ".to_string()
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(17),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(10),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(9),
        ],
    )
    .header(
        Row::new(vec![
            "time", "in", "cached", "out", "total", "last in", "window",
        ])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::default().title(title).borders(Borders::ALL))
    .highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(progress) = &app.loading {
        draw_load_progress(frame, progress, area);
        return;
    }

    let selected = app
        .selected_session()
        .map(|session| {
            let cost = app.selected_cost();
            format!(
                "{} | {} lines | estimated ${:.4}",
                session.path.display(),
                session.line_count,
                cost.total_cost
            )
        })
        .unwrap_or_else(|| "no selection".to_string());
    let status = format!(
        "{} | {} | reloaded {}s ago",
        app.status,
        selected,
        app.last_reload.elapsed().as_secs()
    );
    frame.render_widget(
        Paragraph::new(status)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Left),
        area,
    );
}

fn draw_load_progress(frame: &mut Frame, progress: &LoadProgress, area: Rect) {
    let ratio = if progress.total == 0 {
        0.0
    } else {
        (progress.current as f64 / progress.total as f64).clamp(0.0, 1.0)
    };
    let path = progress
        .path
        .as_ref()
        .map(|path| one_line(&path.display().to_string(), 70))
        .unwrap_or_default();
    let label = if progress.total == 0 {
        progress.phase.label().to_string()
    } else if path.is_empty() {
        format!(
            "{} {}/{}",
            progress.phase.label(),
            progress.current,
            progress.total
        )
    } else {
        format!(
            "{} {}/{}  {}",
            progress.phase.label(),
            progress.current,
            progress.total,
            path
        )
    };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .title(" Loading ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        )
        .gauge_style(
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn cache_dir_for_sessions(root: &Path) -> PathBuf {
    let cache_root = dirs_next::cache_dir()
        .or_else(|| dirs_next::home_dir().map(|home| home.join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("codex-cost")
        .join("index");
    cache_root.join(hash_hex(root.to_string_lossy().as_bytes()))
}

impl IndexLock {
    fn try_acquire(cache_dir: &Path) -> Result<Option<Self>> {
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

fn index_lock_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("index.lock")
}

fn run_index_worker(
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

fn run_index_writer(
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

fn run_readonly_index_worker(
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
    match load_cached_index(&root, &cache_dir) {
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

fn send_cached_snapshot(root: &Path, cache_dir: &Path, tx: &Sender<LoadMessage>) {
    match load_cached_index(root, cache_dir) {
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

fn send_reconciled_cache(root: &Path, cache_dir: &Path, tx: &Sender<LoadMessage>) -> bool {
    match reconcile_session_cache(root, cache_dir, |progress| {
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

fn send_reconciled_cache_for_paths(
    root: &Path,
    cache_dir: &Path,
    tx: &Sender<LoadMessage>,
    paths: BTreeSet<PathBuf>,
) {
    let result = reconcile_session_cache_for_paths(root, cache_dir, paths, |progress| {
        let _ = tx.send(LoadMessage::Progress(progress));
    })
    .map_err(|err| format!("{err:#}"));
    let _ = tx.send(LoadMessage::Loaded(result));
}

fn run_watch_loop(
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

fn run_readonly_manifest_poll(root: PathBuf, cache_dir: PathBuf, tx: Sender<LoadMessage>) {
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
        match load_cached_index(&root, &cache_dir) {
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
            Some(parse_session_with_fingerprint_or_error(
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
            let session = parse_session_or_error(&file.path);
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

            let parsed =
                parse_session_with_fingerprint_or_error(root, &path, &relative_path, metadata)?;
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

fn parse_session_or_error(path: &Path) -> Session {
    match parse_session(path) {
        Ok(session) => session,
        Err(err) => parse_error_session(path, err),
    }
}

fn parse_error_session(path: &Path, err: anyhow::Error) -> Session {
    Session {
        id: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string(),
        timestamp: String::new(),
        path: path.to_path_buf(),
        cwd: None,
        model: None,
        model_provider: None,
        first_user_message: None,
        final_assistant_message: None,
        token_events: Vec::new(),
        goal: GoalUsage::default(),
        web_search_calls: 0,
        line_count: 0,
        parse_errors: vec![format!("{err:#}")],
        search_messages: Vec::new(),
        cached_final_usage: None,
        max_request_input_tokens: 0,
        token_event_count: 0,
    }
}

fn postings_from_docs(docs: &[CachedSessionDoc]) -> BTreeMap<String, Vec<usize>> {
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

fn updated_or_rebuilt_postings(
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

fn same_doc_order(left: &[CachedSessionDoc], right: &[CachedSessionDoc]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.relative_path == right.relative_path)
}

fn remap_postings_for_doc_order(
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

fn update_postings_for_doc(
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

fn persist_cache(
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

fn compact_docs_for_cache(docs: &[CachedSessionDoc]) -> Vec<CachedSessionDoc> {
    let mut cached_docs = docs.to_vec();
    for doc in &mut cached_docs {
        doc.session.compact_for_cache();
    }
    cached_docs
}

fn load_manifest(cache_dir: &Path) -> Result<Option<CacheManifest>> {
    let path = cache_dir.join("manifest.json");
    if !path.exists() {
        return Ok(None);
    }
    let manifest: CacheManifest = read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "manifest.json", manifest.schema_version)?;
    Ok(Some(manifest))
}

fn load_sessions_cache(cache_dir: &Path) -> Result<Option<SessionsCache>> {
    let path = cache_dir.join("sessions.json");
    if !path.exists() {
        return Ok(None);
    }
    let cache: SessionsCache = read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "sessions.json", cache.schema_version)?;
    Ok(Some(cache))
}

fn load_postings_cache(cache_dir: &Path) -> Result<Option<PostingsCache>> {
    let path = cache_dir.join("postings.json");
    if !path.exists() {
        return Ok(None);
    }
    let cache: PostingsCache = read_json(&path).with_context(|| cache_delete_hint(cache_dir))?;
    ensure_cache_schema(cache_dir, "postings.json", cache.schema_version)?;
    Ok(Some(cache))
}

fn load_merkle_snapshot(cache_dir: &Path) -> Result<Option<MerkleSnapshot>> {
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

fn ensure_cache_schema(cache_dir: &Path, file_name: &str, actual: u32) -> Result<()> {
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

fn ensure_cache_root(root: &Path, cache_dir: &Path, manifest: &CacheManifest) -> Result<()> {
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

fn incompatible_cache_message(cache_dir: &Path, detail: &str) -> String {
    format!(
        "search index cache is incompatible ({detail}); delete the cache folder and restart: {}",
        cache_dir.display()
    )
}

fn cache_delete_hint(cache_dir: &Path) -> String {
    format!(
        "delete the search index cache folder and restart: {}",
        cache_dir.display()
    )
}

#[cfg(test)]
fn build_merkle_snapshot(root: &Path, previous: Option<&MerkleSnapshot>) -> Result<MerkleSnapshot> {
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

fn build_merkle_plan<F>(
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
    fn has_changes(&self) -> bool {
        self.has_deleted_paths
            || self
                .files
                .iter()
                .any(|file| file.reused_fingerprint.is_none())
    }
}

fn collect_fingerprints(node: &MerkleNode, fingerprints: &mut BTreeMap<String, FileFingerprint>) {
    if let Some(fingerprint) = &node.fingerprint {
        fingerprints.insert(node.relative_path.clone(), fingerprint.clone());
    }
    for child in &node.children {
        collect_fingerprints(child, fingerprints);
    }
}

fn build_merkle_root(fingerprints: &BTreeMap<String, FileFingerprint>) -> MerkleNode {
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

fn insert_merkle_leaf(
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

fn finalize_merkle_node(node: MerkleBuilderNode) -> MerkleNode {
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

fn file_metadata_parts(path: &Path) -> Result<FileMetadataParts> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat session file {}", path.display()))?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(system_time_unix_nanos)
        .unwrap_or_default();

    Ok(FileMetadataParts {
        size: metadata.len(),
        modified_unix_nanos,
    })
}

fn hash_file_fingerprint(
    path: &Path,
    relative_path: &str,
    metadata: FileMetadataParts,
) -> Result<FileFingerprint> {
    let content = fs::read(path)
        .with_context(|| format!("failed to hash session file {}", path.display()))?;
    let content_hash = hash_hex(&content);
    Ok(fingerprint_from_hash(relative_path, metadata, content_hash))
}

fn fingerprint_from_hash(
    relative_path: &str,
    metadata: FileMetadataParts,
    content_hash: String,
) -> FileFingerprint {
    let leaf_hash = hash_text(&format!(
        "file\0{}\0{}\0{}\0{}",
        relative_path, metadata.size, metadata.modified_unix_nanos, content_hash
    ));
    FileFingerprint {
        size: metadata.size,
        modified_unix_nanos: metadata.modified_unix_nanos,
        content_hash,
        leaf_hash,
    }
}

fn directory_hash(relative_path: &str, children: &[MerkleNode]) -> String {
    let mut text = format!("dir\0{relative_path}\0");
    for child in children {
        text.push_str(&child.name);
        text.push('\0');
        text.push_str(&child.hash);
        text.push('\0');
    }
    hash_text(&text)
}

fn relative_path_string(root: &Path, path: &Path) -> Result<String> {
    if let Ok(relative) = path.strip_prefix(root) {
        return Ok(path_components_string(relative));
    }

    let canonical_root = root.canonicalize().ok();
    let canonical_path = canonical_equivalent_path(path);
    if let (Some(root), Some(path)) = (canonical_root.as_ref(), canonical_path.as_ref()) {
        if let Ok(relative) = path.strip_prefix(root) {
            return Ok(path_components_string(relative));
        }
    }

    bail!("{} is not under {}", path.display(), root.display())
}

fn canonical_equivalent_path(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return path.canonicalize().ok();
    }

    let parent = path.parent()?.canonicalize().ok()?;
    let file_name = path.file_name()?;
    Some(parent.join(file_name))
}

fn path_components_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    {
        let mut file =
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn hash_text(text: &str) -> String {
    hash_hex(text.as_bytes())
}

fn hash_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_bytes(&digest)
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", *byte));
    }
    out
}

fn system_time_unix_nanos(time: SystemTime) -> Option<u64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    duration
        .as_secs()
        .checked_mul(1_000_000_000)?
        .checked_add(u64::from(duration.subsec_nanos()))
}

fn unix_seconds_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[cfg(test)]
fn load_sessions_with_progress<F>(root: &Path, mut progress: F) -> Result<LoadResult>
where
    F: FnMut(LoadProgress),
{
    reconcile_session_cache(root, &cache_dir_for_sessions(root), |load_progress| {
        progress(load_progress);
    })
}

fn discover_session_paths(root: &Path) -> Vec<PathBuf> {
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

fn parse_session_with_fingerprint_or_error(
    _root: &Path,
    path: &Path,
    relative_path: &str,
    metadata: FileMetadataParts,
) -> Result<ParsedSessionFile> {
    match parse_session_with_fingerprint(path, relative_path, metadata) {
        Ok(parsed) => Ok(parsed),
        Err(err) => Ok(ParsedSessionFile {
            session: parse_error_session(path, err),
            fingerprint: hash_file_fingerprint(path, relative_path, metadata)?,
        }),
    }
}

fn parse_session_with_fingerprint(
    path: &Path,
    relative_path: &str,
    metadata: FileMetadataParts,
) -> Result<ParsedSessionFile> {
    let (session, fingerprint) = parse_session_inner(path, Some((relative_path, metadata)))?;
    Ok(ParsedSessionFile {
        session,
        fingerprint: fingerprint.expect("fingerprint requested"),
    })
}

fn parse_session(path: &Path) -> Result<Session> {
    let (session, _fingerprint) = parse_session_inner(path, None)?;
    Ok(session)
}

fn parse_session_inner(
    path: &Path,
    fingerprint_input: Option<(&str, FileMetadataParts)>,
) -> Result<(Session, Option<FileFingerprint>)> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = fingerprint_input.as_ref().map(|_| Sha256::new());

    let mut id = String::new();
    let mut timestamp = String::new();
    let mut cwd = None;
    let mut model = None;
    let mut model_provider = None;
    let mut first_user_message = None;
    let mut final_assistant_message = None;
    let mut search_messages = Vec::new();
    let mut token_events = Vec::new();
    let mut goal = GoalUsage::default();
    let mut web_search_calls = 0;
    let mut parse_errors = Vec::new();
    let mut line_count = 0;
    let mut previous_total_usage: Option<TokenUsage> = None;
    let mut current_model: Option<String> = None;

    let mut line_bytes = Vec::new();
    loop {
        line_bytes.clear();
        let next_line = line_count + 1;
        let bytes_read = reader
            .read_until(b'\n', &mut line_bytes)
            .with_context(|| format!("failed to read line {next_line}"))?;
        if bytes_read == 0 {
            break;
        }
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&line_bytes);
        }
        line_count += 1;
        let line_idx = line_count;
        let line = std::str::from_utf8(&line_bytes)
            .with_context(|| format!("line {line_idx} is not valid UTF-8"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                parse_errors.push(format!("line {}: {}", line_idx, err));
                continue;
            }
        };

        let top_timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if timestamp.is_empty() && !top_timestamp.is_empty() {
            timestamp = top_timestamp.clone();
        }

        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "session_meta" => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                if let Some(meta_id) = payload.get("id").and_then(Value::as_str) {
                    id = meta_id.to_string();
                }
                if let Some(meta_ts) = payload.get("timestamp").and_then(Value::as_str) {
                    timestamp = meta_ts.to_string();
                }
                if let Some(meta_cwd) = payload.get("cwd").and_then(Value::as_str) {
                    cwd = Some(meta_cwd.to_string());
                }
                if let Some(provider) = payload.get("model_provider").and_then(Value::as_str) {
                    model_provider = Some(provider.to_string());
                }
            }
            "turn_context" => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                if let Some(turn_cwd) = payload.get("cwd").and_then(Value::as_str) {
                    cwd = Some(turn_cwd.to_string());
                }
                if let Some(turn_model) = model_from_payload(Some(payload)) {
                    current_model = Some(turn_model.clone());
                    model = Some(turn_model.to_string());
                }
            }
            "response_item" => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                match payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "message" => {
                        let role = payload
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let text = extract_message_text(payload);
                        if role == "user" && !text.is_empty() {
                            record_user_message(
                                &mut first_user_message,
                                &mut search_messages,
                                &text,
                            );
                        } else if role == "assistant" && !text.is_empty() {
                            final_assistant_message = Some(text);
                        }
                    }
                    "web_search_call" => {
                        web_search_calls += 1;
                    }
                    _ => {}
                }
                if let Some(raw_usage) = usage_from_exec_result(&value) {
                    if raw_usage.is_zero() {
                        continue;
                    }
                    if let Some(parsed_model) = model_from_result(&value) {
                        current_model = Some(parsed_model.clone());
                        model = Some(parsed_model);
                    } else if model.is_none() {
                        model = current_model.clone().or_else(|| Some("gpt-5".to_string()));
                    }
                    let total = previous_total_usage
                        .clone()
                        .unwrap_or_default()
                        .saturating_add(&raw_usage);
                    previous_total_usage = Some(total.clone());
                    token_events.push(TokenEvent {
                        timestamp: timestamp_from_result(&value)
                            .unwrap_or_else(|| top_timestamp.clone()),
                        total,
                        last: raw_usage.normalize_total(),
                        context_window: None,
                    });
                }
            }
            "event_msg" => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                match payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "token_count" => {
                        let info = payload.get("info").unwrap_or(&Value::Null);
                        if let Some(parsed_model) = model_from_payload(Some(payload))
                            .or_else(|| model_from_payload(Some(info)))
                        {
                            current_model = Some(parsed_model.clone());
                            model = Some(parsed_model);
                        } else if model.is_none() {
                            model = current_model.clone();
                        }

                        let total_usage = usage_from_token_count(info);
                        let last_usage = info
                            .get("last_token_usage")
                            .and_then(usage_from_value)
                            .or_else(|| {
                                total_usage.as_ref().map(|usage| {
                                    usage.saturating_sub(previous_total_usage.as_ref())
                                })
                            });
                        let Some(last_usage) = last_usage else {
                            continue;
                        };
                        if last_usage.is_zero() {
                            continue;
                        }
                        let total = total_usage
                            .unwrap_or_else(|| {
                                previous_total_usage
                                    .clone()
                                    .unwrap_or_default()
                                    .saturating_add(&last_usage)
                            })
                            .normalize_total();
                        previous_total_usage = Some(total.clone());
                        token_events.push(TokenEvent {
                            timestamp: top_timestamp,
                            total,
                            last: last_usage.normalize_total(),
                            context_window: info
                                .get("model_context_window")
                                .and_then(Value::as_u64),
                        });
                    }
                    "thread_goal_updated" => {
                        if let Some(goal_value) = payload.get("goal") {
                            if let Some(objective) =
                                goal_value.get("objective").and_then(Value::as_str)
                            {
                                goal.objective = Some(objective.to_string());
                            }
                            if let Some(status) = goal_value.get("status").and_then(Value::as_str) {
                                goal.status = Some(status.to_string());
                            }
                            goal.tokens_used = goal_value.get("tokensUsed").and_then(Value::as_u64);
                            goal.time_used_seconds =
                                goal_value.get("timeUsedSeconds").and_then(Value::as_u64);
                        }
                    }
                    "user_message" => {
                        if let Some(message) = payload.get("message").and_then(Value::as_str) {
                            record_user_message(
                                &mut first_user_message,
                                &mut search_messages,
                                message,
                            );
                        }
                    }
                    "agent_message" => {
                        if let Some(message) = payload.get("message").and_then(Value::as_str) {
                            final_assistant_message = Some(message.to_string());
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                if let Some(raw_usage) = usage_from_exec_result(&value) {
                    if raw_usage.is_zero() {
                        continue;
                    }
                    if let Some(parsed_model) = model_from_result(&value) {
                        current_model = Some(parsed_model.clone());
                        model = Some(parsed_model);
                    } else if model.is_none() {
                        model = current_model.clone().or_else(|| Some("gpt-5".to_string()));
                    }
                    let total = previous_total_usage
                        .clone()
                        .unwrap_or_default()
                        .saturating_add(&raw_usage);
                    previous_total_usage = Some(total.clone());
                    token_events.push(TokenEvent {
                        timestamp: timestamp_from_result(&value)
                            .unwrap_or_else(|| top_timestamp.clone()),
                        total,
                        last: raw_usage.normalize_total(),
                        context_window: None,
                    });
                }
            }
        }
    }

    if id.is_empty() {
        id = infer_id_from_path(path);
    }
    if timestamp.is_empty() {
        timestamp = path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|_| String::from("unknown"))
            .unwrap_or_default();
    }

    let cached_final_usage = token_events.last().map(|event| event.total.clone());
    let max_request_input_tokens = token_events
        .iter()
        .map(|event| event.last.input_tokens)
        .max()
        .unwrap_or_default();
    let token_event_count = token_events.len();
    let session = Session {
        id,
        timestamp,
        path: path.to_path_buf(),
        cwd,
        model,
        model_provider,
        first_user_message,
        final_assistant_message,
        token_events,
        goal,
        web_search_calls,
        line_count,
        parse_errors,
        search_messages,
        cached_final_usage,
        max_request_input_tokens,
        token_event_count,
    };
    let fingerprint = match (fingerprint_input, hasher) {
        (Some((relative_path, metadata)), Some(hasher)) => Some(fingerprint_from_hash(
            relative_path,
            metadata,
            hex_bytes(&hasher.finalize()),
        )),
        _ => None,
    };

    Ok((session, fingerprint))
}

fn extract_message_text(payload: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(content) = payload.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else if let Some(text) = item.get("input_text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else if let Some(text) = item.get("output_text").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
        }
    }
    parts.join("\n")
}

fn record_user_message(
    first_user_message: &mut Option<String>,
    search_messages: &mut Vec<String>,
    text: &str,
) {
    if !is_searchable_user_message(text) {
        return;
    }
    if first_user_message.is_none() {
        *first_user_message = Some(text.to_string());
    }
    search_messages.push(text.to_string());
}

fn is_searchable_user_message(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.starts_with("<environment_context>")
}

fn non_empty_json_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn model_from_payload(value: Option<&Value>) -> Option<String> {
    let value = value?;
    ["model", "model_name"]
        .into_iter()
        .find_map(|key| non_empty_json_string(value.get(key)))
        .or_else(|| {
            value
                .get("metadata")
                .and_then(|metadata| non_empty_json_string(metadata.get("model")))
        })
}

fn model_from_result(value: &Value) -> Option<String> {
    model_from_payload(Some(value))
        .or_else(|| model_from_payload(value.get("data")))
        .or_else(|| model_from_payload(value.get("result")))
        .or_else(|| model_from_payload(value.get("response")))
        .or_else(|| model_from_payload(value.get("payload")))
}

fn usage_from_token_count(info: &Value) -> Option<TokenUsage> {
    info.get("total_token_usage").and_then(usage_from_value)
}

fn usage_from_value(value: &Value) -> Option<TokenUsage> {
    value
        .is_object()
        .then(|| TokenUsage::from_value(value).normalize_total())
}

fn usage_object_from_result(value: &Value) -> Option<&Value> {
    value
        .get("usage")
        .or_else(|| value.get("data").and_then(|data| data.get("usage")))
        .or_else(|| value.get("result").and_then(|result| result.get("usage")))
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("usage"))
        })
        .or_else(|| {
            value
                .get("payload")
                .and_then(|payload| payload.get("usage"))
        })
}

fn usage_from_exec_result(value: &Value) -> Option<TokenUsage> {
    let usage = usage_object_from_result(value)?;
    let input = json_u64(usage.get("input_tokens"))
        .or_else(|| json_u64(usage.get("prompt_tokens")))
        .or_else(|| json_u64(usage.get("input")))
        .unwrap_or(0);
    let cached = json_u64(usage.get("cached_input_tokens"))
        .or_else(|| json_u64(usage.get("cache_read_input_tokens")))
        .or_else(|| json_u64(usage.get("cached_tokens")))
        .unwrap_or(0);
    let output = json_u64(usage.get("output_tokens"))
        .or_else(|| json_u64(usage.get("completion_tokens")))
        .or_else(|| json_u64(usage.get("output")))
        .unwrap_or(0);
    let reasoning = json_u64(usage.get("reasoning_output_tokens"))
        .or_else(|| json_u64(usage.get("reasoning_tokens")))
        .unwrap_or(0);
    let total = json_u64(usage.get("total_tokens")).unwrap_or(0);
    let usage = TokenUsage {
        input_tokens: input,
        cached_input_tokens: cached,
        output_tokens: output,
        reasoning_output_tokens: reasoning,
        total_tokens: total,
    }
    .normalize_total();
    (!usage.is_zero()).then_some(usage)
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn timestamp_from_result(value: &Value) -> Option<String> {
    timestamp_value(value.get("timestamp"))
        .or_else(|| timestamp_value(value.get("created_at")))
        .or_else(|| timestamp_value(value.get("createdAt")))
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| timestamp_value(data.get("timestamp")))
        })
        .or_else(|| {
            value
                .get("result")
                .and_then(|result| timestamp_value(result.get("timestamp")))
        })
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| timestamp_value(response.get("timestamp")))
        })
        .or_else(|| {
            value
                .get("payload")
                .and_then(|payload| timestamp_value(payload.get("timestamp")))
        })
}

fn timestamp_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn infer_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    stem.rsplit('-')
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("-")
}

fn estimate_cost(session: &Session, pricing: &Pricing, include_web_cost: bool) -> CostEstimate {
    let usage = session.final_usage().cloned().unwrap_or_default();
    let cached = usage.cached_input_tokens.min(usage.input_tokens);
    let uncached = usage.input_tokens.saturating_sub(cached);
    let model = session.model.as_deref().unwrap_or_default();
    let mut estimate = CostEstimate {
        uncached_input_tokens: uncached,
        cached_input_tokens: cached,
        output_tokens: usage.output_tokens,
        known_model_price: false,
        ..CostEstimate::default()
    };

    if let Some(model_price) = pricing.models.get(model) {
        let long_context_applied = model_price
            .long_context_threshold
            .map(|threshold| session.max_request_input() > threshold)
            .unwrap_or(false);
        let multiplier = if long_context_applied {
            model_price.long_context_multiplier.unwrap_or(1.0)
        } else {
            1.0
        };
        estimate.token_cost = multiplier
            * ((uncached as f64 / 1_000_000.0) * model_price.input_per_m
                + (cached as f64 / 1_000_000.0) * model_price.cached_input_per_m
                + (usage.output_tokens as f64 / 1_000_000.0) * model_price.output_per_m);
        estimate.long_context_applied = long_context_applied;
        estimate.known_model_price = true;
    }

    if include_web_cost {
        estimate.web_search_cost =
            (session.web_search_calls as f64 / 1_000.0) * pricing.web_search_per_1k;
    }
    estimate.total_cost = estimate.token_cost + estimate.web_search_cost;
    estimate
}

fn short_id(id: &str) -> String {
    if id.len() <= 13 {
        id.to_string()
    } else {
        id.chars().take(13).collect()
    }
}

fn short_timestamp(timestamp: &str) -> String {
    if timestamp.len() >= 19 {
        timestamp[0..19].replace('T', " ")
    } else {
        timestamp.to_string()
    }
}

fn one_line(text: &str, max_len: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_len {
        normalized
    } else {
        let keep = max_len.saturating_sub(1);
        let mut out: String = normalized.chars().take(keep).collect();
        out.push('…');
        out
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("codex-cost-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
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

        let session = parse_session(&path).unwrap();
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

        let session = parse_session(&path).unwrap();

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

        let session = parse_session(&path).unwrap();
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

        let session = parse_session(&path).unwrap();
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

        let parsed = parse_session_with_fingerprint(
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

        let first = reconcile_session_cache(&sessions_dir, &cache_dir, |_progress| {}).unwrap();
        let cached = load_cached_index(&sessions_dir, &cache_dir)
            .unwrap()
            .expect("cache should load");

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
        let second = reconcile_session_cache(&sessions_dir, &cache_dir, |_progress| {}).unwrap();

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

        reconcile_session_cache(&sessions_dir, &cache_dir, |_progress| {}).unwrap();
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
        let result = reconcile_session_cache_for_paths(
            &sessions_dir,
            &cache_dir,
            BTreeSet::from([second_path.clone()]),
            |update| {
                if update.phase == LoadPhase::Parsing {
                    progress.push((update.current, update.total));
                }
            },
        )
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

        reconcile_session_cache(&sessions_dir, &cache_dir, |_progress| {}).unwrap();
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
        let result = reconcile_session_cache_for_paths(
            &sessions_dir,
            &cache_dir,
            BTreeSet::from([second_path]),
            |update| {
                if update.phase == LoadPhase::Parsing {
                    progress.push((update.current, update.total));
                }
            },
        )
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

        let err = reconcile_session_cache(&sessions_dir, &cache_dir, |_progress| {})
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
}
