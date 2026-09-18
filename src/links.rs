//! Resolves markdown link destinations to absolute filesystem paths.
//!
//! Assumes every relative destination is meant relative to the directory of
//! the document being rendered, which is how editors and static-site tools
//! interpret them. Destinations that carry a URL scheme (`https:`, `mailto:`,
//! `file:` …), are protocol-relative (`//host/…`) or are pure fragments
//! (`#section`) are already fully qualified and pass through untouched.
//!
//! Each resolved [`Destination`] has two faces: the text shown after the link
//! and the URI a terminal opens when the link is clicked. Local files become
//! `file://` URIs; fragments and protocol-relative links have nothing a
//! terminal could open, so they are shown but not clickable.
//!
//! Gotcha: resolution is lexical, not filesystem-backed. A link to a file that
//! does not exist still resolves to the absolute path it *would* have, and
//! `..` segments collapse without consulting symlinks. Only the document's own
//! directory is canonicalized, once, when the resolver is built.

use std::path::{Component, Path, PathBuf};

/// Where a link points once resolved against its document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// A file on this machine, as an absolute path, plus the `#fragment` the
    /// link carried (empty when it had none).
    Local { path: PathBuf, fragment: String },
    /// A destination with a URL scheme (`https:`, `mailto:`, `file:` …),
    /// left exactly as written.
    Url(String),
    /// Shown as written but not clickable: a bare `#fragment`, a
    /// protocol-relative `//host/…` link, or an empty destination.
    Opaque(String),
}

impl Destination {
    /// The text shown after the link text: the full path (with fragment) for
    /// local files, the destination as written for everything else.
    pub fn display(&self) -> String {
        match self {
            Destination::Local { path, fragment } => format!("{}{fragment}", path.display()),
            Destination::Url(text) | Destination::Opaque(text) => text.clone(),
        }
    }

    /// The URI for the terminal to open on click, if there is one.
    ///
    /// Local files become `file://` URIs with the path percent-encoded and
    /// the fragment dropped, since file openers do not understand fragments.
    /// URLs keep their own escapes but get any remaining space, control or
    /// non-ASCII byte encoded, since the OSC 8 payload must be printable
    /// ASCII. A URI longer than the 2083-byte limit is not offered at all.
    pub fn uri(&self) -> Option<String> {
        let uri = match self {
            Destination::Local { path, .. } => {
                format!("file://{}", percent_encode_path(&path.to_string_lossy()))
            }
            Destination::Url(url) => percent_encode_unprintable(url),
            Destination::Opaque(_) => return None,
        };
        (uri.len() <= MAX_URI_BYTES).then_some(uri)
    }
}

/// Longest URI the OSC 8 hyperlink convention lets a terminal accept.
const MAX_URI_BYTES: usize = 2083;

/// Knows the directory a document lives in so relative links can be anchored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkResolver {
    base_dir: PathBuf,
}

impl LinkResolver {
    /// Anchors relative links at `base_dir`, which callers should pass absolute.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Builds a resolver for the document at `document`, anchoring relative
    /// links in its parent directory.
    ///
    /// The parent is canonicalized when it exists so symlinked checkouts show
    /// their real location; otherwise it is made absolute against the current
    /// directory, which still yields a full path for a not-yet-created file.
    pub fn for_document(document: &Path) -> std::io::Result<Self> {
        let parent = match document.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let base_dir = match parent.canonicalize() {
            Ok(dir) => dir,
            Err(_) => normalize(&std::env::current_dir()?.join(parent)),
        };
        Ok(Self::new(base_dir))
    }

    /// Classifies one link destination, anchoring local paths at the
    /// document's directory.
    pub fn resolve(&self, dest: &str) -> Destination {
        if dest.is_empty() || dest.starts_with('#') || dest.starts_with("//") {
            return Destination::Opaque(dest.to_string());
        }
        if has_scheme(dest) {
            return Destination::Url(dest.to_string());
        }

        let (path_part, fragment) = match dest.find('#') {
            Some(idx) => (&dest[..idx], &dest[idx..]),
            None => (dest, ""),
        };
        let decoded = percent_decode(path_part);
        let path = Path::new(&decoded);
        let absolute = if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.base_dir.join(path))
        };

        Destination::Local {
            path: absolute,
            fragment: fragment.to_string(),
        }
    }
}

/// Reports whether `dest` opens with an RFC 3986 scheme (`letter (letter |
/// digit | + | - | .)* :`).
///
/// A single letter before the colon is treated as a scheme too; Windows drive
/// letters are not a concern for a Unix-path resolver.
fn has_scheme(dest: &str) -> bool {
    let Some(colon) = dest.find(':') else {
        return false;
    };
    let scheme = &dest[..colon];
    let mut chars = scheme.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Decodes `%XX` escapes so `My%20File.md` resolves to the file on disk.
///
/// Malformed escapes and byte sequences that are not valid UTF-8 leave the
/// input unchanged rather than inventing a path.
fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }

    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| hex_pair(bytes[i + 1], bytes[i + 2]))
            .flatten();
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// Escapes everything in a path that is not an RFC 3986 unreserved character
/// or a `/`, so the path is a valid `file://` URI path component.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Escapes every byte a URL may not carry over the wire: whitespace, control
/// characters and anything outside ASCII. Existing `%XX` escapes are kept.
fn percent_encode_unprintable(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for byte in url.bytes() {
        match byte {
            0x21..=0x7E => out.push(char::from(byte)),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let hi = (hi as char).to_digit(16)?;
    let lo = (lo as char).to_digit(16)?;
    u8::try_from(hi * 16 + lo).ok()
}

/// Collapses `.` and `..` components without touching the filesystem.
///
/// `..` at the root stays at the root, matching how shells resolve `/..`.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut acc, component| {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    let popped = acc.pop();
                    if !popped && acc.as_os_str().is_empty() {
                        acc.push("..");
                    }
                }
                other => acc.push(other.as_os_str()),
            }
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> LinkResolver {
        LinkResolver::new(PathBuf::from("/docs/guide"))
    }

    #[test]
    fn relative_links_anchor_at_the_document_directory() {
        assert_eq!(
            resolver().resolve("intro.md").display(),
            "/docs/guide/intro.md"
        );
        assert_eq!(
            resolver().resolve("./intro.md").display(),
            "/docs/guide/intro.md"
        );
        assert_eq!(
            resolver().resolve("img/a.png").display(),
            "/docs/guide/img/a.png"
        );
    }

    #[test]
    fn parent_segments_collapse() {
        assert_eq!(
            resolver().resolve("../README.md").display(),
            "/docs/README.md"
        );
        assert_eq!(resolver().resolve("../../../../x").display(), "/x");
    }

    #[test]
    fn absolute_paths_are_normalized_not_rebased() {
        assert_eq!(resolver().resolve("/etc/./hosts").display(), "/etc/hosts");
    }

    #[test]
    fn fragments_are_preserved_after_the_path() {
        assert_eq!(
            resolver().resolve("intro.md#setup").display(),
            "/docs/guide/intro.md#setup"
        );
    }

    #[test]
    fn urls_fragments_and_protocol_relative_pass_through() {
        for dest in [
            "https://example.com/a.md",
            "mailto:someone@example.com",
            "file:///tmp/x.md",
            "#section",
            "//cdn.example.com/x.png",
            "",
        ] {
            assert_eq!(resolver().resolve(dest).display(), dest, "{dest}");
        }
    }

    #[test]
    fn percent_escapes_decode_in_local_paths() {
        assert_eq!(
            resolver().resolve("My%20File.md").display(),
            "/docs/guide/My File.md"
        );
        assert_eq!(
            resolver().resolve("100%25.md").display(),
            "/docs/guide/100%.md"
        );
        // Malformed escapes are left alone.
        assert_eq!(
            resolver().resolve("bad%zz.md").display(),
            "/docs/guide/bad%zz.md"
        );
        assert_eq!(
            resolver().resolve("trail%2").display(),
            "/docs/guide/trail%2"
        );
    }

    #[test]
    fn local_files_click_through_as_file_uris_without_the_fragment() {
        assert_eq!(
            resolver().resolve("intro.md#setup").uri().as_deref(),
            Some("file:///docs/guide/intro.md")
        );
        assert_eq!(
            resolver().resolve("My%20File.md").uri().as_deref(),
            Some("file:///docs/guide/My%20File.md")
        );
        assert_eq!(
            resolver().resolve("caf\u{e9}.md").uri().as_deref(),
            Some("file:///docs/guide/caf%C3%A9.md")
        );
    }

    #[test]
    fn urls_click_through_as_written_and_opaque_links_do_not() {
        assert_eq!(
            resolver()
                .resolve("https://example.com/a.md?x=1")
                .uri()
                .as_deref(),
            Some("https://example.com/a.md?x=1")
        );
        assert_eq!(
            resolver()
                .resolve("https://example.com/a b\u{e9}%20")
                .uri()
                .as_deref(),
            Some("https://example.com/a%20b%C3%A9%20")
        );
        let too_long = format!("https://example.com/{}", "x".repeat(MAX_URI_BYTES));
        assert_eq!(resolver().resolve(&too_long).uri(), None);
        assert_eq!(resolver().resolve("#section").uri(), None);
        assert_eq!(resolver().resolve("//cdn.example.com/x.png").uri(), None);
        assert_eq!(resolver().resolve("").uri(), None);
    }

    #[test]
    fn for_document_uses_the_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let doc = dir.path().join("notes.md");
        std::fs::write(&doc, "# hi").expect("write");
        let resolver = LinkResolver::for_document(&doc).expect("resolver");
        let expected = dir.path().canonicalize().expect("canonical");

        assert_eq!(
            resolver.resolve("other.md").display(),
            format!("{}/other.md", expected.display())
        );
    }

    #[test]
    fn for_document_with_missing_parent_still_yields_an_absolute_path() {
        let resolver =
            LinkResolver::for_document(Path::new("no/such/dir/notes.md")).expect("resolver");

        assert!(Path::new(&resolver.resolve("x.md").display()).is_absolute());
    }
}
