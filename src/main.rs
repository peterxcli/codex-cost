use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use serde_json::Value;
use walkdir::WalkDir;

#[derive(Debug, Default)]
struct Args {
    sessions: Option<PathBuf>,
    pricing: Option<PathBuf>,
    no_web_cost: bool,
}

#[derive(Clone, Debug, Default)]
struct TokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Debug)]
struct TokenEvent {
    timestamp: String,
    total: TokenUsage,
    last: TokenUsage,
    context_window: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct GoalUsage {
    objective: Option<String>,
    status: Option<String>,
    tokens_used: Option<u64>,
    time_used_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
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
    raw_lower: String,
    parse_errors: Vec<String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    List,
    Detail,
}

struct App {
    sessions_dir: PathBuf,
    pricing: Pricing,
    include_web_cost: bool,
    sessions: Vec<Session>,
    filtered: Vec<usize>,
    query: String,
    list_state: ListState,
    table_state: TableState,
    focus: Focus,
    show_detail: bool,
    status: String,
    last_reload: Instant,
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
        self.token_events.last().map(|event| &event.total)
    }

    fn max_request_input(&self) -> u64 {
        self.token_events
            .iter()
            .map(|event| event.last.input_tokens)
            .max()
            .unwrap_or_default()
    }

    fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        contains_ignore_case(&self.id, needle)
            || contains_ignore_case(&self.path.display().to_string(), needle)
            || self
                .model
                .as_deref()
                .map(|text| contains_ignore_case(text, needle))
                .unwrap_or(false)
            || self
                .model_provider
                .as_deref()
                .map(|text| contains_ignore_case(text, needle))
                .unwrap_or(false)
            || self
                .cwd
                .as_deref()
                .map(|text| contains_ignore_case(text, needle))
                .unwrap_or(false)
            || self
                .first_user_message
                .as_deref()
                .map(|text| contains_ignore_case(text, needle))
                .unwrap_or(false)
            || self
                .parse_errors
                .iter()
                .any(|error| contains_ignore_case(error, needle))
            || self.raw_lower.contains(needle)
    }
}

impl App {
    fn new(sessions_dir: PathBuf, pricing: Pricing, include_web_cost: bool) -> Result<Self> {
        let mut app = Self {
            sessions_dir,
            pricing,
            include_web_cost,
            sessions: Vec::new(),
            filtered: Vec::new(),
            query: String::new(),
            list_state: ListState::default(),
            table_state: TableState::default(),
            focus: Focus::List,
            show_detail: false,
            status: String::new(),
            last_reload: Instant::now(),
        };
        app.reload()?;
        Ok(app)
    }

    fn reload(&mut self) -> Result<()> {
        self.sessions = load_sessions(&self.sessions_dir)?;
        self.sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        self.apply_filter();
        self.status = format!(
            "loaded {} sessions from {}",
            self.sessions.len(),
            self.sessions_dir.display()
        );
        self.last_reload = Instant::now();
        Ok(())
    }

    fn apply_filter(&mut self) {
        let needle = self.query.to_lowercase();
        self.filtered = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(idx, session)| session.matches(&needle).then_some(idx))
            .collect();
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
                if let Err(err) = self.reload() {
                    self.status = format!("reload failed: {err:#}");
                }
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
        Ok(false)
    }
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    let sessions_dir = args.sessions.unwrap_or_else(default_sessions_dir);
    let pricing = Pricing::load(args.pricing.as_deref())?;
    let app = App::new(sessions_dir, pricing, !args.no_web_cost)?;
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
                    println!("codex-cost-tui {}", env!("CARGO_PKG_VERSION"));
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
        "codex-cost-tui {}\n\nUSAGE:\n    codex-cost-tui [--sessions PATH] [--pricing PATH] [--no-web-cost]\n\nOPTIONS:\n    --sessions PATH    Codex session directory containing rollout JSONL files\n    --pricing PATH     Optional pricing JSON override\n    --no-web-cost      Disable web-search call cost in estimates\n    -h, --help         Print help\n    -V, --version      Print version",
        env!("CARGO_PKG_VERSION")
    );
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
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
        " Codex Cost TUI | {} sessions | {} matches ",
        app.sessions.len(),
        app.filtered.len()
    );
    let text = Line::from(vec![
        Span::styled("Search: ", Style::default().fg(Color::Yellow)),
        Span::raw(app.query.as_str()),
        Span::styled("  ", Style::default()),
        Span::styled(
            "Enter detail  Tab focus  r reload  Esc clear/back  q quit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray));
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_session_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.filtered.iter().map(|idx| {
        let session = &app.sessions[*idx];
        let cost = estimate_cost(session, &app.pricing, app.include_web_cost);
        let usage = session.final_usage().cloned().unwrap_or_default();
        Row::new(vec![
            Cell::from(short_timestamp(&session.timestamp)),
            Cell::from(short_id(&session.id)),
            Cell::from(session.model.clone().unwrap_or_else(|| "-".to_string())),
            Cell::from(format!("${:.2}", cost.total_cost)),
            Cell::from(format_tokens(usage.total_tokens)),
            Cell::from(one_line(
                session.first_user_message.as_deref().unwrap_or("-"),
                80,
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
}

fn draw_session_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|idx| {
            let session = &app.sessions[*idx];
            let cost = estimate_cost(session, &app.pricing, app.include_web_cost);
            let line = Line::from(vec![
                Span::styled(short_id(&session.id), Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::raw(session.model.as_deref().unwrap_or("-")),
                Span::raw(" "),
                Span::styled(
                    format!("${:.2}", cost.total_cost),
                    Style::default().fg(Color::Green),
                ),
                Span::raw(" "),
                Span::raw(one_line(
                    session.first_user_message.as_deref().unwrap_or("-"),
                    50,
                )),
            ]);
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
        session.token_events.len(),
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
    .block(
        Block::default()
            .title(" Token Events ")
            .borders(Borders::ALL),
    )
    .highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
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

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_lowercase().contains(needle_lower)
}

fn load_sessions(root: &Path) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    if !root.exists() {
        return Ok(sessions);
    }

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            match parse_session(entry.path()) {
                Ok(session) => sessions.push(session),
                Err(err) => {
                    sessions.push(Session {
                        id: entry
                            .path()
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        timestamp: String::new(),
                        path: entry.path().to_path_buf(),
                        cwd: None,
                        model: None,
                        model_provider: None,
                        first_user_message: None,
                        final_assistant_message: None,
                        token_events: Vec::new(),
                        goal: GoalUsage::default(),
                        web_search_calls: 0,
                        line_count: 0,
                        raw_lower: String::new(),
                        parse_errors: vec![format!("{err:#}")],
                    });
                }
            }
        }
    }
    Ok(sessions)
}

fn parse_session(path: &Path) -> Result<Session> {
    let mut raw = String::new();
    File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .read_to_string(&mut raw)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut id = String::new();
    let mut timestamp = String::new();
    let mut cwd = None;
    let mut model = None;
    let mut model_provider = None;
    let mut first_user_message = None;
    let mut final_assistant_message = None;
    let mut token_events = Vec::new();
    let mut goal = GoalUsage::default();
    let mut web_search_calls = 0;
    let mut parse_errors = Vec::new();
    let mut line_count = 0;
    let mut previous_total_usage: Option<TokenUsage> = None;
    let mut current_model: Option<String> = None;

    for (line_idx, line) in reader.lines().enumerate() {
        line_count += 1;
        let line = line.with_context(|| format!("failed to read line {}", line_idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                parse_errors.push(format!("line {}: {}", line_idx + 1, err));
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
                        if role == "user" && first_user_message.is_none() && !text.is_empty() {
                            first_user_message = Some(text);
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

    Ok(Session {
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
        raw_lower: raw.to_lowercase(),
        parse_errors,
    })
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
        let dir = std::env::temp_dir().join(format!("codex-cost-tui-{name}-{nanos}"));
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
}
