//! Terminal text wrapping primitives, lifted from chat-tui.
//!
//! Assumes callers pass UTF-8 byte offsets that come from Rust string
//! boundaries. The wrapper measures grapheme clusters by terminal display width
//! and returns byte ranges so the renderer can slice the original styled spans
//! without inventing its own cursor math.
//!
//! Gotcha: width-zero and too-narrow viewports still consume one grapheme per
//! line. That keeps every caller from reintroducing the classic zero-progress
//! wrapping loop.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Controls how a line chooses its soft wrap point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapMode {
    /// Break at the exact grapheme that would overflow; code blocks use this
    /// so indentation and spacing survive verbatim.
    Grapheme,
    /// Prefer the last whitespace run and drop it; prose uses this.
    WordTrim,
}

/// Width and wrapping behavior for one wrapping pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WrapConfig {
    width: usize,
    mode: WrapMode,
}

impl WrapConfig {
    pub(crate) fn new(width: usize, mode: WrapMode) -> Self {
        Self { width, mode }
    }
}

/// One rendered line as a byte range into the original text plus its display width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WrappedLine {
    start: usize,
    end: usize,
    width: usize,
}

impl WrappedLine {
    pub(crate) fn start(self) -> usize {
        self.start
    }

    pub(crate) fn end(self) -> usize {
        self.end
    }

    fn new(start: usize, end: usize, width: usize) -> Self {
        Self { start, end, width }
    }
}

/// Wraps text into byte ranges, preserving hard newlines and guaranteeing progress.
pub(crate) fn wrap_text(text: &str, config: WrapConfig) -> Vec<WrappedLine> {
    if text.is_empty() {
        return vec![WrappedLine::new(0, 0, 0)];
    }

    let mut lines = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let next = next_line(text, start, config.width, config.mode);
        lines.push(next.line);
        start = next.next_start;

        if next.trailing_hard_break {
            lines.push(WrappedLine::new(start, start, 0));
            break;
        }
    }

    if lines.is_empty() {
        lines.push(WrappedLine::new(0, 0, 0));
    }

    lines
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NextLine {
    line: WrappedLine,
    next_start: usize,
    trailing_hard_break: bool,
}

/// Finds the next wrapped line starting at `start`, using grapheme-width accounting.
fn next_line(text: &str, start: usize, max_width: usize, mode: WrapMode) -> NextLine {
    let mut width = 0;
    let mut end = start;
    let mut last_space: Option<SpaceBreak> = None;
    let slice = &text[start..];

    for (relative, grapheme) in slice.grapheme_indices(true) {
        let byte_index = start + relative;

        if grapheme == "\n" || grapheme == "\r\n" {
            let after_break = byte_index + grapheme.len();
            return NextLine {
                line: WrappedLine::new(start, byte_index, width),
                next_start: after_break,
                trailing_hard_break: after_break == text.len(),
            };
        }

        let grapheme_width = UnicodeWidthStr::width(grapheme);
        let would_overflow = width + grapheme_width > max_width;
        if would_overflow && end > start {
            return soft_break_line(text, start, end, width, last_space, mode);
        }

        let next_end = byte_index + grapheme.len();
        if would_overflow {
            return NextLine {
                line: WrappedLine::new(start, next_end, grapheme_width),
                next_start: next_end,
                trailing_hard_break: false,
            };
        }

        if is_soft_wrap_space(grapheme) {
            last_space = Some(SpaceBreak {
                start: byte_index,
                end: next_end,
                width_before: width,
            });
        }

        width += grapheme_width;
        end = next_end;
    }

    NextLine {
        line: WrappedLine::new(start, text.len(), width),
        next_start: text.len(),
        trailing_hard_break: false,
    }
}

/// Chooses either the last word boundary or the measured overflow point.
fn soft_break_line(
    text: &str,
    start: usize,
    measured_end: usize,
    measured_width: usize,
    last_space: Option<SpaceBreak>,
    mode: WrapMode,
) -> NextLine {
    match (mode, last_space) {
        (WrapMode::WordTrim, Some(space)) if space.start > start => {
            let next_start = trim_soft_break_spaces(text, space.end);
            NextLine {
                line: WrappedLine::new(start, space.start, space.width_before),
                next_start,
                trailing_hard_break: false,
            }
        }
        _ => NextLine {
            line: WrappedLine::new(start, measured_end, measured_width),
            next_start: measured_end,
            trailing_hard_break: false,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpaceBreak {
    start: usize,
    end: usize,
    width_before: usize,
}

/// Treats spaces and tabs as soft wrap points without swallowing hard newlines.
fn is_soft_wrap_space(grapheme: &str) -> bool {
    grapheme
        .chars()
        .all(|ch| ch.is_whitespace() && ch != '\n' && ch != '\r')
}

/// Skips spaces after a word wrap so wrapped lines do not start with padding.
fn trim_soft_break_spaces(text: &str, mut index: usize) -> usize {
    while index < text.len() {
        let Some(grapheme) = text[index..].graphemes(true).next() else {
            break;
        };

        if !is_soft_wrap_space(grapheme) {
            break;
        }

        index += grapheme.len();
    }

    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_grapheme_consumes_even_when_width_is_too_narrow() {
        let lines = wrap_text("界", WrapConfig::new(1, WrapMode::Grapheme));

        assert_eq!(lines, vec![WrappedLine::new(0, "界".len(), 2)]);
    }

    #[test]
    fn word_trim_prefers_spaces_and_drops_wrapped_padding() {
        let text = "alpha beta";
        let lines = wrap_text(text, WrapConfig::new(8, WrapMode::WordTrim));
        let rendered: Vec<&str> = lines
            .iter()
            .map(|line| &text[line.start()..line.end()])
            .collect();

        assert_eq!(rendered, vec!["alpha", "beta"]);
    }

    #[test]
    fn grapheme_mode_keeps_every_character() {
        let text = "ab cd ef";
        let lines = wrap_text(text, WrapConfig::new(3, WrapMode::Grapheme));
        let joined: String = lines
            .iter()
            .map(|line| &text[line.start()..line.end()])
            .collect();

        assert_eq!(joined, text);
    }

    #[test]
    fn hard_newlines_split_lines() {
        let text = "a\nb";
        let lines = wrap_text(text, WrapConfig::new(10, WrapMode::WordTrim));
        let rendered: Vec<&str> = lines
            .iter()
            .map(|line| &text[line.start()..line.end()])
            .collect();

        assert_eq!(rendered, vec!["a", "b"]);
    }
}
