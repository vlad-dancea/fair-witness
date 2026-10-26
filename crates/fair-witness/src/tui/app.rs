use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self as terminal, EventStream, KeyCode, KeyEvent, KeyModifiers};
use fair_witness_core::review::ModelChoice;
use fair_witness_core::session::{Event, Record};
use fair_witness_core::watcher::{self, Kind, Listed};
use futures::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::UnboundedReceiver;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::debug_pane::DebugLog;
use super::model_panel::{self, ModelPanel, ModelSettings, Outcome};

const TICK: Duration = Duration::from_millis(100);
/// From this width on, the debug pane sits beside the changes, not below.
const SIDE_BY_SIDE_WIDTH: u16 = 140;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Leads a verdict, pointing from the changes above it to the result.
const RESULT_ARROW: &str = " ↳ ";
/// An explicit green: terminal themes often remap the ANSI one.
const IN_LINE_GREEN: Color = Color::Rgb(0x6c, 0xc6, 0x44);

pub enum Update {
    Session(Record),
    NotifyFailed(String),
}

/// A line in the changes pane; a changed file carries the number of its
/// change set, shown at the right edge, and a verdict wraps under the text
/// after its arrow.
struct LogLine {
    line: Line<'static>,
    set: Option<usize>,
    verdict: bool,
}

impl From<Line<'static>> for LogLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            set: None,
            verdict: false,
        }
    }
}

impl LogLine {
    fn verdict(spans: Vec<Span<'static>>) -> Self {
        let mut line = vec![Span::raw(RESULT_ARROW)];
        line.extend(spans);
        Self {
            line: Line::from(line),
            set: None,
            verdict: true,
        }
    }

    fn render(&self, width: usize) -> Vec<Line<'static>> {
        if self.verdict {
            return wrap_hanging(&self.line, width, RESULT_ARROW.width());
        }
        let Some(set) = self.set else {
            return vec![self.line.clone()];
        };
        let tag = format!("#{set}");
        let used = self.line.width() + tag.len();
        let gap = if used < width { width - used } else { 1 };
        let mut line = self.line.clone();
        line.push_span(Span::raw(" ".repeat(gap)));
        line.push_span(tag.add_modifier(Modifier::DIM));
        vec![line]
    }
}

/// Wraps at spaces where it can, mid-word where it must, keeping each span's
/// style; continuation rows start `indent` columns in.
fn wrap_hanging(line: &Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let indent = if indent < width / 2 { indent } else { 0 };
    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut row_width = 0;
    let mut break_row = |row: &mut Vec<Span<'static>>, row_width: &mut usize| {
        if let Some(last) = row.last_mut() {
            *last = Span::styled(last.content.trim_end().to_string(), last.style);
        }
        rows.push(Line::from(std::mem::replace(
            row,
            vec![Span::raw(" ".repeat(indent))],
        )));
        *row_width = indent;
    };
    for span in &line.spans {
        for word in span.content.split_inclusive(' ') {
            let visible = word.trim_end().width();
            if row_width + visible > width && row_width > indent {
                break_row(&mut row, &mut row_width);
            }
            for c in word.chars() {
                let c_width = c.width().unwrap_or(0);
                if c != ' ' && row_width + c_width > width && row_width > indent {
                    break_row(&mut row, &mut row_width);
                }
                match row.last_mut() {
                    Some(last) if last.style == span.style => last.content.to_mut().push(c),
                    _ => row.push(Span::styled(c.to_string(), span.style)),
                }
                row_width += c_width;
            }
        }
    }
    break_row(&mut row, &mut row_width);
    rows
}

pub struct Screen {
    root: PathBuf,
    status: Vec<String>,
    log: Vec<LogLine>,
    lines_from_bottom: usize,
    debug: DebugLog,
    debug_lines_from_bottom: usize,
    debug_shown: bool,
    debug_focused: bool,
    reviewing_since: Option<Instant>,
    settings: Option<ModelSettings>,
    panel: Option<ModelPanel>,
    quit: bool,
}

impl Screen {
    pub fn new(root: &Path, status: Vec<String>, settings: Option<ModelSettings>) -> Self {
        Self {
            root: root.to_path_buf(),
            status,
            log: Vec::new(),
            lines_from_bottom: 0,
            debug: DebugLog::default(),
            debug_lines_from_bottom: 0,
            debug_shown: false,
            debug_focused: false,
            reviewing_since: None,
            settings,
            panel: None,
            quit: false,
        }
    }

    pub async fn run(mut self, updates: UnboundedReceiver<Update>) -> Result<()> {
        let mut terminal = ratatui::init();
        let result = self.event_loop(&mut terminal, updates).await;
        ratatui::restore();
        result
    }

    async fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        mut updates: UnboundedReceiver<Update>,
    ) -> Result<()> {
        let mut terminal_events = EventStream::new();
        // Redraws the spinner and the model list while it loads.
        let mut tick = tokio::time::interval(TICK);
        let mut watching = true;
        while !self.quit {
            if let Some(panel) = &mut self.panel {
                let outcome = panel.checked();
                self.handle_outcome(outcome);
            }
            terminal.draw(|frame| self.draw(frame))?;
            tokio::select! {
                event = terminal_events.next() => match event {
                    Some(Ok(terminal::Event::Key(key))) if key.is_press() => self.handle_key(key),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                },
                update = updates.recv(), if watching => match update {
                    Some(update) => {
                        self.apply(update);
                        while let Ok(update) = updates.try_recv() {
                            self.apply(update);
                        }
                    }
                    None => {
                        watching = false;
                        self.reviewing_since = None;
                        self.log.push(Line::from("Stopped watching.".red()).into());
                    }
                },
                _ = tick.tick() => {}
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if let Some(panel) = &mut self.panel {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.quit = true;
                return;
            }
            let outcome = panel.handle_key(key);
            self.handle_outcome(outcome);
            return;
        }
        match key.code {
            KeyCode::Char('m') => {
                self.panel = self.settings.as_ref().map(ModelPanel::open);
            }
            KeyCode::Char('d') => {
                self.debug_shown = !self.debug_shown;
                self.debug_focused = self.debug_shown;
            }
            KeyCode::Tab | KeyCode::BackTab if self.debug_shown => {
                self.debug_focused = !self.debug_focused;
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('c') if self.debug_shown => self.debug.toggle_collapsed(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(1),
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            KeyCode::End | KeyCode::Char('G') => *self.focused_scroll() = 0,
            _ => {}
        }
    }

    fn handle_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Open => {}
            Outcome::Closed => self.panel = None,
            Outcome::Chosen(choice, unchecked) => {
                self.panel = None;
                self.use_model(choice, unchecked);
            }
        }
    }

    /// Uses `choice` from the next review on, saying why it could not be checked, if it could
    /// not.
    fn use_model(&mut self, choice: ModelChoice, unchecked: Option<String>) {
        let Some(settings) = &self.settings else {
            return;
        };
        let described = model_panel::describe(&choice);
        if let Some(why) = unchecked {
            let warning = format!(
                "Could not check {described}, so the reviews will show whether the API takes it: \
                 {why}"
            );
            self.log.push(Line::from(warning.yellow()).into());
        }
        if let Err(e) = settings.apply(choice) {
            self.log.push(
                Line::from(
                    format!("Checking with {described} until you quit; could not save it: {e:#}")
                        .yellow(),
                )
                .into(),
            );
        }
    }

    fn focused_scroll(&mut self) -> &mut usize {
        if self.debug_focused {
            &mut self.debug_lines_from_bottom
        } else {
            &mut self.lines_from_bottom
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        let scroll = self.focused_scroll();
        *scroll = scroll.saturating_add(lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        let scroll = self.focused_scroll();
        *scroll = scroll.saturating_sub(lines);
    }

    fn apply(&mut self, update: Update) {
        let record = match update {
            Update::Session(record) => record,
            Update::NotifyFailed(e) => {
                let failed = format!("  could not show a notification: {e}");
                self.log.push(Line::from(failed.yellow()).into());
                return;
            }
        };
        self.debug.add(&record);
        match record.event {
            Event::Changes { set, changes } => {
                for listed in watcher::summarize(&changes) {
                    self.log.push(LogLine {
                        line: change_line(&listed),
                        set: Some(set),
                        verdict: false,
                    });
                }
            }
            Event::Reviewing { .. } => self.reviewing_since = Some(Instant::now()),
            Event::Trace { .. } => {}
            Event::Verdict { verdict, .. } if verdict.contradicts => {
                self.reviewing_since = None;
                let field = verdict
                    .dmp_field
                    .map(|field| format!(" ({field})"))
                    .unwrap_or_default();
                self.log.push(LogLine::verdict(vec![
                    format!("contradicts the DMP{field}:").red().bold(),
                    Span::raw(format!(" {}", verdict.explanation)),
                ]));
            }
            Event::Verdict { verdict, .. } => {
                self.reviewing_since = None;
                self.log.push(LogLine::verdict(vec![
                    "in line with the DMP:".fg(IN_LINE_GREEN),
                    Span::raw(format!(" {}", verdict.explanation)),
                ]))
            }
            Event::ReviewFailed { error, .. } => {
                self.reviewing_since = None;
                self.log.push(LogLine::verdict(vec![
                    format!("review failed: {error}").red(),
                ]))
            }
            Event::WatchErrors { errors, .. } => {
                for e in errors {
                    self.log
                        .push(Line::from(format!("watch error: {e}").red()).into());
                }
            }
            Event::LogStopped { error } => {
                let stopped = format!("stopped writing the log in .fair-witness/log/: {error}");
                self.log.push(Line::from(stopped.red()).into());
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let mut header_lines = vec![Line::from(vec![
            "Watching ".into(),
            self.root.display().to_string().bold(),
        ])];
        header_lines.extend(self.status.iter().cloned().map(Line::from));
        if let Some(settings) = &self.settings {
            let choice = settings.reviewer.choice();
            header_lines.push(Line::from(vec![
                "Using ".into(),
                model_panel::describe(&choice).bold(),
                format!(" from {}.", settings.url).into(),
            ]));
        }
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(header_lines.len() as u16 + 2),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        frame.render_widget(
            Paragraph::new(header_lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" fair-witness ".bold()),
                ),
            header,
        );

        if self.debug_shown {
            let direction = if body.width >= SIDE_BY_SIDE_WIDTH {
                Direction::Horizontal
            } else {
                Direction::Vertical
            };
            let [log, debug] = Layout::new(
                direction,
                [Constraint::Percentage(40), Constraint::Percentage(60)],
            )
            .areas(body);
            self.draw_log(frame, log);
            self.draw_debug(frame, debug);
        } else {
            self.draw_log(frame, body);
        }

        let mut keys = vec![
            " q".bold(),
            " quit  ".into(),
            "↑↓ PgUp PgDn".bold(),
            " scroll  ".into(),
            "End".bold(),
            " latest  ".into(),
            "d".bold(),
            if self.debug_shown {
                " hide debug".into()
            } else {
                " debug".into()
            },
        ];
        if self.debug_shown {
            keys.extend(["  Tab".bold(), " switch pane".into(), "  c".bold()]);
            keys.push(if self.debug.collapsed() {
                " expand".into()
            } else {
                " collapse".into()
            });
        }
        if self.settings.is_some() {
            keys.extend(["  m".bold(), " model".into()]);
        }
        frame.render_widget(
            Line::from(keys).style(Style::new().fg(Color::DarkGray)),
            footer,
        );

        if let Some(panel) = &mut self.panel {
            panel.draw(frame, frame.area());
        }
    }

    fn draw_log(&mut self, frame: &mut Frame, area: Rect) {
        let block = pane(" Changes ", self.debug_shown, !self.debug_focused);
        if self.log.is_empty() {
            let waiting =
                Paragraph::new("No changes yet.".add_modifier(Modifier::DIM)).block(block);
            frame.render_widget(waiting, area);
            return;
        }
        let width = block.inner(area).width as usize;
        let mut lines: Vec<Line<'static>> = self
            .log
            .iter()
            .flat_map(|line| line.render(width))
            .collect();
        if let Some(since) = self.reviewing_since {
            let elapsed = since.elapsed();
            let frame = SPINNER[(elapsed.as_millis() / TICK.as_millis()) as usize % SPINNER.len()];
            lines.push(Line::from(vec![
                format!("  {frame} ").cyan(),
                format!("checking against the DMP… {}s", elapsed.as_secs())
                    .add_modifier(Modifier::DIM),
            ]));
        }
        draw_scrolled(frame, area, block, lines, &mut self.lines_from_bottom);
    }

    fn draw_debug(&mut self, frame: &mut Frame, area: Rect) {
        let block = pane(
            " Debug: what the AI sees and does ",
            true,
            self.debug_focused,
        );
        if self.debug.is_empty() {
            let waiting = Paragraph::new(
                "Nothing sent to the AI yet; it starts with the next change."
                    .add_modifier(Modifier::DIM),
            )
            .wrap(Wrap { trim: false })
            .block(block);
            frame.render_widget(waiting, area);
            return;
        }
        let lines = self.debug.lines(block.inner(area).width).to_vec();
        draw_scrolled(frame, area, block, lines, &mut self.debug_lines_from_bottom);
    }
}

/// While both panes show, the focused one has a bold title and the other is
/// dimmed; a colour would not do, as terminal themes remap the ANSI ones.
fn pane(title: &str, split: bool, focused: bool) -> Block<'_> {
    let block = Block::bordered().border_type(BorderType::Rounded);
    match (split, focused) {
        (false, _) => block.title(title),
        (true, true) => block.title(title.bold()),
        (true, false) => block
            .title(title)
            .border_style(Style::new().add_modifier(Modifier::DIM)),
    }
}

/// Shows the end of `lines`, or further up by `lines_from_bottom`, which is
/// clamped to what there is to scroll.
fn draw_scrolled(
    frame: &mut Frame,
    area: Rect,
    block: Block,
    lines: Vec<Line<'static>>,
    lines_from_bottom: &mut usize,
) {
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    let max_scroll = total.saturating_sub(inner.height as usize);
    *lines_from_bottom = (*lines_from_bottom).min(max_scroll);
    let top = u16::try_from(max_scroll - *lines_from_bottom).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)).block(block), area);
}

fn change_line(listed: &Listed) -> Line<'static> {
    let kind = listed.kind();
    let color = match kind {
        Kind::Created => Color::Green,
        Kind::Changed => Color::Blue,
        Kind::Renamed => Color::Yellow,
        Kind::Deleted => Color::Red,
    };
    Line::from(vec![
        format!("{}:", kind.verb()).fg(color),
        Span::raw(format!(" {}", listed.describe())),
    ])
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fair_witness_core::project::{AiConfig, Project};
    use fair_witness_core::review::{Reviewer, Trace, Verdict};
    use fair_witness_core::watcher::Change;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;

    fn screen() -> Screen {
        let root = Path::new("/home/researcher/sleep-study");
        let ai = AiConfig {
            url: "http://localhost:11434/v1".to_string(),
            model: "qwen3:8b".to_string(),
            effort: "medium".parse().unwrap(),
        };
        let settings = ModelSettings {
            project: Project::new(root.to_path_buf()),
            reviewer: Arc::new(Reviewer::new(&ai, None).unwrap()),
            url: ai.url,
            api_key: None,
        };
        let status = vec!["Comparing changes with project #7 - Sleep and screen time.".to_string()];
        Screen::new(root, status, Some(settings))
    }

    /// Three change sets: one in line with the DMP, one that contradicts it after the model
    /// read a file, and one whose review failed.
    fn reviewed() -> Screen {
        let mut screen = screen();
        let choice = ModelChoice {
            model: "qwen3:8b".to_string(),
            effort: "medium".parse().unwrap(),
        };
        let reply = |turn, text: &str, name: &str, arguments| Trace::Reply {
            turn,
            reasoning: None,
            texts: vec![text.to_string()],
            calls: vec![(name.to_string(), arguments)],
            stop_reason: Some("tool_calls".to_string()),
            tokens_in: Some(1200),
            tokens_out: Some(80),
            elapsed_ms: 3400,
        };
        let events = [
            Event::Changes {
                set: 1,
                changes: vec![Change::Created("notes.md".into())],
            },
            Event::Reviewing { set: 1 },
            Event::Trace {
                set: 1,
                trace: Trace::Setup {
                    system: "Judge each change against the DMP.\nAnswer with report_verdict."
                        .to_string(),
                    dmp: "{}".to_string(),
                    tools: vec!["read_file".to_string(), "report_verdict".to_string()],
                },
            },
            Event::Trace {
                set: 1,
                trace: Trace::Model(choice),
            },
            Event::Trace {
                set: 1,
                trace: Trace::Prompt("Created: notes.md".to_string()),
            },
            Event::Verdict {
                set: 1,
                verdict: Verdict {
                    contradicts: false,
                    dmp_field: None,
                    explanation: "Meeting notes are not research data.".to_string(),
                },
            },
            Event::Changes {
                set: 2,
                changes: vec![
                    Change::Created("data".into()),
                    Change::Created("data/participants.csv".into()),
                ],
            },
            Event::Reviewing { set: 2 },
            Event::Trace {
                set: 2,
                trace: Trace::Prompt("Created: data/participants.csv".to_string()),
            },
            Event::Trace {
                set: 2,
                trace: Trace::Request { turn: 1 },
            },
            Event::Trace {
                set: 2,
                trace: reply(
                    1,
                    "A new table; let me see what it holds.",
                    "read_file",
                    json!({ "path": "data/participants.csv" }),
                ),
            },
            Event::Trace {
                set: 2,
                trace: Trace::ToolResult {
                    name: "read_file".to_string(),
                    output: "name,born,sleep_hours\nAnna Muster,1990-04-02,6.5".to_string(),
                    failed: false,
                },
            },
            Event::Trace {
                set: 2,
                trace: Trace::Request { turn: 2 },
            },
            Event::Trace {
                set: 2,
                trace: reply(
                    2,
                    "Names and birth dates are personal data.",
                    "report_verdict",
                    json!({
                        "contradicts": true,
                        "dmp_field": "dataset[0].personal_data",
                        "explanation": "The table names participants with their birth dates, \
                                        but the DMP says the project keeps no personal data."
                    }),
                ),
            },
            Event::Verdict {
                set: 2,
                verdict: Verdict {
                    contradicts: true,
                    dmp_field: Some("dataset[0].personal_data".to_string()),
                    explanation: "The table names participants with their birth dates, but \
                                  the DMP says the project keeps no personal data."
                        .to_string(),
                },
            },
            Event::Changes {
                set: 3,
                changes: vec![
                    Change::Renamed {
                        from: "draft.md".into(),
                        to: "notes/draft.md".into(),
                    },
                    Change::Deleted("old.csv".into()),
                ],
            },
            Event::Reviewing { set: 3 },
            Event::Trace {
                set: 3,
                trace: Trace::Failed("could not reach http://localhost:11434/v1/".to_string()),
            },
            Event::ReviewFailed {
                set: 3,
                error: "could not reach http://localhost:11434/v1/".to_string(),
            },
        ];
        let time = "2026-10-23T09:41:00Z".parse().unwrap();
        for event in events {
            screen.apply(Update::Session(Record { time, event }));
        }
        screen
    }

    fn drawn(screen: &mut Screen, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| screen.draw(frame)).unwrap();
        format!("{:#?}", terminal.backend().buffer())
    }

    /// Snapshots `drawn` with the clock time a review started masked.
    macro_rules! assert_screen {
        ($name:literal, $drawn:expr) => {
            insta::with_settings!({ filters => vec![(r"\d\d:\d\d:\d\d", "hh:mm:ss")] }, {
                insta::assert_snapshot!($name, $drawn);
            });
        };
    }

    #[test]
    fn draws_the_screen_before_any_change() {
        assert_screen!("waiting", drawn(&mut screen(), 80, 12));
    }

    #[test]
    fn draws_the_changes_and_their_verdicts() {
        assert_screen!("changes", drawn(&mut reviewed(), 80, 24));
    }

    #[test]
    fn draws_the_debug_pane_beside_the_changes_on_a_wide_terminal() {
        let mut screen = reviewed();
        screen.handle_key(KeyCode::Char('d').into());
        assert_screen!("debug_beside", drawn(&mut screen, 150, 40));
    }

    #[test]
    fn draws_the_debug_pane_below_the_changes_on_a_narrow_terminal() {
        let mut screen = reviewed();
        screen.handle_key(KeyCode::Char('d').into());
        assert_screen!("debug_below", drawn(&mut screen, 80, 50));
    }

    #[test]
    fn sends_the_arrows_to_the_focused_pane_and_dims_the_other() {
        let mut screen = reviewed();
        let dimmed = |screen: &mut Screen, x, y| {
            let mut terminal = Terminal::new(TestBackend::new(150, 40)).unwrap();
            terminal.draw(|frame| screen.draw(frame)).unwrap();
            let cell = &terminal.backend().buffer()[(x, y)];
            cell.modifier.contains(Modifier::DIM)
        };
        // The top left corners of the changes pane and the debug pane.
        let (changes, debug) = ((0, 5), (60, 5));

        screen.handle_key(KeyCode::Char('d').into());
        screen.handle_key(KeyCode::Up.into());
        assert_eq!(
            (screen.lines_from_bottom, screen.debug_lines_from_bottom),
            (0, 1)
        );
        assert!(dimmed(&mut screen, changes.0, changes.1));
        assert!(!dimmed(&mut screen, debug.0, debug.1));

        screen.handle_key(KeyCode::Tab.into());
        screen.handle_key(KeyCode::Up.into());
        assert_eq!(screen.lines_from_bottom, 1);
        assert!(!dimmed(&mut screen, changes.0, changes.1));
        assert!(dimmed(&mut screen, debug.0, debug.1));
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn puts_the_change_set_at_the_right_edge() {
        let line = LogLine {
            line: Line::from("created: a.csv"),
            set: Some(3),
            verdict: false,
        };
        assert_eq!(text(&line.render(20)[0]), "created: a.csv    #3");
    }

    #[test]
    fn puts_the_change_set_after_text_too_long_for_the_row() {
        let line = LogLine {
            line: Line::from("created: data/raw/a.csv"),
            set: Some(12),
            verdict: false,
        };
        assert_eq!(text(&line.render(20)[0]), "created: data/raw/a.csv #12");
    }

    #[test]
    fn indents_the_rest_of_a_verdict_under_its_text() {
        let line = LogLine::verdict(vec![
            "in line:".green(),
            Span::raw(" the file is fine and verylongwordhere"),
        ]);
        let rows: Vec<String> = line.render(16).iter().map(text).collect();
        assert_eq!(
            rows,
            [
                " ↳ in line: the",
                "   file is fine",
                "   and",
                "   verylongwordh",
                "   ere"
            ]
        );
    }
}
