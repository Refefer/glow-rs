//! The rendered document: styled visual lines plus the clickable link
//! regions laid over each one.
//!
//! Assumes the renderer builds lines out of [`Run`]s and only ever slices,
//! pads or concatenates them, so a run's link target rides along through
//! wrapping and table layout untouched. A [`DocLine`] can only be built from
//! runs, which is what keeps its link column ranges honest: they are measured
//! off the same text the line displays.
//!
//! Gotcha: two links to the same URL are still two links. Runs cut from one
//! markdown link share a single `Rc<str>` target, and regions merge only on
//! pointer identity, so adjacent-but-separate links never fuse.

use std::ops::Range;
use std::rc::Rc;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// A styled run of text and, when it is part of a link, the link's target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    text: String,
    style: Style,
    target: Option<Rc<str>>,
}

impl Run {
    /// A run that may belong to a link; `target` is what clicking it opens.
    pub fn new(text: impl Into<String>, style: Style, target: Option<Rc<str>>) -> Self {
        Self {
            text: text.into(),
            style,
            target,
        }
    }

    /// A run that is not part of any link.
    pub fn plain(text: impl Into<String>, style: Style) -> Self {
        Self::new(text, style, None)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Display width in terminal cells.
    pub fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    /// The same style and target over `range` (byte offsets) of this run's text.
    pub fn slice(&self, range: Range<usize>) -> Self {
        Self {
            text: self.text[range].to_string(),
            style: self.style,
            target: self.target.clone(),
        }
    }
}

/// The columns a link covers on one visual line, and where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRegion {
    pub columns: Range<usize>,
    pub target: Rc<str>,
}

/// One visual line: its styled text and the links laid over it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocLine {
    line: Line<'static>,
    links: Vec<LinkRegion>,
}

impl DocLine {
    /// Lays `runs` out left to right, recording where each link falls.
    ///
    /// Consecutive runs of the same link become one region, so a link whose
    /// text changed style midway is still a single clickable span.
    pub fn from_runs(runs: Vec<Run>) -> Self {
        let mut links: Vec<LinkRegion> = Vec::new();
        let mut spans = Vec::with_capacity(runs.len());
        let mut column = 0;
        for run in runs {
            let width = run.width();
            if let Some(target) = &run.target
                && width > 0
            {
                match links.last_mut() {
                    Some(last)
                        if Rc::ptr_eq(&last.target, target) && last.columns.end == column =>
                    {
                        last.columns.end = column + width;
                    }
                    _ => links.push(LinkRegion {
                        columns: column..column + width,
                        target: Rc::clone(target),
                    }),
                }
            }
            column += width;
            spans.push(Span::styled(run.text, run.style));
        }
        Self {
            line: Line::from(spans),
            links,
        }
    }

    pub fn line(&self) -> &Line<'static> {
        &self.line
    }

    pub fn links(&self) -> &[LinkRegion] {
        &self.links
    }

    /// The line's text with styling dropped.
    pub fn text(&self) -> String {
        self.line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}

/// A rendered document as the pager scrolls over it: one entry per visual
/// line, already wrapped to the width it was rendered at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    lines: Vec<DocLine>,
}

impl Document {
    pub fn new(lines: Vec<DocLine>) -> Self {
        Self { lines }
    }

    pub fn lines(&self) -> &[DocLine] {
        &self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(url: &str) -> Rc<str> {
        Rc::from(url)
    }

    #[test]
    fn regions_measure_columns_off_the_displayed_text() {
        let target = link("https://example.com");
        let line = DocLine::from_runs(vec![
            Run::plain("see ", Style::default()),
            Run::new("wide 界", Style::default(), Some(Rc::clone(&target))),
            Run::plain(" after", Style::default()),
        ]);

        assert_eq!(line.text(), "see wide 界 after");
        assert_eq!(
            line.links(),
            [LinkRegion {
                columns: 4..11,
                target
            }]
        );
    }

    #[test]
    fn runs_of_one_link_merge_and_separate_links_do_not() {
        let first = link("https://a.example");
        let second = link("https://a.example");
        let line = DocLine::from_runs(vec![
            Run::new("ab", Style::default(), Some(Rc::clone(&first))),
            Run::new(
                "cd",
                Style::default().fg(ratatui::style::Color::Red),
                Some(Rc::clone(&first)),
            ),
            Run::new("ef", Style::default(), Some(Rc::clone(&second))),
        ]);

        let columns: Vec<Range<usize>> = line.links().iter().map(|l| l.columns.clone()).collect();
        assert_eq!(columns, [0..4, 4..6]);
    }

    #[test]
    fn empty_runs_add_no_regions_and_slices_keep_the_target() {
        let target = link("file:///x");
        let run = Run::new("hello", Style::default(), Some(Rc::clone(&target)));
        let line = DocLine::from_runs(vec![run.slice(0..0), run.slice(1..3)]);

        assert_eq!(line.text(), "el");
        assert_eq!(line.links().len(), 1);
        assert_eq!(line.links()[0].columns, 0..2);
    }
}
