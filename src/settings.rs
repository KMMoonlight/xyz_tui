use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::Paragraph,
};

use crate::{
    help::Context,
    subscriptions::{Action, Load, Request, Response, Subscriptions},
};

pub struct Settings {
    history: Subscriptions,
    seconds: Option<u64>,
    stats: Load,
    logout_selected: bool,
    logout_failed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            history: Subscriptions::history(),
            seconds: None,
            stats: Load::default(),
            logout_selected: false,
            logout_failed: false,
        }
    }
}

impl Settings {
    pub fn enter(&mut self) -> Vec<Request> {
        let mut requests = self.history.enter();
        if !self.stats.requested {
            self.stats.begin();
            requests.push(Request::ListeningTime);
        }
        requests
    }

    pub fn help_context(&self) -> Context {
        match self.history.help_context() {
            Context::Settings { needs_login, .. } => Context::Settings {
                needs_login: needs_login || self.stats.needs_login(),
                logout_selected: self.logout_selected,
            },
            detail => detail,
        }
    }

    pub fn key(&mut self, key: KeyCode) -> Action {
        if self.history.is_detail() {
            return self.history.key(key);
        }
        match key {
            KeyCode::Tab | KeyCode::BackTab => {
                self.logout_selected = !self.logout_selected;
                Action::None
            }
            KeyCode::Esc | KeyCode::Backspace => {
                self.logout_failed = false;
                Action::Back
            }
            KeyCode::Enter if self.logout_selected => Action::Logout,
            KeyCode::Enter if self.stats.needs_login() => Action::Login,
            KeyCode::Char('r') => {
                let mut requests = match self.history.key(key) {
                    Action::Load(requests) => requests,
                    _ => vec![],
                };
                if self.stats.ready() {
                    self.stats.begin();
                    requests.push(Request::ListeningTime);
                }
                Action::Load(requests)
            }
            _ if self.logout_selected => Action::None,
            _ => self.history.key(key),
        }
    }

    pub fn apply(&mut self, response: Response) {
        if let Response::ListeningTime(result) = response {
            match result {
                Ok(seconds) => {
                    self.seconds = Some(seconds);
                    self.stats.finish(None);
                }
                Err(error) => self.stats.finish(Some(error)),
            }
        } else {
            self.history.apply(response);
        }
    }

    pub fn logout_failed(&mut self) {
        self.logout_failed = true;
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if self.history.is_detail() {
            self.history.draw(frame, area);
            return;
        }
        let message = self.stats.message();
        let [heading, duration, status, history] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(u16::from(!message.is_empty())),
            Constraint::Min(0),
        ])
        .areas(area);
        let logout = Rect::new(
            heading.right().saturating_sub(8.min(heading.width)),
            heading.y,
            8.min(heading.width),
            heading.height,
        );
        frame.render_widget(
            Paragraph::new("设置")
                .centered()
                .style(Style::default().add_modifier(Modifier::BOLD)),
            heading,
        );
        let listening = self.seconds.map_or_else(
            || "累计收听时长 · —".into(),
            |seconds| {
                if area.width < 38 {
                    format!("累计 · {}小时{}分钟", seconds / 3600, seconds % 3600 / 60)
                } else {
                    format!(
                        "累计收听 · {} 小时 {} 分钟",
                        seconds / 3600,
                        seconds % 3600 / 60
                    )
                }
            },
        );
        frame.render_widget(Paragraph::new(listening).centered(), duration);
        frame.render_widget(
            Paragraph::new(format!("收听时长：{message}"))
                .centered()
                .style(Style::default().fg(Color::Yellow)),
            status,
        );
        self.history
            .draw_focused(frame, history, !self.logout_selected);
        let label = if self.logout_failed {
            "退出失败"
        } else {
            "退出登录"
        };
        let style = if self.logout_selected {
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(Paragraph::new(label).centered().style(style), logout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::Error,
        content::{Episode, Page},
    };
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::{Value, json};

    fn feed(ids: &[&str], cursor: Option<Value>) -> Response {
        Response::Feed(Ok(Page {
            items: ids
                .iter()
                .map(|id| {
                    serde_json::from_value::<Episode>(json!({
                        "eid":id,"title":format!("历史单集 {id}"),"duration":3661,
                        "pubDate":"2026-09-14T17:30:00Z","podcast":{"title":"播客名称"}
                    }))
                    .unwrap()
                })
                .collect(),
            cursor,
        }))
    }

    fn render(settings: &mut Settings, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| settings.draw(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(settings: &mut Settings) -> String {
        render(settings, 80, 24)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "")
    }

    #[test]
    fn history_shares_navigation_details_pagination_and_keeps_selection() {
        let mut settings = Settings::default();
        assert!(matches!(
            settings.enter().as_slice(),
            [Request::Feed(None), Request::ListeningTime]
        ));
        settings.apply(feed(&["a", "b"], Some(json!("older"))));
        settings.apply(Response::ListeningTime(Ok(9474419)));
        assert!(text(&mut settings).contains("累计收听·2631小时46分钟"));
        settings.key(KeyCode::Down);
        assert!(matches!(settings.key(KeyCode::Char('y')), Action::Add(id) if id == "b"));
        assert!(
            matches!(settings.key(KeyCode::Enter), Action::Load(r) if matches!(&r[..], [Request::Detail(id), Request::Comments(_, None)] if id == "b"))
        );
        assert!(matches!(settings.help_context(), Context::Episode { .. }));
        settings.key(KeyCode::Tab);
        assert!(text(&mut settings).contains("评论（只读）"));
        assert!(matches!(settings.key(KeyCode::Enter), Action::Play(id) if id == "b"));
        settings.key(KeyCode::Esc);
        assert!(
            matches!(settings.key(KeyCode::PageDown), Action::Load(r) if matches!(&r[..], [Request::Feed(Some(c))] if c == "older"))
        );
        settings.apply(Response::Feed(Err(Error::Network)));
        assert!(text(&mut settings).contains("历史单集b"));
        assert!(
            matches!(settings.key(KeyCode::Char('r')), Action::Load(r) if matches!(&r[..], [Request::Feed(Some(c)), Request::ListeningTime] if c == "older"))
        );
        settings.apply(feed(&["b", "c"], None));
        settings.apply(Response::ListeningTime(Ok(9474419)));
        settings.key(KeyCode::PageUp);
        assert!(matches!(settings.key(KeyCode::Char('y')), Action::Add(id) if id == "b"));
        assert!(matches!(settings.key(KeyCode::Esc), Action::Back));
        assert!(settings.enter().is_empty());
        assert!(matches!(settings.key(KeyCode::Char('y')), Action::Add(id) if id == "b"));
    }

    #[test]
    fn stats_and_history_fail_independently_and_rate_limits_gate_refresh() {
        let mut settings = Settings::default();
        settings.enter();
        settings.apply(Response::ListeningTime(Ok(3600)));
        settings.apply(feed(&["a"], None));
        settings.key(KeyCode::Char('r'));
        settings.apply(Response::ListeningTime(Err(Error::RateLimited(
            std::time::Duration::from_secs(60),
        ))));
        settings.apply(Response::Feed(Err(Error::Network)));
        let body = text(&mut settings);
        assert!(
            body.contains("1小时0分钟") && body.contains("历史单集a") && body.contains("秒后重试")
        );
        assert!(
            matches!(settings.key(KeyCode::Char('r')), Action::Load(r) if matches!(&r[..], [Request::Feed(None)]))
        );
        settings.apply(feed(&[], None));
        assert!(text(&mut settings).contains("暂无收听历史"));
        settings.apply(Response::ListeningTime(Err(Error::Http(
            reqwest::StatusCode::UNAUTHORIZED,
        ))));
        assert!(matches!(settings.key(KeyCode::Enter), Action::Login));
        settings.key(KeyCode::Tab);
        assert!(matches!(settings.key(KeyCode::Enter), Action::Logout));
        settings.logout_failed();
        assert!(text(&mut settings).contains("退出失败"));
        assert!(matches!(settings.key(KeyCode::Enter), Action::Logout));
    }

    #[test]
    fn layout_keeps_two_line_rows_and_only_highlights_the_focused_control() {
        let mut settings = Settings::default();
        settings.enter();
        settings.apply(feed(&["a", "b"], None));
        settings.apply(Response::ListeningTime(Ok(0)));
        for (width, height) in [(24, 12), (32, 16), (80, 24), (112, 40)] {
            let buffer = render(&mut settings, width, height);
            let title = buffer
                .content
                .iter()
                .position(|cell| cell.symbol() == "历" && cell.fg == Color::Cyan)
                .unwrap();
            assert!(matches!(
                buffer.content[title + width as usize].symbol(),
                "播" | "…"
            ));
            assert_eq!(buffer.content[title + width as usize].fg, Color::DarkGray);
            settings.key(KeyCode::Tab);
            let buffer = render(&mut settings, width, height);
            assert!(!buffer.content.iter().any(|cell| cell.fg == Color::Cyan));
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == "退" && cell.fg == Color::Red)
            );
            settings.key(KeyCode::BackTab);
        }
        for (width, height) in [(0, 0), (1, 1), (24, 8)] {
            render(&mut settings, width, height);
        }
    }
}
