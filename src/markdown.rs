use html2text::render::text_renderer::{RichAnnotation, RichDecorator, TaggedLine, TextDecorator};
use pulldown_cmark::{Options, Parser, html};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::content::safe_multiline;

/// CommonMark accepts embedded HTML, which also covers the API's HTML shownotes.
/// The HTML renderer never fetches images, stylesheets or link destinations.
pub fn render(source: &str, width: u16) -> Vec<Line<'static>> {
    let source = safe_multiline(source);
    let mut html = String::new();
    html::push_html(
        &mut html,
        Parser::new_ext(
            &source,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
        ),
    );
    html2text::parse(html.as_bytes())
        .render(usize::from(width.max(4)), TerminalDecorator)
        .into_lines()
        .into_iter()
        .map(|line| {
            Line::from(
                line.tagged_strings()
                    .map(|text| {
                        let mut style = Style::default();
                        for annotation in &text.tag {
                            style = match annotation {
                                RichAnnotation::Strong => style.add_modifier(Modifier::BOLD),
                                RichAnnotation::Emphasis => style.add_modifier(Modifier::ITALIC),
                                RichAnnotation::Strikeout => {
                                    style.add_modifier(Modifier::CROSSED_OUT)
                                }
                                RichAnnotation::Code | RichAnnotation::Preformat(_) => {
                                    style.fg(Color::Yellow)
                                }
                                RichAnnotation::Link(_) => {
                                    style.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
                                }
                                _ => style,
                            };
                        }
                        Span::styled(safe_multiline(&text.s), style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

// Keep semantic annotations without the plain-text asterisks and backticks.
struct TerminalDecorator;
impl TextDecorator for TerminalDecorator {
    type Annotation = RichAnnotation;
    fn decorate_link_start(&mut self, url: &str) -> (String, RichAnnotation) {
        RichDecorator::new().decorate_link_start(url)
    }
    fn decorate_link_end(&mut self) -> String {
        String::new()
    }
    fn decorate_em_start(&mut self) -> (String, RichAnnotation) {
        (String::new(), RichAnnotation::Emphasis)
    }
    fn decorate_em_end(&mut self) -> String {
        String::new()
    }
    fn decorate_strong_start(&mut self) -> (String, RichAnnotation) {
        (String::new(), RichAnnotation::Strong)
    }
    fn decorate_strong_end(&mut self) -> String {
        String::new()
    }
    fn decorate_strikeout_start(&mut self) -> (String, RichAnnotation) {
        (String::new(), RichAnnotation::Strikeout)
    }
    fn decorate_strikeout_end(&mut self) -> String {
        String::new()
    }
    fn decorate_code_start(&mut self) -> (String, RichAnnotation) {
        (String::new(), RichAnnotation::Code)
    }
    fn decorate_code_end(&mut self) -> String {
        String::new()
    }
    fn decorate_preformat_first(&mut self) -> RichAnnotation {
        RichAnnotation::Preformat(false)
    }
    fn decorate_preformat_cont(&mut self) -> RichAnnotation {
        RichAnnotation::Preformat(true)
    }
    fn decorate_image(&mut self, src: &str, title: &str) -> (String, RichAnnotation) {
        RichDecorator::new().decorate_image(src, title)
    }
    fn header_prefix(&mut self, level: usize) -> String {
        RichDecorator::new().header_prefix(level)
    }
    fn quote_prefix(&mut self) -> String {
        "│ ".into()
    }
    fn unordered_item_prefix(&mut self) -> String {
        "• ".into()
    }
    fn ordered_item_prefix(&mut self, i: i64) -> String {
        format!("{i}. ")
    }
    fn make_subblock_decorator(&self) -> Self {
        Self
    }
    fn finalise(&mut self, _links: Vec<String>) -> Vec<TaggedLine<RichAnnotation>> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_html_render_styles_lists_links_and_entities_without_controls() {
        for source in [
            "# 简介\n\n**重点** & 内容\n\n- 第一项\n- [链接](https://example.com)\n\n`代码`",
            "<h1>简介</h1><p><strong>重点</strong> &amp; 内容</p><ul><li>第一项</li><li><a href='https://example.com'>链接</a></li></ul><code>代码</code>",
        ] {
            let lines = render(source, 30);
            let plain = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(plain.contains("简介"));
            assert!(plain.contains("& 内容"));
            assert!(plain.contains("第一项"));
            assert!(!plain.contains("*重点*"));
            assert!(!plain.contains("`代码`"));
            assert!(!plain.contains("<strong>"));
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| span.content.contains("重点")
                        && span.style.add_modifier.contains(Modifier::BOLD))
            );
            assert!(lines.iter().flat_map(|line| &line.spans).any(|span| span.content.contains("链接") && span.style.fg == Some(Color::Cyan)));
        }
        let plain = render("<p>&#27;[31m正文</p><script>hidden()</script>", 8)
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(!plain.contains('\u{1b}'));
        assert!(!plain.contains("hidden()"));
    }
}
