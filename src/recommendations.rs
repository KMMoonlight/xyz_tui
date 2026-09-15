use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    help::Context,
    subscriptions::{Action, Request, Response, Subscriptions},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Hot,
    Trending,
    New,
    Editor,
    ForYou,
}

impl Kind {
    pub const ALL: [Self; 5] = [
        Self::Hot,
        Self::Trending,
        Self::New,
        Self::Editor,
        Self::ForYou,
    ];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|kind| *kind == self).unwrap()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hot => "最热榜",
            Self::Trending => "锋芒榜",
            Self::New => "新星榜",
            Self::Editor => "编辑推荐",
            Self::ForYou => "为你推荐",
        }
    }

    pub fn category(self) -> Option<&'static str> {
        match self {
            Self::Hot => Some("HOT_EPISODES_IN_24_HOURS"),
            Self::Trending => Some("SKYROCKET_EPISODES"),
            Self::New => Some("NEW_STAR_EPISODES"),
            Self::Editor | Self::ForYou => None,
        }
    }
}

pub struct Recommendations {
    selected: Kind,
    lists: [Subscriptions; Kind::ALL.len()],
}

impl Default for Recommendations {
    fn default() -> Self {
        Self {
            selected: Kind::default(),
            lists: Kind::ALL.map(Subscriptions::recommendations),
        }
    }
}

impl Recommendations {
    pub fn enter(&mut self) -> (Kind, Vec<Request>) {
        (self.selected, self.lists[self.selected.index()].enter())
    }

    pub fn help_context(&self) -> Context {
        self.lists[self.selected.index()].help_context()
    }

    pub fn key(&mut self, key: KeyCode) -> (Kind, Action) {
        if !self.lists[self.selected.index()].is_detail() {
            let index = self.selected.index();
            let next = match key {
                KeyCode::Tab => Some((index + 1) % Kind::ALL.len()),
                KeyCode::BackTab => Some((index + Kind::ALL.len() - 1) % Kind::ALL.len()),
                KeyCode::Char(n @ '1'..='5') => Some((n as u8 - b'1') as usize),
                _ => None,
            };
            if let Some(index) = next {
                self.selected = Kind::ALL[index];
                let (kind, requests) = self.enter();
                return (kind, Action::Load(requests));
            }
        }
        (self.selected, self.lists[self.selected.index()].key(key))
    }

    pub fn apply(&mut self, kind: Kind, response: Response) {
        self.lists[kind.index()].apply(response);
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let list = &mut self.lists[self.selected.index()];
        if list.is_detail() || area.width < 24 || area.height < 10 {
            list.draw(frame, area);
            return;
        }
        let tabs = Line::from(
            Kind::ALL
                .into_iter()
                .flat_map(|kind| {
                    let style = if kind == self.selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    [Span::styled(kind.label(), style), Span::raw("   ")]
                })
                .collect::<Vec<_>>(),
        );
        let tabs = if tabs.width() > area.width as usize {
            Line::styled(
                format!(
                    "‹ {} · {}/5 ›",
                    self.selected.label(),
                    self.selected.index() + 1
                ),
                Style::default().fg(Color::Cyan),
            )
        } else {
            tabs
        };
        let [tabs_area, content] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(8)]).areas(area);
        frame.render_widget(Paragraph::new(tabs).centered(), tabs_area);
        list.draw(frame, content);
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
    use serde_json::json;

    fn feed(ids: &[&str], cursor: Option<serde_json::Value>) -> Response {
        Response::Feed(Ok(Page {
            items: ids
                .iter()
                .map(|id| Episode {
                    eid: (*id).into(),
                    title: format!("单集 {id}"),
                    duration: Some(600),
                    shownotes: Some("# 完整简介".into()),
                    ..Default::default()
                })
                .collect(),
            cursor,
        }))
    }
    fn render(lists: &mut Recommendations, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| lists.draw(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "")
    }
    #[test]
    fn categories_keep_selections_and_route_late_responses_to_their_own_lists() {
        let mut lists = Recommendations::default();
        assert_eq!(lists.enter().1.len(), 1);
        lists.key(KeyCode::Tab);
        lists.apply(Kind::Hot, feed(&["hot-a", "hot-b"], None));
        lists.apply(Kind::Trending, feed(&["trending"], None));
        let text = render(&mut lists, 100, 24);
        assert!(text.contains("单集trending") && !text.contains("单集hot-a"));
        lists.key(KeyCode::BackTab);
        lists.key(KeyCode::Down);
        assert!(
            matches!(lists.key(KeyCode::Enter), (Kind::Hot, Action::Load(requests)) if requests.len() == 2)
        );
        assert!(matches!(lists.help_context(), Context::Episode { .. }));
        let detail = render(&mut lists, 100, 24);
        assert!(detail.contains("完整简介") && !detail.contains("评论（只读）"));
        // Detail Tab switches the episode panel without changing recommendation category.
        lists.key(KeyCode::Tab);
        assert_eq!(lists.selected, Kind::Hot);
        let detail = render(&mut lists, 100, 24);
        assert!(!detail.contains("完整简介") && detail.contains("评论（只读）"));
        assert!(!detail.contains("锋芒榜"));
        assert!(
            matches!(lists.key(KeyCode::Enter), (Kind::Hot, Action::Play(id)) if id == "hot-b")
        );
        lists.key(KeyCode::Esc);
        assert!(matches!(lists.key(KeyCode::Char('y')), (_, Action::Add(id)) if id == "hot-b"));
        lists.key(KeyCode::Tab);
        lists.key(KeyCode::BackTab);
        assert!(matches!(lists.key(KeyCode::Char('y')), (_, Action::Add(id)) if id == "hot-b"));
        assert!(lists.enter().1.is_empty());
        assert!(matches!(lists.key(KeyCode::Esc), (_, Action::Back)));
    }
    #[test]
    fn editor_pagination_retry_and_login_keep_other_categories_available() {
        let mut lists = Recommendations::default();
        lists.key(KeyCode::Char('4'));
        lists.apply(Kind::Editor, feed(&["first"], Some(json!("older"))));
        assert!(
            matches!(lists.key(KeyCode::Char('n')), (Kind::Editor, Action::Load(requests)) if matches!(&requests[0], Request::Feed(Some(c)) if c == "older"))
        );
        lists.apply(Kind::Editor, Response::Feed(Err(Error::Network)));
        assert!(render(&mut lists, 80, 24).contains("单集first"));
        assert!(
            matches!(lists.key(KeyCode::Char('r')), (_, Action::Load(requests)) if matches!(&requests[0], Request::Feed(Some(c)) if c == "older"))
        );
        lists.apply(Kind::Editor, feed(&["older"], None));
        lists.key(KeyCode::Char('p'));
        assert!(render(&mut lists, 80, 24).contains("单集first"));
        lists.key(KeyCode::Char('r'));
        lists.apply(
            Kind::Editor,
            Response::Feed(Err(Error::Http(reqwest::StatusCode::UNAUTHORIZED))),
        );
        assert!(matches!(lists.key(KeyCode::Enter), (_, Action::Login)));
        lists.key(KeyCode::Char('5'));
        lists.apply(Kind::ForYou, feed(&["personal"], None));
        for (width, height) in [(24, 8), (32, 12), (60, 18), (112, 40)] {
            let text = render(&mut lists, width, height);
            assert!(text.contains("为你推荐"), "{text}");
        }
    }
}
