# glow-rs

A small terminal markdown viewer: render a file, page through it with
`less`-style keys, and see every local link as a full path.

The renderer is the one chat-tui uses for assistant messages — pulldown-cmark
events turned into pre-wrapped, styled ratatui lines with box-drawing tables,
`│ `-prefixed blockquotes, and syntax-highlighted fences — lifted into a
standalone binary.

## Usage

```
glow-rs [OPTIONS] <FILE>

Options:
  -w, --width <COLS>  Column width to render at. Omit to follow the terminal
                      width, re-wrapping whenever the window is resized.
  -h, --help          Print help
  -V, --version       Print version
```

- **Width.** Without `-w` the document is wrapped to the terminal width and
  re-wrapped on every resize, keeping your proportional position in the file.
  With `-w N` it is always wrapped to `N` columns; if the terminal is narrower
  than `N` the right edge is clipped (like `less -S`), never re-wrapped.
- **Links.** Relative link and image destinations resolve to absolute paths
  anchored at the document's directory, with `..` collapsed and `%20`-style
  escapes decoded, and are shown after the link text as `text (/full/path)`.
  URLs with a scheme (`https:`, `mailto:`, `file:` …), protocol-relative
  `//host/…` links and bare `#fragment` anchors are left as written.
- **Clickable links.** Link text and the path after it are emitted as OSC 8
  hyperlinks, so in a terminal that supports them (most modern ones) a click,
  or Ctrl/Cmd-click depending on the terminal, opens the URL or the local file
  via `file://`. Mouse capture stays off, so clicking and selecting text keep
  working the way your terminal does them. Fragment-only and `//host` links
  have nothing a terminal could open and are not clickable.
- **Not a terminal?** When stdout is a pipe or file, the rendered text is
  written plainly (no styles, no pager) at `-w` or 80 columns.

## Keys

| Action         | Keys                                         |
|----------------|----------------------------------------------|
| Line down / up | `j` / `k`, `↓` / `↑`, `Enter`, `^E` / `^Y`, `^N` / `^P` |
| Half page      | `J` / `K`, `d` / `u`, `^D` / `^U`             |
| Full page      | `f` / `b`, `Space`, `PgDn` / `PgUp`, `^F` / `^B`, `^V` |
| Top / bottom   | `g` / `G`, `<` / `>`, `Home` / `End`          |
| Quit           | `q`, `Q`, `Esc`, `^C`                         |

The bottom row works like `less`'s prompt: the file name on the first screen,
`:` while paging, `(END)` when the last line is on screen, and `~` on rows
past the end of the file.

## Building

```
cargo build --release
cargo test
cargo clippy --all-targets
```

The binary lands in the workspace `target-dir` set in `.cargo/config.toml`.

## Layout

| File              | Role |
|-------------------|------|
| `src/main.rs`     | CLI (clap), file loading, TTY vs. piped dispatch |
| `src/markdown.rs` | pulldown-cmark → styled, pre-wrapped `ratatui::Text` |
| `src/wrap.rs`     | grapheme-aware width wrapping shared by prose, code and table cells |
| `src/links.rs`    | `LinkResolver`: relative destinations → `Destination` (display text + click URI) |
| `src/document.rs` | `Document`/`DocLine`: rendered lines plus the link regions on each |
| `src/hyperlinks.rs` | OSC 8 painter: re-prints link cells after each frame so the terminal makes them clickable |
| `src/pager.rs`    | terminal ownership, key map, `Viewport` scroll math, resize/reflow |
