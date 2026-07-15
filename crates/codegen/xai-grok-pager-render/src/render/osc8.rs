//! Link detection for the scrollback render pass.
//!
//! Provides [`LinkOverlay`] / [`OverlayLink`] (link positions collected during
//! rendering) and [`scan_lines_for_url_overlays`] for detecting plain-text URLs
//! and absolute file paths across all block types. The collected links are
//! handed to the terminal as `LinkSpan`s and emitted as OSC 8 hyperlinks by the
//! frame diff (see `xai_ratatui_inline::Terminal::flush_with_links`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use linkify::{LinkFinder, LinkKind};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

/// A single link region on screen.
#[derive(Debug, Clone)]
pub struct OverlayLink {
    pub screen_row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub url: Arc<str>,
    pub id: Option<u32>,
}

/// Accumulates link positions for post-flush OSC 8 emission.
#[derive(Debug, Clone)]
pub struct LinkOverlay {
    links: Vec<OverlayLink>,
}

impl Default for LinkOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkOverlay {
    pub fn new() -> Self {
        Self { links: Vec::new() }
    }

    pub fn push(&mut self, link: OverlayLink) {
        debug_assert!(
            link.col_start <= link.col_end,
            "OverlayLink col_start ({}) > col_end ({})",
            link.col_start,
            link.col_end
        );
        if link.col_start > link.col_end {
            return; // Silently skip inverted ranges in release mode.
        }
        self.links.push(link);
    }

    /// Append all links from `other` (clones each `OverlayLink`).
    ///
    /// Each link is routed through [`Self::push`] so the
    /// `col_start <= col_end` invariant is enforced (inverted ranges are
    /// silently dropped in release builds, just like the single-link path).
    pub fn extend_from(&mut self, other: &LinkOverlay) {
        self.links.reserve(other.links.len());
        for link in &other.links {
            self.push(link.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    pub fn links(&self) -> &[OverlayLink] {
        &self.links
    }

    /// Returns `true` if an existing link overlaps the given screen region.
    pub fn overlaps(&self, screen_row: u16, col_start: u16, col_end: u16) -> bool {
        self.links
            .iter()
            .any(|l| l.screen_row == screen_row && l.col_start < col_end && col_start < l.col_end)
    }
}

fn link_finder() -> &'static LinkFinder {
    static FINDER: OnceLock<LinkFinder> = OnceLock::new();
    FINDER.get_or_init(|| {
        let mut f = LinkFinder::new();
        f.kinds(&[LinkKind::Url]);
        f
    })
}

/// One path segment without spaces (`main.rs`, `.grok`, `@scope`). Leading `.`
/// matches dot-directories and `%` matches percent-encoded segments — grok
/// session media lives under `~/.grok/sessions/%2F…/images/1.jpg`.
const PATH_SEGMENT: &str = r"[a-zA-Z0-9_@.%][a-zA-Z0-9._+@%\-]*";

/// Final path segment may contain *internal* spaces for macOS app bundles and
/// similarly named files (`Demo App.app`). Requires a `.ext` suffix
/// after the last space so trailing prose (`…/bar here.`) is not consumed.
const PATH_SEGMENT_SPACED: &str =
    r"[a-zA-Z0-9_@.%][a-zA-Z0-9._+@%\-]*(?: [a-zA-Z0-9._+@%\-]+)+\.[a-zA-Z0-9][a-zA-Z0-9._+@%\-]*";

/// Relative file path (`images/1.png`, `.grok/x.txt`) — one or more `/`-joined
/// directory segments plus a filename that has an extension. No leading `/`
/// or `~` (those are the absolute forms). The required extension keeps
/// slashed prose ("and/or", "TCP/IP") out; the caller still gates on the file
/// existing under `cwd`.
fn relative_file_path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pat = format!(
            r"(?:{seg}/)+[a-zA-Z0-9_@%+\-]+(?:\.[a-zA-Z0-9_@%+\-]+)+",
            seg = PATH_SEGMENT,
        );
        regex::Regex::new(&pat).expect("relative file path regex")
    })
}

fn file_path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Absolute (`/Users/me/x.md`) or home-relative (`~/Desktop/x.md`) paths.
        // Leading `~` is expanded to $HOME when building the `file://` URL.
        //
        // The *final* segment may include internal spaces when it looks like a
        // filename with an extension (tutor report: `…/Demo App.app`
        // only linkified up to the space). Intermediate segments stay
        // space-free so `…/bar here.` does not eat the word `here`.
        // Alternation prefers the spaced form first so it wins over the shorter
        // no-space prefix at the same start position.
        let pat = format!(
            r"~?/(?:{seg}/)+(?:{spaced}|{seg})",
            seg = PATH_SEGMENT,
            spaced = PATH_SEGMENT_SPACED,
        );
        regex::Regex::new(&pat).expect("file path regex")
    })
}

/// Paths wrapped in single or double quotes, including spaces in any segment.
/// Group 1 is the opening quote; group 2 is the path (no surrounding quotes).
/// Caller must verify the character immediately after the path is the same quote.
fn quoted_file_path_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Opening quote + path; closing quote checked in code (regex crate has
        // no backreferences). Path allows spaces in segments; at least two
        // `/`-separated components required.
        // `"/Users/me/My Dir/file.app"` or `'~/Desktop/My Notes/todo.md'`
        let seg = r#"[^/"']+"#;
        let pat = format!(r#"(["'])(~?/(?:{seg}/)+{seg})"#);
        regex::Regex::new(&pat).expect("quoted file path regex")
    })
}

/// Turn a display path (`/abs/…` or `~/…`) into a `file://` URL, expanding `~/`.
/// Relative paths fail — use [`tool_path_file_url`] to join cwd first.
pub fn path_to_file_url(path: &str) -> Option<Arc<str>> {
    tool_path_file_url(path, None)
}

fn file_path_to_url(path: &Path) -> Option<Arc<str>> {
    url::Url::from_file_path(path)
        .ok()
        .map(|u| Arc::from(u.as_str()))
}

#[cfg(test)]
fn tool_path_file_url_with_home(
    path: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<Arc<str>> {
    let target =
        crate::render::tool_paths::resolve_tool_path_target_with_home(Path::new(path), cwd, home)?;
    file_path_to_url(&target)
}

/// `file://` URL for a Read/Edit target, joining ordinary relative paths to `cwd`.
pub fn tool_path_file_url(path: &str, cwd: Option<&Path>) -> Option<Arc<str>> {
    let target = crate::render::tool_paths::resolve_tool_path_target(path, cwd)?;
    file_path_to_url(&target)
}

/// Resolve a markdown link destination that names a local file into a `file://`
/// URL, so paths the model emits (`[videos/1.mp4](videos/1.mp4)`) open on click.
///
/// Web/scheme URLs, `mailto:`/`tel:`, and anchors return `None`.
///
/// - **Absolute / `~`** paths resolve directly (must be an existing file).
/// - **Relative** paths (`images/1.jpg`) resolve against `media_paths` — the
///   absolute paths of media actually generated in this transcript — by
///   matching a unique entry whose path ends with those components. This ties
///   each short path to the exact file its message produced (correct across
///   forks/resumes) and never opens an arbitrary or out-of-session file; an
///   ambiguous or absent match is left unlinked.
pub fn local_link_to_file_url(dest: &str, media_paths: &[PathBuf]) -> Option<Arc<str>> {
    let dest = dest.trim();
    if dest.is_empty() || dest.starts_with('#') || dest.contains("://") {
        return None;
    }
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("mailto:") || lower.starts_with("tel:") {
        return None;
    }
    let path = Path::new(dest);
    let target = crate::render::tool_paths::resolve_tool_path_target(dest, None)?;
    let resolved: PathBuf = if target.is_absolute() {
        target
    } else {
        // Relative: match a single generated-media file ending with these
        // components. Unique match only, so a forked transcript with a duplicate
        // name resolves to neither rather than the wrong one.
        let mut hits = media_paths.iter().filter(|p| p.ends_with(path));
        let first = hits.next()?.clone();
        if hits.next().is_some() {
            return None;
        }
        first
    };
    if !resolved.is_file() {
        return None;
    }
    url::Url::from_file_path(&resolved)
        .ok()
        .map(|u| Arc::from(u.as_str()))
}

/// Convert a display-cell column to a `u16` suitable for overlay coordinates.
///
/// Returns `None` when the column (plus content offset) would overflow
/// `u16`, in which case the caller should skip the link.
fn to_overlay_col(content_x: u16, col: usize) -> Option<u16> {
    let col16 = u16::try_from(col).ok()?;
    content_x.checked_add(col16)
}

/// One visual row of a logical (pre-wrap) line: its screen row plus the byte
/// range its text occupies within the joined logical string.
struct RowSegment {
    screen_row: u16,
    start: usize,
    end: usize,
}

/// Scan ratatui [`Line`]s for plain-text URLs and file paths, appending
/// corresponding [`OverlayLink`] entries to the overlay.
///
/// Runs on all blocks. For markdown blocks, existing hyperlinks are
/// already in the overlay; detected links that overlap are skipped.
///
/// Each item is `(screen_row, line, joiner)` where `joiner` is the soft-wrap
/// joiner to the *previous* row (see `BlockLine::joiner`): `None` = hard
/// break, `Some("")` = mid-word wrap, `Some(" ")` = word wrap. Consecutive
/// rows connected by `Some(..)` joiners are re-joined into one logical line
/// before matching, so a long path or URL soft-wrapped across rows (imagine
/// media lives at `~/.grok/sessions/%2F…/images/1.jpg`, which wraps in
/// narrow panes) is detected whole and each row's fragment gets its own
/// clickable overlay region. Spans within a row are likewise concatenated so
/// styling boundaries never truncate a match.
///
/// Detects three kinds of links:
/// 1. **URLs** via the `linkify` crate (http, https, mailto).
/// 2. **Absolute and `~`-relative file paths** via regex, emitted as
///    `file://` URLs (a leading `~/` is expanded to the home directory).
/// 3. **Relative file paths** (`images/1.png`) that uniquely match a generated
///    media file in `media_paths` — so prose like "and/or" is never linkified.
pub fn scan_lines_for_url_overlays<'a>(
    lines: impl Iterator<Item = (u16, &'a Line<'static>, Option<&'a str>)>,
    content_x: u16,
    media_paths: &[PathBuf],
    overlay: &mut LinkOverlay,
) {
    // Joined text + row segments for the logical line currently being
    // accumulated. Buffers are reused across groups to avoid per-row
    // allocation on every render frame.
    let mut group_text = String::new();
    let mut group_rows: Vec<RowSegment> = Vec::new();

    for (screen_row, line, joiner) in lines {
        // A `None` joiner is a hard break: flush the current group and start
        // a new logical line. (A `Some` joiner with no accumulated rows —
        // e.g. a wrap continuation scrolled in at the top of the viewport —
        // also starts a new group; its fragment is scanned standalone.)
        match joiner {
            Some(j) if !group_rows.is_empty() => group_text.push_str(j),
            _ => {
                scan_logical_line(&group_text, &group_rows, content_x, media_paths, overlay);
                group_text.clear();
                group_rows.clear();
            }
        }
        let start = group_text.len();
        for span in &line.spans {
            group_text.push_str(span.content.as_ref());
        }
        group_rows.push(RowSegment {
            screen_row,
            start,
            end: group_text.len(),
        });
    }
    scan_logical_line(&group_text, &group_rows, content_x, media_paths, overlay);
}

/// Push one [`OverlayLink`] per visual row that `match_range` (a byte range in
/// the joined logical `text`) overlaps.
///
/// Returns `true` if at least one overlay region was pushed. Returns `false`
/// without pushing anything when any row segment would overlap an existing
/// overlay link (e.g. a markdown hyperlink already mapped for that region) or
/// when a column exceeds `u16`.
fn push_link_segments(
    text: &str,
    rows: &[RowSegment],
    content_x: u16,
    match_range: std::ops::Range<usize>,
    url: &Arc<str>,
    overlay: &mut LinkOverlay,
) -> bool {
    let mut segments: Vec<(u16, u16, u16)> = Vec::new();
    for row in rows {
        // Intersect the match with this row's byte range; joiner bytes
        // between rows belong to no row and are clamped away.
        let start = match_range.start.max(row.start);
        let end = match_range.end.min(row.end);
        if start >= end {
            continue;
        }
        let col_start = UnicodeWidthStr::width(&text[row.start..start]);
        let col_end = col_start + UnicodeWidthStr::width(&text[start..end]);
        let (Some(cs), Some(ce)) = (
            to_overlay_col(content_x, col_start),
            to_overlay_col(content_x, col_end),
        ) else {
            return false;
        };
        if overlay.overlaps(row.screen_row, cs, ce) {
            return false;
        }
        segments.push((row.screen_row, cs, ce));
    }
    if segments.is_empty() {
        return false;
    }
    for (screen_row, col_start, col_end) in segments {
        overlay.push(OverlayLink {
            screen_row,
            col_start,
            col_end,
            url: Arc::clone(url),
            id: None,
        });
    }
    true
}

/// Run URL / file-path detection over one joined logical line and emit
/// per-row overlay regions for every match (see [`push_link_segments`]).
fn scan_logical_line(
    text: &str,
    rows: &[RowSegment],
    content_x: u16,
    media_paths: &[PathBuf],
    overlay: &mut LinkOverlay,
) {
    if text.is_empty() || rows.is_empty() {
        return;
    }
    let scheme_filter = crate::terminal::hyperlinks::SchemeFilter::Standard;
    let finder = link_finder();
    let path_re = file_path_regex();
    let quoted_path_re = quoted_file_path_regex();
    let rel_path_re = relative_file_path_regex();

    // Byte ranges consumed by URL links, populated lazily on first safe URL
    // hit to avoid allocation in the common case of lines with no URLs.
    let mut url_byte_ranges: Option<Vec<std::ops::Range<usize>>> = None;
    // Byte ranges already turned into file-path overlays (quoted first, then
    // plain) so later passes do not double-link.
    let mut path_byte_ranges: Vec<std::ops::Range<usize>> = Vec::new();

    for link in finder.links(text) {
        let url = link.as_str();
        if !crate::link_opener::is_safe_to_open(url, scheme_filter) {
            continue;
        }
        url_byte_ranges
            .get_or_insert_with(Vec::new)
            .push(link.start()..link.end());

        let url: Arc<str> = Arc::from(url);
        push_link_segments(
            text,
            rows,
            content_x,
            link.start()..link.end(),
            &url,
            overlay,
        );
    }

    let range_overlaps_urls = |start: usize, end: usize| -> bool {
        url_byte_ranges
            .as_ref()
            .is_some_and(|ranges| ranges.iter().any(|r| start < r.end && r.start < end))
    };

    // Pass 1: quoted paths — spaces allowed in every segment.
    for caps in quoted_path_re.captures_iter(text) {
        let open_q = caps.get(1).expect("open quote");
        let path_m = caps.get(2).expect("path group");
        // Require a matching closing quote immediately after the path.
        let close_idx = path_m.end();
        if text.as_bytes().get(close_idx) != Some(&open_q.as_str().as_bytes()[0]) {
            continue;
        }
        if range_overlaps_urls(path_m.start(), path_m.end()) {
            continue;
        }
        let Some(file_url) = path_to_file_url(path_m.as_str()) else {
            continue;
        };

        // Clickable region is the path text only (not the quotes).
        if push_link_segments(
            text,
            rows,
            content_x,
            path_m.start()..path_m.end(),
            &file_url,
            overlay,
        ) {
            path_byte_ranges.push(path_m.start()..path_m.end());
        }
    }

    // Pass 2: unquoted paths (final segment may include spaces + ext).
    for m in path_re.find_iter(text) {
        if range_overlaps_urls(m.start(), m.end())
            || path_byte_ranges
                .iter()
                .any(|r| m.start() < r.end && r.start < m.end())
        {
            continue;
        }
        if m.start() > 0 {
            let prev = text.as_bytes()[m.start() - 1];
            if prev.is_ascii_alphanumeric()
                || matches!(prev, b'_' | b'.' | b'+' | b'@' | b'-' | b':' | b'/' | b'~')
            {
                continue;
            }
        }
        // Drop trailing sentence punctuation so a path ending a sentence
        // (`…/images/1.jpg.`) links to the file, not `file.jpg.`.
        let path = m
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        if path.is_empty() {
            continue;
        }
        let path_end = m.start() + path.len();
        let Some(file_url) = path_to_file_url(path) else {
            continue;
        };

        if push_link_segments(
            text,
            rows,
            content_x,
            m.start()..path_end,
            &file_url,
            overlay,
        ) {
            path_byte_ranges.push(m.start()..path_end);
        }
    }

    // Pass 3: relative paths that uniquely match a generated media file
    // (so bare `word/word.ext` prose is not over-linkified).
    if !media_paths.is_empty() {
        for m in rel_path_re.find_iter(text) {
            if range_overlaps_urls(m.start(), m.end())
                || path_byte_ranges
                    .iter()
                    .any(|r| m.start() < r.end && r.start < m.end())
            {
                continue;
            }
            if m.start() > 0 {
                let prev = text.as_bytes()[m.start() - 1];
                if prev.is_ascii_alphanumeric()
                    || matches!(
                        prev,
                        b'_' | b'.' | b'+' | b'@' | b'-' | b':' | b'/' | b'~' | b'%'
                    )
                {
                    continue;
                }
            }
            let path = m
                .as_str()
                .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
            let Some(file_url) = local_link_to_file_url(path, media_paths) else {
                continue;
            };
            let path_end = m.start() + path.len();

            if push_link_segments(
                text,
                rows,
                content_x,
                m.start()..path_end,
                &file_url,
                overlay,
            ) {
                path_byte_ranges.push(m.start()..path_end);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::style::Color;

    /// Scan rows as independent logical lines (hard breaks between rows) —
    /// the common shape for tests that don't exercise soft-wrap joining.
    fn scan_unjoined<'a>(
        lines: impl Iterator<Item = (u16, &'a Line<'static>)>,
        content_x: u16,
        media_paths: &[PathBuf],
        overlay: &mut LinkOverlay,
    ) {
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            lines.map(|(row, line)| (row, line, None)).collect();
        scan_lines_for_url_overlays(rows.into_iter(), content_x, media_paths, overlay);
    }

    // ── local_link_to_file_url ──

    #[test]
    fn local_link_relative_resolves_to_generated_media() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("images")).unwrap();
        std::fs::write(dir.path().join("images/1.jpg"), b"x").unwrap();
        let media = vec![dir.path().join("images/1.jpg")];

        // Short session-relative path matches the generated media by suffix.
        let url = local_link_to_file_url("images/1.jpg", &media).unwrap();
        assert!(
            url.starts_with("file://") && url.ends_with("/images/1.jpg"),
            "got {url}"
        );
    }

    #[test]
    fn local_link_ignores_web_anchor_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("images")).unwrap();
        std::fs::write(dir.path().join("images/1.jpg"), b"x").unwrap();
        let media = vec![dir.path().join("images/1.jpg")];

        assert!(local_link_to_file_url("https://x.ai", &media).is_none());
        assert!(local_link_to_file_url("mailto:a@b.c", &media).is_none());
        assert!(local_link_to_file_url("#section", &media).is_none());
        // Relative path that isn't a known generated media file.
        assert!(local_link_to_file_url("images/2.jpg", &media).is_none());
        // No known media at all.
        assert!(local_link_to_file_url("images/1.jpg", &[]).is_none());
    }

    #[test]
    fn local_link_relative_rejects_ambiguous_and_traversal() {
        // Two generated files with the same session-relative name (e.g. a fork):
        // an ambiguous match resolves to neither, never the wrong one.
        let dir = tempfile::tempdir().unwrap();
        for sub in ["a", "b"] {
            std::fs::create_dir_all(dir.path().join(sub).join("images")).unwrap();
            std::fs::write(dir.path().join(sub).join("images/1.jpg"), b"x").unwrap();
        }
        let media = vec![
            dir.path().join("a/images/1.jpg"),
            dir.path().join("b/images/1.jpg"),
        ];
        assert!(local_link_to_file_url("images/1.jpg", &media).is_none());
        // A `..` never matches a clean absolute media path, so it can't escape.
        assert!(local_link_to_file_url("../images/1.jpg", &media).is_none());
    }

    // ── tool_path_file_url ──

    #[test]
    fn tool_path_file_url_resolves_relative_against_cwd() {
        let cwd = Path::new("/Users/me/project");
        let url = tool_path_file_url("src/main.rs", Some(cwd)).expect("url");
        assert!(url.starts_with("file://"), "got {url}");
        assert!(url.contains("/Users/me/project/src/main.rs"), "got {url}");
    }

    #[test]
    fn tool_path_file_url_accepts_absolute_without_existing_file() {
        let url = tool_path_file_url("/tmp/does-not-exist-xyz/foo.rs", None).expect("url");
        assert!(url.starts_with("file://"), "got {url}");
        assert!(url.contains("foo.rs"), "got {url}");
    }

    #[test]
    fn tool_path_file_url_preserves_parent_segments_for_os_resolution() {
        let url = tool_path_file_url("/repo/link/../target.rs", None).expect("url");
        assert!(url.contains("/repo/link/../target.rs"), "got {url}");
    }

    #[test]
    fn unresolved_tilde_never_manufactures_a_cwd_file_url() {
        assert!(
            tool_path_file_url_with_home("~/target.rs", Some(Path::new("/repo")), None).is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn tool_path_file_url_preserves_non_utf8_cwd_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let cwd = PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\x80".to_vec()));
        let url = tool_path_file_url("main.rs", Some(&cwd)).expect("url");
        assert!(url.contains("/tmp/non-utf8-%80/main.rs"), "got {url}");
        assert!(
            !url.contains("%EF%BF%BD"),
            "lossy replacement leaked: {url}"
        );
    }

    // ── LinkOverlay ──

    #[test]
    fn overlay_empty_by_default() {
        let overlay = LinkOverlay::new();
        assert!(overlay.is_empty());
        assert!(overlay.links().is_empty());
    }

    #[test]
    fn overlay_push_and_access() {
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 5,
            col_start: 10,
            col_end: 20,
            url: "https://example.com".into(),
            id: Some(1),
        });
        assert!(!overlay.is_empty());
        assert_eq!(overlay.links().len(), 1);
        assert_eq!(overlay.links()[0].screen_row, 5);
    }

    // ── scan_lines_for_url_overlays ──

    use ratatui::text::{Line as RLine, Span as RSpan};

    fn make_line(text: &str) -> Line<'static> {
        RLine::from(RSpan::raw(text.to_string()))
    }

    fn make_styled_line(spans: Vec<(&str, Color)>) -> Line<'static> {
        RLine::from(
            spans
                .into_iter()
                .map(|(t, c)| RSpan::styled(t.to_string(), ratatui::style::Style::default().fg(c)))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn scan_detects_url_in_plain_text() {
        let line = make_line("See https://example.com for details.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((5, &line)), 2, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        let link = &overlay.links()[0];
        assert_eq!(&*link.url, "https://example.com");
        assert_eq!(link.screen_row, 5);
        // "See " = 4 display cols, content_x = 2
        assert_eq!(link.col_start, 6);
        assert_eq!(link.col_end, 6 + 19); // "https://example.com" = 19 chars
        assert_eq!(link.id, None);
    }

    #[test]
    fn scan_detects_multiple_urls_on_one_line() {
        let line = make_line("https://a.example and https://b.example end");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        assert_eq!(&*overlay.links()[0].url, "https://a.example");
        assert_eq!(&*overlay.links()[1].url, "https://b.example");
        assert!(overlay.links()[0].col_end <= overlay.links()[1].col_start);
    }

    #[test]
    fn scan_across_multiple_spans() {
        let line = make_styled_line(vec![
            ("Visit ", Color::Gray),
            ("https://example.com", Color::Blue),
            (" now.", Color::Gray),
        ]);
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, "https://example.com");
        // "Visit " = 6 display cols (in first span)
        // The URL is in its own span, so col_start = 6
        assert_eq!(overlay.links()[0].col_start, 6);
    }

    #[test]
    fn scan_multiple_rows() {
        let line1 = make_line("See https://first.com here.");
        let line2 = make_line("And https://second.com there.");
        let lines: Vec<(u16, &Line<'static>)> = vec![(10, &line1), (11, &line2)];
        let mut overlay = LinkOverlay::new();
        scan_unjoined(lines.into_iter(), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        assert_eq!(overlay.links()[0].screen_row, 10);
        assert_eq!(overlay.links()[1].screen_row, 11);
    }

    #[test]
    fn scan_skips_unsafe_schemes() {
        let line = make_line("Bad: javascript://evil.com/alert(1) ok.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert!(
            overlay.is_empty(),
            "javascript:// scheme should be filtered"
        );
    }

    #[test]
    fn scan_no_urls_produces_empty() {
        let line = make_line("No links in this ordinary text.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert!(overlay.is_empty());
    }

    #[test]
    fn scan_trailing_punctuation_excluded() {
        let line = make_line("Visit https://example.com.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            "https://example.com",
            "trailing dot should be excluded by linkify"
        );
    }

    #[test]
    fn scan_url_with_path_and_query() {
        let line = make_line("Go to https://example.com/path?key=val#sec end.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            "https://example.com/path?key=val#sec"
        );
    }

    #[test]
    fn scan_content_x_offset_applied() {
        let line = make_line("https://x.ai");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 10, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(overlay.links()[0].col_start, 10);
        assert_eq!(overlay.links()[0].col_end, 10 + 12);
    }

    // ── File path detection ──

    #[test]
    fn scan_detects_absolute_file_path() {
        let line = make_line("Error in /Users/foo/src/main.rs at line 10");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, "file:///Users/foo/src/main.rs");
    }

    #[test]
    fn scan_detects_relative_path_when_generated_media() {
        // Both media kinds the model prints: `images/N.ext` and `videos/N.ext`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("images")).unwrap();
        std::fs::create_dir(dir.path().join("videos")).unwrap();
        std::fs::write(dir.path().join("images/1.png"), b"x").unwrap();
        std::fs::write(dir.path().join("videos/1.mp4"), b"x").unwrap();
        let media = vec![
            dir.path().join("images/1.png"),
            dir.path().join("videos/1.mp4"),
        ];

        for (line_text, suffix) in [
            ("Saved to images/1.png in the workspace.", "/images/1.png"),
            ("Video saved to videos/1.mp4.", "/videos/1.mp4"),
        ] {
            let line = make_line(line_text);
            let mut overlay = LinkOverlay::new();
            scan_unjoined(std::iter::once((0, &line)), 0, &media, &mut overlay);
            assert_eq!(overlay.links().len(), 1, "{line_text}");
            let url = &*overlay.links()[0].url;
            assert!(
                url.starts_with("file://") && url.ends_with(suffix),
                "got {url}"
            );
        }
    }

    #[test]
    fn scan_ignores_relative_path_when_not_generated_media() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("images")).unwrap();
        std::fs::write(dir.path().join("images/1.png"), b"x").unwrap();
        let media = vec![dir.path().join("images/1.png")];

        // A path that isn't a known generated media file → not linkified.
        let line = make_line("edit images/2.png please");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &media, &mut overlay);
        assert!(overlay.links().is_empty());
        // No known media at all → relative paths never resolve.
        let line = make_line("edit images/1.png please");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);
        assert!(overlay.links().is_empty());
    }

    #[test]
    fn scan_detects_grok_session_media_path() {
        // Dot-directory (`.grok`), percent-encoded session segment, and a
        // trailing sentence period — the shape of `image_gen` output prose.
        let line = make_line("Saved to /Users/alice/.grok/sessions/%2Fabc/00000000/images/1.jpg.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            // `%` is itself percent-encoded (`%25`) when building the file URL.
            "file:///Users/alice/.grok/sessions/%252Fabc/00000000/images/1.jpg",
        );
    }

    #[test]
    fn scan_detects_media_path_soft_wrapped_across_rows() {
        // Regression: `image_gen` output prose wraps the long session path
        // across visual rows (`joiner: Some("")` mid-word break). Previously
        // each row was scanned in isolation, so only the `/Users/alice`
        // fragment on the first row matched and became clickable.
        let row0 =
            make_line("Image generated and saved to /Users/alice/.grok/sessions/%2FUsers%2Fali");
        let row1 = make_line("ce%2Fcode%2Fxai/00000000-0000-0000-0000-000000000001/images/1.jpg");
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            vec![(3, &row0, None), (4, &row1, Some(""))];
        let mut overlay = LinkOverlay::new();
        scan_lines_for_url_overlays(rows.into_iter(), 2, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2, "one overlay region per row");
        let expected_url = "file:///Users/alice/.grok/sessions/%252FUsers%252Fali\
                            ce%252Fcode%252Fxai/00000000-0000-0000-0000-000000000001/images/1.jpg";
        for link in overlay.links() {
            assert_eq!(&*link.url, expected_url);
        }
        // Row 0: path starts after the prose and runs to the row's end.
        let prose = "Image generated and saved to ";
        let l0 = &overlay.links()[0];
        assert_eq!(l0.screen_row, 3);
        assert_eq!(l0.col_start, 2 + UnicodeWidthStr::width(prose) as u16);
        assert_eq!(
            l0.col_end,
            2 + UnicodeWidthStr::width(
                "Image generated and saved to /Users/alice/.grok/sessions/%2FUsers%2Fali"
            ) as u16
        );
        // Row 1: the continuation fragment covers the entire row.
        let l1 = &overlay.links()[1];
        assert_eq!(l1.screen_row, 4);
        assert_eq!(l1.col_start, 2);
        assert_eq!(
            l1.col_end,
            2 + UnicodeWidthStr::width(
                "ce%2Fcode%2Fxai/00000000-0000-0000-0000-000000000001/images/1.jpg"
            ) as u16
        );
    }

    #[test]
    fn scan_wrapped_path_trailing_sentence_period_excluded() {
        // Wrapped path ending mid-sentence: trailing `.` on the last row is
        // trimmed from the clickable region.
        let row0 = make_line("Saved to /Users/me/.grok/sessions/%2Fabc/019f3a86/ima");
        let row1 = make_line("ges/1.jpg. Enjoy!");
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            vec![(0, &row0, None), (1, &row1, Some(""))];
        let mut overlay = LinkOverlay::new();
        scan_lines_for_url_overlays(rows.into_iter(), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        for link in overlay.links() {
            assert_eq!(
                &*link.url,
                "file:///Users/me/.grok/sessions/%252Fabc/019f3a86/images/1.jpg"
            );
        }
        assert_eq!(overlay.links()[1].col_start, 0);
        assert_eq!(
            overlay.links()[1].col_end,
            UnicodeWidthStr::width("ges/1.jpg") as u16
        );
    }

    #[test]
    fn scan_wrapped_relative_media_path_resolves() {
        // A relative media path split by a mid-word wrap still resolves
        // against the generated-media list.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("images")).unwrap();
        std::fs::write(dir.path().join("images/1.png"), b"x").unwrap();
        let media = vec![dir.path().join("images/1.png")];

        let row0 = make_line("Saved to images/1.p");
        let row1 = make_line("ng in the workspace.");
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            vec![(0, &row0, None), (1, &row1, Some(""))];
        let mut overlay = LinkOverlay::new();
        scan_lines_for_url_overlays(rows.into_iter(), 0, &media, &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        for link in overlay.links() {
            assert!(
                link.url.starts_with("file://") && link.url.ends_with("/images/1.png"),
                "got {}",
                link.url
            );
        }
    }

    #[test]
    fn scan_url_soft_wrapped_across_rows() {
        let row0 = make_line("See https://example.com/some/lo");
        let row1 = make_line("ng/path?key=val for details.");
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            vec![(0, &row0, None), (1, &row1, Some(""))];
        let mut overlay = LinkOverlay::new();
        scan_lines_for_url_overlays(rows.into_iter(), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        for link in overlay.links() {
            assert_eq!(&*link.url, "https://example.com/some/long/path?key=val");
        }
    }

    #[test]
    fn scan_word_break_joiner_restores_source_space() {
        // A `Some(" ")` joiner re-inserts the collapsed space, so a spaced
        // final segment (`Demo App.app`) wrapped at the space still
        // matches as one path.
        let row0 = make_line("open /tmp/release/Demo");
        let row1 = make_line("App.app now");
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            vec![(0, &row0, None), (1, &row1, Some(" "))];
        let mut overlay = LinkOverlay::new();
        scan_lines_for_url_overlays(rows.into_iter(), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        for link in overlay.links() {
            assert_eq!(&*link.url, "file:///tmp/release/Demo%20App.app");
        }
        // Row 1's region covers only `App.app` (the joiner space belongs
        // to no row).
        assert_eq!(overlay.links()[1].col_start, 0);
        assert_eq!(
            overlay.links()[1].col_end,
            UnicodeWidthStr::width("App.app") as u16
        );
    }

    #[test]
    fn scan_hard_break_rows_not_joined() {
        // `None` joiner = separate source lines: fragments must not be glued
        // into a single false path across rows.
        let row0 = make_line("prefix /Users/alice");
        let row1 = make_line("suffix.txt more");
        let rows: Vec<(u16, &Line<'static>, Option<&str>)> =
            vec![(0, &row0, None), (1, &row1, None)];
        let mut overlay = LinkOverlay::new();
        scan_lines_for_url_overlays(rows.into_iter(), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, "file:///Users/alice");
        assert_eq!(overlay.links()[0].screen_row, 0);
    }

    #[test]
    fn scan_path_split_across_styled_spans_single_row() {
        // Markdown styling can split one row into multiple spans; the path
        // must still be matched across span boundaries.
        let line = make_styled_line(vec![
            ("Saved to ", Color::Gray),
            ("/Users/foo/", Color::Blue),
            ("images/1.jpg", Color::Green),
        ]);
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, "file:///Users/foo/images/1.jpg");
        assert_eq!(
            overlay.links()[0].col_start,
            UnicodeWidthStr::width("Saved to ") as u16
        );
    }

    #[test]
    fn scan_file_path_stops_at_colon() {
        let line = make_line("/Users/foo/bar.rs:45:10: error message");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            "file:///Users/foo/bar.rs",
            "colon-delimited line number should be excluded"
        );
    }

    #[test]
    fn scan_ignores_single_component_path() {
        // "/home" alone has only one component — not useful as a file link.
        let line = make_line("See /home for info.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert!(
            overlay.is_empty(),
            "single-component absolute path should not be linkified"
        );
    }

    #[test]
    fn scan_file_path_does_not_overlap_url() {
        // The path portion of a URL should not be detected as a file path.
        let line = make_line("Visit https://example.com/foo/bar here.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            "https://example.com/foo/bar",
            "URL should be detected, not the path portion"
        );
    }

    #[test]
    fn scan_url_and_file_path_coexist() {
        let line = make_line("See https://docs.rs/foo and /Users/me/src/lib.rs end.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 2);
        let urls: Vec<&str> = overlay.links().iter().map(|l| &*l.url).collect();
        assert!(urls.contains(&"https://docs.rs/foo"));
        assert!(urls.contains(&"file:///Users/me/src/lib.rs"));
    }

    #[test]
    fn scan_file_path_with_dots_and_hyphens() {
        let line = make_line("Reading /tmp/grok-impl-summary.md now.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, "file:///tmp/grok-impl-summary.md");
    }

    #[test]
    fn scan_file_path_with_at_sign() {
        let line = make_line("In /node_modules/@scope/package/index.js now.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            "file:///node_modules/@scope/package/index.js"
        );
    }

    #[test]
    fn scan_file_path_with_space_in_segment_quoted() {
        // Tutor report: path underline/click target stopped at the space in
        // `Demo App.app` (macOS app bundle name), inside quotes.
        let path = "/Users/alice/src/app/release/mac-arm64/Demo App.app";
        let line = make_line(&format!("open \"{path}\""));
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(
            overlay.links().len(),
            1,
            "expected one link for spaced path"
        );
        let link = &overlay.links()[0];
        assert_eq!(
            &*link.url, "file:///Users/alice/src/app/release/mac-arm64/Demo%20App.app",
            "space must be percent-encoded in the file URL"
        );
        // Clickable region must cover the *entire* displayed path, including
        // the segment after the space — not stop at `Demo`.
        let prefix = "open \"";
        assert_eq!(link.col_start, UnicodeWidthStr::width(prefix) as u16);
        assert_eq!(
            link.col_end,
            (UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(path)) as u16
        );
    }

    #[test]
    fn scan_file_path_with_space_in_segment_unquoted() {
        // Same filename without surrounding quotes — final segment has a
        // space plus extension, so the unquoted regex should still match.
        let path = "/tmp/release/Demo App.app";
        let line = make_line(&format!("open {path} now"));
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(
            &*overlay.links()[0].url,
            "file:///tmp/release/Demo%20App.app"
        );
        assert_eq!(overlay.links()[0].col_start, 5); // "open "
        assert_eq!(
            overlay.links()[0].col_end,
            5 + UnicodeWidthStr::width(path) as u16
        );
    }

    #[test]
    fn scan_file_path_space_does_not_swallow_trailing_sentence() {
        // A space followed by prose (no `.ext` in the final segment) must not
        // extend the link past the real path.
        let line = make_line("See /tmp/foo/bar here.");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, "file:///tmp/foo/bar");
        // "See " = 4 cols; path is 12 cols (`/tmp/foo/bar`).
        assert_eq!(overlay.links()[0].col_start, 4);
        assert_eq!(overlay.links()[0].col_end, 4 + 12);
    }

    // ── Home-relative (`~/`) path detection ──

    #[test]
    fn scan_detects_tilde_file_path() {
        // The user's example: a `~/Desktop/…md` path in agent output.
        let raw = "~/Desktop/grok-pager-retention-findings-2026-06-06.md";
        // Skip when no home directory is resolvable (e.g. minimal sandbox):
        // the tilde stays unexpanded and cannot form an absolute file URL.
        let Ok(expected) = url::Url::from_file_path(shellexpand::tilde(raw).as_ref()) else {
            return;
        };

        let line = make_line(&format!("Findings report {raw} done."));
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        let link = &overlay.links()[0];
        // `~` is expanded to the home directory in the file URL.
        assert_eq!(&*link.url, expected.as_str());
        assert!(link.url.starts_with("file:///"));
        assert!(!link.url.contains('~'), "tilde must be expanded in the URL");
        // The clickable region covers the displayed `~/…` text, tilde included.
        // "Findings report " = 16 display cols.
        assert_eq!(link.col_start, 16);
        assert_eq!(link.col_end, 16 + UnicodeWidthStr::width(raw) as u16);
    }

    #[test]
    fn scan_tilde_path_at_line_start() {
        let raw = "~/notes/todo.md";
        let Ok(expected) = url::Url::from_file_path(shellexpand::tilde(raw).as_ref()) else {
            return;
        };
        let line = make_line(raw);
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert_eq!(overlay.links().len(), 1);
        assert_eq!(&*overlay.links()[0].url, expected.as_str());
        assert_eq!(overlay.links()[0].col_start, 0);
    }

    #[test]
    fn scan_tilde_after_alnum_not_linkified() {
        // A `~` glued to a preceding word is not a home-relative path.
        let line = make_line("approx~/foo/bar here");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);
        assert!(
            overlay.is_empty(),
            "tilde preceded by alphanumeric should not linkify"
        );
    }

    #[test]
    fn scan_single_component_tilde_path_not_linkified() {
        // `~/projects` has a single component — mirrors the absolute
        // single-component rule (`/home`) and is not linkified.
        let line = make_line("cd ~/projects now");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);
        assert!(
            overlay.is_empty(),
            "single-component tilde path should not be linkified"
        );
    }

    #[test]
    fn scan_relative_path_not_partially_linkified() {
        // A relative path like `crates/codegen/xai-grok-pager/src/render` should
        // NOT produce a link for the `/xai-grok-pager/src/render` substring.
        let line = make_line("find crates/codegen/xai-grok-pager/src/render -name '*.rs'");
        let mut overlay = LinkOverlay::new();
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);

        assert!(
            overlay.is_empty(),
            "relative path substrings should not be partially linkified"
        );
    }

    #[test]
    fn overlaps_detects_exact_match() {
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 5,
            col_start: 10,
            col_end: 20,
            url: Arc::from("https://a.example"),
            id: None,
        });
        assert!(overlay.overlaps(5, 10, 20));
        assert!(!overlay.overlaps(6, 10, 20)); // different row
    }

    #[test]
    fn overlaps_detects_partial() {
        let mut overlay = LinkOverlay::new();
        overlay.push(OverlayLink {
            screen_row: 0,
            col_start: 10,
            col_end: 20,
            url: Arc::from("https://a.example"),
            id: None,
        });
        assert!(overlay.overlaps(0, 15, 25)); // right overlap
        assert!(overlay.overlaps(0, 5, 15)); // left overlap
        assert!(!overlay.overlaps(0, 20, 30)); // adjacent, no overlap
        assert!(!overlay.overlaps(0, 0, 10)); // adjacent left
    }

    #[test]
    fn scan_skips_url_overlapping_existing_link() {
        let line = make_line("See https://example.com for details.");
        let mut overlay = LinkOverlay::new();
        // Pre-populate with a markdown hyperlink covering the same region.
        // "See " = 4 cols, URL = 19 cols.
        overlay.push(OverlayLink {
            screen_row: 0,
            col_start: 4,
            col_end: 23,
            url: Arc::from("https://example.com"),
            id: None,
        });
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);
        // Should still have only the original link — scanner skipped the duplicate.
        assert_eq!(overlay.links().len(), 1);
    }

    #[test]
    fn scan_adds_non_overlapping_url_alongside_existing() {
        let line = make_line("A https://second.example end.");
        let mut overlay = LinkOverlay::new();
        // Existing link on a different column range.
        overlay.push(OverlayLink {
            screen_row: 0,
            col_start: 50,
            col_end: 70,
            url: Arc::from("https://first.example"),
            id: None,
        });
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);
        assert_eq!(overlay.links().len(), 2);
    }

    #[test]
    fn scan_skips_file_path_overlapping_existing_link() {
        let line = make_line("Error in /Users/foo/src/main.rs at line 10");
        let mut overlay = LinkOverlay::new();
        // Pre-populate a markdown hyperlink covering the file path region.
        // "Error in " = 9 cols, path = 22 cols.
        overlay.push(OverlayLink {
            screen_row: 0,
            col_start: 9,
            col_end: 31,
            url: Arc::from("file:///Users/foo/src/main.rs"),
            id: None,
        });
        scan_unjoined(std::iter::once((0, &line)), 0, &[], &mut overlay);
        assert_eq!(overlay.links().len(), 1);
    }

    #[test]
    fn scan_columns_beyond_u16_max_skipped() {
        // Simulate a line where the URL would start beyond u16::MAX columns.
        // We can't easily build a 65k-char line in a unit test, so we verify
        // the helper directly.
        assert!(to_overlay_col(u16::MAX, 1).is_none());
        assert!(to_overlay_col(u16::MAX - 5, 10).is_none());
        assert_eq!(to_overlay_col(10, 5), Some(15));
        assert_eq!(to_overlay_col(0, 0), Some(0));
    }
}
