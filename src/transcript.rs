use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Wrap},
};
use serde::Deserialize;

use crate::{auth::Error, content::plain_text, playlist::fit};

#[derive(Deserialize)]
struct Segment {
    #[serde(rename = "startMs")]
    start_ms: u64,
    text: String,
}

#[derive(Default)]
pub struct Transcript {
    segments: Vec<Segment>,
}

impl Transcript {
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        let mut segments: Vec<Segment> =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)?;
        for segment in &mut segments {
            segment.text = plain_text(&segment.text);
        }
        segments.retain(|segment| !segment.text.is_empty());
        segments.sort_by_key(|segment| segment.start_ms);
        let mut merged: Vec<Segment> = Vec::new();
        for segment in segments {
            if let Some(last) = merged.last_mut()
                && last.start_ms == segment.start_ms
            {
                last.text.push(' ');
                last.text.push_str(&segment.text);
            } else {
                merged.push(segment);
            }
        }
        Ok(Self { segments: merged })
    }

    fn index_at(&self, seconds: f64) -> Option<usize> {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        self.segments
            .partition_point(|segment| segment.start_ms as f64 <= seconds * 1000.0)
            .checked_sub(1)
    }

    pub fn has_current(&self, seconds: f64) -> bool {
        self.index_at(seconds).is_some()
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, seconds: f64) {
        let Some(index) = self.index_at(seconds) else {
            return;
        };
        if area.height < 3 || area.width == 0 {
            return;
        }
        let padding = 2.min(area.width / 4);
        let area = Rect::new(
            area.x + padding,
            area.y,
            area.width - padding * 2,
            area.height,
        );
        let muted = Style::default().fg(Color::DarkGray);
        let mut top = area.y;
        if let Some(previous) = index.checked_sub(1).and_then(|i| self.segments.get(i)) {
            frame.render_widget(
                Paragraph::new(fit(&previous.text, area.width as usize)).style(muted),
                Rect::new(area.x, area.y, area.width, 1),
            );
            top += 1;
        }
        let current = Paragraph::new(self.segments[index].text.as_str())
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .wrap(Wrap { trim: true });
        let height = (current.line_count(area.width).min(u16::MAX as usize) as u16)
            .min((area.bottom() - top).saturating_sub(u16::from(index + 1 < self.segments.len())));
        frame.render_widget(current, Rect::new(area.x, top, area.width, height));
        top += height;
        for next in &self.segments[index + 1..] {
            if top >= area.bottom() {
                break;
            }
            let paragraph = Paragraph::new(next.text.as_str())
                .style(muted)
                .wrap(Wrap { trim: true });
            let height = (paragraph.line_count(area.width).min(u16::MAX as usize) as u16)
                .min(area.bottom() - top);
            frame.render_widget(paragraph, Rect::new(area.x, top, area.width, height));
            top += height;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    #[test]
    fn timestamps_support_forward_backward_and_subsecond_positions() {
        let transcript = Transcript::from_json(
            &serde_json::to_vec(&json!([
                {"startMs": 1500, "text":"second"},
                {"startMs": 500, "text":"first\npart"},
                {"startMs": 500, "text":"continued"},
                {"startMs": 700, "text":" \n "}
            ]))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(transcript.segments.len(), 2);
        assert_eq!(transcript.segments[0].text, "first part continued");
        for (seconds, expected) in [
            (0.0, None),
            (0.5, Some(0)),
            (1.499, Some(0)),
            (1.5, Some(1)),
            (50.0, Some(1)),
            (0.75, Some(0)),
            (-1.0, None),
            (f64::NAN, None),
        ] {
            assert_eq!(transcript.index_at(seconds), expected);
        }
        assert!(Transcript::from_json(br#"[{"startMs":-1,"text":"bad"}]"#).is_err());
        assert!(Transcript::from_json(b"not json").is_err());
        assert!(!Transcript::from_json(b"[]").unwrap().has_current(10.0));
    }

    #[test]
    fn current_sentence_wraps_and_context_is_muted() {
        let text = "当前字幕需要换行显示，保持完整的中文内容。";
        let transcript = Transcript::from_json(
            &serde_json::to_vec(&json!([
                {"startMs":0,"text":"previous"},
                {"startMs":1000,"text":text},
                {"startMs":2000,"text":"next"}
            ]))
            .unwrap(),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(30, 9)).unwrap();
        terminal
            .draw(|frame| transcript.draw(frame, frame.area(), 1.5))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let highlighted = buffer
            .content
            .iter()
            .filter(|cell| cell.fg == Color::Cyan)
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "");
        assert_eq!(highlighted, text);
        assert_eq!(buffer[(2, 0)].fg, Color::DarkGray);
        let last_current = (0..9)
            .rfind(|y| (0..30).any(|x| buffer[(x, *y)].fg == Color::Cyan))
            .unwrap();
        assert_eq!(buffer[(2, last_current + 1)].symbol(), "n");
        assert_eq!(buffer[(2, last_current + 1)].fg, Color::DarkGray);
    }
}
