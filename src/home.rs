use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
    widgets::Paragraph,
};

use crate::{auth::Error, content::PlaylistEntry, playlist::Playlist};

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
    Remove(String),
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
    logout_failed: bool,
    playlist: Playlist,
}

impl Home {
    pub fn key(&mut self, key: KeyCode) -> Action {
        if let Screen::Detail(menu) = self.screen {
            return match key {
                KeyCode::Esc | KeyCode::Backspace => {
                    self.screen = Screen::Menu;
                    self.logout_failed = false;
                    Action::None
                }
                KeyCode::Enter if menu == Menu::Settings => Action::Logout,
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
                None
            }
            _ => None,
        };
        if let Some(index) = next {
            self.selected = Menu::ALL[index];
            self.logout_failed = false;
        }
        Action::None
    }

    pub fn logout_failed(&mut self) {
        self.logout_failed = true;
    }

    pub fn loading_playlist(&mut self) {
        self.playlist.begin();
    }

    pub fn apply_playlist(&mut self, result: Result<Vec<PlaylistEntry>, Error>) {
        self.playlist.apply(result);
    }

    pub fn progress(&mut self, eid: &str, seconds: f64) {
        self.playlist.set_progress(eid, seconds);
    }

    pub fn remove(&mut self, eid: &str) {
        self.playlist.remove(eid);
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if area.width < 24 || area.height < 8 {
            frame.render_widget(Paragraph::new("请放大终端窗口").centered(), area);
            return;
        }
        match self.screen {
            Screen::Menu => self.draw_menu(frame, area),
            Screen::Detail(Menu::Settings) => self.draw_settings(frame, area),
            Screen::Detail(Menu::Playlist) => {
                let content = centered(
                    area,
                    96.min(area.width.saturating_sub(2)),
                    area.height.saturating_sub(2).min(24),
                );
                self.playlist.draw(frame, content);
            }
            Screen::Detail(menu) => {
                let content = centered(area, 72, area.height.saturating_sub(2).min(20));
                text(
                    frame,
                    row(content, 0),
                    menu.label(),
                    Style::default().add_modifier(Modifier::BOLD),
                );
                text(frame, row(content, content.height - 1), "Esc 返回", muted());
            }
        }
    }

    fn draw_menu(&self, frame: &mut Frame, area: Rect) {
        let menu_area = centered(area, 28, 6);
        for (index, menu) in Menu::ALL.into_iter().enumerate() {
            let style = if menu == self.selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
            };
            text(frame, row(menu_area, index as u16), menu.label(), style);
        }
        text(frame, row(menu_area, 5), "Enter 进入 · q 退出", muted());
    }

    fn draw_settings(&self, frame: &mut Frame, area: Rect) {
        let content = centered(area, 28, 5);
        text(frame, row(content, 0), "设置", muted());
        text(
            frame,
            row(content, 2),
            "退出登录",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::UNDERLINED),
        );
        let hint = if self.logout_failed {
            "退出失败 · Enter 重试"
        } else {
            "Enter 确认 · Esc 返回"
        };
        text(frame, row(content, 4), hint, muted());
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

fn row(area: Rect, offset: u16) -> Rect {
    Rect::new(area.x, area.y + offset, area.width, 1)
}

fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn text(frame: &mut Frame, area: Rect, label: &str, style: Style) {
    frame.render_widget(Paragraph::new(Span::styled(label, style)).centered(), area);
}
