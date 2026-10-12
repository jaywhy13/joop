use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::engine::{Engine, Level, Snapshot};
use crate::state::{Session, Status};
use crate::supervisor::Supervisor;
use crate::zellij::TabHost;

/// The dashboard: which issues are in flight, which Zellij tab each one is in, and the
/// last thing its agent said.
pub struct Dashboard {
    engine: Arc<Engine>,
    tabs: Arc<dyn TabHost>,
    selected: TableState,
    notice: Option<String>,
}

impl Dashboard {
    pub fn new(engine: Arc<Engine>, tabs: Arc<dyn TabHost>) -> Self {
        Self {
            engine,
            tabs,
            selected: TableState::default().with_selected(Some(0)),
            notice: None,
        }
    }

    /// Draw until the user quits, driving `supervisor` from the keyboard.
    pub fn run(mut self, supervisor: &Supervisor) -> Result<()> {
        let mut terminal = enter_terminal()?;
        let result = self.event_loop(&mut terminal, supervisor);
        leave_terminal(&mut terminal)?;
        result
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        supervisor: &Supervisor,
    ) -> Result<()> {
        loop {
            let snapshot = self.engine.snapshot();
            terminal.draw(|frame| self.draw(frame, &snapshot))?;

            if !event::poll(Duration::from_millis(500))? {
                continue;
            }

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                (KeyCode::Char('p'), _) | (KeyCode::Char('r'), _) => {
                    supervisor.poll_now();
                    self.notice = Some("polling GitHub now…".into());
                }
                (KeyCode::Char('l'), _) => {
                    supervisor.check_limits_now();
                    self.notice = Some("re-reading Claude usage…".into());
                }
                (KeyCode::Char('o'), _) | (KeyCode::Enter, _) => self.focus_selected(&snapshot),
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_selection(1, &snapshot),
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_selection(-1, &snapshot),
                _ => {}
            }
        }
    }

    fn move_selection(&mut self, delta: isize, snapshot: &Snapshot) {
        if snapshot.sessions.is_empty() {
            return;
        }
        let last = snapshot.sessions.len() - 1;
        let current = self.selected.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, last as isize) as usize;
        self.selected.select(Some(next));
    }

    /// Jump the terminal to the selected agent's tab.
    fn focus_selected(&mut self, snapshot: &Snapshot) {
        let Some(session) = self.selected_session(snapshot) else {
            return;
        };

        // Focusing is a tab switch with no input, so an empty line is never typed.
        self.notice = match self.tabs.focus(&session.tab_name) {
            Ok(()) => Some(format!("focused tab {}", session.tab_name)),
            Err(error) => Some(format!("could not focus {}: {error:#}", session.tab_name)),
        };
    }

    fn selected_session<'a>(&self, snapshot: &'a Snapshot) -> Option<&'a Session> {
        snapshot.sessions.get(self.selected.selected().unwrap_or(0))
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>, snapshot: &Snapshot) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(8),
            Constraint::Length(1),
        ])
        .split(frame.area());

        self.draw_header(frame, areas[0], snapshot);
        self.draw_sessions(frame, areas[1], snapshot);
        self.draw_events(frame, areas[2], snapshot);
        self.draw_keys(frame, areas[3]);
    }

    fn draw_header(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snapshot: &Snapshot) {
        let mut spans = vec![
            Span::styled(snapshot.repo.clone(), Style::new().bold()),
            Span::raw(format!("  label:{}  ", snapshot.label)),
        ];

        match &snapshot.pause {
            Some(pause) => spans.push(Span::styled(
                format!(
                    "PAUSED on Claude {} limit ({:.0}%){}",
                    pause.snapshot.limit,
                    pause.snapshot.percent(),
                    pause
                        .until
                        .map(|until| format!(", resumes {}", relative(until, snapshot.now)))
                        .unwrap_or_default()
                ),
                Style::new().fg(Color::Black).bg(Color::Yellow).bold(),
            )),
            None => {
                let usage = snapshot
                    .limits
                    .as_ref()
                    .map(|limits| format!("Claude {} {:.0}%", limits.limit, limits.percent()))
                    .unwrap_or_else(|| "Claude usage unknown".into());
                spans.push(Span::styled(
                    format!("watching · {usage}"),
                    Style::new().fg(Color::Green),
                ));
            }
        }

        spans.push(Span::raw(format!(
            "  last poll: {}",
            snapshot
                .last_poll_at
                .map(|at| relative(at, snapshot.now))
                .unwrap_or_else(|| "never".into())
        )));

        let mut lines = vec![Line::from(spans)];
        if let Some(notice) = &self.notice {
            lines.push(Line::from(Span::styled(
                notice.clone(),
                Style::new().fg(Color::Cyan),
            )));
        }

        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" joop ")),
            area,
        );
    }

    fn draw_sessions(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect, snapshot: &Snapshot) {
        let header = Row::new(
            [
                "issue",
                "title",
                "status",
                "tab",
                "last update",
                "due",
                "latest",
            ]
            .map(|title| Cell::from(title).style(Style::new().bold())),
        );

        let rows: Vec<Row> = snapshot
            .sessions
            .iter()
            .map(|session| {
                let due = if session.status.is_finished() {
                    "—".to_string()
                } else {
                    relative(snapshot.policy.deadline_for(session), snapshot.now)
                };

                Row::new(vec![
                    Cell::from(format!("#{}", session.issue_number)),
                    Cell::from(truncate(&session.issue_title, 34)),
                    Cell::from(session.status.as_str()).style(status_style(session.status)),
                    Cell::from(tab_label(session)),
                    Cell::from(
                        session
                            .last_update_at
                            .map(|at| relative(at, snapshot.now))
                            .unwrap_or_else(|| "never".into()),
                    ),
                    Cell::from(due),
                    Cell::from(truncate(session.detail.as_deref().unwrap_or("—"), 40)),
                ])
            })
            .collect();

        let title = format!(" sessions ({}) ", snapshot.sessions.len());
        if rows.is_empty() {
            frame.render_widget(
                Paragraph::new(format!(
                    "\n  No issues in flight. Label an issue `{}` in {} and press p to poll.",
                    snapshot.label, snapshot.repo
                ))
                .block(Block::default().borders(Borders::ALL).title(title)),
                area,
            );
            return;
        }

        let table = Table::new(
            rows,
            [
                Constraint::Length(7),
                Constraint::Min(20),
                Constraint::Length(11),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Min(16),
            ],
        )
        .header(header)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .block(Block::default().borders(Borders::ALL).title(title));

        frame.render_stateful_widget(table, area, &mut self.selected);
    }

    fn draw_events(&self, frame: &mut ratatui::Frame<'_>, area: Rect, snapshot: &Snapshot) {
        let lines: Vec<Line<'_>> = snapshot
            .events
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .map(|event| {
                Line::from(vec![
                    Span::styled(
                        event.at.format("%H:%M:%S ").to_string(),
                        Style::new().fg(Color::DarkGray),
                    ),
                    Span::styled(event.message.clone(), level_style(event.level)),
                ])
            })
            .collect();

        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" activity ")),
            area,
        );
    }

    fn draw_keys(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " p poll now · l check limits · ↑↓ select · o focus tab · q quit",
                Style::new().fg(Color::DarkGray),
            ))),
            area,
        );
    }
}

fn tab_label(session: &Session) -> String {
    match &session.tab_id {
        Some(id) => format!("{} ({id})", session.tab_name),
        None => session.tab_name.clone(),
    }
}

fn status_style(status: Status) -> Style {
    match status {
        Status::Done => Style::new().fg(Color::Green),
        Status::Review => Style::new().fg(Color::Cyan),
        Status::Working | Status::Started => Style::new().fg(Color::Blue),
        Status::Dispatched => Style::new().fg(Color::Gray),
        Status::Blocked | Status::Stale => Style::new().fg(Color::Yellow),
        Status::Failed | Status::Dead => Style::new().fg(Color::Red),
    }
}

fn level_style(level: Level) -> Style {
    match level {
        Level::Info => Style::new().fg(Color::Gray),
        Level::Warn => Style::new().fg(Color::Yellow),
        Level::Error => Style::new().fg(Color::Red),
    }
}

/// "3m ago" / "in 17m", which is what the dashboard needs rather than a timestamp.
pub fn relative(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (at - now).num_seconds();
    let ahead = seconds > 0;
    let magnitude = seconds.abs();

    let text = if magnitude < 60 {
        format!("{magnitude}s")
    } else if magnitude < 3600 {
        format!("{}m", magnitude / 60)
    } else if magnitude < 86_400 {
        format!("{}h{}m", magnitude / 3600, (magnitude % 3600) / 60)
    } else {
        format!("{}d", magnitude / 86_400)
    };

    if ahead {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

pub fn truncate(text: &str, width: usize) -> String {
    let flattened = text.replace(['\n', '\r'], " ");
    if flattened.chars().count() <= width {
        return flattened;
    }
    let kept: String = flattened.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("could not put the terminal into raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("could not open the dashboard screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("could not start the dashboard")
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    #[test]
    fn past_times_read_as_ago() {
        assert_eq!(relative(now() - Duration::seconds(30), now()), "30s ago");
        assert_eq!(relative(now() - Duration::minutes(3), now()), "3m ago");
        assert_eq!(relative(now() - Duration::minutes(90), now()), "1h30m ago");
        assert_eq!(relative(now() - Duration::days(2), now()), "2d ago");
    }

    #[test]
    fn future_deadlines_read_as_in() {
        assert_eq!(relative(now() + Duration::minutes(17), now()), "in 17m");
    }

    #[test]
    fn long_titles_are_shortened_with_an_ellipsis() {
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn newlines_in_agent_details_do_not_break_the_table() {
        assert_eq!(truncate("one\ntwo", 10), "one two");
    }

    #[test]
    fn multibyte_titles_are_cut_on_character_boundaries() {
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn every_status_has_a_colour() {
        for status in [
            Status::Dispatched,
            Status::Started,
            Status::Working,
            Status::Blocked,
            Status::Review,
            Status::Done,
            Status::Failed,
            Status::Stale,
            Status::Dead,
        ] {
            assert_ne!(status_style(status), Style::new(), "{status} has no colour");
        }
    }
}
