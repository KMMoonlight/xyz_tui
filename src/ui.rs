use qrcode::{Color as QrColor, EcLevel, QrCode};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

pub fn playback_progress(position: f64, duration: Option<f64>, width: u16) -> Line<'static> {
    let duration = duration.filter(|seconds| seconds.is_finite() && *seconds > 0.0);
    let time = format!(
        "{} / {}",
        crate::playlist::format_duration(position as u64),
        duration
            .map(|seconds| crate::playlist::format_duration(seconds as u64))
            .unwrap_or_else(|| "--:--".into()),
    );
    let bar_width = usize::from(width).saturating_sub(time.len() + 3);
    if bar_width < 4 {
        return Line::raw(time);
    }
    let filled = duration.map_or(0, |duration| {
        ((position / duration).clamp(0.0, 1.0) * bar_width as f64) as usize
    });
    Line::from(vec![
        Span::raw("["),
        Span::styled("#".repeat(filled), Style::default().fg(Color::Cyan)),
        Span::raw("-".repeat(bar_width - filled)),
        Span::raw(format!("] {time}")),
    ])
}

pub struct Code {
    qr: QrCode,
}

impl Code {
    pub fn new(url: &str) -> Result<Self, qrcode::types::QrError> {
        Ok(Self {
            qr: QrCode::with_error_correction_level(url.as_bytes(), EcLevel::L)?,
        })
    }

    pub fn width(&self) -> u16 {
        self.qr.width() as u16 + 8
    }
    pub fn height(&self) -> u16 {
        self.width().div_ceil(2)
    }

    fn dark(&self, x: usize, y: usize) -> bool {
        let size = self.qr.width();
        (4..size + 4).contains(&x)
            && (4..size + 4).contains(&y)
            && self.qr[(x - 4, y - 4)] == QrColor::Dark
    }
}

impl Widget for &Code {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        for y in 0..self.height().min(area.height) {
            for x in 0..self.width().min(area.width) {
                let symbol = match (
                    self.dark(x as usize, y as usize * 2),
                    self.dark(x as usize, y as usize * 2 + 1),
                ) {
                    (true, true) => "█",
                    (true, false) => "▀",
                    (false, true) => "▄",
                    (false, false) => " ",
                };
                buffer[(area.x + x, area.y + y)]
                    .set_symbol(symbol)
                    .set_fg(Color::Black)
                    .set_bg(Color::White);
            }
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, code: Option<&Code>, message: &str) {
    let code =
        code.filter(|code| area.width >= code.width() + 2 && area.height >= code.height() + 2);
    let height = code.map_or(1, |code| code.height() + 2);
    let top = area.y + area.height.saturating_sub(height) / 2;
    let message_y = if let Some(code) = code {
        frame.render_widget(
            code,
            Rect::new(
                area.x + (area.width - code.width()) / 2,
                top,
                code.width(),
                code.height(),
            ),
        );
        top + code.height() + 1
    } else {
        top
    };
    frame.render_widget(
        Paragraph::new(message).centered(),
        Rect::new(area.x, message_y, area.width, 1).intersection(area),
    );
}
