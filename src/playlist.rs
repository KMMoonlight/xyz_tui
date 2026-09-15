use std::time::Instant;

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Cell, Paragraph, Row, Table, TableState},
};
use reqwest::StatusCode;
use unicode_segmentation::UnicodeSegmentation;

use crate::{auth::Error, content::PlaylistEntry};

#[derive(Default)]
pub struct Playlist {
    entries: Vec<PlaylistEntry>,
    state: TableState,
    requested: bool,
    loading: bool,
    error: Option<Error>,
    retry_at: Option<Instant>,
}

impl Playlist {
    pub fn selected_id(&self) -> Option<String> {
        self.state
            .selected()
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.eid.clone())
    }

    pub fn next_after(&self, eid: &str) -> Option<String> {
        let next = self
            .entries
            .iter()
            .position(|entry| entry.eid == eid)
            .map_or(0, |index| index + 1);
        self.entries.get(next).map(|entry| entry.eid.clone())
    }

    pub fn set_progress(&mut self, eid: &str, seconds: f64) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.eid == eid) {
            entry.progress = Some(seconds);
            entry.progress_failed = false;
        }
    }

    pub fn remove(&mut self, eid: &str) {
        let selected = self.state.selected().unwrap_or(0);
        let selected_id = self.selected_id();
        self.entries.retain(|entry| entry.eid != eid);
        self.state.select((!self.entries.is_empty()).then(|| {
            selected_id
                .and_then(|eid| self.entries.iter().position(|entry| entry.eid == eid))
                .unwrap_or_else(|| selected.min(self.entries.len() - 1))
        }));
        self.loading = false;
    }
    pub fn requested(&self) -> bool {
        self.requested
    }

    pub fn can_reload(&self) -> bool {
        !self.loading
            && self
                .retry_at
                .is_none_or(|deadline| Instant::now() >= deadline)
    }

    pub fn needs_login(&self) -> bool {
        matches!(self.error, Some(Error::Http(StatusCode::UNAUTHORIZED)))
    }

    pub fn begin(&mut self) {
        self.requested = true;
        self.loading = true;
        self.error = None;
        self.retry_at = None;
    }

    pub fn apply(&mut self, result: Result<Vec<PlaylistEntry>, Error>) {
        self.loading = false;
        match result {
            Ok(entries) => {
                let selected_id = self
                    .state
                    .selected()
                    .and_then(|index| self.entries.get(index));
                let selected = selected_id
                    .and_then(|old| entries.iter().position(|entry| entry.eid == old.eid))
                    .unwrap_or(0);
                self.state.select((!entries.is_empty()).then_some(selected));
                self.entries = entries;
                self.error = None;
                self.retry_at = None;
            }
            Err(error) => {
                if let Error::RateLimited(delay) = &error {
                    self.retry_at = Instant::now().checked_add(*delay);
                }
                self.error = Some(error);
            }
        }
    }

    pub fn key(&mut self, key: KeyCode) {
        if self.entries.is_empty() {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        let last = self.entries.len() - 1;
        let selected = match key {
            KeyCode::Down | KeyCode::Char('j') => (current + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::PageDown => (current + 10).min(last),
            KeyCode::PageUp => current.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => current,
        };
        self.state.select(Some(selected));
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let muted = Style::default().fg(Color::DarkGray);
        let title = if self.entries.is_empty() {
            "播放列表".to_owned()
        } else {
            format!(
                "播放列表  {}/{}",
                self.state.selected().unwrap_or(0) + 1,
                self.entries.len()
            )
        };
        frame.render_widget(
            Paragraph::new(title)
                .centered()
                .style(Style::default().add_modifier(Modifier::BOLD)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let status = if let Some(error) = &self.error {
            error.to_string()
        } else if self.loading && !self.entries.is_empty() {
            "刷新中…".into()
        } else {
            String::new()
        };
        let status_height = u16::from(!status.is_empty());
        let list_top = area.y + if area.height >= 10 { 2 } else { 1 };
        let list_area = Rect::new(
            area.x,
            list_top,
            area.width,
            area.bottom()
                .saturating_sub(status_height)
                .saturating_sub(list_top),
        );
        if self.entries.is_empty() {
            let message = if self.loading {
                "加载中…"
            } else if self.error.is_some() {
                ""
            } else {
                "暂无单集"
            };
            frame.render_widget(
                Paragraph::new(message).centered().style(muted),
                Rect::new(
                    list_area.x,
                    list_area.y + list_area.height / 2,
                    list_area.width,
                    1,
                ),
            );
        } else {
            let selected = self.state.selected();
            let rows = self.entries.iter().enumerate().map(|(index, entry)| {
                let title = match &entry.episode {
                    Ok(episode) => episode.title.clone(),
                    Err(error) => error.to_string(),
                };
                let style = if selected == Some(index) {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if entry.episode.is_err() {
                    muted
                } else {
                    Style::default()
                };
                Row::new([Cell::from(Text::from(vec![
                    Line::styled(fit(&title, area.width as usize), style),
                    Line::styled(metadata(entry, area.width as usize), muted),
                ]))])
                .height(2)
                .bottom_margin(1)
            });
            frame.render_stateful_widget(
                Table::new(rows, [Constraint::Fill(1)]),
                list_area,
                &mut self.state,
            );
        }
        frame.render_widget(
            Paragraph::new(status).centered().style(muted),
            Rect::new(
                area.x,
                area.bottom().saturating_sub(status_height),
                area.width,
                status_height,
            ),
        );
    }
}

fn metadata(entry: &PlaylistEntry, width: usize) -> String {
    let Ok(episode) = &entry.episode else {
        return String::new();
    };
    let remaining = entry
        .remaining()
        .map(format_duration)
        .unwrap_or_else(|| "--:--".into());
    let remaining = format!("剩余 {remaining}");
    let podcast = episode
        .podcast
        .as_ref()
        .map(|podcast| podcast.title.as_str())
        .unwrap_or_default();
    let podcast_width = width.saturating_sub(Line::from(remaining.as_str()).width() + 3);
    if podcast.is_empty() || podcast_width == 0 {
        remaining
    } else {
        format!("{} · {remaining}", fit(podcast, podcast_width))
    }
}

pub(crate) fn fit(value: &str, width: usize) -> String {
    if Line::from(value).width() <= width {
        return value.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        let next = Line::from(grapheme).width();
        if used + next >= width {
            break;
        }
        output.push_str(grapheme);
        used += next;
    }
    output.push('…');
    output
}

pub(crate) fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{Episode, Podcast};
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Duration;

    fn entry(eid: &str) -> PlaylistEntry {
        PlaylistEntry {
            eid: eid.into(),
            progress: Some(0.0),
            progress_failed: false,
            episode: Ok(Episode {
                eid: eid.into(),
                title: format!("单集 {eid}"),
                duration: Some(65),
                podcast: None,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn next_episode_follows_queue_order_independently_of_selection() {
        let mut list = Playlist::default();
        assert_eq!(list.next_after("a"), None);
        list.apply(Ok(vec![entry("a"), entry("b"), entry("c")]));
        list.key(KeyCode::End);
        assert_eq!(list.next_after("a").as_deref(), Some("b"));
        assert_eq!(list.next_after("b").as_deref(), Some("c"));
        assert_eq!(list.next_after("c"), None);
        list.remove("a");
        assert_eq!(list.next_after("a").as_deref(), Some("b"));
        assert_eq!(list.selected_id().as_deref(), Some("c"));
    }

    #[test]
    fn refresh_retains_selection_by_id_and_errors_preserve_existing_list() {
        let mut list = Playlist::default();
        list.begin();
        assert!(!list.can_reload());
        list.apply(Ok(vec![entry("a"), entry("b")]));
        list.key(KeyCode::Down);
        list.begin();
        list.apply(Ok(vec![entry("b"), entry("a")]));
        assert_eq!(list.state.selected(), Some(0));
        list.begin();
        list.apply(Err(Error::RateLimited(Duration::from_secs(30))));
        assert_eq!(list.entries.len(), 2);
        assert_eq!(list.entries[0].eid, "b");
        assert!(!list.can_reload());
        list.retry_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(list.can_reload());
        list.begin();
        list.apply(Err(Error::Http(StatusCode::UNAUTHORIZED)));
        assert!(list.needs_login());
        list.begin();
        list.apply(Ok(vec![]));
        assert_eq!(list.state.selected(), None);
        assert!(!list.needs_login());
    }

    #[test]
    fn scrolling_keeps_selection_visible_on_a_small_terminal() {
        let mut list = Playlist::default();
        list.apply(Ok((0..30).map(|index| entry(&index.to_string())).collect()));
        list.key(KeyCode::End);
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        terminal
            .draw(|frame| list.draw(frame, frame.area()))
            .unwrap();
        assert!(list.state.offset() > 0);
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("29"));
        list.key(KeyCode::Home);
        terminal
            .draw(|frame| list.draw(frame, frame.area()))
            .unwrap();
        assert_eq!(list.state.offset(), 0);
    }

    #[test]
    fn two_line_entries_keep_remaining_time_visible_and_metadata_muted_when_selected() {
        let mut entry = entry("first");
        entry.progress = Some(5.5);
        let episode = entry.episode.as_mut().unwrap();
        episode.title = "测试音频标题".into();
        episode.podcast = Some(Podcast {
            title: "很长很长的中文播客名称".into(),
        });
        let mut list = Playlist::default();
        list.apply(Ok(vec![entry]));
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        terminal
            .draw(|frame| list.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].symbol(), "测");
        assert_eq!(buffer[(0, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(0, 2)].fg, Color::DarkGray);
        assert!(!buffer[(0, 2)].modifier.contains(Modifier::BOLD));
        let metadata = (0..24).map(|x| buffer[(x, 2)].symbol()).collect::<String>();
        assert!(metadata.contains("1:00"));
        assert!(metadata.contains('…'));
    }

    #[test]
    fn truncated_text_preserves_graphemes_and_terminal_width() {
        let text = "👨‍👩‍👧‍👦播客时间";
        assert_eq!(fit(text, 5), "👨‍👩‍👧‍👦播…");
        assert!(Line::from(fit(text, 5)).width() <= 5);
    }
}
