use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::debug_pane::DebugLog;
use super::model_panel::{self, ModelPanel, ModelSettings, Outcome};
use crate::agent::{ModelChoice, Trace, Verdict};
use crate::watcher::Change;

const TICK: Duration = Duration::from_millis(100);
/// From this width on, the debug pane sits beside the changes, not below.
const SIDE_BY_SIDE_WIDTH: u16 = 140;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Leads a verdict, pointing from the changes above it to the result.
const RESULT_ARROW: &str = " ↳ ";
/// An explicit green: terminal themes often remap the ANSI one.
const IN_LINE_GREEN: Color = Color::Rgb(0x6c, 0xc6, 0x44);

pub enum Update {
    /// A change set, numbered from 1 in the order seen.
    Changes(usize, Vec<Change>),
    Reviewing(usize),
    Verdict(Result<Verdict>),
    Trace(Trace),
    WatchErrors(Vec<String>),
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

    pub fn run(mut self, updates: Receiver<Update>) -> Result<()> {
        ratatui::run(|terminal| self.event_loop(terminal, updates))
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        updates: Receiver<Update>,
    ) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(TICK)?
                && let Event::Key(key) = event::read()?
                && key.is_press()
            {
                self.handle_key(key);
            }
            loop {
                match updates.try_recv() {
                    Ok(update) => self.apply(update),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.reviewing_since = None;
                        self.log.push(Line::from("Stopped watching.".red()).into());
                        break;
                    }
                }
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
            match panel.handle_key(key) {
                Outcome::Open => {}
                Outcome::Closed => self.panel = None,
                Outcome::Chosen(choice) => {
                    self.panel = None;
                    self.use_model(choice);
                }
            }
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

    fn use_model(&mut self, choice: ModelChoice) {
        let Some(settings) = &self.settings else {
            return;
        };
        let described = model_panel::describe(&choice);
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
        if let Update::Verdict(_) = update {
            self.reviewing_since = None;
        }
        match update {
            Update::Changes(set, changes) => {
                for change in &changes {
                    self.log.push(LogLine {
                        line: change_line(&self.root, change),
                        set: Some(set),
                        verdict: false,
                    });
                }
            }
            Update::Reviewing(set) => {
                self.reviewing_since = Some(Instant::now());
                self.debug.review_started(set);
            }
            Update::Trace(trace) => self.debug.add(trace),
            Update::Verdict(Ok(verdict)) if verdict.contradicts => {
                let field = verdict
                    .dmp_field
                    .map(|field| format!(" ({field})"))
                    .unwrap_or_default();
                self.log.push(LogLine::verdict(vec![
                    format!("contradicts the DMP{field}:").red().bold(),
                    Span::raw(format!(" {}", verdict.explanation)),
                ]));
            }
            Update::Verdict(Ok(verdict)) => self.log.push(LogLine::verdict(vec![
                "in line with the DMP:".fg(IN_LINE_GREEN),
                Span::raw(format!(" {}", verdict.explanation)),
            ])),
            Update::Verdict(Err(e)) => self.log.push(LogLine::verdict(vec![
                format!("review failed: {e:#}").red(),
            ])),
            Update::WatchErrors(errors) => {
                for e in errors {
                    self.log
                        .push(Line::from(format!("watch error: {e}").red()).into());
                }
            }
            Update::NotifyFailed(e) => self
                .log
                .push(Line::from(format!("  could not show a notification: {e}").yellow()).into()),
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
                        .title(" damap-helper ".bold()),
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
        let block = pane(" Changes ", self.debug_shown && !self.debug_focused);
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
        let block = pane(" Debug: what the AI sees and does ", self.debug_focused);
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

fn pane(title: &str, focused: bool) -> Block<'_> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title);
    if focused {
        block.border_style(Style::new().fg(Color::Cyan))
    } else {
        block
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

fn change_line(root: &Path, change: &Change) -> Line<'static> {
    let relative = |path: &Path| {
        path.strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let (label, color, text) = match change {
        Change::Created(path) => ("created:", Color::Green, relative(path)),
        Change::Changed(path) => ("changed:", Color::Blue, relative(path)),
        Change::Renamed { from, to } => (
            "renamed:",
            Color::Yellow,
            format!("{} -> {}", relative(from), relative(to)),
        ),
        Change::Deleted(path) => ("deleted:", Color::Red, relative(path)),
    };
    Line::from(vec![label.fg(color), Span::raw(format!(" {text}"))])
}

#[cfg(test)]
mod tests {
    use super::*;

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
