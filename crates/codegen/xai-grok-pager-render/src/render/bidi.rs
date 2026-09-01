use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_bidi::Level;
use unicode_bidi::{BidiClass, BidiInfo, bidi_class};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

static RTL_BIDI_ENABLED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn is_enabled() -> bool {
    RTL_BIDI_ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(enabled: bool) {
    RTL_BIDI_ENABLED.store(enabled, Ordering::Relaxed);
}

#[inline]
pub fn needs_bidi(text: &str) -> bool {
    text.chars().any(is_rtl_affecting)
}

#[inline]
fn is_rtl_affecting(character: char) -> bool {
    matches!(
        bidi_class(character),
        BidiClass::R | BidiClass::AL | BidiClass::AN
    )
}

#[inline]
fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn strip_bidi_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_bidi_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .filter(|character| !is_bidi_control(*character))
            .collect(),
    )
}

pub(crate) fn paragraph_level(text: &str) -> Level {
    let cleaned = strip_bidi_controls(text);
    let bidi = BidiInfo::new(cleaned.as_ref(), None);
    bidi.paragraphs
        .first()
        .map(|paragraph| paragraph.level)
        .unwrap_or_else(Level::ltr)
}

pub fn visual_text(text: &str) -> Cow<'_, str> {
    if !is_enabled() {
        return Cow::Borrowed(text);
    }
    let cleaned = strip_bidi_controls(text);
    if is_table_row(cleaned.as_ref()) || !needs_bidi(cleaned.as_ref()) {
        return cleaned;
    }
    let level = paragraph_level(cleaned.as_ref());
    Cow::Owned(visual_text_with_level(cleaned.as_ref(), level))
}

pub(crate) fn visual_text_with_level(text: &str, level: Level) -> String {
    if text.contains('\n') {
        return text
            .split('\n')
            .map(|line| {
                if needs_bidi(line) && !is_table_row(line) {
                    visual_text_line(line, level)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    visual_text_line(text, level)
}

fn visual_text_line(text: &str, level: Level) -> String {
    debug_assert!(!text.contains('\n'));
    if is_table_row(text) {
        return text.to_string();
    }
    let cleaned = strip_bidi_controls(text);
    let prefix = chrome_prefix_len(cleaned.as_ref());
    if prefix >= cleaned.len() {
        return cleaned.into_owned();
    }
    let (chrome, body) = cleaned.split_at(prefix);
    if !needs_bidi(body) {
        return cleaned.into_owned();
    }
    let mut out = String::with_capacity(cleaned.len());
    out.push_str(chrome);
    out.push_str(&reorder_body(body, level));
    out
}

pub fn visual_line(line: &Line<'_>) -> Option<Line<'static>> {
    if !is_enabled() {
        return None;
    }
    visual_line_with_level(line, None)
}

pub(crate) fn visual_line_with_level(
    line: &Line<'_>,
    level: Option<Level>,
) -> Option<Line<'static>> {
    if !is_enabled() {
        return None;
    }

    let has_rtl = line
        .spans
        .iter()
        .any(|value| needs_bidi(value.content.as_ref()));
    let has_controls = line
        .spans
        .iter()
        .any(|value| value.content.chars().any(is_bidi_control));
    if !has_rtl && !has_controls {
        return None;
    }

    let mut flat = String::new();
    let mut span_bounds: Vec<(Range<usize>, Style)> = Vec::with_capacity(line.spans.len());
    for span in &line.spans {
        let start = flat.len();
        for character in span.content.chars() {
            if !is_bidi_control(character) {
                flat.push(character);
            }
        }
        if flat.len() > start {
            span_bounds.push((start..flat.len(), span.style));
        }
    }
    if flat.is_empty() || is_table_row(&flat) {
        return None;
    }

    let para_level = level.unwrap_or_else(|| paragraph_level(&flat));
    let prefix = chrome_prefix_len(&flat);
    let body = &flat[prefix..];

    if body.is_empty() || !needs_bidi(body) {
        if !has_controls {
            return None;
        }
        let mut out_spans: Vec<Span<'static>> = Vec::new();
        append_graphemes_styled(&flat, 0, &span_bounds, &mut out_spans, false);
        let mut visual = Line::from(out_spans);
        visual.style = line.style;
        visual.alignment = line.alignment;
        return Some(visual);
    }

    let mut out_spans: Vec<Span<'static>> = Vec::new();
    if prefix > 0 {
        append_graphemes_styled(&flat[..prefix], 0, &span_bounds, &mut out_spans, false);
    }
    append_reordered_body(body, prefix, &span_bounds, &mut out_spans, para_level);

    let mut visual = Line::from(out_spans);
    visual.style = line.style;
    visual.alignment = line.alignment;
    Some(visual)
}

pub fn logical_slice_for_visual_cols(text: &str, vis_start: usize, vis_end: usize) -> String {
    if vis_start >= vis_end || text.is_empty() {
        return String::new();
    }
    if !is_enabled() {
        return slice_display_cols(text, vis_start, vis_end);
    }
    let cleaned = strip_bidi_controls(text);
    let text = cleaned
        .as_ref()
        .split('\n')
        .next()
        .unwrap_or(cleaned.as_ref());
    if !needs_bidi(text) || is_table_row(text) {
        return slice_display_cols(text, vis_start, vis_end);
    }
    let prefix = chrome_prefix_len(text);
    let prefix_cols = str_cells(&text[..prefix]);
    let mut out = String::new();

    if vis_start < prefix_cols {
        out.push_str(&slice_display_cols(
            &text[..prefix],
            vis_start,
            vis_end.min(prefix_cols),
        ));
    }
    if vis_end <= prefix_cols {
        return out;
    }

    let body = &text[prefix..];
    let body_vs = vis_start.saturating_sub(prefix_cols);
    let body_ve = vis_end.saturating_sub(prefix_cols);
    if body_vs >= body_ve {
        return out;
    }
    if !needs_bidi(body) {
        out.push_str(&slice_display_cols(body, body_vs, body_ve));
        return out;
    }

    let level = paragraph_level(text);
    let graphemes: Vec<&str> = body.graphemes(true).collect();
    let visual_order = visual_grapheme_order(body, level);
    let mut vis_col_of = vec![0usize; graphemes.len()];
    let mut vcol = 0usize;
    for &gi in &visual_order {
        vis_col_of[gi] = vcol;
        vcol += UnicodeWidthStr::width(graphemes[gi]);
    }
    for (gi, grapheme) in graphemes.iter().enumerate() {
        let width = UnicodeWidthStr::width(*grapheme);
        let vc = vis_col_of[gi];
        if width == 0 {
            continue;
        }
        if vc < body_ve && vc + width > body_vs {
            out.push_str(grapheme);
        }
    }
    out
}

pub fn visual_col_to_logical_col(text: &str, visual_col: usize) -> usize {
    if !is_enabled() {
        return visual_col;
    }
    let cleaned = strip_bidi_controls(text);
    let text = cleaned
        .as_ref()
        .split('\n')
        .next()
        .unwrap_or(cleaned.as_ref());
    if !needs_bidi(text) || is_table_row(text) {
        return visual_col;
    }
    let prefix = chrome_prefix_len(text);
    let prefix_cols = str_cells(&text[..prefix]);
    let body = &text[prefix..];
    if visual_col < prefix_cols || !needs_bidi(body) {
        return visual_col.min(prefix_cols + str_cells(body));
    }

    let body_vis = visual_col - prefix_cols;
    let level = paragraph_level(text);
    let graphemes: Vec<&str> = body.graphemes(true).collect();
    let order = visual_grapheme_order(body, level);
    let mut logical_col_of = vec![0usize; graphemes.len()];
    let mut lc = 0usize;
    for (gi, grapheme) in graphemes.iter().enumerate() {
        logical_col_of[gi] = lc;
        lc += UnicodeWidthStr::width(*grapheme);
    }
    let mut vcol = 0usize;
    for &gi in &order {
        let width = UnicodeWidthStr::width(graphemes[gi]);
        if width == 0 {
            continue;
        }
        if body_vis < vcol + width {
            return prefix_cols + logical_col_of[gi];
        }
        vcol += width;
    }
    prefix_cols + str_cells(body)
}

pub fn logical_cols_to_visual(
    text: &str,
    logical_start: usize,
    logical_end: usize,
) -> Vec<(usize, usize)> {
    if logical_start >= logical_end || text.is_empty() {
        return Vec::new();
    }
    if !is_enabled() {
        return vec![(logical_start, logical_end)];
    }
    let cleaned = strip_bidi_controls(text);
    let text = cleaned
        .as_ref()
        .split('\n')
        .next()
        .unwrap_or(cleaned.as_ref());
    if !needs_bidi(text) || is_table_row(text) {
        return vec![(logical_start, logical_end)];
    }
    let prefix = chrome_prefix_len(text);
    let prefix_cols = str_cells(&text[..prefix]);
    if logical_end <= prefix_cols {
        return vec![(logical_start, logical_end)];
    }

    let body = &text[prefix..];
    if !needs_bidi(body) {
        return vec![(logical_start, logical_end)];
    }

    let body_log_start = logical_start.saturating_sub(prefix_cols);
    let body_log_end = logical_end.saturating_sub(prefix_cols);
    let mut ranges = Vec::new();
    if logical_start < prefix_cols {
        ranges.push((logical_start, prefix_cols.min(logical_end)));
    }
    if body_log_start < body_log_end {
        let level = paragraph_level(text);
        for (vs, ve) in body_logical_cols_to_visual(body, body_log_start, body_log_end, level) {
            ranges.push((vs + prefix_cols, ve + prefix_cols));
        }
    }
    merge_adjacent_ranges(ranges)
}

fn is_table_row(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(ch)
            if ('\u{2500}'..='\u{257F}').contains(&ch) && ch != '\u{2502}' && ch != '\u{2503}' =>
        {
            true
        }
        Some('\u{2502}') => {
            let mut in_prefix = true;
            for ch in chars {
                if in_prefix && (ch == '\u{2502}' || ch == ' ') {
                    continue;
                }
                in_prefix = false;
                if ch == '\u{2502}' {
                    return true;
                }
            }
            false
        }
        Some('|') => true,
        _ => false,
    }
}

fn chrome_prefix_len(text: &str) -> usize {
    let bq = blockquote_prefix_len(text);
    bq + marker_prefix_len(&text[bq..])
}

fn marker_prefix_len(text: &str) -> usize {
    for prefix in [
        "$ ",
        "\u{276F} ",
        "> ",
        "\u{21BB}  ",
        "• ",
        "- ",
        "* ",
        "\u{25C8} ",
        "\u{2666} ",
    ] {
        if text.starts_with(prefix) {
            return prefix.len();
        }
    }
    {
        let bytes = text.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index > 0 && index + 1 < bytes.len() && bytes[index] == b'.' && bytes[index + 1] == b' '
        {
            return index + 2;
        }
    }
    let spaces = text.bytes().take_while(|&byte| byte == b' ').count();
    if spaces > 0 && spaces < text.len() {
        spaces
    } else {
        0
    }
}

fn blockquote_prefix_len(text: &str) -> usize {
    const BAR_BYTES: usize = '\u{2502}'.len_utf8();
    let mut len = 0;
    let mut chars = text.chars();
    while let Some('\u{2502}') = chars.next() {
        if chars.next() == Some(' ') {
            len += BAR_BYTES + 1;
        } else {
            break;
        }
    }
    len
}

fn reorder_body(body: &str, level: Level) -> String {
    let bidi = BidiInfo::new(body, Some(level));
    let mut out = String::with_capacity(body.len());
    for para in &bidi.paragraphs {
        let (levels, runs) = bidi.visual_runs(para, para.range.clone());
        for run in runs {
            let slice = &body[run.clone()];
            if levels[run.start].is_rtl() {
                for grapheme in slice.graphemes(true).collect::<Vec<_>>().into_iter().rev() {
                    out.push_str(&mirror_grapheme(grapheme));
                }
            } else {
                out.push_str(slice);
            }
        }
    }
    out
}

fn append_reordered_body(
    body: &str,
    body_byte_base: usize,
    span_bounds: &[(Range<usize>, Style)],
    out: &mut Vec<Span<'static>>,
    level: Level,
) {
    let bidi = BidiInfo::new(body, Some(level));
    for para in &bidi.paragraphs {
        let (levels, runs) = bidi.visual_runs(para, para.range.clone());
        for run in runs {
            let slice = &body[run.clone()];
            let abs = body_byte_base + run.start;
            let rtl = levels[run.start].is_rtl();
            append_graphemes_styled(slice, abs, span_bounds, out, rtl);
        }
    }
}

fn append_graphemes_styled(
    slice: &str,
    abs_byte_start: usize,
    span_bounds: &[(Range<usize>, Style)],
    out: &mut Vec<Span<'static>>,
    reverse_rtl: bool,
) {
    let mut graphemes: Vec<(usize, &str)> = slice.grapheme_indices(true).collect();
    if reverse_rtl {
        graphemes.reverse();
    }
    for (rel, grapheme) in graphemes {
        let mirrored;
        let text = if reverse_rtl {
            mirrored = mirror_grapheme(grapheme);
            mirrored.as_str()
        } else {
            grapheme
        };
        append_str_styled(text, abs_byte_start + rel, span_bounds, out);
    }
}

fn append_str_styled(
    value: &str,
    abs_byte: usize,
    span_bounds: &[(Range<usize>, Style)],
    out: &mut Vec<Span<'static>>,
) {
    let style = style_at(abs_byte, span_bounds);
    if let Some(last) = out.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(value);
        return;
    }
    out.push(Span::styled(value.to_string(), style));
}

fn style_at(byte: usize, span_bounds: &[(Range<usize>, Style)]) -> Style {
    match span_bounds.binary_search_by(|(range, _)| {
        if byte < range.start {
            std::cmp::Ordering::Greater
        } else if byte >= range.end {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(index) => span_bounds[index].1,
        Err(_) => Style::default(),
    }
}

fn mirror_grapheme(grapheme: &str) -> String {
    grapheme
        .chars()
        .map(|character| unicode_bidi_mirroring::get_mirrored(character).unwrap_or(character))
        .collect()
}

fn visual_grapheme_order(body: &str, level: Level) -> Vec<usize> {
    let grapheme_starts: Vec<usize> = body
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .collect();
    let bidi = BidiInfo::new(body, Some(level));
    let mut order = Vec::with_capacity(grapheme_starts.len());
    for para in &bidi.paragraphs {
        let (levels, runs) = bidi.visual_runs(para, para.range.clone());
        for run in runs {
            let mut idxs: Vec<usize> = grapheme_starts
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte >= run.start && **byte < run.end)
                .map(|(index, _)| index)
                .collect();
            if levels[run.start].is_rtl() {
                idxs.reverse();
            }
            order.extend(idxs);
        }
    }
    order
}

fn body_logical_cols_to_visual(
    body: &str,
    logical_start: usize,
    logical_end: usize,
    level: Level,
) -> Vec<(usize, usize)> {
    let graphemes: Vec<&str> = body.graphemes(true).collect();
    let mut logical_meta: Vec<(usize, usize)> = Vec::new();
    let mut col = 0usize;
    for grapheme in &graphemes {
        let width = UnicodeWidthStr::width(*grapheme);
        logical_meta.push((col, width));
        col += width;
    }
    let order = visual_grapheme_order(body, level);
    let mut selected = Vec::new();
    let mut vcol = 0usize;
    for &gi in &order {
        let (lc, width) = logical_meta[gi];
        if width > 0 && lc < logical_end && lc + width > logical_start {
            selected.push((vcol, vcol + width));
        }
        vcol += width;
    }
    merge_adjacent_ranges(selected)
}

fn slice_display_cols(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        let next = col + width;
        if next > start && col < end {
            out.push_str(grapheme);
        }
        col = next;
        if col >= end {
            break;
        }
    }
    out
}

fn str_cells(value: &str) -> usize {
    value.graphemes(true).map(UnicodeWidthStr::width).sum()
}

fn merge_adjacent_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|(start, _)| *start);
    let mut merged = Vec::with_capacity(ranges.len());
    let (mut cs, mut ce) = ranges[0];
    for &(start, end) in &ranges[1..] {
        if start <= ce {
            ce = ce.max(end);
        } else {
            merged.push((cs, ce));
            cs = start;
            ce = end;
        }
    }
    merged.push((cs, ce));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::SafeBuf;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    const AR: &str = "سلام";
    const AR_V: &str = "مالس";
    const FA: &str = "خوب";
    const FA_V: &str = "بوخ";

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnabledGuard(bool);
    impl Drop for EnabledGuard {
        fn drop(&mut self) {
            set_enabled(self.0);
        }
    }

    fn with_enabled<Output>(callback: impl FnOnce() -> Output) -> Output {
        let _g = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let _latch = EnabledGuard(is_enabled());
        set_enabled(true);
        callback()
    }

    fn paint_plain(line: &str, width: u16) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        buf.set_line_safe_bidi(0, 0, &Line::from(line.to_string()), width);
        (0..width)
            .map(|column| {
                buf.cell((column, 0))
                    .map(|character| character.symbol())
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn disabled_is_identity() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        set_enabled(false);
        assert_eq!(visual_text(AR).as_ref(), AR);
        assert!(visual_line(&Line::from(AR)).is_none());
        assert_eq!(paint_plain(&format!("Hi {AR}"), 20), format!("Hi {AR}"));
    }

    #[test]
    fn enabled_reorders_and_keeps_english_leading() {
        with_enabled(|| {
            assert_eq!(visual_text(AR).as_ref(), AR_V);
            assert_eq!(visual_text(FA).as_ref(), FA_V);
            assert_eq!(
                visual_text(&format!("Hello {AR} world")).as_ref(),
                format!("Hello {AR_V} world")
            );
        });
    }

    #[test]
    fn combining_mark_stays_with_base() {
        with_enabled(|| {
            let value = "ب\u{064E}";
            let visual = visual_text(value);
            assert_eq!(visual.as_ref(), value);
            assert!(visual.as_ref().contains('\u{064E}'));
        });
    }

    #[test]
    fn mirrors_parens_inside_rtl_run() {
        with_enabled(|| {
            let value = format!("ا({AR})ب");
            assert_eq!(visual_text(&value).as_ref(), format!("ب({AR_V})ا"));
        });
    }

    #[test]
    fn strips_bidi_overrides_even_on_latin() {
        with_enabled(|| {
            let value = format!("\u{202E}{AR}");
            assert!(!visual_text(&value).contains('\u{202E}'));
            assert_eq!(visual_text("\u{202E}Hello").as_ref(), "Hello");
        });
    }

    #[test]
    fn table_and_chrome_unchanged_structure() {
        with_enabled(|| {
            let row = format!("│ {AR} │ cell │");
            assert_eq!(visual_text(&row).as_ref(), row);
            assert_eq!(
                visual_text(&format!("│ {AR}")).as_ref(),
                format!("│ {AR_V}")
            );
            assert_eq!(
                visual_text(&format!("• {AR}")).as_ref(),
                format!("• {AR_V}")
            );
            assert_eq!(
                visual_text(&format!("1. {AR}")).as_ref(),
                format!("1. {AR_V}")
            );
        });
    }

    #[test]
    fn shared_paragraph_level_differs_from_per_row() {
        with_enabled(|| {
            let para = format!("{AR} hello {FA}");
            let shared = paragraph_level(&para);
            let row2 = format!("hello {FA}");
            let auto = visual_text_with_level(&row2, paragraph_level(&row2));
            let forced = visual_text_with_level(&row2, shared);
            assert_eq!(auto, format!("hello {FA_V}"));
            assert_ne!(auto, forced);
            assert!(forced.contains(FA_V));
        });
    }

    #[test]
    fn clipboard_slice_is_logical() {
        with_enabled(|| {
            assert_eq!(logical_slice_for_visual_cols(AR, 0, 4), AR);
            assert_eq!(logical_slice_for_visual_cols(AR, 0, 1), "م");
            let mixed = format!("Hi {AR}");
            assert_eq!(logical_slice_for_visual_cols(&mixed, 3, 7), AR);
        });
    }

    #[test]
    fn logical_cols_map_to_visual_cells() {
        with_enabled(|| {
            let mixed = format!("Hi {AR}");
            assert_eq!(logical_cols_to_visual(&mixed, 3, 7), vec![(3, 7)]);
            assert_eq!(logical_cols_to_visual(AR, 0, 4), vec![(0, 4)]);
            assert_eq!(logical_cols_to_visual(AR, 0, 1), vec![(3, 4)]);
        });
    }

    #[test]
    fn visual_line_styles() {
        with_enabled(|| {
            let red = Style::default().fg(Color::Red);
            let blue = Style::default().fg(Color::Blue);
            let line = Line::from(vec![Span::styled("Hi ", red), Span::styled(AR, blue)]);
            let visual = visual_line(&line).expect("reorder");
            let flat: String = visual
                .spans
                .iter()
                .map(|value| value.content.to_string())
                .collect();
            assert_eq!(flat, format!("Hi {AR_V}"));
            assert_eq!(visual.spans[0].style.fg, Some(Color::Red));
        });
    }

    #[test]
    fn paint_matches_visual_text_and_column_map() {
        with_enabled(|| {
            let logical = format!("Hi {AR}");
            let painted = paint_plain(&logical, 20);
            assert_eq!(painted, format!("Hi {AR_V}"));
            assert_eq!(painted, visual_text(&logical).as_ref());

            let ranges = logical_cols_to_visual(&logical, 3, 7);
            assert_eq!(ranges, vec![(3, 7)]);
            let (vs, ve) = ranges[0];
            let painted_chars: Vec<char> = painted.chars().collect();
            let cell_slice: String = painted_chars[vs..ve].iter().collect();
            assert_eq!(cell_slice, AR_V);

            assert_eq!(logical_slice_for_visual_cols(&logical, vs, ve), AR);
        });
    }

    #[test]
    fn keeps_zwnj() {
        with_enabled(|| {
            let with_zwnj = "ب\u{200C}ج\u{200C}د";
            assert_eq!(visual_text(with_zwnj).as_ref(), "دج\u{200C}ب\u{200C}");
        });
    }

    #[test]
    fn visual_col_to_logical_col_inverts_paint() {
        with_enabled(|| {
            assert_eq!(visual_col_to_logical_col(AR, 0), 3);
            assert_eq!(visual_col_to_logical_col(AR, 3), 0);
            let mixed = format!("Hi {AR}");
            assert_eq!(visual_col_to_logical_col(&mixed, 0), 0);
            assert_eq!(visual_col_to_logical_col(&mixed, 3), 6);
            assert_eq!(visual_col_to_logical_col(&mixed, 6), 3);
            assert_eq!(visual_col_to_logical_col(AR, 99), 4);
        });
    }

    #[test]
    fn nested_quote_and_list_marker_stay_left() {
        with_enabled(|| {
            assert_eq!(
                visual_text(&format!("│ • {FA}")).as_ref(),
                format!("│ • {FA_V}")
            );
            assert_eq!(
                visual_text(&format!("│ 1. {FA}")).as_ref(),
                format!("│ 1. {FA_V}")
            );
            assert_eq!(paint_plain(&format!("│ • {FA}"), 20), format!("│ • {FA_V}"));
            assert_eq!(
                visual_text(&format!("• {FA}")).as_ref(),
                format!("• {FA_V}")
            );
        });
    }

    #[test]
    fn control_prefixed_table_row_maps_identity() {
        with_enabled(|| {
            let row = "\u{200F}| x | بت |";
            assert_eq!(visual_col_to_logical_col(row, 4), 4);
            assert_eq!(logical_cols_to_visual(row, 2, 6), vec![(2, 6)]);
            assert_eq!(
                logical_slice_for_visual_cols(row, 0, 3),
                slice_display_cols("| x | بت |", 0, 3)
            );
        });
    }

    #[test]
    fn visual_col_to_logical_col_identity_when_disabled() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        set_enabled(false);
        assert_eq!(visual_col_to_logical_col(AR, 2), 2);
        assert_eq!(visual_col_to_logical_col("plain", 3), 3);
    }

    #[test]
    fn set_line_safe_does_not_reorder() {
        with_enabled(|| {
            let area = Rect::new(0, 0, 20, 1);
            let mut buf = Buffer::empty(area);
            buf.set_line_safe(0, 0, &Line::from(AR.to_string()), 20);
            let got: String = (0..4u16)
                .map(|column| {
                    buf.cell((column, 0))
                        .map(|character| character.symbol())
                        .unwrap_or("")
                })
                .collect();
            assert_eq!(got, AR);
        });
    }
}
