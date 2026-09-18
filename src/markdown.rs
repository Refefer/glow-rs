//! Markdown-to-ratatui rendering, lifted from chat-tui's assistant-message
//! renderer.
//!
//! Assumes CommonMark with GitHub-flavored tables, strikethrough and ASCII
//! grid tables. The renderer favors readable terminal output: width-aware
//! line wrapping, box-drawing borders for tables, `│ `-prefixed blockquotes,
//! styled headings, syntax-highlighted fences, and link/image destinations
//! appended after their text — with local destinations resolved to full paths
//! by the supplied [`LinkResolver`]. Link text and the appended destination
//! both carry the link's URI, so the pager can make them clickable.
//!
//! Gotcha: lines are pre-wrapped to the supplied width before returning so
//! `document.len()` is the visual line count the pager scrolls over.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use synoptic::{Highlighter, TokOpt, from_extension};
use unicode_width::UnicodeWidthStr;

use crate::document::{DocLine, Document, Run};
use crate::links::LinkResolver;
use crate::wrap::{WrapConfig, WrapMode, wrap_text};

/// Tab width used when highlighting code blocks (purely affects synoptic's
/// internal column tracking, not the rendered output).
const CODE_TAB_WIDTH: usize = 4;

/// Style definitions for markdown elements.
struct MarkdownStyle {
    bold: Style,
    italic: Style,
    inline_code: Style,
    code_block: Style,
    h1: Style,
    h2: Style,
    h3: Style,
    h4_h6: Style,
    bullet: Style,
    blockquote: Style,
    link: Style,
    strikethrough: Style,
    rule: Style,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            bold: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            italic: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::ITALIC),
            inline_code: Style::default().fg(Color::Yellow),
            code_block: Style::default().fg(Color::Yellow),
            h1: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            h2: Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            h3: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            h4_h6: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
            bullet: Style::default().fg(Color::Cyan),
            blockquote: Style::default().fg(Color::DarkGray),
            link: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
            strikethrough: Style::default().add_modifier(Modifier::CROSSED_OUT),
            rule: Style::default().fg(Color::DarkGray),
        }
    }
}

/// A logical (not yet wrapped) line of styled runs.
type Runs = Vec<Run>;

/// A styled cell: a sequence of logical lines, where each logical line is a
/// list of styled runs. Supports `<br>`-induced line breaks within cells.
type StyledCell = Vec<Runs>;

/// A link whose text is still being emitted.
struct OpenLink {
    /// What is appended after the link text, e.g. ` (/docs/a.md)`.
    display: String,
    /// What clicking any of the link's runs opens; `None` when the
    /// destination is not something a terminal can open.
    target: Option<Rc<str>>,
}

/// A run of `text` that belongs to `link` when one is open.
fn run_in(text: impl Into<String>, style: Style, link: Option<&OpenLink>) -> Run {
    Run::new(text, style, link.and_then(|l| l.target.clone()))
}

/// Accumulated table data collected during the event-driven first pass.
struct TableData {
    alignments: Vec<pulldown_cmark::Alignment>,
    header_rows: Vec<Vec<StyledCell>>,
    data_rows: Vec<Vec<StyledCell>>,
    current_row: Vec<StyledCell>,
    current_cell: StyledCell,
    current_cell_line: Runs,
}

impl TableData {
    fn new(alignments: Vec<pulldown_cmark::Alignment>) -> Self {
        Self {
            alignments,
            header_rows: Vec::new(),
            data_rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: Vec::new(),
            current_cell_line: Vec::new(),
        }
    }
}

/// Renders markdown to a [`Document`] pre-wrapped to `width` cells.
///
/// Fenced code blocks with a recognized language are syntax-highlighted.
/// Link and image destinations go through `links`, so relative paths come out
/// absolute. `width` is clamped to a minimum of 1 cell.
pub fn render_markdown(content: &str, width: usize, links: &LinkResolver) -> Document {
    let width = width.max(1);
    let content = preprocess_grid_tables(content);
    let styles = MarkdownStyle::default();
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(&content, options);

    let mut lines: Vec<Runs> = Vec::new();
    let mut current_spans: Runs = Vec::new();
    let mut style_stack: Vec<Style> = Vec::new();
    let mut had_paragraph = false;
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();
    let mut in_blockquote = false;

    let mut list_stack: Vec<(bool, u64)> = Vec::new();
    let mut pending_item_prefix: Option<Runs> = None;

    let mut open_link: Option<OpenLink> = None;
    let mut table_data: Option<TableData> = None;

    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) if had_paragraph => {
                flush_line(&mut lines, &mut current_spans);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut lines, &mut current_spans);
                lines.push(Vec::new());
                had_paragraph = true;
            }

            Event::Start(Tag::Heading { level, .. }) => {
                let style = match level {
                    HeadingLevel::H1 => styles.h1,
                    HeadingLevel::H2 => styles.h2,
                    HeadingLevel::H3 => styles.h3,
                    _ => styles.h4_h6,
                };
                let prefix = match level {
                    HeadingLevel::H1 => "# ",
                    HeadingLevel::H2 => "## ",
                    HeadingLevel::H3 => "### ",
                    HeadingLevel::H4 => "#### ",
                    HeadingLevel::H5 => "##### ",
                    HeadingLevel::H6 => "###### ",
                };
                style_stack.push(style);
                current_spans.push(Run::plain(prefix, style));
            }
            Event::End(TagEnd::Heading(_)) => {
                style_stack.pop();
                flush_line(&mut lines, &mut current_spans);
                lines.push(Vec::new());
                had_paragraph = true;
            }

            Event::Start(Tag::Strong) => {
                style_stack.push(styles.bold);
            }
            Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }

            Event::Start(Tag::Emphasis) => {
                style_stack.push(styles.italic);
            }
            Event::End(TagEnd::Emphasis) => {
                style_stack.pop();
            }

            Event::Start(Tag::Strikethrough) => {
                style_stack.push(styles.strikethrough);
            }
            Event::End(TagEnd::Strikethrough) => {
                style_stack.pop();
            }

            Event::Start(Tag::BlockQuote(_)) => {
                in_blockquote = true;
                style_stack.push(styles.blockquote);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                in_blockquote = false;
                style_stack.pop();
                flush_line(&mut lines, &mut current_spans);
            }

            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().map(str::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let code_lines = split_code_lines(&code_buf);
                lines.extend(highlight_code_block(
                    &code_lines,
                    code_lang.as_deref(),
                    width,
                    &styles,
                ));
                code_buf.clear();
                code_lang = None;
                if lines.last().is_some_and(|l| !l.is_empty()) {
                    lines.push(Vec::new());
                }
            }

            Event::Start(Tag::List(start)) => {
                if let Some(n) = start {
                    list_stack.push((true, n));
                } else {
                    list_stack.push((false, 0));
                }
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() && lines.last().is_none_or(|l| !l.is_empty()) {
                    lines.push(Vec::new());
                }
            }

            Event::Start(Tag::Item) => {
                let indent_level = list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(indent_level);
                if let Some((is_ordered, num)) = list_stack.last_mut() {
                    if *is_ordered {
                        let prefix = format!("{}{}. ", indent, num);
                        *num += 1;
                        pending_item_prefix = Some(vec![Run::plain(prefix, styles.bullet)]);
                    } else {
                        let prefix = format!("{}- ", indent);
                        pending_item_prefix = Some(vec![Run::plain(prefix, styles.bullet)]);
                    }
                }
            }
            Event::End(TagEnd::Item) => {
                flush_line(&mut lines, &mut current_spans);
            }

            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. }) => {
                let destination = links.resolve(&dest_url);
                open_link = Some(OpenLink {
                    display: destination.display(),
                    target: destination.uri().map(Rc::from),
                });
                style_stack.push(styles.link);
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                style_stack.pop();
                if let Some(link) = open_link.take() {
                    let effective = effective_style(&style_stack);
                    let url_span = run_in(format!(" ({})", link.display), effective, Some(&link));
                    if let Some(ref mut td) = table_data {
                        td.current_cell_line.push(url_span);
                    } else {
                        if let Some(prefix) = pending_item_prefix.take() {
                            current_spans.extend(prefix);
                        }
                        current_spans.push(url_span);
                    }
                }
            }

            Event::Code(text) => {
                if let Some(ref mut td) = table_data {
                    td.current_cell_line.push(run_in(
                        text.to_string(),
                        styles.inline_code,
                        open_link.as_ref(),
                    ));
                } else {
                    if let Some(prefix) = pending_item_prefix.take() {
                        current_spans.extend(prefix);
                    }
                    current_spans.push(run_in(
                        text.to_string(),
                        styles.inline_code,
                        open_link.as_ref(),
                    ));
                }
            }

            Event::Text(text) if in_code_block => {
                code_buf.push_str(&text);
            }

            Event::Text(text) => {
                let style = effective_style(&style_stack);

                if let Some(ref mut td) = table_data {
                    td.current_cell_line
                        .push(run_in(text.to_string(), style, open_link.as_ref()));
                } else {
                    if let Some(prefix) = pending_item_prefix.take() {
                        current_spans.extend(prefix);
                    }

                    if in_blockquote {
                        for (i, line) in text.split('\n').enumerate() {
                            if i > 0 {
                                flush_line(&mut lines, &mut current_spans);
                            }
                            if i > 0 || current_spans.is_empty() {
                                current_spans.push(Run::plain("│ ", styles.blockquote));
                            }
                            if !line.is_empty() {
                                current_spans.push(run_in(
                                    line.to_string(),
                                    style,
                                    open_link.as_ref(),
                                ));
                            }
                        }
                    } else if let Some(rule_line) = try_render_labeled_rule(&text, width) {
                        flush_line(&mut lines, &mut current_spans);
                        lines.push(rule_line);
                    } else {
                        current_spans.push(run_in(text.to_string(), style, open_link.as_ref()));
                    }
                }
            }

            Event::SoftBreak => {
                if let Some(ref mut td) = table_data {
                    td.current_cell_line
                        .push(run_in(" ", Style::default(), open_link.as_ref()));
                } else {
                    current_spans.push(run_in(" ", Style::default(), open_link.as_ref()));
                }
            }
            Event::HardBreak => {
                if let Some(ref mut td) = table_data {
                    let line = std::mem::take(&mut td.current_cell_line);
                    td.current_cell.push(line);
                } else {
                    flush_line(&mut lines, &mut current_spans);
                }
            }

            Event::Rule => {
                flush_line(&mut lines, &mut current_spans);
                lines.push(vec![Run::plain("─".repeat(width), styles.rule)]);
                lines.push(Vec::new());
            }

            Event::Start(Tag::Table(alignments)) => {
                flush_line(&mut lines, &mut current_spans);
                table_data = Some(TableData::new(alignments));
            }
            Event::End(TagEnd::Table) => {
                if let Some(td) = table_data.take() {
                    let table_lines = render_styled_table(&td, width, styles.rule);
                    lines.extend(table_lines);
                    lines.push(Vec::new());
                    had_paragraph = true;
                }
            }

            Event::Start(Tag::TableHead) => {
                if let Some(ref mut td) = table_data {
                    td.current_row = Vec::new();
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(ref mut td) = table_data {
                    let row = std::mem::take(&mut td.current_row);
                    td.header_rows.push(row);
                }
            }

            Event::Start(Tag::TableRow) => {
                if let Some(ref mut td) = table_data {
                    td.current_row = Vec::new();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(ref mut td) = table_data {
                    let row = std::mem::take(&mut td.current_row);
                    td.data_rows.push(row);
                }
            }

            Event::Start(Tag::TableCell) => {
                if let Some(ref mut td) = table_data {
                    td.current_cell = Vec::new();
                    td.current_cell_line = Vec::new();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(ref mut td) = table_data {
                    let line = std::mem::take(&mut td.current_cell_line);
                    if !line.is_empty() {
                        td.current_cell.push(line);
                    }
                    if td.current_cell.is_empty() {
                        td.current_cell.push(Vec::new());
                    }
                    let cell = std::mem::take(&mut td.current_cell);
                    td.current_row.push(cell);
                }
            }

            Event::InlineHtml(html) => {
                let tag = html.trim().to_lowercase();
                if tag == "<br>" || tag == "<br/>" || tag == "<br />" {
                    if let Some(ref mut td) = table_data {
                        let line = std::mem::take(&mut td.current_cell_line);
                        td.current_cell.push(line);
                    } else {
                        flush_line(&mut lines, &mut current_spans);
                    }
                }
            }

            _ => {}
        }
    }

    if !current_spans.is_empty() {
        flush_line(&mut lines, &mut current_spans);
    }

    while lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }

    let mut wrapped_lines: Vec<DocLine> = Vec::with_capacity(lines.len());
    for line in lines {
        if line.is_empty() {
            wrapped_lines.push(DocLine::default());
        } else {
            wrapped_lines.extend(
                wrap_runs(&line, width, WrapMode::WordTrim)
                    .into_iter()
                    .map(DocLine::from_runs),
            );
        }
    }

    Document::new(wrapped_lines)
}

/// Splits buffered code-block text into lines, dropping the single trailing
/// empty line left by the block's final newline.
fn split_code_lines(code: &str) -> Vec<String> {
    let mut lines: Vec<String> = code.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

thread_local! {
    /// Per-extension highlighter templates. `from_extension` compiles regex
    /// rules — expensive to redo for every code block on every re-render — so
    /// build once per extension and clone the unrun template per use (`run`
    /// mutates per-block state, so each block needs its own copy). `None`
    /// templates (unknown languages) are cached too, to skip repeated lookups.
    static HIGHLIGHTER_TEMPLATES: RefCell<HashMap<String, Option<Highlighter>>> =
        RefCell::new(HashMap::new());
}

/// Returns a fresh runnable highlighter for `ext`, reusing a cached template so
/// the regex rules are compiled at most once per extension per thread.
fn highlighter_for_ext(ext: &str) -> Option<Highlighter> {
    HIGHLIGHTER_TEMPLATES.with(|cache| {
        cache
            .borrow_mut()
            .entry(ext.to_string())
            .or_insert_with(|| from_extension(ext, CODE_TAB_WIDTH))
            .clone()
    })
}

/// Renders one fenced code block to styled, width-wrapped lines.
///
/// When the language resolves to a synoptic highlighter, each token is
/// colored by kind; otherwise every line falls back to the flat `code_block`
/// style. Lines hard-wrap by width (grapheme) so code keeps its indentation
/// and is never reflowed.
fn highlight_code_block(
    code: &[String],
    lang: Option<&str>,
    width: usize,
    styles: &MarkdownStyle,
) -> Vec<Runs> {
    let highlighter = lang.and_then(|l| highlighter_for_ext(&fence_lang_to_ext(l)));

    let mut out: Vec<Runs> = Vec::with_capacity(code.len());
    match highlighter {
        Some(mut hl) => {
            hl.run(code);
            for (y, line) in code.iter().enumerate() {
                let spans = hl
                    .line(y, line)
                    .into_iter()
                    .map(|tok| match tok {
                        TokOpt::Some(text, kind) => {
                            Run::plain(text, code_style(&kind, styles.code_block))
                        }
                        TokOpt::None(text) => Run::plain(text, styles.code_block),
                    })
                    .collect();
                out.extend(wrap_code_line(spans, width));
            }
        }
        None => {
            for line in code {
                let spans = vec![Run::plain(line.clone(), styles.code_block)];
                out.extend(wrap_code_line(spans, width));
            }
        }
    }
    out
}

/// Hard-wraps one code line's spans by width, preserving an empty line as blank.
fn wrap_code_line(runs: Runs, width: usize) -> Vec<Runs> {
    if runs.iter().all(|r| r.text().is_empty()) {
        return vec![Vec::new()];
    }
    wrap_runs(&runs, width, WrapMode::Grapheme)
}

/// Maps a fence info string (a language *name*) to the file *extension*
/// `synoptic::from_extension` keys on. Names that already match an accepted
/// extension (e.g. `bash`, `ruby`, `go`, `json`) pass through unchanged.
fn fence_lang_to_ext(lang: &str) -> String {
    let lang = lang.trim().to_lowercase();
    match lang.as_str() {
        "rust" => "rs",
        "python" => "py",
        "javascript" | "node" => "js",
        "typescript" => "ts",
        "csharp" | "c#" => "cs",
        "kotlin" => "kt",
        "haskell" => "hs",
        "golang" => "go",
        "c++" => "cpp",
        "shell" | "zsh" => "sh",
        "yml" => "yaml",
        other => other,
    }
    .to_string()
}

/// Maps a synoptic token kind to a terminal style, reusing the named-ANSI
/// palette so highlighted code matches the surrounding markdown. Unrecognized
/// kinds (and plain text) fall back to `base` (the flat code color), so an
/// unsupported language — which synoptic tokenizes as one plain run — renders
/// exactly like a flat block.
fn code_style(kind: &str, base: Style) -> Style {
    match kind {
        "keyword" => Style::default().fg(Color::Magenta),
        "string" | "character" => Style::default().fg(Color::Green),
        "comment" => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        "digit" | "boolean" => Style::default().fg(Color::Yellow),
        "function" | "macro" => Style::default().fg(Color::Cyan),
        "struct" | "namespace" | "attribute" | "table" => Style::default().fg(Color::Blue),
        // Markdown-source kinds (synoptic's `md` rule set): a ```markdown
        // fence shows structure, not flat code.
        "heading" => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        "bold" => Style::default().add_modifier(Modifier::BOLD),
        "italic" => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::ITALIC),
        "strikethrough" => Style::default().add_modifier(Modifier::CROSSED_OUT),
        "link" | "image" => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED),
        "quote" => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        "list" => Style::default().fg(Color::Cyan),
        "linebreak" => Style::default().fg(Color::DarkGray),
        "math" => Style::default().fg(Color::Yellow),
        _ => base,
    }
}

/// Checks for `─── Label ───` content and renders it as a width-filling rule.
fn try_render_labeled_rule(text: &str, width: usize) -> Option<Runs> {
    let trimmed = text.trim();
    if !trimmed.starts_with('─') || !trimmed.ends_with('─') {
        return None;
    }
    let start = trimmed.find(|c: char| c != '─')?;
    let end = trimmed.rfind(|c: char| c != '─')? + 1;
    let label = trimmed[start..end].trim();
    if label.is_empty() {
        return None;
    }

    let label_with_space = format!(" {} ", label);
    let label_width = UnicodeWidthStr::width(label_with_space.as_str());
    let remaining = width.saturating_sub(label_width);
    let left_count = remaining / 2;
    let right_count = remaining.saturating_sub(left_count);

    let rule_style = Style::default().fg(Color::DarkGray);
    let label_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    Some(vec![
        Run::plain("─".repeat(left_count), rule_style),
        Run::plain(label_with_space, label_style),
        Run::plain("─".repeat(right_count), rule_style),
    ])
}

/// Pushes the buffered runs as a finished logical line.
fn flush_line(lines: &mut Vec<Runs>, runs: &mut Runs) {
    if runs.is_empty() {
        return;
    }
    lines.push(std::mem::take(runs));
}

/// Combines all stacked styles into one effective style for the next emission.
fn effective_style(stack: &[Style]) -> Style {
    stack
        .iter()
        .fold(Style::default(), |style, next| style.patch(*next))
}

/// Sums the display width of every run in the sequence.
fn visible_width_of_runs(runs: &[Run]) -> usize {
    runs.iter().map(Run::width).sum()
}

/// Builds a styled horizontal table border using box-drawing characters.
fn styled_border_line(
    left: char,
    mid: char,
    right: char,
    fill: char,
    widths: &[usize],
    style: Style,
) -> Runs {
    let mut s = String::new();
    s.push(left);
    for (i, &w) in widths.iter().enumerate() {
        for _ in 0..w + 2 {
            s.push(fill);
        }
        if i + 1 < widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    vec![Run::plain(s, style)]
}

/// Wraps a run sequence to fit within `width` cells, preserving each run's
/// style and link target.
///
/// `mode` selects the wrap strategy: prose uses [`WrapMode::WordTrim`]; code
/// uses [`WrapMode::Grapheme`] so lines hard-wrap by width without reflowing
/// words or dropping indentation.
fn wrap_runs(runs: &[Run], width: usize, mode: WrapMode) -> Vec<Runs> {
    let combined = runs.iter().map(Run::text).collect::<String>();
    let wrapped = wrap_text(&combined, WrapConfig::new(width, mode));

    wrapped
        .into_iter()
        .map(|line| runs_for_range(runs, line.start(), line.end()))
        .collect()
}

/// Slices the original styled spans to the byte range selected by the shared wrapper.
fn runs_for_range(runs: &[Run], range_start: usize, range_end: usize) -> Runs {
    let mut output = Vec::new();
    let mut run_start = 0;

    for run in runs {
        let run_end = run_start + run.text().len();
        let start = range_start.max(run_start);
        let end = range_end.min(run_end);

        if start < end {
            output.push(run.slice(start - run_start..end - run_start));
        }

        run_start = run_end;
    }

    output
}

/// Emits one styled table row with wrapped multi-line cells and column padding.
fn render_styled_row(
    out: &mut Vec<Runs>,
    row: &[StyledCell],
    col_widths: &[usize],
    num_cols: usize,
    border_style: Style,
) {
    let empty_cell: StyledCell = vec![vec![]];

    let mut wrapped_cells: Vec<Vec<Runs>> = Vec::with_capacity(num_cols);
    for i in 0..num_cols {
        let cell = if i < row.len() { &row[i] } else { &empty_cell };
        let mut all_wrapped: Vec<Runs> = Vec::new();
        for logical_line in cell {
            let wrapped = wrap_runs(logical_line, col_widths[i], WrapMode::WordTrim);
            all_wrapped.extend(wrapped);
        }
        if all_wrapped.is_empty() {
            all_wrapped.push(Vec::new());
        }
        wrapped_cells.push(all_wrapped);
    }

    let max_lines = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
    let empty_spans: Runs = Vec::new();

    for line_idx in 0..max_lines {
        let mut line_spans: Runs = vec![Run::plain("│", border_style)];
        for (col, wrapped) in wrapped_cells.iter().enumerate() {
            let cell_line = if line_idx < wrapped.len() {
                &wrapped[line_idx]
            } else {
                &empty_spans
            };
            let w = col_widths[col];
            let content_width = visible_width_of_runs(cell_line);

            line_spans.push(Run::plain(" ", Style::default()));
            line_spans.extend(cell_line.iter().cloned());
            let pad = w.saturating_sub(content_width);
            if pad > 0 {
                line_spans.push(Run::plain(" ".repeat(pad), Style::default()));
            }
            line_spans.push(Run::plain(" ", Style::default()));
            line_spans.push(Run::plain("│", border_style));
        }
        out.push(line_spans);
    }
}

/// Renders a complete styled table with box-drawing borders and per-column wrap.
fn render_styled_table(td: &TableData, available_width: usize, border_style: Style) -> Vec<Runs> {
    let num_cols = td.alignments.len().max(
        td.header_rows
            .iter()
            .chain(td.data_rows.iter())
            .map(|r| r.len())
            .max()
            .unwrap_or(0),
    );
    if num_cols == 0 {
        return Vec::new();
    }

    let all_rows: Vec<&Vec<StyledCell>> =
        td.header_rows.iter().chain(td.data_rows.iter()).collect();
    let mut max_widths = vec![0usize; num_cols];
    for row in &all_rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                let cell_width = cell
                    .iter()
                    .map(|line| visible_width_of_runs(line))
                    .max()
                    .unwrap_or(0);
                max_widths[i] = max_widths[i].max(cell_width);
            }
        }
    }

    let col_widths = calculate_col_widths(&max_widths, num_cols, available_width);

    let mut out_lines: Vec<Runs> = Vec::new();

    out_lines.push(styled_border_line(
        '┌',
        '┬',
        '┐',
        '─',
        &col_widths,
        border_style,
    ));

    for header in &td.header_rows {
        render_styled_row(&mut out_lines, header, &col_widths, num_cols, border_style);
    }

    out_lines.push(styled_border_line(
        '├',
        '┼',
        '┤',
        '─',
        &col_widths,
        border_style,
    ));

    for (i, row) in td.data_rows.iter().enumerate() {
        render_styled_row(&mut out_lines, row, &col_widths, num_cols, border_style);
        if i + 1 < td.data_rows.len() {
            out_lines.push(styled_border_line(
                '├',
                '┼',
                '┤',
                '─',
                &col_widths,
                border_style,
            ));
        }
    }

    out_lines.push(styled_border_line(
        '└',
        '┴',
        '┘',
        '─',
        &col_widths,
        border_style,
    ));

    out_lines
}

/// Recognizes a `+---+` style grid table border row.
fn is_grid_border(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('+') || !trimmed.ends_with('+') || trimmed.len() < 3 {
        return false;
    }
    trimmed[1..trimmed.len() - 1]
        .chars()
        .all(|c| c == '-' || c == '=' || c == '+')
}

/// Recognizes a `|...|` style grid table content row.
fn is_grid_content(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 2
}

/// Splits a `|...|` grid table content row into trimmed cell strings.
fn split_grid_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = &trimmed[1..trimmed.len() - 1];
    inner.split('|').map(|s| s.trim().to_string()).collect()
}

/// Rewrites ASCII grid tables (`+---+`) into GFM markdown so pulldown-cmark sees them.
fn preprocess_grid_tables(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut in_code_fence = false;

    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            result.push(lines[i].to_string());
            i += 1;
            continue;
        }

        if !in_code_fence && is_grid_border(lines[i]) {
            let start = i;
            let mut j = i + 1;
            while j < lines.len() && (is_grid_border(lines[j]) || is_grid_content(lines[j])) {
                j += 1;
            }
            let end = j;

            if end - start >= 3 && is_grid_border(lines[end - 1]) {
                let mut header_cells: Option<Vec<String>> = None;
                let mut data_rows: Vec<Vec<String>> = Vec::new();

                for &line in &lines[start..end] {
                    if is_grid_border(line) {
                        continue;
                    }
                    if is_grid_content(line) {
                        let cells = split_grid_cells(line);
                        if header_cells.is_none() {
                            header_cells = Some(cells);
                        } else {
                            data_rows.push(cells);
                        }
                    }
                }

                if let Some(hdr) = header_cells {
                    let num_cols = hdr.len();

                    if !result.is_empty() && result.last().is_some_and(|l| !l.trim().is_empty()) {
                        result.push(String::new());
                    }

                    result.push(format!("| {} |", hdr.join(" | ")));

                    let sep: Vec<&str> = (0..num_cols).map(|_| "---").collect();
                    result.push(format!("| {} |", sep.join(" | ")));

                    for row in &data_rows {
                        let mut padded = row.clone();
                        while padded.len() < num_cols {
                            padded.push(String::new());
                        }
                        padded.truncate(num_cols);
                        result.push(format!("| {} |", padded.join(" | ")));
                    }

                    result.push(String::new());

                    i = end;
                    continue;
                }
            }
        }

        result.push(lines[i].to_string());
        i += 1;
    }

    result.join("\n")
}

const MIN_COL_WIDTH: usize = 10;

/// Picks column widths that fit the table inside `available_width`, preferring
/// natural widths for narrow columns and shrinking only the widest ones.
fn calculate_col_widths(
    max_widths: &[usize],
    num_cols: usize,
    available_width: usize,
) -> Vec<usize> {
    let overhead = num_cols + 1 + num_cols * 2;
    let usable = available_width.saturating_sub(overhead);

    let mut widths: Vec<usize> = max_widths.iter().map(|&w| w.max(MIN_COL_WIDTH)).collect();
    while widths.len() < num_cols {
        widths.push(MIN_COL_WIDTH);
    }

    let total: usize = widths.iter().sum();
    if total <= usable {
        return widths;
    }

    let min_total = num_cols * MIN_COL_WIDTH;
    if usable <= min_total {
        let per_col = (usable / num_cols).max(1);
        return vec![per_col; num_cols];
    }

    let mut result = widths.clone();
    let mut fixed = vec![false; num_cols];

    loop {
        let unfixed_count = fixed.iter().filter(|&&f| !f).count();
        if unfixed_count == 0 {
            break;
        }

        let fixed_total: usize = (0..num_cols).filter(|&i| fixed[i]).map(|i| result[i]).sum();
        let remaining = usable.saturating_sub(fixed_total);
        let fair_share = remaining / unfixed_count;

        let mut newly_fixed = false;
        for i in 0..num_cols {
            if !fixed[i] && result[i] <= fair_share {
                fixed[i] = true;
                newly_fixed = true;
            }
        }

        if !newly_fixed {
            let fixed_total: usize = (0..num_cols).filter(|&i| fixed[i]).map(|i| result[i]).sum();
            let remaining = usable.saturating_sub(fixed_total);
            let per_col = remaining / unfixed_count;
            let mut extra = remaining % unfixed_count;
            for i in 0..num_cols {
                if !fixed[i] {
                    result[i] = per_col
                        + if extra > 0 {
                            extra -= 1;
                            1
                        } else {
                            0
                        };
                }
            }
            break;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn links() -> LinkResolver {
        LinkResolver::new(PathBuf::from("/docs"))
    }

    fn render(content: &str, width: usize) -> Document {
        render_markdown(content, width, &links())
    }

    fn rendered_text(doc: &Document) -> Vec<String> {
        doc.lines().iter().map(DocLine::text).collect()
    }

    /// Flattens a rendered document into (content, foreground-color) per span.
    fn spans_with_fg(doc: &Document) -> Vec<(String, Option<Color>)> {
        doc.lines()
            .iter()
            .flat_map(|line| {
                line.line()
                    .spans
                    .iter()
                    .map(|span| (span.content.to_string(), span.style.fg))
            })
            .collect()
    }

    /// Every link region in the document as (line index, columns, target).
    fn link_regions(doc: &Document) -> Vec<(usize, std::ops::Range<usize>, String)> {
        doc.lines()
            .iter()
            .enumerate()
            .flat_map(|(i, line)| {
                line.links()
                    .iter()
                    .map(move |l| (i, l.columns.clone(), l.target.to_string()))
            })
            .collect()
    }

    #[test]
    fn lists_render_as_multiple_lines() {
        let text = render("Here are files:\n\n- `Cargo.toml`\n- `README.md`", 80);
        let lines = rendered_text(&text);

        assert!(lines.iter().any(|line| line.contains("Here are files:")));
        assert!(lines.iter().any(|line| line.contains("- Cargo.toml")));
        assert!(lines.iter().any(|line| line.contains("- README.md")));
    }

    #[test]
    fn tables_render_with_box_drawing_borders() {
        let text = render("| Header | Value |\n|---|---|\n| Cell | One |", 80);
        let joined: String = rendered_text(&text).join("\n");

        for corner in ['┌', '┐', '├', '┤', '└', '┘'] {
            assert!(joined.contains(corner), "missing {corner} in {joined}");
        }
        assert!(joined.contains("Header"));
        assert!(joined.contains("Cell"));
    }

    #[test]
    fn blockquote_renders_with_vertical_bar_prefix() {
        let text = render("> quoted text", 80);
        let joined: String = rendered_text(&text).join("\n");

        assert!(joined.contains("│ quoted text"));
    }

    #[test]
    fn grid_tables_are_recognized() {
        let input = "+--------+-------+\n| Name   | Value |\n+========+=======+\n| Apple  | 1     |\n+--------+-------+";
        let text = render(input, 80);
        let joined: String = rendered_text(&text).join("\n");

        assert!(joined.contains("Name"));
        assert!(joined.contains("Apple"));
        assert!(joined.contains('┌'));
    }

    #[test]
    fn labeled_rule_renders_centered() {
        let text = render("─── Section ───", 30);
        let lines = rendered_text(&text);
        let rule = lines
            .iter()
            .find(|l| l.contains("Section"))
            .expect("labeled rule line present");

        assert!(rule.contains('─'));
        assert_eq!(UnicodeWidthStr::width(rule.as_str()), 30);
    }

    #[test]
    fn horizontal_rule_fills_the_width() {
        let text = render("above\n\n---\n\nbelow", 24);
        let lines = rendered_text(&text);

        assert!(lines.contains(&"─".repeat(24)));
    }

    #[test]
    fn long_lines_are_pre_wrapped_to_width() {
        let input = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let text = render(input, 20);
        assert!(text.lines().len() > 1);
        for line in text.lines() {
            let visible = line.text();
            assert!(
                UnicodeWidthStr::width(visible.as_str()) <= 20,
                "line `{}` exceeds 20 cells",
                visible
            );
        }
    }

    #[test]
    fn wide_character_wrapping_makes_progress_in_narrow_width() {
        let text = render("界", 1);
        assert_eq!(rendered_text(&text), vec!["界"]);
    }

    #[test]
    fn zero_width_is_clamped_to_one() {
        let text = render("hello world", 0);
        assert!(!text.lines().is_empty());
    }

    #[test]
    fn relative_links_resolve_to_full_paths() {
        let text = render("see [the guide](guide/intro.md#setup)", 80);
        let joined: String = rendered_text(&text).join("\n");

        assert_eq!(joined, "see the guide (/docs/guide/intro.md#setup)");
    }

    #[test]
    fn remote_links_are_left_alone() {
        let text = render("[example](https://example.com)", 80);
        let joined: String = rendered_text(&text).join("\n");

        assert_eq!(joined, "example (https://example.com)");
    }

    #[test]
    fn images_resolve_like_links() {
        let text = render("![diagram](../img/arch.png)", 80);
        let joined: String = rendered_text(&text).join("\n");

        assert_eq!(joined, "diagram (/img/arch.png)");
    }

    #[test]
    fn links_inside_table_cells_resolve() {
        let text = render("| Doc |\n|---|\n| [a](a.md) |", 80);
        let joined: String = rendered_text(&text).join("\n");

        assert!(joined.contains("a (/docs/a.md)"), "{joined}");
    }

    #[test]
    fn list_item_starting_with_a_link_keeps_its_bullet() {
        let text = render("- [a](a.md)\n- [b](b.md)", 80);
        let lines = rendered_text(&text);

        assert!(lines.contains(&"- a (/docs/a.md)".to_string()), "{lines:?}");
        assert!(lines.contains(&"- b (/docs/b.md)".to_string()), "{lines:?}");
    }

    #[test]
    fn link_text_and_destination_share_one_clickable_region() {
        let text = render("see [the guide](guide/intro.md#setup) now", 80);

        assert_eq!(
            link_regions(&text),
            [(0, 4..42, String::from("file:///docs/guide/intro.md"))]
        );
        assert_eq!(
            rendered_text(&text)[0][4..42],
            *"the guide (/docs/guide/intro.md#setup)"
        );
    }

    #[test]
    fn a_wrapped_link_is_one_target_on_every_line_it_spans() {
        let text = render("[a fairly long link text](https://example.com/x)", 12);
        let regions = link_regions(&text);

        assert!(regions.len() > 1, "{regions:?}");
        assert!(regions.iter().all(|(_, _, t)| t == "https://example.com/x"));
        let lines: Vec<usize> = regions.iter().map(|(i, _, _)| *i).collect();
        assert_eq!(lines, (0..regions.len()).collect::<Vec<_>>());
    }

    #[test]
    fn fragment_only_links_are_shown_but_not_clickable() {
        let text = render("[jump](#setup)", 80);

        assert_eq!(rendered_text(&text), ["jump (#setup)"]);
        assert!(link_regions(&text).is_empty());
    }

    #[test]
    fn links_inside_table_cells_keep_their_columns() {
        let text = render("| Doc |\n|---|\n| [a](a.md) |", 80);
        let regions = link_regions(&text);
        let (row, columns, target) = regions.first().cloned().expect("a link region");

        let line = &rendered_text(&text)[row];
        let cell: String = line
            .chars()
            .skip(columns.start)
            .take(columns.len())
            .collect();
        assert_eq!(cell, "a (/docs/a.md)");
        assert_eq!(target, "file:///docs/a.md");
    }

    #[test]
    fn empty_content_yields_empty_text() {
        let text = render("", 80);
        assert!(text.lines().is_empty());
    }

    #[test]
    fn fenced_rust_block_highlights_keywords() {
        // Info string is a language *name* ("rust"), not the "rs" extension —
        // exercises fence_lang_to_ext as well as the highlighting path.
        let text = render("```rust\nfn main() {}\n```", 40);
        let spans = spans_with_fg(&text);

        // `fn` is a keyword → magenta, distinct from the flat code color.
        assert!(
            spans
                .iter()
                .any(|(c, fg)| c == "fn" && *fg == Some(Color::Magenta)),
            "expected a magenta `fn` keyword span, got {spans:?}"
        );
    }

    #[test]
    fn fence_without_language_renders_plain() {
        let text = render("```\nlet x = 1\n```", 40);
        let spans = spans_with_fg(&text);

        assert!(spans.iter().all(|(_, fg)| *fg != Some(Color::Magenta)));
        assert!(
            spans
                .iter()
                .any(|(c, fg)| c.contains("let x = 1") && *fg == Some(Color::Yellow))
        );
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let text = render("```nonsense-lang\ncode here\n```", 40);
        let spans = spans_with_fg(&text);

        assert!(
            spans
                .iter()
                .any(|(c, fg)| c.contains("code here") && *fg == Some(Color::Yellow))
        );
    }

    #[test]
    fn long_code_line_hard_wraps_without_word_trim() {
        let src = "let data = aaaa bbbb cccc dddd eeee ffff gggg;";
        let text = render(&format!("```\n{src}\n```"), 10);
        let rendered = rendered_text(&text);
        let nonblank: Vec<&String> = rendered.iter().filter(|l| !l.trim().is_empty()).collect();

        assert!(nonblank.len() >= 2, "long line should wrap: {rendered:?}");
        let joined: String = nonblank.iter().map(|s| s.as_str()).collect();
        assert_eq!(joined, src, "every character preserved across wraps");
    }

    #[test]
    fn four_backtick_fence_nests_three_backtick_fences() {
        let doc = "````markdown\n# Title\n```json\n{ \"a\": 1 }\n```\n````";
        let text = render(doc, 60);
        let joined = rendered_text(&text).join("\n");

        assert!(joined.contains("# Title"), "{joined}");
        assert!(joined.contains("```json"), "{joined}");
        assert!(joined.contains("{ \"a\": 1 }"), "{joined}");
    }

    #[test]
    fn narrower_width_produces_more_lines() {
        let doc = "one two three four five six seven eight nine ten eleven twelve";
        assert!(render(doc, 20).lines().len() > render(doc, 200).lines().len());
    }
}
