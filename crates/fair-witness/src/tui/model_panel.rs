use std::sync::Arc;

use anyhow::{Context, Result, bail};
use fair_witness_core::project::{Effort, Project};
use fair_witness_core::review::{self, ChoiceCheck, ModelChoice, ModelStep};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use tokio::sync::oneshot::{self, Receiver, error::TryRecvError};
use tokio::task::JoinHandle;

pub struct ModelSettings {
    pub project: Project,
    pub reviewer: Arc<ModelStep>,
    pub url: String,
    pub api_key: Option<String>,
}

impl ModelSettings {
    pub fn apply(&self, choice: ModelChoice) -> Result<()> {
        self.reviewer.set_choice(choice.clone());
        let mut config = self
            .project
            .config()?
            .context("this folder is not set up any more")?;
        let ai = config
            .ai
            .as_mut()
            .context("no AI model is set up any more")?;
        // A model of an API given with --ai-url would be saved under the API from `setup`.
        if ai.url != self.url {
            bail!("{} is not the API chosen in `fair-witness setup`", self.url);
        }
        ai.model = choice.model;
        ai.effort = choice.effort;
        self.project.save_config(&config)
    }
}

pub fn describe(choice: &ModelChoice) -> String {
    match choice.effort.sent() {
        Some(effort) => format!("{} ({effort} effort)", choice.model),
        None => choice.model.clone(),
    }
}

enum Models {
    Loading(Receiver<Result<Vec<String>>>),
    Loaded(Vec<String>),
    Failed(String),
}

#[derive(PartialEq)]
enum Focus {
    Model,
    Effort,
}

pub enum Outcome {
    Open,
    Closed,
    /// A choice to use, with why it could not be checked, if it could not.
    Chosen(ModelChoice, Option<String>),
}

/// Whether the API takes the choice, which the panel asks before using it.
enum Check {
    None,
    Running {
        choice: ModelChoice,
        checked: Receiver<ChoiceCheck>,
        _task: CheckTask,
    },
    Refused {
        choice: ModelChoice,
        message: String,
    },
}

/// The task that checks a choice. It stops when the panel closes, so that a check nobody waits
/// for sends no request without the token limit, which a model may answer at length.
struct CheckTask(JoinHandle<()>);

impl Drop for CheckTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct ModelPanel {
    reviewer: Arc<ModelStep>,
    current: ModelChoice,
    models: Models,
    filter: String,
    model: ListState,
    efforts: Vec<Effort>,
    effort: ListState,
    focus: Focus,
    check: Check,
}

impl ModelPanel {
    pub fn open(settings: &ModelSettings) -> Self {
        let (sender, models) = oneshot::channel();
        let url = settings.url.clone();
        let api_key = settings.api_key.clone();
        tokio::spawn(async move {
            let listed = review::list_models(&url, api_key.as_deref()).await;
            let _ = sender.send(listed);
        });
        let current = settings.reviewer.choice();
        let mut efforts = Effort::offered();
        // An effort given with --effort or in config.toml need not be one of those offered.
        if !efforts.contains(&current.effort) {
            efforts.push(current.effort.clone());
        }
        let effort = efforts.iter().position(|e| *e == current.effort);
        Self {
            reviewer: settings.reviewer.clone(),
            current,
            models: Models::Loading(models),
            filter: String::new(),
            model: ListState::default(),
            efforts,
            effort: ListState::default().with_selected(effort),
            focus: Focus::Model,
            check: Check::None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Closed,
            // The choice being checked stays as it is until the API answers.
            _ if matches!(self.check, Check::Running { .. }) => {}
            KeyCode::Enter => match self.chosen() {
                // The one in use needs no check.
                Some(choice) if choice == self.current => return Outcome::Chosen(choice, None),
                Some(choice) => self.start_check(choice),
                None => {}
            },
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.focus = match self.focus {
                    Focus::Model => Focus::Effort,
                    Focus::Effort => Focus::Model,
                }
            }
            KeyCode::Up => self.list().select_previous(),
            KeyCode::Down => self.list().select_next(),
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.focus = Focus::Model;
                self.refilter();
            }
            _ => {}
        }
        Outcome::Open
    }

    fn start_check(&mut self, choice: ModelChoice) {
        let (sender, checked) = oneshot::channel();
        let reviewer = self.reviewer.clone();
        let asked = choice.clone();
        let task = tokio::spawn(async move {
            let _ = sender.send(reviewer.try_choice(&asked).await);
        });
        self.check = Check::Running {
            choice,
            checked,
            _task: CheckTask(task),
        };
    }

    /// What came of checking the choice: `Chosen` once the API took it or the check could not
    /// tell, and `Open` while it checks, after the API refused it, or before any check.
    pub fn checked(&mut self) -> Outcome {
        let Check::Running {
            choice, checked, ..
        } = &mut self.check
        else {
            return Outcome::Open;
        };
        let checked = match checked.try_recv() {
            Ok(checked) => checked,
            Err(TryRecvError::Empty) => return Outcome::Open,
            Err(TryRecvError::Closed) => ChoiceCheck::Unknown("the check stopped".to_string()),
        };
        let choice = choice.clone();
        self.check = Check::None;
        match checked {
            ChoiceCheck::Accepted => Outcome::Chosen(choice, None),
            ChoiceCheck::Unknown(why) => Outcome::Chosen(choice, Some(why)),
            ChoiceCheck::Refused(message) => {
                self.check = Check::Refused { choice, message };
                Outcome::Open
            }
        }
    }

    fn list(&mut self) -> &mut ListState {
        match self.focus {
            Focus::Model => &mut self.model,
            Focus::Effort => &mut self.effort,
        }
    }

    fn chosen(&self) -> Option<ModelChoice> {
        let model = self
            .model
            .selected()
            .and_then(|i| self.visible().get(i).map(|model| model.to_string()))?;
        let effort = self
            .effort
            .selected()
            .unwrap_or(0)
            .min(self.efforts.len() - 1);
        let effort = self.efforts[effort].clone();
        Some(ModelChoice { model, effort })
    }

    fn visible(&self) -> Vec<&str> {
        let Models::Loaded(models) = &self.models else {
            return Vec::new();
        };
        let filter = self.filter.to_lowercase();
        let mut visible: Vec<&str> = models.iter().map(String::as_str).collect();
        if !visible.contains(&self.current.model.as_str()) {
            visible.insert(0, &self.current.model);
        }
        visible.retain(|model| model.to_lowercase().contains(&filter));
        visible
    }

    fn refilter(&mut self) {
        let visible = self.visible();
        let selected = visible
            .iter()
            .position(|model| *model == self.current.model)
            .or((!visible.is_empty()).then_some(0));
        self.model.select(selected);
    }

    fn poll(&mut self) {
        let Models::Loading(receiver) = &mut self.models else {
            return;
        };
        self.models = match receiver.try_recv() {
            Ok(Ok(models)) => Models::Loaded(models),
            Ok(Err(e)) => Models::Failed(format!("{e:#}")),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Closed) => Models::Failed("could not list the models".into()),
        };
        self.refilter();
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.poll();
        let rows = self.visible().len().max(self.efforts.len()) as u16;
        let [area] = Layout::horizontal([Constraint::Max(72)])
            .flex(Flex::Center)
            .areas(area);
        let check = self.check_message();
        // The borders and the padding take 4 columns, and a blank row goes above the message.
        let check_rows = check.as_ref().map_or(0, |check| {
            check.line_count(area.width.saturating_sub(4)) as u16 + 1
        });
        let [area] = Layout::vertical([Constraint::Max(rows + 5 + check_rows)])
            .flex(Flex::Center)
            .areas(area);
        frame.render_widget(Clear, area);

        let keys = Line::from(vec![
            " ↑↓".bold(),
            " choose  ".into(),
            "Tab".bold(),
            " model/effort  ".into(),
            "Enter".bold(),
            " use  ".into(),
            "Esc".bold(),
            " cancel ".into(),
        ])
        .style(Style::new().fg(Color::DarkGray));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" Change model ".bold())
            .title_bottom(keys)
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [filter, _, columns, check_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(check_rows),
        ])
        .areas(inner);
        if let Some(check) = check {
            let [_, message] =
                Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(check_area);
            frame.render_widget(check, message);
        }
        self.draw_filter(frame, filter);
        // Room for the longest effort and the mark of the one in use, since --effort takes any,
        // but at most half the width, so that the models stay readable.
        let width = self.efforts.iter().map(|e| e.name().chars().count() + 2);
        let width = (width.max().unwrap_or(0) as u16).min(columns.width / 2);
        let [models, divider, efforts] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Length(width),
        ])
        .areas(columns);
        self.draw_models(frame, models);
        frame.render_widget(
            Block::new()
                .borders(Borders::LEFT)
                .border_style(Style::new().fg(Color::DarkGray)),
            divider.inner(Margin::new(1, 0)),
        );
        self.draw_efforts(frame, efforts);
    }

    /// What the panel says about the check of the choice, if anything.
    fn check_message(&self) -> Option<Paragraph<'static>> {
        let text = match &self.check {
            Check::None => return None,
            Check::Running { choice, .. } => Text::styled(
                format!("Checking {} with the API…", describe(choice)),
                Modifier::DIM,
            ),
            // The API's message can span lines, which `Text` keeps apart.
            Check::Refused { choice, message } => Text::styled(
                format!("The API refuses {}: {message}", describe(choice)),
                Color::Red,
            ),
        };
        Some(Paragraph::new(text).wrap(Wrap { trim: false }))
    }

    fn draw_filter(&self, frame: &mut Frame, area: Rect) {
        let line = if self.filter.is_empty() {
            Line::from(vec![
                "/ ".fg(Color::DarkGray),
                "type to filter".add_modifier(Modifier::DIM),
            ])
        } else {
            Line::from(vec!["/ ".fg(Color::Cyan), Span::raw(self.filter.clone())])
        };
        frame.render_widget(line, area);
    }

    fn draw_models(&mut self, frame: &mut Frame, area: Rect) {
        let list = header(frame, area, "Model", self.focus == Focus::Model);
        let message = match &self.models {
            Models::Loading(_) => Some("Loading models…".add_modifier(Modifier::DIM)),
            Models::Failed(e) => Some(e.clone().red()),
            Models::Loaded(_) if self.visible().is_empty() => {
                Some("No model matches.".add_modifier(Modifier::DIM))
            }
            Models::Loaded(_) => None,
        };
        if let Some(message) = message {
            frame.render_widget(Paragraph::new(message), list);
            return;
        }
        let items: Vec<ListItem> = self
            .visible()
            .into_iter()
            .map(|model| item(model, model == self.current.model))
            .collect();
        frame.render_stateful_widget(
            highlighted(List::new(items), self.focus == Focus::Model),
            list,
            &mut self.model,
        );
    }

    fn draw_efforts(&mut self, frame: &mut Frame, area: Rect) {
        let list = header(frame, area, "Effort", self.focus == Focus::Effort);
        let items: Vec<ListItem> = self
            .efforts
            .iter()
            .map(|effort| item(effort.name(), *effort == self.current.effort))
            .collect();
        frame.render_stateful_widget(
            highlighted(List::new(items), self.focus == Focus::Effort),
            list,
            &mut self.effort,
        );
    }
}

/// Draws a column title and returns the area below it.
fn header(frame: &mut Frame, area: Rect, title: &str, focused: bool) -> Rect {
    let [title_area, rest] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    let style = if focused {
        Style::new().fg(Color::Cyan).bold()
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(Line::styled(title.to_uppercase(), style), title_area);
    rest
}

fn item(name: &str, current: bool) -> ListItem<'static> {
    let mut spans = vec![Span::raw(name.to_string())];
    if current {
        spans.push(" •".green());
    }
    ListItem::new(Line::from(spans))
}

fn highlighted(list: List<'_>, focused: bool) -> List<'_> {
    let style = if focused {
        Style::new().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::new().fg(Color::Cyan)
    };
    list.highlight_style(style)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::Duration;

    use fair_witness_core::project::AiConfig;
    use fair_witness_core::testing::{
        EFFORT_REFUSED, Reply, bad_request, fake_model, one_token, too_many_requests,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;

    /// A panel that has listed the models of a fake server, with qwen3:8b at `effort` in use. The
    /// server answers the checks with `checks`.
    async fn loaded(effort: &str, checks: Vec<Reply>) -> ModelPanel {
        let mut replies: Vec<Reply> = vec![
            json!({
                "object": "list",
                "data": [
                    { "id": "qwen3:8b", "object": "model" },
                    { "id": "llama3.2", "object": "model" },
                    { "id": "mistral-small", "object": "model" }
                ]
            })
            .into(),
        ];
        replies.extend(checks);
        let (url, _server) = fake_model(replies);
        let ai = AiConfig {
            url: url.clone(),
            model: "qwen3:8b".to_string(),
            effort: effort.parse().unwrap(),
        };
        let settings = ModelSettings {
            project: Project::new(PathBuf::from("/home/researcher/sleep-study")),
            reviewer: Arc::new(ModelStep::new(&ai, None).unwrap()),
            url,
            api_key: None,
        };
        let mut panel = ModelPanel::open(&settings);
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(panel.models, Models::Loading(_)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
                panel.poll();
            }
        })
        .await
        .expect("the models did not load in time");
        panel
    }

    /// What happens once the API has answered the check of the choice.
    async fn once_checked(panel: &mut ModelPanel) -> Outcome {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outcome = panel.checked();
                if !matches!(panel.check, Check::Running { .. }) {
                    return outcome;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the check did not end in time")
    }

    fn drawn(panel: &mut ModelPanel) -> String {
        drawn_rows(panel, 14)
    }

    fn drawn_rows(panel: &mut ModelPanel, rows: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, rows)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        format!("{:#?}", terminal.backend().buffer())
    }

    #[tokio::test]
    async fn lists_the_models_and_marks_the_one_in_use() {
        insta::assert_snapshot!(drawn(&mut loaded("medium", Vec::new()).await));
    }

    #[tokio::test]
    async fn filters_the_models_and_moves_to_the_efforts() {
        let mut panel = loaded("medium", Vec::new()).await;
        for key in [
            KeyCode::Char('l'),
            KeyCode::Char('l'),
            KeyCode::Tab,
            KeyCode::Down,
        ] {
            panel.handle_key(key.into());
        }
        insta::assert_snapshot!(drawn(&mut panel));
    }

    #[tokio::test]
    async fn keeps_an_effort_it_does_not_offer_however_long() {
        let effort = "ultra".repeat(13);
        let mut panel = loaded(&effort, Vec::new()).await;

        let shown = drawn(&mut panel);
        assert!(shown.contains("│ mistral-small "), "{shown}");
        // The choice in use needs no check.
        let Outcome::Chosen(choice, None) = panel.handle_key(KeyCode::Enter.into()) else {
            panic!("nothing chosen");
        };
        assert_eq!(
            choice,
            ModelChoice {
                model: "qwen3:8b".to_string(),
                effort: effort.parse().unwrap(),
            }
        );
    }

    #[tokio::test]
    async fn turns_down_an_effort_the_api_refuses_with_its_message() {
        let mut panel = loaded(
            "medium",
            vec![bad_request(EFFORT_REFUSED), bad_request(EFFORT_REFUSED)],
        )
        .await;

        for key in [KeyCode::Tab, KeyCode::Down, KeyCode::Enter] {
            assert!(matches!(panel.handle_key(key.into()), Outcome::Open));
        }
        let checking = drawn(&mut panel);
        // While it checks, the choice stays as it is.
        panel.handle_key(KeyCode::Down.into());
        let outcome = once_checked(&mut panel).await;

        assert!(
            checking.contains("│ Checking qwen3:8b (high effort) with the API…"),
            "{checking}"
        );
        assert!(matches!(outcome, Outcome::Open));
        insta::assert_snapshot!(drawn_rows(&mut panel, 22));
    }

    #[tokio::test]
    async fn uses_a_choice_the_api_takes_or_that_it_could_not_check() {
        let mut outcomes = Vec::new();
        for answer in [
            one_token().into(),
            too_many_requests(None, "Rate limit exceeded."),
        ] {
            let mut panel = loaded("medium", vec![answer]).await;
            for key in [KeyCode::Tab, KeyCode::Down, KeyCode::Enter] {
                panel.handle_key(key.into());
            }
            match once_checked(&mut panel).await {
                Outcome::Chosen(choice, unchecked) => outcomes.push((choice, unchecked)),
                _ => panic!("nothing chosen"),
            }
        }

        let high = ModelChoice {
            model: "qwen3:8b".to_string(),
            effort: "high".parse().unwrap(),
        };
        assert_eq!(
            outcomes,
            [
                (high.clone(), None),
                (
                    high,
                    Some(
                        "the API answered 429 Too Many Requests: Rate limit exceeded.".to_string()
                    )
                ),
            ]
        );
    }

    #[tokio::test]
    async fn stops_checking_once_it_closes() {
        // An API that takes the check's request and never answers.
        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", api.local_addr().unwrap());
        let (asked, request) = oneshot::channel();
        let closed = tokio::task::spawn_blocking(move || {
            let (mut stream, _) = api.accept().unwrap();
            let mut buffer = [0; 4096];
            stream.read_exact(&mut buffer[..1]).unwrap();
            asked.send(()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            // Reads the rest of the request until the connection closes.
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => return true,
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
        });
        let mut panel = loaded("medium", Vec::new()).await;
        let ai = AiConfig {
            url,
            model: "qwen3:8b".to_string(),
            effort: "medium".parse().unwrap(),
        };
        panel.reviewer = Arc::new(ModelStep::new(&ai, None).unwrap());
        for key in [KeyCode::Tab, KeyCode::Down, KeyCode::Enter] {
            panel.handle_key(key.into());
        }
        request.await.unwrap();

        assert!(matches!(
            panel.handle_key(KeyCode::Esc.into()),
            Outcome::Closed
        ));
        drop(panel);

        assert!(
            closed.await.unwrap(),
            "the check went on after the panel closed"
        );
    }
}
