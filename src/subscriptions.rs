use std::{collections::HashSet, time::Instant};

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use reqwest::StatusCode;
use serde_json::Value;

use crate::{
    auth::{Credentials, Error},
    content::{Api, Comment, Episode, Page},
    markdown,
    playlist::{fit, format_duration},
    recommendations::Kind,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    Subscriptions,
    Recommendation(Kind),
    History,
}

impl Source {
    pub const SLOTS: usize = 4 * (2 + Kind::ALL.len());

    pub fn slot(self) -> usize {
        4 * match self {
            Self::Subscriptions => 0,
            Self::Recommendation(kind) => kind.index() + 1,
            Self::History => Kind::ALL.len() + 1,
        }
    }
}

#[derive(Clone)]
pub enum Request {
    Feed(Option<Value>),
    Detail(String),
    Comments(String, Option<Value>),
    ListeningTime,
}

impl Request {
    pub fn slot(&self) -> usize {
        match self {
            Self::Feed(_) => 0,
            Self::Detail(_) => 1,
            Self::Comments(..) => 2,
            Self::ListeningTime => 3,
        }
    }

    pub async fn fetch(self, api: Api, credentials: Credentials, source: Source) -> Response {
        match self {
            Self::Feed(cursor) => Response::Feed(match source {
                Source::Recommendation(kind) => {
                    api.recommendations(&credentials, kind, cursor).await
                }
                Source::Subscriptions => api.subscriptions(&credentials, cursor).await,
                Source::History => api.listening_history(&credentials, cursor).await,
            }),
            Self::ListeningTime => {
                Response::ListeningTime(api.listening_seconds(&credentials).await)
            }
            Self::Detail(eid) => {
                let result = api.episode_detail(&credentials, &eid).await;
                Response::Detail(eid, result)
            }
            Self::Comments(eid, cursor) => {
                let result = api.comments(&credentials, &eid, cursor).await;
                Response::Comments(eid, result)
            }
        }
    }
}

pub enum Response {
    Feed(Result<Page<Episode>, Error>),
    Detail(String, Result<Episode, Error>),
    Comments(String, Result<Page<Comment>, Error>),
    ListeningTime(Result<u64, Error>),
}

// Account::request must see HTTP 401 so it can renew the shared login.
impl Response {
    pub fn into_result(self) -> Result<Self, Error> {
        match self {
            Self::Feed(Err(error))
            | Self::Detail(_, Err(error))
            | Self::Comments(_, Err(error))
            | Self::ListeningTime(Err(error)) => Err(error),
            response => Ok(response),
        }
    }
}

impl Request {
    pub fn failed(self, error: Error) -> Response {
        match self {
            Self::Feed(_) => Response::Feed(Err(error)),
            Self::Detail(eid) => Response::Detail(eid, Err(error)),
            Self::Comments(eid, _) => Response::Comments(eid, Err(error)),
            Self::ListeningTime => Response::ListeningTime(Err(error)),
        }
    }
}

#[derive(Default)]
pub(crate) struct Load {
    pub(crate) requested: bool,
    loading: bool,
    error: Option<Error>,
    retry_at: Option<Instant>,
}

impl Load {
    pub(crate) fn ready(&self) -> bool {
        !self.loading && self.retry_at.is_none_or(|until| Instant::now() >= until)
    }
    pub(crate) fn begin(&mut self) {
        self.requested = true;
        self.loading = true;
        self.error = None;
    }
    pub(crate) fn finish(&mut self, error: Option<Error>) {
        self.loading = false;
        self.retry_at = error.as_ref().and_then(|error| {
            if let Error::RateLimited(delay) = error {
                Instant::now().checked_add(*delay)
            } else {
                None
            }
        });
        self.error = error;
    }
    pub(crate) fn needs_login(&self) -> bool {
        matches!(self.error, Some(Error::Http(StatusCode::UNAUTHORIZED)))
    }
    pub(crate) fn message(&self) -> String {
        if self.loading {
            "加载中…".into()
        } else if self.needs_login() {
            "登录已失效".into()
        } else if let Some(error) = &self.error {
            let wait = self
                .retry_at
                .and_then(|until| until.checked_duration_since(Instant::now()));
            if let Some(wait) = wait {
                format!("{error} · {} 秒后重试", wait.as_secs() + 1)
            } else {
                error.to_string()
            }
        } else {
            String::new()
        }
    }
}

trait Identified {
    fn id(&self) -> &str;
}
impl Identified for Episode {
    fn id(&self) -> &str {
        &self.eid
    }
}
impl Identified for Comment {
    fn id(&self) -> &str {
        &self.id
    }
}

struct CachedPage<T> {
    data: Page<T>,
    state: TableState,
    scroll: usize,
}

struct Paged<T> {
    pages: Vec<CachedPage<T>>,
    index: usize,
    pending: Option<Value>,
    load: Load,
}

impl<T> Default for Paged<T> {
    fn default() -> Self {
        Self {
            pages: vec![],
            index: 0,
            pending: None,
            load: Load::default(),
        }
    }
}

impl<T: Identified> Paged<T> {
    fn current(&self) -> Option<&CachedPage<T>> {
        self.pages.get(self.index)
    }
    fn current_mut(&mut self) -> Option<&mut CachedPage<T>> {
        self.pages.get_mut(self.index)
    }
    fn begin(&mut self, cursor: Option<Value>) {
        self.pending = cursor;
        self.load.begin();
    }
    fn apply(&mut self, result: Result<Page<T>, Error>) {
        let result = result.and_then(|mut page| {
            if self.pending.is_some() {
                let ids: HashSet<_> = self
                    .pages
                    .iter()
                    .flat_map(|page| &page.data.items)
                    .map(Identified::id)
                    .collect();
                page.items.retain(|item| !ids.contains(item.id()));
            }
            if page.cursor.is_some()
                && (page.items.is_empty()
                    || self.pending.is_some()
                        && self.pages.iter().any(|old| old.data.cursor == page.cursor))
            {
                return Err(Error::InvalidResponse);
            }
            Ok(page)
        });
        match result {
            Ok(page) => {
                let selected = self
                    .current()
                    .and_then(|page| page.state.selected().and_then(|i| page.data.items.get(i)))
                    .map(|item| item.id().to_owned());
                let mut state = TableState::default();
                if !page.items.is_empty() {
                    state.select(Some(if self.pending.is_none() {
                        selected
                            .and_then(|id| page.items.iter().position(|item| item.id() == id))
                            .unwrap_or(0)
                    } else {
                        0
                    }));
                }
                if self.pending.is_none() {
                    self.pages.clear();
                }
                self.pages.push(CachedPage {
                    data: page,
                    state,
                    scroll: 0,
                });
                self.index = self.pages.len() - 1;
                self.load.finish(None);
            }
            Err(error) => self.load.finish(Some(error)),
        }
    }
    fn next(&mut self) -> Option<Option<Value>> {
        if self.load.loading {
            return None;
        }
        if self.index + 1 < self.pages.len() {
            self.index += 1;
            self.load.error = None;
            return None;
        }
        if !self.load.ready() {
            return None;
        }
        let cursor = self.current()?.data.cursor.clone()?;
        self.begin(Some(cursor.clone()));
        Some(Some(cursor))
    }
    fn previous(&mut self) {
        if self.load.loading {
            return;
        }
        self.index = self.index.saturating_sub(1);
        self.load.error = None;
    }
    fn reload(&mut self) -> Option<Option<Value>> {
        if !self.load.ready() {
            return None;
        }
        let cursor = if self.load.error.is_some() {
            self.pending.clone()
        } else {
            None
        };
        self.begin(cursor.clone());
        Some(cursor)
    }
    fn label(&self) -> String {
        let last = self
            .current()
            .is_some_and(|page| page.data.cursor.is_none());
        format!(
            "第 {} 页{}",
            self.index + 1,
            if last { " · 已到底" } else { "" }
        )
    }
}

#[derive(Default)]
pub struct Subscriptions {
    feed: Paged<Episode>,
    detail: Option<Detail>,
    source: Source,
}

pub enum Action {
    None,
    Back,
    Login,
    Logout,
    Play(String),
    Add(String),
    Load(Vec<Request>),
}

impl Subscriptions {
    pub fn recommendations(kind: Kind) -> Self {
        Self {
            source: Source::Recommendation(kind),
            ..Self::default()
        }
    }

    pub fn history() -> Self {
        Self {
            source: Source::History,
            ..Self::default()
        }
    }

    pub fn is_detail(&self) -> bool {
        self.detail.is_some()
    }

    pub fn help_context(&self) -> crate::help::Context {
        if let Some(detail) = &self.detail {
            crate::help::Context::Episode {
                needs_login: detail.load.needs_login() || detail.comments.load.needs_login(),
            }
        } else if matches!(self.source, Source::Recommendation(_)) {
            crate::help::Context::Recommendations {
                needs_login: self.feed.load.needs_login(),
            }
        } else if self.source == Source::History {
            crate::help::Context::Settings {
                needs_login: self.feed.load.needs_login(),
                logout_selected: false,
            }
        } else {
            crate::help::Context::Subscriptions {
                needs_login: self.feed.load.needs_login(),
            }
        }
    }

    pub fn enter(&mut self) -> Vec<Request> {
        if self.feed.load.requested {
            return vec![];
        }
        self.feed.begin(None);
        vec![Request::Feed(None)]
    }
    pub fn key(&mut self, key: KeyCode) -> Action {
        if let Some(detail) = &mut self.detail {
            if matches!(key, KeyCode::Esc | KeyCode::Backspace) {
                self.detail = None;
                return Action::None;
            }
            return detail.key(key);
        }
        match key {
            KeyCode::Esc | KeyCode::Backspace => Action::Back,
            KeyCode::Enter if self.feed.load.needs_login() => Action::Login,
            KeyCode::Enter => {
                let Some(episode) = self.selected().cloned() else {
                    return Action::None;
                };
                let eid = episode.eid.clone();
                self.detail = Some(Detail::new(episode));
                Action::Load(vec![
                    Request::Detail(eid.clone()),
                    Request::Comments(eid, None),
                ])
            }
            KeyCode::Char('y') => self
                .selected()
                .map(|episode| Action::Add(episode.eid.clone()))
                .unwrap_or(Action::None),
            KeyCode::PageDown | KeyCode::Right | KeyCode::Char('n' | 'l') => self
                .feed
                .next()
                .map(|cursor| Action::Load(vec![Request::Feed(cursor)]))
                .unwrap_or(Action::None),
            KeyCode::PageUp | KeyCode::Left | KeyCode::Char('p' | 'h') => {
                self.feed.previous();
                Action::None
            }
            KeyCode::Char('r') => self
                .feed
                .reload()
                .map(|cursor| Action::Load(vec![Request::Feed(cursor)]))
                .unwrap_or(Action::None),
            key => {
                if let Some(page) = self.feed.current_mut() {
                    let current = page.state.selected().unwrap_or(0);
                    let last = page.data.items.len().saturating_sub(1);
                    let selected = match key {
                        KeyCode::Down | KeyCode::Char('j') => (current + 1).min(last),
                        KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
                        KeyCode::Home | KeyCode::Char('g') => 0,
                        KeyCode::End | KeyCode::Char('G') => last,
                        _ => current,
                    };
                    if !page.data.items.is_empty() {
                        page.state.select(Some(selected));
                    }
                }
                Action::None
            }
        }
    }
    fn selected(&self) -> Option<&Episode> {
        let page = self.feed.current()?;
        page.data.items.get(page.state.selected()?)
    }
    pub fn apply(&mut self, response: Response) {
        match response {
            Response::ListeningTime(_) => {}
            Response::Feed(result) => self.feed.apply(result),
            Response::Detail(eid, result) => {
                if let Some(detail) = &mut self.detail
                    && detail.episode.eid == eid
                {
                    match result {
                        Ok(episode) => {
                            detail.episode = episode;
                            detail.note_width = 0;
                            detail.load.finish(None);
                        }
                        Err(error) => detail.load.finish(Some(error)),
                    }
                }
            }
            Response::Comments(eid, result) => {
                if let Some(detail) = &mut self.detail
                    && detail.episode.eid == eid
                {
                    detail.comments.apply(result);
                }
            }
        }
    }
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.draw_focused(frame, area, true);
    }

    pub fn draw_focused(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        if area.width < 24 || area.height < 8 {
            frame.render_widget(Paragraph::new("请放大终端窗口").centered(), area);
            return;
        }
        if let Some(detail) = &mut self.detail {
            detail.draw(frame, area);
            return;
        }
        let message = self.feed.load.message();
        let [heading, list, status] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(u16::from(!message.is_empty())),
        ])
        .areas(area);
        let list = Rect {
            height: list.height / 2 * 2,
            ..list
        };
        let title = match self.source {
            Source::Subscriptions => "订阅列表".to_owned(),
            Source::Recommendation(kind) => format!("推荐列表 · {}", kind.label()),
            Source::History => "收听历史".to_owned(),
        };
        frame.render_widget(
            Paragraph::new(format!("{title} · {}", self.feed.label()))
                .centered()
                .style(bold()),
            heading,
        );
        let empty = self
            .feed
            .current()
            .is_none_or(|page| page.data.items.is_empty());
        if empty {
            frame.render_widget(
                Paragraph::new(if self.feed.load.loading {
                    "加载中…"
                } else if self.feed.load.error.is_some() {
                    ""
                } else if self.source == Source::History {
                    "暂无收听历史"
                } else if matches!(self.source, Source::Recommendation(_)) {
                    "暂无推荐内容"
                } else {
                    "暂无订阅更新"
                })
                .centered()
                .style(muted()),
                list,
            );
        } else if let Some(page) = self.feed.current_mut() {
            let rows = page.data.items.iter().enumerate().map(|(index, episode)| {
                let style = if focused && page.state.selected() == Some(index) {
                    bold().fg(Color::Cyan)
                } else {
                    Style::default()
                };
                Row::new([Cell::from(Text::from(vec![
                    Line::styled(
                        fit(
                            &if matches!(self.source, Source::Recommendation(kind) if kind.category().is_some())
                            {
                                format!("{:02}. {}", index + 1, episode.title)
                            } else {
                                episode.title.clone()
                            },
                            area.width as usize,
                        ),
                        style,
                    ),
                    Line::styled(episode_metadata(episode, area.width), muted()),
                ]))])
                .height(2)
            });
            frame.render_stateful_widget(
                Table::new(rows, [Constraint::Percentage(100)]),
                list,
                &mut page.state,
            );
        }
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::Yellow)),
            status,
        );
    }
}

struct Detail {
    episode: Episode,
    load: Load,
    comments: Paged<Comment>,
    comments_tab: bool,
    note_width: u16,
    note_lines: Vec<Line<'static>>,
    note_scroll: usize,
    note_height: usize,
    comment_height: usize,
    comment_line_count: usize,
}

impl Detail {
    fn new(episode: Episode) -> Self {
        let mut load = Load::default();
        load.begin();
        let mut comments = Paged::default();
        comments.begin(None);
        Self {
            episode,
            load,
            comments,
            comments_tab: false,
            note_width: 0,
            note_lines: vec![],
            note_scroll: 0,
            note_height: 1,
            comment_height: 1,
            comment_line_count: 0,
        }
    }
    fn key(&mut self, key: KeyCode) -> Action {
        let eid = self.episode.eid.clone();
        match key {
            KeyCode::Enter if self.load.needs_login() || self.comments.load.needs_login() => {
                Action::Login
            }
            KeyCode::Enter => Action::Play(eid),
            KeyCode::Char('y') => Action::Add(eid),
            KeyCode::Tab | KeyCode::BackTab => {
                self.comments_tab = !self.comments_tab;
                Action::None
            }
            KeyCode::Char('n') | KeyCode::Right if self.comments_tab => self
                .comments
                .next()
                .map(|cursor| Action::Load(vec![Request::Comments(eid, cursor)]))
                .unwrap_or(Action::None),
            KeyCode::Char('p') | KeyCode::Left if self.comments_tab => {
                self.comments.previous();
                Action::None
            }
            KeyCode::Char('r') => {
                let mut requests = vec![];
                if !self.comments_tab && self.load.ready() {
                    self.load.begin();
                    requests.push(Request::Detail(eid.clone()));
                }
                if self.comments_tab
                    && let Some(cursor) = self.comments.reload()
                {
                    requests.push(Request::Comments(eid, cursor));
                }
                Action::Load(requests)
            }
            key => {
                if self.comments_tab {
                    if let Some(page) = self.comments.current_mut() {
                        scroll(
                            &mut page.scroll,
                            key,
                            self.comment_height,
                            self.comment_line_count,
                        );
                    }
                } else {
                    scroll(
                        &mut self.note_scroll,
                        key,
                        self.note_height,
                        self.note_lines.len(),
                    );
                }
                Action::None
            }
        }
    }
    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let [tabs, title, meta, body] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .areas(area);
        let tab_style = |selected| {
            if selected {
                bold().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
            } else {
                muted()
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("详情", tab_style(!self.comments_tab)),
                Span::raw("   "),
                Span::styled("评论", tab_style(self.comments_tab)),
            ]))
            .centered(),
            tabs,
        );
        frame.render_widget(
            Paragraph::new(fit(&self.episode.title, area.width as usize)).style(bold()),
            title,
        );
        frame.render_widget(
            Paragraph::new(episode_metadata(&self.episode, area.width)).style(muted()),
            meta,
        );
        if self.comments_tab {
            self.draw_comments(frame, body);
        } else {
            self.draw_notes(frame, body);
        }
    }

    fn draw_notes(&mut self, frame: &mut Frame, area: Rect) {
        let note_block = Block::default().borders(Borders::TOP).border_style(muted());
        let note_area = note_block.inner(area);
        frame.render_widget(note_block, area);
        let message = self.load.message();
        self.note_height = note_area
            .height
            .saturating_sub(u16::from(!message.is_empty())) as usize;
        if self.note_width != note_area.width {
            self.note_width = note_area.width;
            let source = self
                .episode
                .shownotes
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or(self.episode.description.as_deref())
                .unwrap_or("");
            self.note_lines = markdown::render(source, note_area.width);
            if self.note_lines.is_empty() {
                self.note_lines.push(Line::styled("暂无简介", muted()));
            }
        }
        self.note_scroll = self
            .note_scroll
            .min(self.note_lines.len().saturating_sub(self.note_height));
        let mut visible: Vec<_> = self
            .note_lines
            .iter()
            .skip(self.note_scroll)
            .take(self.note_height)
            .cloned()
            .collect();
        if !message.is_empty() && note_area.height > 0 {
            visible.resize(self.note_height, Line::default());
            visible.push(Line::styled(message, Style::default().fg(Color::Yellow)));
        }
        frame.render_widget(Paragraph::new(visible), note_area);
    }

    fn draw_comments(&mut self, frame: &mut Frame, area: Rect) {
        let comment_block = Block::default()
            .borders(Borders::TOP)
            .title(format!(" 评论（只读）· {} ", self.comments.label(),))
            .border_style(muted());
        let comment_area = comment_block.inner(area);
        frame.render_widget(comment_block, area);
        let message = self.comments.load.message();
        let content_height = comment_area
            .height
            .saturating_sub(u16::from(!message.is_empty()));
        self.comment_height = content_height as usize;
        let mut lines = vec![];
        if let Some(page) = self.comments.current() {
            for comment in &page.data.items {
                let author = comment
                    .author
                    .as_ref()
                    .map(|author| author.nickname.as_str())
                    .unwrap_or("匿名用户");
                lines.push(Line::from(vec![
                    Span::styled(author.to_owned(), bold().fg(Color::Cyan)),
                    Span::styled(
                        format!(
                            " · {} · 赞 {}",
                            timestamp(comment.created_at.as_deref(), false),
                            comment.like_count
                        ),
                        muted(),
                    ),
                ]));
                lines.extend(comment.text.lines().map(|line| Line::from(line.to_owned())));
                lines.push(Line::default());
            }
        }
        if lines.is_empty() && message.is_empty() {
            lines.push(Line::styled("暂无评论", muted()));
        }
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        self.comment_line_count = paragraph.line_count(comment_area.width);
        let offset = self
            .comments
            .current_mut()
            .map(|page| {
                page.scroll = page
                    .scroll
                    .min(self.comment_line_count.saturating_sub(self.comment_height));
                page.scroll
            })
            .unwrap_or(0);
        frame.render_widget(
            paragraph.scroll((offset.min(u16::MAX as usize) as u16, 0)),
            Rect {
                height: content_height,
                ..comment_area
            },
        );
        if !message.is_empty() && comment_area.height > 0 {
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(Color::Yellow)),
                Rect::new(
                    comment_area.x,
                    comment_area.bottom() - 1,
                    comment_area.width,
                    1,
                ),
            );
        }
    }
}

fn scroll(position: &mut usize, key: KeyCode, height: usize, count: usize) {
    let max = count.saturating_sub(height);
    *position = match key {
        KeyCode::Down | KeyCode::Char('j') => position.saturating_add(1).min(max),
        KeyCode::Up | KeyCode::Char('k') => position.saturating_sub(1),
        KeyCode::PageDown => position.saturating_add(height.max(1)).min(max),
        KeyCode::PageUp => position.saturating_sub(height.max(1)),
        KeyCode::Home | KeyCode::Char('g') => 0,
        KeyCode::End | KeyCode::Char('G') => max,
        _ => *position,
    };
}
fn podcast(episode: &Episode) -> &str {
    episode
        .podcast
        .as_ref()
        .map(|podcast| podcast.title.as_str())
        .filter(|title| !title.is_empty())
        .unwrap_or("未知播客")
}
fn episode_metadata(episode: &Episode, width: u16) -> String {
    let duration = episode
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "--:--".into());
    let details = format!(
        "{duration} · {}",
        timestamp(episode.pub_date.as_deref(), width < 38),
    );
    let details = fit(&details, usize::from(width));
    let available = usize::from(width).saturating_sub(Line::from(details.as_str()).width());
    let name = fit(podcast(episode), available.saturating_sub(2));
    let gap = available.saturating_sub(Line::from(name.as_str()).width());
    format!("{name}{}{details}", " ".repeat(gap))
}
fn timestamp(value: Option<&str>, short: bool) -> String {
    let Some(value) = value.and_then(|s| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    }) else {
        return "时间未知".into();
    };
    let value = value.to_offset(time::UtcOffset::from_hms(8, 0, 0).unwrap());
    if short {
        format!(
            "{:04}-{:02}-{:02}",
            value.year(),
            u8::from(value.month()),
            value.day()
        )
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            value.year(),
            u8::from(value.month()),
            value.day(),
            value.hour(),
            value.minute()
        )
    }
}
fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    use std::time::Duration;

    fn episode(eid: &str) -> Episode {
        serde_json::from_value(json!({"eid":eid,"title":format!("音频标题 {eid}"),"duration":3661,"pubDate":"2026-09-14T17:30:00Z","podcast":{"title":"播客名称"},"shownotes":"# 音频简介\n\n**重点**\n\n- 第一项\n- 第二项"})).unwrap()
    }
    fn page(ids: &[&str], cursor: Option<Value>) -> Page<Episode> {
        Page {
            items: ids.iter().map(|id| episode(id)).collect(),
            cursor,
        }
    }
    fn rendered(list: &mut Subscriptions, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| list.draw(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| {
                let mut text = String::new();
                let mut x = 0;
                while x < row.len() {
                    let symbol = row[x].symbol();
                    text.push_str(symbol);
                    x += Line::from(symbol).width().max(1);
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn pagination_caches_selection_and_retries_failed_next_page_without_losing_content() {
        let mut list = Subscriptions::default();
        assert_eq!(list.enter().len(), 1);
        let cursor = json!({"id":"next"});
        list.apply(Response::Feed(Ok(page(&["a", "b"], Some(cursor.clone())))));
        list.key(KeyCode::Down);
        assert!(
            matches!(list.key(KeyCode::PageDown), Action::Load(requests) if matches!(&requests[0],Request::Feed(Some(c)) if c == &cursor))
        );
        assert!(matches!(list.key(KeyCode::PageDown), Action::None));
        list.apply(Response::Feed(Err(Error::Network)));
        assert_eq!(list.selected().unwrap().eid, "b");
        assert!(
            matches!(list.key(KeyCode::Char('r')),Action::Load(requests) if matches!(&requests[0],Request::Feed(Some(c)) if c == &cursor))
        );
        list.apply(Response::Feed(Ok(page(&["b", "c"], None))));
        assert_eq!(list.selected().unwrap().eid, "c");
        assert_eq!(list.feed.current().unwrap().data.items.len(), 1);
        list.key(KeyCode::PageUp);
        assert_eq!(list.selected().unwrap().eid, "b");
        assert!(matches!(list.key(KeyCode::PageDown), Action::None));
        assert_eq!(list.selected().unwrap().eid, "c");
        assert!(matches!(list.key(KeyCode::PageDown), Action::None));
        assert_eq!(list.feed.index, 1);
    }
    #[test]
    fn refresh_errors_rate_limits_and_cyclic_cursors_preserve_the_page() {
        let mut list = Subscriptions::default();
        list.enter();
        list.apply(Response::Feed(Ok(page(&["a"], Some(json!(1))))));
        list.key(KeyCode::Char('n'));
        list.apply(Response::Feed(Err(Error::RateLimited(
            Duration::from_secs(30),
        ))));
        assert!(matches!(list.key(KeyCode::Char('r')), Action::None));
        assert_eq!(list.selected().unwrap().eid, "a");
        list.feed.load.retry_at = None;
        list.key(KeyCode::Char('r'));
        list.apply(Response::Feed(Ok(page(&["b"], Some(json!(1))))));
        assert!(matches!(list.feed.load.error, Some(Error::InvalidResponse)));
        assert_eq!(list.feed.pages.len(), 1);
    }
    #[test]
    fn list_enter_opens_detail_and_second_enter_plays_while_escape_restores_selection() {
        let mut list = Subscriptions::default();
        list.enter();
        list.apply(Response::Feed(Ok(page(&["a", "b"], None))));
        list.key(KeyCode::Down);
        assert!(matches!(list.key(KeyCode::Char('y')),Action::Add(id) if id == "b"));
        assert!(matches!(list.key(KeyCode::Enter),Action::Load(requests) if requests.len()==2));
        assert!(matches!(list.key(KeyCode::Enter),Action::Play(id) if id == "b"));
        list.apply(Response::Detail("a".into(), Ok(episode("a"))));
        assert_eq!(list.detail.as_ref().unwrap().episode.eid, "b");
        list.key(KeyCode::Esc);
        assert!(list.detail.is_none());
        assert_eq!(list.selected().unwrap().eid, "b");
        assert!(matches!(list.key(KeyCode::Esc), Action::Back));
    }
    #[test]
    fn detail_tabs_show_one_panel_and_preserve_independent_scroll_positions() {
        let mut list = Subscriptions::default();
        list.enter();
        list.apply(Response::Feed(Ok(page(&["a"], None))));
        let text = rendered(&mut list, 80, 24);
        for label in ["音频标题 a", "播客名称", "1:01:01", "2026-09-15 01:30"] {
            assert!(text.contains(label), "{text}");
        }
        list.key(KeyCode::Enter);
        let mut detail = episode("a");
        detail.shownotes = Some(format!("# 音频简介\n\n{}", "- 更多内容\n".repeat(40)));
        list.apply(Response::Detail("a".into(), Ok(detail)));
        let comment: Comment = serde_json::from_value(json!({"id":"c","text":format!("评论正文\n{}", "第二行\n".repeat(40)),"author":{"nickname":"听众甲"},"createdAt":"2026-09-15T02:30:00Z","likeCount":3})).unwrap();
        list.apply(Response::Comments(
            "a".into(),
            Ok(Page {
                items: vec![comment],
                cursor: Some(json!({"id":"next"})),
            }),
        ));
        let text = rendered(&mut list, 80, 24);
        for label in [
            "音频标题 a",
            "播客名称",
            "1:01:01",
            "2026-09-15 01:30",
            "音频简介",
            "详情",
            "评论",
        ] {
            assert!(text.contains(label), "{text}");
        }
        assert!(!text.contains("评论正文"));
        assert_eq!(list.detail.as_ref().unwrap().note_height, 19);
        assert!(matches!(list.key(KeyCode::Char('n')), Action::None));
        list.key(KeyCode::End);
        rendered(&mut list, 80, 24);
        let note_scroll = list.detail.as_ref().unwrap().note_scroll;
        assert!(note_scroll > 0);
        list.key(KeyCode::Tab);
        let text = rendered(&mut list, 80, 24);
        for label in [
            "音频标题 a",
            "播客名称",
            "评论（只读）",
            "听众甲",
            "评论正文",
        ] {
            assert!(text.contains(label), "{text}");
        }
        assert!(!text.contains("音频简介") && !text.contains("更多内容"));
        assert_eq!(list.detail.as_ref().unwrap().comment_height, 19);
        list.key(KeyCode::End);
        rendered(&mut list, 80, 24);
        let comment_scroll = list
            .detail
            .as_ref()
            .unwrap()
            .comments
            .current()
            .unwrap()
            .scroll;
        assert!(comment_scroll > 0);
        list.key(KeyCode::BackTab);
        rendered(&mut list, 80, 24);
        assert_eq!(list.detail.as_ref().unwrap().note_scroll, note_scroll);
        assert!(
            matches!(list.key(KeyCode::Char('r')), Action::Load(requests) if matches!(requests.as_slice(), [Request::Detail(id)] if id == "a"))
        );
        list.key(KeyCode::Tab);
        rendered(&mut list, 80, 24);
        assert_eq!(
            list.detail
                .as_ref()
                .unwrap()
                .comments
                .current()
                .unwrap()
                .scroll,
            comment_scroll
        );
        assert!(
            matches!(list.key(KeyCode::Char('n')),Action::Load(requests) if matches!(&requests[0],Request::Comments(id,Some(_)) if id=="a"))
        );
        list.apply(Response::Comments("a".into(), Err(Error::Network)));
        assert!(
            matches!(list.key(KeyCode::Char('r')), Action::Load(requests) if matches!(requests.as_slice(), [Request::Comments(id, Some(_))] if id == "a"))
        );
        for (width, height) in [(24, 8), (32, 12), (60, 18), (112, 40)] {
            rendered(&mut list, width, height);
            list.key(KeyCode::Tab);
            rendered(&mut list, width, height);
        }
    }
    #[test]
    fn timestamps_convert_to_beijing_before_formatting() {
        assert_eq!(
            timestamp(Some("2026-09-14T17:30:00Z"), false),
            "2026-09-15 01:30"
        );
        assert_eq!(
            timestamp(Some("2026-09-15T01:30:00+08:00"), true),
            "2026-09-15"
        );
        assert_eq!(timestamp(Some("bad"), false), "时间未知");
    }
}
