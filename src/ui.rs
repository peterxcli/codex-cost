use std::io;
use std::ops::Range;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};

use crate::app::{App, Focus, InputMode};
use crate::models::Session;
use crate::pricing::estimate_cost;
use crate::search::unique_search_terms;
use crate::worker::LoadProgress;

pub(crate) fn match_highlight_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

pub(crate) fn highlight_matches(
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

pub(crate) fn search_cursor_position(app: &App, area: Rect) -> Option<(u16, u16)> {
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

pub(crate) struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    restored: bool,
}

impl TerminalSession {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                Self::restore_stdout();
                return Err(err.into());
            }
        };

        Ok(Self {
            terminal,
            restored: false,
        })
    }

    pub(crate) fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        self.restored = true;
        Ok(())
    }

    fn restore_stdout() {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

pub(crate) fn run_tui(mut app: App) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;

    let result = loop {
        app.poll_loader();
        terminal
            .terminal_mut()
            .draw(|frame| draw(frame, &mut app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key)? {
                    break Ok(());
                }
            }
        }
    };

    terminal.restore()?;
    result
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
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

pub(crate) fn draw_search(frame: &mut Frame, app: &App, area: Rect) {
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

pub(crate) fn draw_session_table(frame: &mut Frame, app: &mut App, area: Rect) {
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

pub(crate) fn draw_session_list(frame: &mut Frame, app: &mut App, area: Rect) {
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

pub(crate) fn session_table_empty_message(app: &App) -> Option<String> {
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

pub(crate) fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
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

pub(crate) fn draw_detail_summary(frame: &mut Frame, app: &App, session: &Session, area: Rect) {
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

pub(crate) fn draw_detail_text(frame: &mut Frame, session: &Session, area: Rect) {
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

pub(crate) fn draw_token_events(frame: &mut Frame, app: &mut App, session: &Session, area: Rect) {
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

pub(crate) fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
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

pub(crate) fn draw_load_progress(frame: &mut Frame, progress: &LoadProgress, area: Rect) {
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

pub(crate) fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    }
}
pub(crate) fn short_id(id: &str) -> String {
    if id.len() <= 13 {
        id.to_string()
    } else {
        id.chars().take(13).collect()
    }
}

pub(crate) fn short_timestamp(timestamp: &str) -> String {
    if timestamp.len() >= 19 {
        timestamp[0..19].replace('T', " ")
    } else {
        timestamp.to_string()
    }
}

pub(crate) fn one_line(text: &str, max_len: usize) -> String {
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

pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub(crate) fn format_duration(seconds: u64) -> String {
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
