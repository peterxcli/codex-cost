use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{ListState, TableState};

use crate::cache::CacheStore;
use crate::models::Session;
use crate::pricing::{estimate_cost, CostEstimate, Pricing};
use crate::search::SearchIndex;
use crate::worker::{
    IndexLaunchMode, IndexWorker, IndexWorkerMode, LoadMessage, LoadPhase, LoadProgress,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    List,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputMode {
    Browse,
    Search,
}

impl InputMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            InputMode::Browse => "browse",
            InputMode::Search => "search",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SortKey {
    TotalCost,
    Timestamp,
    Tokens,
    WebSearches,
    Model,
    Session,
    FirstPrompt,
}

impl SortKey {
    pub(crate) fn next(self) -> Self {
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

    pub(crate) fn default_direction(self) -> SortDirection {
        match self {
            SortKey::Model | SortKey::Session | SortKey::FirstPrompt => SortDirection::Ascending,
            SortKey::TotalCost | SortKey::Timestamp | SortKey::Tokens | SortKey::WebSearches => {
                SortDirection::Descending
            }
        }
    }

    pub(crate) fn label(self) -> &'static str {
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
pub(crate) enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    pub(crate) fn reverse(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SortDirection::Ascending => "asc",
            SortDirection::Descending => "desc",
        }
    }
}

pub(crate) struct App {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) pricing: Pricing,
    pub(crate) include_web_cost: bool,
    pub(crate) sessions: Vec<Session>,
    pub(crate) search_index: SearchIndex,
    pub(crate) filtered: Vec<usize>,
    pub(crate) query: String,
    pub(crate) list_state: ListState,
    pub(crate) table_state: TableState,
    pub(crate) focus: Focus,
    pub(crate) input_mode: InputMode,
    pub(crate) show_detail: bool,
    pub(crate) status: String,
    pub(crate) last_reload: Instant,
    pub(crate) loading: Option<LoadProgress>,
    pub(crate) loader: Option<Receiver<LoadMessage>>,
    pub(crate) sort_key: SortKey,
    pub(crate) sort_direction: SortDirection,
    pub(crate) index_launch_mode: IndexLaunchMode,
}
pub(crate) fn compare_sessions_for_sort(
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
impl App {
    pub(crate) fn new(
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

    pub(crate) fn initial_state(
        sessions_dir: PathBuf,
        pricing: Pricing,
        include_web_cost: bool,
        index_launch_mode: IndexLaunchMode,
    ) -> Result<Self> {
        let cache_dir = CacheStore::new(sessions_dir.clone())
            .cache_dir()
            .to_path_buf();
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

    pub(crate) fn start_reload(&mut self) {
        self.start_reload_with_mode(self.index_launch_mode.worker_mode());
    }

    pub(crate) fn start_reload_with_mode(&mut self, index_worker_mode: IndexWorkerMode) {
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
            IndexWorker::run(sessions_dir, cache_dir, tx.clone(), index_worker_mode);
            let _ = tx.send(LoadMessage::Finished);
        });
    }

    pub(crate) fn poll_loader(&mut self) {
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

    pub(crate) fn apply_filter(&mut self) {
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

    pub(crate) fn sort_filtered(&mut self) {
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

    pub(crate) fn cycle_sort_key(&mut self) {
        self.sort_key = self.sort_key.next();
        self.sort_direction = self.sort_key.default_direction();
        self.apply_filter();
    }

    pub(crate) fn reverse_sort_direction(&mut self) {
        self.sort_direction = self.sort_direction.reverse();
        self.apply_filter();
    }

    pub(crate) fn selected_session(&self) -> Option<&Session> {
        let selected = self.list_state.selected()?;
        let idx = *self.filtered.get(selected)?;
        self.sessions.get(idx)
    }

    pub(crate) fn selected_cost(&self) -> CostEstimate {
        self.selected_session()
            .map(|session| estimate_cost(session, &self.pricing, self.include_web_cost))
            .unwrap_or_default()
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
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

    pub(crate) fn move_detail(&mut self, delta: isize) {
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

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if Self::is_control_c(key) {
            return Ok(true);
        }

        let should_quit = match self.input_mode {
            InputMode::Search => {
                self.handle_search_key(key);
                false
            }
            InputMode::Browse => self.handle_browse_key(key),
        };
        Ok(should_quit)
    }

    fn is_control_c(key: KeyEvent) -> bool {
        key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.input_mode = InputMode::Browse;
            }
            KeyCode::Esc => {
                self.input_mode = InputMode::Browse;
                self.clear_query();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.apply_filter();
            }
            KeyCode::Char(c) if Self::is_plain_text_key(key) => {
                self.query.push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Esc => {
                self.close_detail_or_clear_query();
                false
            }
            KeyCode::Char('/') => {
                self.input_mode = InputMode::Search;
                false
            }
            KeyCode::Enter => {
                self.toggle_detail();
                false
            }
            KeyCode::Tab => {
                self.toggle_focus();
                false
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.apply_filter();
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_focused(-1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_focused(1);
                false
            }
            KeyCode::PageUp => {
                self.move_focused(-10);
                false
            }
            KeyCode::PageDown => {
                self.move_focused(10);
                false
            }
            KeyCode::Char('r') => {
                self.start_reload();
                false
            }
            KeyCode::Char('s') => {
                self.cycle_sort_key();
                false
            }
            KeyCode::Char('S') => {
                self.reverse_sort_direction();
                false
            }
            KeyCode::Char(_) | KeyCode::Null => false,
            _ => false,
        }
    }

    fn is_plain_text_key(key: KeyEvent) -> bool {
        !key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
    }

    fn clear_query(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.apply_filter();
        }
    }

    fn close_detail_or_clear_query(&mut self) {
        if self.show_detail {
            self.show_detail = false;
            self.focus = Focus::List;
        } else {
            self.clear_query();
        }
    }

    fn toggle_detail(&mut self) {
        self.show_detail = !self.show_detail;
        self.focus = if self.show_detail {
            Focus::Detail
        } else {
            Focus::List
        };
    }

    fn toggle_focus(&mut self) {
        if self.show_detail {
            self.focus = match self.focus {
                Focus::List => Focus::Detail,
                Focus::Detail => Focus::List,
            };
        }
    }

    fn move_focused(&mut self, delta: isize) {
        if self.focus == Focus::Detail {
            self.move_detail(delta);
        } else {
            self.move_selection(delta);
        }
    }
}
