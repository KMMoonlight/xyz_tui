use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

#[derive(Clone, Copy)]
pub enum Context {
    Login,
    Menu,
    Player,
    Playlist {
        needs_login: bool,
    },
    Subscriptions {
        needs_login: bool,
    },
    Episode {
        needs_login: bool,
    },
    Settings {
        needs_login: bool,
        logout_selected: bool,
    },
    Recommendations {
        needs_login: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn help_scrolls_to_all_shortcuts_and_clamps_after_resize() {
        let mut help = Help::default();
        help.toggle();
        let context = Context::Episode { needs_login: false };
        let mut small = Terminal::new(TestBackend::new(32, 10)).unwrap();
        small
            .draw(|frame| help.draw(frame, frame.area(), context, false))
            .unwrap();
        let first = small.backend().buffer().clone();
        assert!(help.max_scroll > 0);
        help.key(KeyCode::PageDown);
        assert_eq!(help.scroll, help.height);
        help.key(KeyCode::End);
        small
            .draw(|frame| help.draw(frame, frame.area(), context, false))
            .unwrap();
        assert_eq!(help.scroll, help.max_scroll);
        assert_ne!(small.backend().buffer(), &first);
        let end = small
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "");
        assert!(end.contains("底部"));
        let mut large = Terminal::new(TestBackend::new(100, 50)).unwrap();
        large
            .draw(|frame| help.draw(frame, frame.area(), context, false))
            .unwrap();
        assert_eq!(help.scroll, 0);
        assert_eq!(help.max_scroll, 0);
        for (width, height) in [(0, 0), (1, 1), (2, 2), (8, 4), (24, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| help.draw(frame, frame.area(), context, false))
                .unwrap();
        }
        help.key(KeyCode::Backspace);
        assert!(!help.open);
    }
}

impl Context {
    fn title(self) -> &'static str {
        match self {
            Self::Login => "登录",
            Self::Menu => "主菜单",
            Self::Player => "播放区",
            Self::Playlist { .. } => "播放列表",
            Self::Subscriptions { .. } => "订阅列表",
            Self::Episode { .. } => "单集详情",
            Self::Settings { .. } => "设置",
            Self::Recommendations { .. } => "推荐列表",
        }
    }

    fn shortcuts(self) -> Vec<(&'static str, &'static str)> {
        let mut rows = match self {
            Self::Login => vec![("r", "刷新二维码 / 重试登录"), ("Esc", "退出应用")],
            Self::Menu => vec![
                ("↓ / → / j / l / Tab", "选择下一项"),
                ("↑ / ← / k / h / Shift+Tab", "选择上一项"),
                ("1–4", "选择对应菜单"),
                ("Enter", "进入选中页面"),
            ],
            Self::Player => vec![("Esc / Backspace", "还原底部播放区")],
            Self::Playlist { needs_login } => vec![
                ("↑ / ↓ / k / j", "选择单集"),
                ("PgUp / PgDn", "向前 / 向后移动 10 项"),
                ("Home / g · End / G", "选择列表首项 / 末项"),
                (
                    "Enter",
                    if needs_login {
                        "重新扫码登录"
                    } else {
                        "播放选中单集并移到播放列表首位"
                    },
                ),
                ("x", "从云端播放列表移除选中单集"),
                ("r", "刷新 / 重试"),
            ],
            Self::Subscriptions { needs_login }
            | Self::Recommendations { needs_login }
            | Self::Settings {
                needs_login,
                logout_selected: false,
            } => vec![
                ("↑ / ↓ / k / j", "选择单集"),
                ("PgUp / ← / p / h", "上一页"),
                ("PgDn / → / n / l", "下一页"),
                ("Home / g · End / G", "选择当前页首项 / 末项"),
                (
                    "Enter",
                    if needs_login {
                        "重新扫码登录"
                    } else {
                        "打开单集详情"
                    },
                ),
                ("y", "加入云端播放列表首位 / 重试"),
                ("r", "刷新 / 重试失败页"),
            ],
            Self::Episode { needs_login } => vec![
                (
                    "Enter",
                    if needs_login {
                        "重新扫码登录"
                    } else {
                        "播放单集并置于云端播放列表首位"
                    },
                ),
                ("y", "加入云端播放列表首位 / 重试"),
                ("Tab / Shift+Tab", "切换详情 / 评论标签"),
                ("↑ / ↓ / k / j", "滚动当前标签内容"),
                ("PgUp / PgDn", "向上 / 向下滚动一屏"),
                ("Home / g · End / G", "滚动到当前标签顶部 / 底部"),
                ("← / p · → / n", "评论标签：上一页 / 下一页"),
                ("r", "刷新 / 重试当前标签"),
            ],
            Self::Settings {
                logout_selected: true,
                ..
            } => vec![
                ("Enter", "退出登录 / 重试退出"),
                ("r", "刷新收听时长与历史 / 重试"),
            ],
        };
        if matches!(self, Self::Recommendations { .. }) {
            rows.insert(
                0,
                ("Tab / Shift+Tab · 1–5", "切换榜单 / 编辑推荐 / 为你推荐"),
            );
        }
        if matches!(self, Self::Settings { .. }) {
            rows.insert(0, ("Tab / Shift+Tab", "切换收听历史 / 退出登录"));
        }
        if !matches!(self, Self::Login | Self::Menu | Self::Player) {
            rows.push(("Esc / Backspace", "返回上一级"));
        }
        rows
    }
}

#[derive(Default)]
pub struct Help {
    pub open: bool,
    scroll: u16,
    max_scroll: u16,
    height: u16,
}

impl Help {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.scroll = 0;
    }

    pub fn key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc | KeyCode::Backspace => self.open = false,
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll);
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(self.height).min(self.max_scroll);
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.height),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = self.max_scroll,
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, context: Context, renewal: bool) {
        if !self.open || area.is_empty() {
            return;
        }
        let accent = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let shortcut = |(keys, action): (&'static str, &'static str)| {
            Line::from(vec![
                Span::styled(format!("{keys}  "), accent),
                Span::raw(action),
            ])
        };
        let mut lines = vec![Line::styled("页面操作（关闭帮助后可用）", accent)];
        lines.extend(context.shortcuts().into_iter().map(shortcut));
        if renewal {
            lines.push(shortcut(("r", "重试保存登录凭据")));
        }
        lines.push(Line::default());
        lines.push(Line::styled("全局操作（帮助框内也可用）", accent));
        lines.extend(
            [
                ("Space", "暂停 / 继续；未载入时恢复最近收听，结束后重播"),
                ("a / d", "后退 / 快进 15 秒"),
                ("t", "显示 / 隐藏播放区；播放新单集时自动展开"),
                ("Shift+T", "播放区显示时：全区域展开 / 还原"),
                ("i", "播放区显示时：打开当前音频详情"),
                ("?（Shift+/）", "打开 / 关闭帮助"),
                ("q / Ctrl+C", "退出应用"),
            ]
            .into_iter()
            .map(shortcut),
        );
        lines.push(Line::default());
        lines.push(Line::styled("帮助框操作", accent));
        lines.extend(
            [
                ("Esc / Backspace", "关闭帮助"),
                ("↑ / ↓ / k / j", "滚动帮助列表"),
                ("PgUp / PgDn", "向上 / 向下滚动一屏"),
                ("Home / g · End / G", "帮助列表顶部 / 底部"),
            ]
            .into_iter()
            .map(shortcut),
        );
        let width = 76.min(area.width.saturating_sub(u16::from(area.width > 4) * 2));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(" 快捷键 · {} ", context.title()))
            .title_bottom(" ? / Esc 关闭 · ↑↓ 滚动 ")
            .padding(Padding::horizontal(u16::from(width >= 8)));
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let content_width = block.inner(Rect::new(0, 0, width, area.height)).width;
        let count = paragraph.line_count(content_width).min(u16::MAX as usize) as u16;
        let height = count.saturating_add(2).min(area.height);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        self.height = block.inner(popup).height;
        self.max_scroll = count.saturating_sub(self.height);
        self.scroll = self.scroll.min(self.max_scroll);
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph.scroll((self.scroll, 0)).block(block), popup);
    }
}
