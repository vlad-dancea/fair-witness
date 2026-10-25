use fair_witness_core::review::{ModelChoice, Trace, Verdict, first_message};
use fair_witness_core::session::{Event, Record};
use jiff::tz::TimeZone;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::model_panel;

/// Oldest entries go first once the pane holds this many, so a long session
/// does not slow down drawing.
const MAX_ENTRIES: usize = 1_000;
/// While collapsed, a tool's output shows at most this many lines.
const COLLAPSED_OUTPUT_LINES: usize = 8;
/// Where the text of a message starts, under its badge.
const INDENT: &str = "  ";
/// Why a reply ended when nothing went wrong; any other reason is shown.
const USUAL_STOPS: [&str; 4] = ["stop", "tool_calls", "end_turn", "tool_use"];

enum Entry {
    /// Starts a review: a rule across the pane.
    Review {
        set: usize,
        time: String,
    },
    /// The system prompt and the tools the AI may call.
    Instructions {
        text: String,
        tools: Vec<String>,
    },
    /// A message from fair-witness to the AI.
    Prompt(String),
    Turn(Turn),
    /// The API refused a request with 429 Too Many Requests, and it goes again after a wait.
    RateLimited {
        turn: usize,
        retry: usize,
        wait_ms: u64,
        body: String,
    },
    Failed(String),
}

/// A request to the AI and, once it comes, the reply.
struct Turn {
    number: usize,
    choice: Option<ModelChoice>,
    reply: Option<Reply>,
}

struct Reply {
    reasoning: Option<String>,
    texts: Vec<String>,
    calls: Vec<Call>,
    stop_reason: Option<String>,
    tokens_in: Option<i32>,
    tokens_out: Option<i32>,
    elapsed_ms: u64,
}

/// A tool the AI called, with what the tool gave back.
struct Call {
    name: String,
    arguments: Value,
    result: Option<String>,
}

/// A logical line of text; wrapped to the pane when drawn, with continuation
/// lines indented under the text, past `key`.
struct BodyLine {
    key: Option<String>,
    key_style: Style,
    text: String,
    style: Style,
}

impl BodyLine {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            key: None,
            key_style: Style::new(),
            text: text.into(),
            style,
        }
    }

    fn keyed(key: &str, text: impl Into<String>) -> Self {
        Self {
            key: Some(format!("{key}: ")),
            key_style: Style::new().bold(),
            text: text.into(),
            style: Style::new(),
        }
    }
}

pub struct DebugLog {
    entries: Vec<Entry>,
    collapsed: bool,
    rendered: Option<(u16, Vec<Line<'static>>)>,
    /// The last setup and model a session reported, which it does not repeat for each review.
    setup: Option<Setup>,
    model: Option<ModelChoice>,
}

struct Setup {
    system: String,
    dmp: String,
    tools: Vec<String>,
}

impl Default for DebugLog {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            collapsed: true,
            rendered: None,
            setup: None,
            model: None,
        }
    }
}

impl DebugLog {
    pub fn collapsed(&self) -> bool {
        self.collapsed
    }

    /// Hides or shows the system prompt, the DMP and long tool output.
    pub fn toggle_collapsed(&mut self) {
        self.collapsed = !self.collapsed;
        self.rendered = None;
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Takes what a session reports about its reviews, from a live run or its log alike, and
    /// skips the rest.
    pub fn add(&mut self, record: &Record) {
        match &record.event {
            Event::Reviewing { set } => {
                let zoned = record.time.to_zoned(TimeZone::system());
                let time = zoned.strftime("%H:%M:%S").to_string();
                self.push(Entry::Review { set: *set, time });
            }
            Event::Trace { trace, .. } => self.add_trace(trace.clone()),
            _ => {}
        }
    }

    fn add_trace(&mut self, trace: Trace) {
        match trace {
            Trace::Setup { system, dmp, tools } => {
                self.setup = Some(Setup { system, dmp, tools });
            }
            Trace::Model(choice) => self.model = Some(choice),
            Trace::Prompt(about_changes) => match &self.setup {
                Some(setup) => {
                    let instructions = Entry::Instructions {
                        text: setup.system.clone(),
                        tools: setup.tools.clone(),
                    };
                    let prompt = Entry::Prompt(first_message(&setup.dmp, &about_changes));
                    self.push(instructions);
                    self.push(prompt);
                }
                None => self.push(Entry::Prompt(about_changes)),
            },
            Trace::Sent(text) => self.push(Entry::Prompt(text)),
            Trace::Request { turn } => self.push(Entry::Turn(Turn {
                number: turn,
                choice: self.model.clone(),
                reply: None,
            })),
            Trace::RateLimited {
                turn,
                retry,
                wait_ms,
                body,
            } => {
                // The request goes again, so it no longer waits for a reply.
                if let Some(Entry::Turn(Turn {
                    number,
                    reply: None,
                    ..
                })) = self.entries.last()
                    && *number == turn
                {
                    self.entries.pop();
                }
                self.push(Entry::RateLimited {
                    turn,
                    retry,
                    wait_ms,
                    body,
                });
            }
            Trace::Reply {
                turn,
                reasoning,
                texts,
                calls,
                stop_reason,
                tokens_in,
                tokens_out,
                elapsed_ms,
            } => {
                let reply = Reply {
                    reasoning,
                    texts,
                    calls: calls
                        .into_iter()
                        .map(|(name, arguments)| Call {
                            name,
                            arguments,
                            result: None,
                        })
                        .collect(),
                    stop_reason,
                    tokens_in,
                    tokens_out,
                    elapsed_ms,
                };
                let waiting = self.this_review().find_map(|entry| match entry {
                    Entry::Turn(waiting) if waiting.number == turn && waiting.reply.is_none() => {
                        Some(waiting)
                    }
                    _ => None,
                });
                match waiting {
                    Some(waiting) => waiting.reply = Some(reply),
                    None => self.push(Entry::Turn(Turn {
                        number: turn,
                        choice: None,
                        reply: Some(reply),
                    })),
                }
            }
            Trace::ToolResult { name, output, .. } => {
                let call = self
                    .this_review()
                    .find_map(|entry| match entry {
                        Entry::Turn(Turn {
                            reply: Some(reply), ..
                        }) => Some(reply),
                        _ => None,
                    })
                    .and_then(|reply| {
                        reply
                            .calls
                            .iter_mut()
                            .find(|call| call.name == name && call.result.is_none())
                    });
                match call {
                    Some(call) => call.result = Some(output),
                    None => self.push(Entry::Prompt(format!("{name} result:\n{output}"))),
                }
            }
            Trace::Failed(e) => self.push(Entry::Failed(e)),
        }
        self.rendered = None;
    }

    /// The log as lines that fit `width` columns.
    pub fn lines(&mut self, width: u16) -> &[Line<'static>] {
        if self.rendered.as_ref().is_none_or(|(w, _)| *w != width) {
            let mut lines = Vec::new();
            for (i, entry) in self.entries.iter().enumerate() {
                if i > 0 {
                    lines.push(Line::default());
                }
                render(entry, width as usize, self.collapsed, &mut lines);
            }
            self.rendered = Some((width, lines));
        }
        &self.rendered.as_ref().expect("rendered above").1
    }

    /// The entries of the review in progress, latest first.
    fn this_review(&mut self) -> impl Iterator<Item = &mut Entry> {
        self.entries
            .iter_mut()
            .rev()
            .take_while(|entry| !matches!(entry, Entry::Review { .. }))
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_ENTRIES);
        }
        self.rendered = None;
    }
}

fn render(entry: &Entry, width: usize, collapsed: bool, lines: &mut Vec<Line<'static>>) {
    let indent = [Span::raw(INDENT)];
    match entry {
        Entry::Review { set, time } => lines.push(rule(*set, time, width)),
        Entry::Instructions { text, tools } => {
            lines.push(badge(
                "SYSTEM PROMPT",
                Color::Cyan,
                "how the AI should judge",
            ));
            let body = if collapsed {
                vec![hidden(text.trim_end().lines().count(), "")]
            } else {
                text_lines(text, Style::new().add_modifier(Modifier::DIM))
            };
            push_body(lines, &indent, &body, width);
            if !tools.is_empty() {
                let listed = BodyLine::keyed("tools", tools.join(", "));
                push_body(lines, &indent, &[listed], width);
            }
        }
        Entry::Prompt(text) => {
            lines.push(badge("FAIR-WITNESS", Color::Blue, "to the AI"));
            push_body(lines, &indent, &prompt_lines(text, collapsed), width);
        }
        Entry::Turn(turn) => render_turn(turn, width, collapsed, lines),
        Entry::RateLimited {
            turn,
            retry,
            wait_ms,
            body,
        } => {
            let details = format!(
                "turn {turn} · retry {retry} in {:.1} s",
                *wait_ms as f64 / 1000.0
            );
            lines.push(badge("RATE LIMIT", Color::Yellow, &details));
            let body = text_lines(body, Style::new().add_modifier(Modifier::DIM));
            push_body(lines, &indent, &body, width);
        }
        Entry::Failed(e) => {
            lines.push(badge("ERROR", Color::Red, "the request failed"));
            let body = text_lines(e, Style::new().fg(Color::Red));
            push_body(lines, &indent, &body, width);
        }
    }
}

fn render_turn(turn: &Turn, width: usize, collapsed: bool, lines: &mut Vec<Line<'static>>) {
    let indent = [Span::raw(INDENT)];
    let mut details = vec![format!("turn {}", turn.number)];
    details.extend(turn.choice.as_ref().map(model_panel::describe));
    let Some(reply) = &turn.reply else {
        lines.push(badge("AI", Color::Magenta, &details.join(" · ")));
        let waiting = BodyLine::new(
            "waiting for the reply…",
            Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
        );
        push_body(lines, &indent, &[waiting], width);
        return;
    };
    let count = |tokens: Option<i32>| tokens.map_or("?".to_string(), |n| n.to_string());
    details.push(format!(
        "{} in · {} out tokens · {:.1} s",
        count(reply.tokens_in),
        count(reply.tokens_out),
        reply.elapsed_ms as f64 / 1000.0
    ));
    if let Some(reason) = &reply.stop_reason
        && !USUAL_STOPS.contains(&reason.as_str())
    {
        details.push(format!("stopped: {reason}"));
    }
    lines.push(badge("AI", Color::Magenta, &details.join(" · ")));

    let mut parts: Vec<Vec<Line<'static>>> = Vec::new();
    if let Some(reasoning) = reply.reasoning.as_ref().filter(|r| !r.trim().is_empty()) {
        let mut part = vec![Line::from(vec![
            Span::raw(INDENT),
            "thinking".magenta().italic(),
        ])];
        let bar = [Span::raw(INDENT), "┆ ".add_modifier(Modifier::DIM)];
        let style = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);
        push_body(&mut part, &bar, &text_lines(reasoning, style), width);
        parts.push(part);
    }
    for text in reply.texts.iter().filter(|t| !t.trim().is_empty()) {
        let mut part = Vec::new();
        push_body(&mut part, &indent, &text_lines(text, Style::new()), width);
        parts.push(part);
    }
    for call in &reply.calls {
        let mut part = Vec::new();
        render_call(call, width, collapsed, &mut part);
        parts.push(part);
    }
    if parts.is_empty() {
        let empty = BodyLine::new("(empty reply)", Style::new().add_modifier(Modifier::DIM));
        push_body(lines, &indent, &[empty], width);
    }
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.extend(part);
    }
}

fn render_call(call: &Call, width: usize, collapsed: bool, lines: &mut Vec<Line<'static>>) {
    if call.name == "report_verdict"
        && call.result.is_none()
        && let Ok(verdict) = serde_json::from_value::<Verdict>(call.arguments.clone())
    {
        verdict_box(&verdict, width, lines);
        return;
    }
    let indent = [Span::raw(INDENT)];
    let name = format!("▸ {} ", call.name);
    let name_style = Style::new().fg(Color::Yellow).bold();
    let inline = match &call.arguments {
        Value::Object(fields) if fields.len() == 1 => match fields.values().next() {
            Some(Value::String(value)) if !value.contains('\n') => Some(value.clone()),
            _ => None,
        },
        _ => None,
    };
    let head = BodyLine {
        key: Some(name),
        key_style: name_style,
        text: inline.clone().unwrap_or_default(),
        style: Style::new(),
    };
    push_body(lines, &indent, &[head], width);
    if inline.is_none() {
        let arguments = [Span::raw(INDENT), Span::raw("  ")];
        push_body(lines, &arguments, &argument_lines(&call.arguments), width);
    }

    let Some(output) = &call.result else {
        return;
    };
    let bar = [Span::raw(INDENT), "  │ ".add_modifier(Modifier::DIM)];
    let mut body = if output.is_empty() {
        vec![BodyLine::new(
            "(empty)",
            Style::new().add_modifier(Modifier::DIM),
        )]
    } else {
        text_lines(output, Style::new())
    };
    if collapsed && body.len() > COLLAPSED_OUTPUT_LINES {
        let more = body.len() - COLLAPSED_OUTPUT_LINES;
        body.truncate(COLLAPSED_OUTPUT_LINES);
        body.push(hidden(more, "more "));
    }
    push_body(lines, &bar, &body, width);
}

/// The AI's verdict in a frame, so the outcome of a review stands out.
fn verdict_box(verdict: &Verdict, width: usize, lines: &mut Vec<Line<'static>>) {
    let (color, title) = if verdict.contradicts {
        (Color::Red, "✗ contradicts the DMP")
    } else {
        (Color::Green, "✓ in line with the DMP")
    };
    let border = Style::new().fg(color);
    let inner = width.saturating_sub(INDENT.len() + 4).max(10);
    let fill = inner.saturating_sub(title.width() + 1);
    lines.push(Line::from(vec![
        Span::raw(INDENT),
        Span::styled("╭─ ", border),
        Span::styled(title, border.bold()),
        Span::styled(format!(" {}╮", "─".repeat(fill)), border),
    ]));
    let mut body = Vec::new();
    if let Some(field) = verdict.dmp_field.as_ref().filter(|_| verdict.contradicts) {
        body.push(BodyLine::keyed("field", field));
    }
    body.push(BodyLine::new(&verdict.explanation, Style::new()));
    for body_line in &body {
        for (i, piece) in wrap(body_line, inner).into_iter().enumerate() {
            let pad = inner.saturating_sub(piece.width());
            let mut spans = vec![Span::raw(INDENT), Span::styled("│ ", border)];
            match &body_line.key {
                Some(key) if i == 0 => {
                    spans.push(Span::styled(key.clone(), body_line.key_style));
                    spans.push(Span::raw(piece[key.len()..].to_string()));
                }
                _ => spans.push(Span::raw(piece)),
            }
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(" │", border));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(vec![
        Span::raw(INDENT),
        Span::styled(format!("╰{}╯", "─".repeat(inner + 2)), border),
    ]));
}

/// Who speaks: a label in reverse video, which stands out in any theme, then
/// details.
fn badge(label: &str, color: Color, details: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::new()
                .fg(color)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
        ),
        Span::styled(
            format!("  {details}"),
            Style::new().add_modifier(Modifier::DIM),
        ),
    ])
}

fn rule(set: usize, time: &str, width: usize) -> Line<'static> {
    let label = format!("━━ Review of change set #{set} ");
    let time = format!(" {time} ━━");
    let fill = width.saturating_sub(label.width() + time.width());
    Line::from(format!("{label}{}{time}", "━".repeat(fill)).bold())
}

/// Stands in for lines hidden while the pane is collapsed.
fn hidden(count: usize, what: &str) -> BodyLine {
    BodyLine::new(
        format!("⋯ {count} {what}lines hidden (c to expand)"),
        Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
    )
}

/// Wraps `body` to `width` columns, starting every row with `prefix`.
fn push_body(
    lines: &mut Vec<Line<'static>>,
    prefix: &[Span<'static>],
    body: &[BodyLine],
    width: usize,
) {
    let prefix_width: usize = prefix.iter().map(Span::width).sum();
    let room = width.saturating_sub(prefix_width).max(10);
    for body_line in body {
        for (i, piece) in wrap(body_line, room).into_iter().enumerate() {
            let mut spans = prefix.to_vec();
            match &body_line.key {
                Some(key) if i == 0 && piece.starts_with(key.as_str()) => {
                    spans.push(Span::styled(key.clone(), body_line.key_style));
                    spans.push(Span::styled(
                        piece[key.len()..].to_string(),
                        body_line.style,
                    ));
                }
                _ => spans.push(Span::styled(piece, body_line.style)),
            }
            lines.push(Line::from(spans));
        }
    }
}

fn text_lines(text: &str, style: Style) -> Vec<BodyLine> {
    text.trim_end()
        .lines()
        .map(|line| BodyLine::new(line, style))
        .collect()
}

/// Like `text_lines`, but dims fenced code blocks such as the DMP's JSON, or
/// stands in for them while collapsed.
fn prompt_lines(text: &str, collapsed: bool) -> Vec<BodyLine> {
    let code = Style::new().add_modifier(Modifier::DIM);
    let mut lines = Vec::new();
    let mut block: Option<(String, usize)> = None;
    for line in text.trim_end().lines() {
        let fence = line.trim_start().strip_prefix("```");
        match (&mut block, fence) {
            (None, Some(language)) => {
                block = Some((language.trim().to_string(), 0));
                if !collapsed {
                    lines.push(BodyLine::new(line, code));
                }
            }
            (Some((language, count)), Some(_)) => {
                if collapsed {
                    lines.push(hidden(*count, &of_language(language)));
                } else {
                    lines.push(BodyLine::new(line, code));
                }
                block = None;
            }
            (Some((_, count)), None) => {
                *count += 1;
                if !collapsed {
                    lines.push(BodyLine::new(line, code));
                }
            }
            (None, None) => lines.push(BodyLine::new(line, Style::new())),
        }
    }
    if let Some((language, count)) = block
        && collapsed
    {
        lines.push(hidden(count, &of_language(&language)));
    }
    lines
}

fn of_language(language: &str) -> String {
    if language.is_empty() {
        String::new()
    } else {
        format!("{language} ")
    }
}

/// Tool arguments as `key: value` lines; strings unquoted, so explanations
/// read as text.
fn argument_lines(arguments: &Value) -> Vec<BodyLine> {
    let Value::Object(fields) = arguments else {
        let pretty = serde_json::to_string_pretty(arguments).unwrap_or_default();
        return text_lines(&pretty, Style::new());
    };
    if fields.is_empty() {
        return vec![BodyLine::new(
            "(no arguments)",
            Style::new().add_modifier(Modifier::DIM),
        )];
    }
    let mut lines = Vec::new();
    for (key, value) in fields {
        let text = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut parts = text.lines();
        lines.push(BodyLine::keyed(key, parts.next().unwrap_or_default()));
        let indent = " ".repeat(key.len() + 2);
        lines.extend(parts.map(|part| BodyLine::new(format!("{indent}{part}"), Style::new())));
    }
    lines
}

/// Wraps at spaces where it can, mid-word where it must; continuation lines
/// are indented like the start of the text, after the key if there is one.
fn wrap(line: &BodyLine, width: usize) -> Vec<String> {
    let key = line.key.as_deref().unwrap_or_default();
    let leading = line.text.len() - line.text.trim_start().len();
    let mut wrapper = Wrapper {
        width,
        indent: (key.width() + leading).min(width / 2),
        pieces: Vec::new(),
        current: String::new(),
        current_width: 0,
    };
    let mut word = String::new();
    for c in key.chars().chain(line.text.chars()) {
        if c == ' ' && !word.trim().is_empty() {
            wrapper.push_word(&word);
            word.clear();
        }
        word.push(c);
    }
    wrapper.push_word(&word);
    wrapper.pieces.push(wrapper.current);
    wrapper.pieces
}

struct Wrapper {
    width: usize,
    indent: usize,
    pieces: Vec<String>,
    current: String,
    current_width: usize,
}

impl Wrapper {
    /// `word` comes with the spaces before it, dropped if it starts a line.
    fn push_word(&mut self, mut word: &str) {
        let word_width: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
        if self.current_width + word_width > self.width && !self.current.trim().is_empty() {
            self.break_line();
            word = word.trim_start();
        }
        for c in word.chars() {
            let c_width = c.width().unwrap_or(0);
            if self.current_width + c_width > self.width && self.current_width > self.indent {
                self.break_line();
            }
            self.current.push(c);
            self.current_width += c_width;
        }
    }

    fn break_line(&mut self) {
        let done = std::mem::replace(&mut self.current, " ".repeat(self.indent));
        self.pieces.push(done.trim_end().to_string());
        self.current_width = self.indent;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use fair_witness_core::project::{AiConfig, Project};
    use fair_witness_core::review::Reviewer;
    use fair_witness_core::session::Session;
    use fair_witness_core::testing::{fake_model, tool_call};
    use jiff::Timestamp;
    use serde_json::json;

    use super::*;

    fn wrapped(text: &str, width: usize) -> Vec<String> {
        wrap(&BodyLine::new(text, Style::new()), width)
    }

    fn texts(log: &mut DebugLog, width: u16) -> Vec<String> {
        log.lines(width)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn prompts() -> DebugLog {
        let mut log = DebugLog::default();
        log.add_trace(Trace::Setup {
            system: "Be careful.\nBe brief.".to_string(),
            dmp: "{\n}".to_string(),
            tools: vec!["read_file".to_string()],
        });
        log.add_trace(Trace::Prompt("Changed: a.csv".to_string()));
        log
    }

    #[test]
    fn collapses_the_system_prompt_and_the_dmp_until_expanded() {
        let mut log = prompts();
        assert_eq!(
            texts(&mut log, 80),
            [
                " SYSTEM PROMPT   how the AI should judge",
                "  ⋯ 2 lines hidden (c to expand)",
                "  tools: read_file",
                "",
                " FAIR-WITNESS   to the AI",
                "  The DMP:",
                "  ⋯ 2 json lines hidden (c to expand)",
                "  ",
                "  Changed: a.csv",
            ],
        );

        log.toggle_collapsed();
        let shown = texts(&mut log, 80);
        assert!(shown.contains(&"  Be brief.".to_string()), "{shown:?}");
        assert!(shown.contains(&"  ```json".to_string()), "{shown:?}");
        assert!(shown.contains(&"  {".to_string()), "{shown:?}");
    }

    #[test]
    fn shows_a_turn_with_its_tool_results_and_the_verdict() {
        let mut log = DebugLog::default();
        let choice = ModelChoice {
            model: "qwen3".to_string(),
            effort: None,
        };
        log.add(&Record {
            time: Timestamp::now(),
            event: Event::Reviewing { set: 3 },
        });
        log.add_trace(Trace::Model(choice));
        log.add_trace(Trace::Request { turn: 1 });
        let reply = |turn, text: &str, calls| Trace::Reply {
            turn,
            reasoning: None,
            texts: vec![text.to_string()],
            calls,
            stop_reason: Some("tool_calls".to_string()),
            tokens_in: Some(100),
            tokens_out: Some(20),
            elapsed_ms: 1340,
        };
        log.add_trace(reply(
            1,
            "Let me look.",
            vec![("read_file".to_string(), json!({ "path": "a.csv" }))],
        ));
        log.add_trace(Trace::ToolResult {
            name: "read_file".to_string(),
            output: "name,age".to_string(),
            failed: false,
        });
        log.add_trace(Trace::Request { turn: 2 });
        let lines = texts(&mut log, 40);
        assert_eq!(
            &lines[lines.len() - 2..],
            [" AI   turn 2 · qwen3", "  waiting for the reply…"]
        );

        log.add_trace(reply(
            2,
            "Done.",
            vec![(
                "report_verdict".to_string(),
                json!({ "contradicts": true, "dmp_field": "personal_data", "explanation": "Ages are personal." }),
            )],
        ));
        let lines = texts(&mut log, 40);
        assert!(
            lines[0].starts_with("━━ Review of change set #3 ━"),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(" ━━"), "{}", lines[0]);
        assert_eq!(
            &lines[1..],
            [
                "",
                " AI   turn 1 · qwen3 · 100 in · 20 out tokens · 1.3 s",
                "  Let me look.",
                "",
                "  ▸ read_file a.csv",
                "    │ name,age",
                "",
                " AI   turn 2 · qwen3 · 100 in · 20 out tokens · 1.3 s",
                "  Done.",
                "",
                "  ╭─ ✗ contradicts the DMP ────────────╮",
                "  │ field: personal_data               │",
                "  │ Ages are personal.                 │",
                "  ╰────────────────────────────────────╯",
            ],
        );
    }

    #[test]
    fn shows_a_request_that_waits_for_the_rate_limit_once() {
        let mut log = DebugLog::default();
        log.add_trace(Trace::Request { turn: 1 });
        log.add_trace(Trace::RateLimited {
            turn: 1,
            retry: 1,
            wait_ms: 4000,
            body: "Retry after 4s.".to_string(),
        });
        log.add_trace(Trace::Request { turn: 1 });
        assert_eq!(
            texts(&mut log, 80),
            [
                " RATE LIMIT   turn 1 · retry 1 in 4.0 s",
                "  Retry after 4s.",
                "",
                " AI   turn 1",
                "  waiting for the reply…",
            ]
        );
    }

    #[test]
    fn shortens_long_tool_output_while_collapsed() {
        let mut log = DebugLog::default();
        log.add_trace(Trace::Reply {
            turn: 1,
            reasoning: None,
            texts: Vec::new(),
            calls: vec![("list_dir".to_string(), json!({ "path": "." }))],
            stop_reason: None,
            tokens_in: None,
            tokens_out: None,
            elapsed_ms: 0,
        });
        let output: Vec<String> = (1..=10).map(|n| format!("file{n}")).collect();
        log.add_trace(Trace::ToolResult {
            name: "list_dir".to_string(),
            output: output.join("\n"),
            failed: false,
        });
        let lines = texts(&mut log, 80);
        assert_eq!(
            lines.last().unwrap(),
            "    │ ⋯ 2 more lines hidden (c to expand)"
        );
        log.toggle_collapsed();
        assert_eq!(texts(&mut log, 80).last().unwrap(), "    │ file10");
    }

    #[tokio::test]
    async fn shows_a_logged_run_as_it_showed_it_live() {
        let root = tempfile::tempdir().unwrap();
        let (endpoint, _server) = fake_model(vec![
            tool_call("read_file", json!({ "path": "participants.csv" })),
            tool_call(
                "report_verdict",
                json!({
                    "contradicts": true,
                    "dmp_field": "dataset[0].personal_data",
                    "explanation": "Participant names, but the DMP says no personal data."
                }),
            ),
        ]);
        let ai = AiConfig {
            url: endpoint,
            model: "test".to_string(),
            effort: None,
        };
        let project = Project::new(root.path().to_path_buf());
        let log = project.run_log().unwrap().events;
        let mut records = Session::new(project)
            .debounce(Duration::from_millis(50))
            .reviewing("{}".to_string(), vec![Arc::new(Reviewer::new(&ai, None))])
            .logging_to(log.clone())
            .start()
            .unwrap();
        fs::write(root.path().join("participants.csv"), "name\nAnna Muster\n").unwrap();

        let mut live = DebugLog::default();
        loop {
            let record = tokio::time::timeout(Duration::from_secs(10), records.recv())
                .await
                .expect("no verdict in time")
                .expect("the session stopped");
            live.add(&record);
            if let Event::Verdict { .. } = record.event {
                break;
            }
        }
        let mut replayed = DebugLog::default();
        for line in fs::read_to_string(&log).unwrap().lines() {
            replayed.add(&serde_json::from_str(line).unwrap());
        }

        assert_eq!(replayed.lines(100), live.lines(100));
        assert!(texts(&mut live, 100).contains(&"    │ Anna Muster".to_string()));
        live.toggle_collapsed();
        replayed.toggle_collapsed();
        assert_eq!(replayed.lines(100), live.lines(100));
    }

    #[test]
    fn wraps_under_the_start_of_the_text() {
        assert_eq!(
            wrapped("  one two three four", 10),
            ["  one two", "  three", "  four"]
        );
        let keyed = BodyLine::keyed("path", "a b c d e");
        assert_eq!(wrap(&keyed, 14), ["path: a b c d", "      e"]);
    }

    #[test]
    fn breaks_words_longer_than_a_line() {
        assert_eq!(wrapped("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn keeps_short_lines_and_blank_lines() {
        assert_eq!(wrapped("short", 10), ["short"]);
        assert_eq!(wrapped("", 10), [""]);
    }
}
