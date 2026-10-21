use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
};

use super::connect::http_client;
use crate::agent::{self, ModelChoice, Reviewer};
use crate::config::{Config, Effort};

pub struct ModelSettings {
    pub reviewer: Arc<Reviewer>,
    pub url: String,
    pub api_key: Option<String>,
}

impl ModelSettings {
    pub fn apply(&self, choice: ModelChoice) -> Result<()> {
        self.reviewer.set_choice(choice.clone());
        let mut config = Config::load()?.context("this folder is not set up any more")?;
        let ai = config
            .ai
            .as_mut()
            .context("no AI model is set up any more")?;
        ai.model = choice.model;
        ai.effort = choice.effort;
        config.save()
    }
}

pub fn describe(choice: &ModelChoice) -> String {
    match choice.effort {
        Some(effort) => format!("{} ({} effort)", choice.model, effort.name()),
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
    Chosen(ModelChoice),
}

const EFFORTS: [Option<Effort>; 5] = [
    None,
    Some(Effort::None),
    Some(Effort::Low),
    Some(Effort::Medium),
    Some(Effort::High),
];

pub struct ModelPanel {
    current: ModelChoice,
    models: Models,
    filter: String,
    model: ListState,
    effort: ListState,
    focus: Focus,
}

impl ModelPanel {
    pub fn open(settings: &ModelSettings) -> Self {
        let (sender, models) = mpsc::channel();
        let url = settings.url.clone();
        let api_key = settings.api_key.clone();
        thread::spawn(move || {
            let listed =
                http_client().and_then(|http| agent::list_models(&http, &url, api_key.as_deref()));
            let _ = sender.send(listed);
        });
        let current = settings.reviewer.choice();
        let effort = EFFORTS.iter().position(|e| *e == current.effort);
        Self {
            current,
            models: Models::Loading(models),
            filter: String::new(),
            model: ListState::default(),
            effort: ListState::default().with_selected(effort),
            focus: Focus::Model,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Closed,
            KeyCode::Enter => {
                return match self.chosen() {
                    Some(choice) => Outcome::Chosen(choice),
                    None => Outcome::Open,
                };
            }
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
        let effort = EFFORTS[self.effort.selected().unwrap_or(0).min(EFFORTS.len() - 1)];
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
        let Models::Loading(receiver) = &self.models else {
            return;
        };
        self.models = match receiver.try_recv() {
            Ok(Ok(models)) => Models::Loaded(models),
            Ok(Err(e)) => Models::Failed(format!("{e:#}")),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Models::Failed("could not list the models".into()),
        };
        self.refilter();
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.poll();
        let rows = self.visible().len().max(EFFORTS.len()) as u16;
        let [area] = Layout::horizontal([Constraint::Max(72)])
            .flex(Flex::Center)
            .areas(area);
        let [area] = Layout::vertical([Constraint::Max(rows + 5)])
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

        let [filter, _, columns] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        self.draw_filter(frame, filter);
        let [models, divider, efforts] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Length(10),
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
        let items: Vec<ListItem> = EFFORTS
            .iter()
            .map(|effort| {
                let name = effort.map_or("default", Effort::name);
                item(name, *effort == self.current.effort)
            })
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
