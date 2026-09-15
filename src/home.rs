use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::Paragraph,
};

use crate::recommendations::Recommendations;
use crate::settings::Settings;
use crate::subscriptions::{self, Source, Subscriptions};
use crate::{auth::Error, content::PlaylistEntry, help::Context, playlist::Playlist};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Menu {
    #[default]
    Playlist,
    Subscriptions,
    Recommendations,
    Settings,
}

impl Menu {
    const ALL: [Self; 4] = [
        Self::Playlist,
        Self::Subscriptions,
        Self::Recommendations,
        Self::Settings,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Playlist => "播放列表",
            Self::Subscriptions => "订阅列表",
            Self::Recommendations => "推荐列表",
            Self::Settings => "设置",
        }
    }
}

pub enum Action {
    None,
    Logout,
    LoadPlaylist,
    Login,
    Play(String),
    PlayAndAdd(String),
    Remove(String),
    Add(String),
    Browse(Source, Vec<subscriptions::Request>),
}

#[derive(Default)]
enum Screen {
    #[default]
    Menu,
    Detail(Menu),
}

#[derive(Default)]
pub struct Home {
    selected: Menu,
    screen: Screen,
    playlist: Playlist,
    subscriptions: Box<Subscriptions>,
    recommendations: Box<Recommendations>,
    settings: Box<Settings>,
}

impl Home {
    pub fn help_context(&self) -> Context {
        match self.screen {
            Screen::Menu => Context::Menu,
            Screen::Detail(Menu::Playlist) => Context::Playlist {
                needs_login: self.playlist.needs_login(),
            },
            Screen::Detail(Menu::Subscriptions) => self.subscriptions.help_context(),
            Screen::Detail(Menu::Settings) => self.settings.help_context(),
            Screen::Detail(Menu::Recommendations) => self.recommendations.help_context(),
        }
    }

    pub fn key(&mut self, key: KeyCode) -> Action {
        if let Screen::Detail(menu) = self.screen {
            if matches!(
                menu,
                Menu::Subscriptions | Menu::Recommendations | Menu::Settings
            ) {
                let (source, action) = if menu == Menu::Recommendations {
                    let (kind, action) = self.recommendations.key(key);
                    (Source::Recommendation(kind), action)
                } else if menu == Menu::Settings {
                    (Source::History, self.settings.key(key))
                } else {
                    (Source::Subscriptions, self.subscriptions.key(key))
                };
                return match action {
                    subscriptions::Action::None => Action::None,
                    subscriptions::Action::Back => {
                        self.screen = Screen::Menu;
                        Action::None
                    }
                    subscriptions::Action::Login => Action::Login,
                    subscriptions::Action::Logout => Action::Logout,
                    subscriptions::Action::Play(eid) => Action::PlayAndAdd(eid),
                    subscriptions::Action::Add(eid) => Action::Add(eid),
                    subscriptions::Action::Load(requests) => Action::Browse(source, requests),
                };
            }
            return match key {
                KeyCode::Esc | KeyCode::Backspace => {
                    self.screen = Screen::Menu;
                    Action::None
                }
                KeyCode::Enter if menu == Menu::Playlist && self.playlist.needs_login() => {
                    Action::Login
                }
                KeyCode::Enter if menu == Menu::Playlist => self
                    .playlist
                    .selected_id()
                    .map(Action::Play)
                    .unwrap_or(Action::None),
                KeyCode::Char('x') if menu == Menu::Playlist => self
                    .playlist
                    .selected_id()
                    .map(Action::Remove)
                    .unwrap_or(Action::None),
                KeyCode::Char('r') if menu == Menu::Playlist && self.playlist.can_reload() => {
                    Action::LoadPlaylist
                }
                key if menu == Menu::Playlist => {
                    self.playlist.key(key);
                    Action::None
                }
                _ => Action::None,
            };
        }

        let index = Menu::ALL
            .iter()
            .position(|menu| *menu == self.selected)
            .unwrap();
        let next = match key {
            KeyCode::Down | KeyCode::Right | KeyCode::Tab | KeyCode::Char('j' | 'l') => {
                Some((index + 1) % Menu::ALL.len())
            }
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab | KeyCode::Char('k' | 'h') => {
                Some((index + Menu::ALL.len() - 1) % Menu::ALL.len())
            }
            KeyCode::Char(number @ '1'..='4') => Some(number as usize - '1' as usize),
            KeyCode::Enter => {
                self.screen = Screen::Detail(self.selected);
                if self.selected == Menu::Playlist && !self.playlist.requested() {
                    return Action::LoadPlaylist;
                }
                if self.selected == Menu::Subscriptions {
                    return Action::Browse(Source::Subscriptions, self.subscriptions.enter());
                }
                if self.selected == Menu::Recommendations {
                    let (kind, requests) = self.recommendations.enter();
                    return Action::Browse(Source::Recommendation(kind), requests);
                }
                if self.selected == Menu::Settings {
                    return Action::Browse(Source::History, self.settings.enter());
                }
                None
            }
            _ => None,
        };
        if let Some(index) = next {
            self.selected = Menu::ALL[index];
        }
        Action::None
    }

    pub fn logout_failed(&mut self) {
        self.settings.logout_failed();
    }

    pub fn loading_playlist(&mut self) {
        self.playlist.begin();
    }

    pub fn apply_playlist(&mut self, result: Result<Vec<PlaylistEntry>, Error>) {
        self.playlist.apply(result);
    }

    pub fn apply_browse(&mut self, source: Source, response: subscriptions::Response) {
        match source {
            Source::Recommendation(kind) => self.recommendations.apply(kind, response),
            Source::Subscriptions => self.subscriptions.apply(response),
            Source::History => self.settings.apply(response),
        }
    }

    pub fn progress(&mut self, eid: &str, seconds: f64) {
        self.playlist.set_progress(eid, seconds);
    }

    pub fn remove(&mut self, eid: &str) {
        self.playlist.remove(eid);
    }

    pub fn next_after(&self, eid: &str) -> Option<String> {
        self.playlist.next_after(eid)
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.width < 24 || area.height < 8 {
            frame.render_widget(Paragraph::new("请放大终端窗口").centered(), area);
            return;
        }
        match self.screen {
            Screen::Menu => self.draw_menu(frame, area),
            Screen::Detail(Menu::Playlist) => {
                let content = centered(
                    area,
                    96.min(area.width.saturating_sub(2)),
                    area.height.saturating_sub(2).min(24),
                );
                self.playlist.draw(frame, content);
            }
            Screen::Detail(
                menu @ (Menu::Subscriptions | Menu::Recommendations | Menu::Settings),
            ) => {
                let content = centered(
                    area,
                    112.min(area.width.saturating_sub(2)),
                    area.height.saturating_sub(2),
                );
                if menu == Menu::Recommendations {
                    self.recommendations.draw(frame, content);
                } else if menu == Menu::Settings {
                    self.settings.draw(frame, content);
                } else {
                    self.subscriptions.draw(frame, content);
                }
            }
        }
    }

    fn draw_menu(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::with_capacity(Menu::ALL.len());
        for menu in Menu::ALL {
            let style = if menu == self.selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
            };
            lines.push(Line::styled(menu.label(), style));
        }
        let content = Text::from(lines);
        let menu_area = centered(area, content.width() as u16, content.height() as u16);
        frame.render_widget(Paragraph::new(content), menu_area);
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
