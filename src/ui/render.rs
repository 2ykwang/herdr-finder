use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;

use crate::{
    provider,
    search::{highlight_spans_terms, metadata_highlight_spans_terms},
    state::{Row, RowKind},
};

#[derive(Clone, Debug)]
pub struct VisualLine {
    pub key: Option<String>,
    pub text: Line<'static>,
    pub is_excerpt: bool,
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn crop(text: &str, cells: usize) -> String {
    let mut used = 0;
    text.graphemes(true)
        .take_while(|g| {
            let next = width(g);
            if used + next > cells {
                false
            } else {
                used += next;
                true
            }
        })
        .collect()
}

pub fn clip(text: &str, cells: usize) -> String {
    if width(text) <= cells {
        text.to_owned()
    } else if cells == 0 {
        String::new()
    } else {
        format!("{}…", crop(text, cells - 1))
    }
}

pub fn middle(text: &str, cells: usize) -> String {
    if width(text) <= cells {
        return text.to_owned();
    }
    if cells < 5 {
        return clip(text, cells);
    }
    let left = crop(text, (cells - 1) / 2);
    let remaining = cells - 1 - width(&left);
    let mut right = Vec::new();
    let mut used = 0;
    for grapheme in text.graphemes(true).rev() {
        if used + width(grapheme) > remaining {
            break;
        }
        used += width(grapheme);
        right.push(grapheme);
    }
    right.reverse();
    format!("{left}…{}", right.concat())
}

pub fn wrapped_cells(text: &str, cells: usize, max_lines: usize) -> Vec<String> {
    if cells == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() && lines.len() < max_lines.max(1) {
        if lines.len() + 1 == max_lines.max(1) {
            lines.push(middle(remaining, cells));
            break;
        }
        let line = crop(remaining, cells);
        if line.is_empty() {
            lines.push(clip(remaining, cells));
            break;
        }
        remaining = &remaining[line.len()..];
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn styled(text: String, terms: &[String], base: Style, theme: Theme) -> Vec<Span<'static>> {
    let spans = highlight_spans_terms(&text, terms);
    styled_spans(text, &spans, base, theme)
}

fn styled_metadata(text: String, row: &Row, base: Style, theme: Theme) -> Vec<Span<'static>> {
    let ranges = metadata_highlight_spans_terms(&text, &row.query_terms, &row.fuzzy_terms);
    styled_spans(text, &ranges, base, theme)
}

fn styled_spans(
    text: String,
    ranges: &[(usize, usize)],
    base: Style,
    theme: Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for &(start, end) in ranges {
        if start < offset
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        if start > offset {
            spans.push(Span::styled(text[offset..start].to_owned(), base));
        }
        spans.push(Span::styled(text[start..end].to_owned(), theme.highlight()));
        offset = end;
    }
    if offset < text.len() {
        spans.push(Span::styled(text[offset..].to_owned(), base));
    }
    spans
}

pub fn provider_label(kind: &str, display: &str) -> String {
    provider::label(kind, display)
}

fn session_line(row: &Row, cells: usize, theme: Theme) -> Line<'static> {
    let Some(session) = row.session.as_ref() else {
        return Line::default();
    };
    let symbol = if session.kind == "terminal" {
        ">"
    } else {
        match session.status.as_str() {
            "working" => "●",
            "blocked" => "!",
            "done" => "✓",
            "idle" => "○",
            _ => "?",
        }
    };
    if cells <= 6 {
        return Line::raw(crop(&format!("    {symbol} "), cells));
    }
    let mut spans = vec![
        Span::raw("    "),
        Span::styled(format!("{symbol} "), theme.status(&session.status)),
    ];
    let mut prefix = 6;
    if cells >= 30 {
        let badge = format!(
            "[{}] ",
            clip(
                &provider_label(&session.kind, &session.display_kind),
                provider::MAX_LABEL_CELLS,
            )
        );
        prefix += width(&badge);
        spans.push(Span::styled(badge, theme.provider(&session.kind)));
    }
    let available = cells.saturating_sub(prefix);
    spans.extend(styled_metadata(
        middle(&session.title, available),
        row,
        theme.normal(),
        theme,
    ));
    Line::from(spans)
}

fn snippet(text: &str, ranges: &[(usize, usize)], cells: usize, theme: Theme) -> Line<'static> {
    if cells == 0 {
        return Line::default();
    }
    let anchor = ranges.last().map_or(0, |r| r.0);
    let mut start = 0;
    if width(&text[..anchor]) > cells / 3 {
        start = anchor;
        let mut used = 0;
        for (offset, g) in text[..anchor].grapheme_indices(true).rev() {
            if used + width(g) > cells / 3 {
                break;
            }
            used += width(g);
            start = offset;
        }
    }
    let left = if start > 0 { "…" } else { "" };
    let body = crop(&text[start..], cells.saturating_sub(width(left)));
    let end = start + body.len();
    let right = if end < text.len() { "…" } else { "" };
    let body = crop(&body, cells.saturating_sub(width(left) + width(right)));
    let end = start + body.len();
    let shifted: Vec<_> = ranges
        .iter()
        .filter_map(|&(a, b)| {
            let a = a.max(start);
            let b = b.min(end);
            (a < b).then(|| (left.len() + a - start, left.len() + b - start))
        })
        .collect();
    Line::from(styled_spans(
        format!("{left}{body}{right}"),
        &shifted,
        theme.muted(),
        theme,
    ))
}

pub fn preview_line(text: &str, terms: &[String], cells: usize, theme: Theme) -> Line<'static> {
    let ranges = highlight_spans_terms(text, terms);
    if ranges.iter().any(|&(_, end)| width(&text[..end]) > cells) {
        return snippet(text, &ranges, cells, theme);
    }
    Line::from(styled(clip(text, cells), terms, theme.normal(), theme))
}

pub fn selection_lines(row: &Row, cells: usize, height: usize, theme: Theme) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let Some(session) = &row.session else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(styled_metadata(
        middle(&session.title, cells),
        row,
        theme.normal().add_modifier(ratatui::style::Modifier::BOLD),
        theme,
    ))];
    let status = match session.status.as_str() {
        "blocked" => "Needs input",
        "done" => "Unseen completion",
        "working" => "Working",
        "idle" => "Idle",
        _ => "Status unknown",
    };
    let context = format!(
        "{} | {status} | {} / {}",
        provider_label(&session.kind, &session.display_kind),
        row.workspace.label,
        row.tab_label
    );
    if height >= 4 {
        lines.push(Line::styled(clip(&context, cells), theme.muted()));
    }
    if height > lines.len() && !session.cwd.is_empty() {
        lines.push(Line::from(styled(
            middle(&session.cwd, cells),
            &row.query_terms,
            theme.muted(),
            theme,
        )));
    }
    let snippet = row
        .snippets
        .iter()
        .find(|line| !highlight_spans_terms(line, &row.query_terms).is_empty())
        .or_else(|| row.snippets.first());
    let evidence = match (row.match_reason.is_empty(), snippet) {
        (false, Some(snippet)) => format!("{} | {snippet}", row.match_reason),
        (false, None) => row.match_reason.clone(),
        (true, Some(snippet)) => snippet.clone(),
        _ if height < 4 => context,
        _ => String::new(),
    };
    if height > lines.len() && !evidence.is_empty() {
        lines.push(preview_line(&evidence, &row.query_terms, cells, theme));
    }
    lines.truncate(height);
    lines
}

pub fn tree_lines(rows: &[Row], cells: usize, theme: Theme) -> Vec<VisualLine> {
    let outer_cells = cells;
    let cells = cells.saturating_sub(2);
    let mut lines = Vec::new();
    for row in rows {
        if row.separator_before {
            lines.push(VisualLine {
                key: None,
                text: Line::raw(""),
                is_excerpt: false,
            });
        }
        if row.kind != RowKind::Session {
            let label = if row.kind == RowKind::Tab {
                &row.tab_label
            } else {
                &row.workspace.label
            };
            let indent = if row.kind == RowKind::Tab { "  " } else { "" };
            let caret = if !row.query_terms.is_empty() {
                " "
            } else if row.collapsed {
                "▸"
            } else {
                "▾"
            };
            let prefix = format!("{indent}{caret} ");
            let suffix = format!(" ({})", row.count);
            let label = clip(label, cells.saturating_sub(width(&prefix) + width(&suffix)));
            let text = crop(&format!("{prefix}{label}{suffix}"), cells);
            let base = if row.kind == RowKind::Workspace {
                theme.workspace()
            } else {
                theme.tab()
            };
            lines.push(VisualLine {
                key: Some(row.key.clone()),
                text: Line::from(styled_metadata(text, row, base, theme)),
                is_excerpt: false,
            });
        } else {
            lines.push(VisualLine {
                key: Some(row.key.clone()),
                text: session_line(row, cells, theme),
                is_excerpt: false,
            });
        }
    }
    for line in &mut lines {
        line.text
            .spans
            .insert(0, Span::styled(crop("  ", outer_cells), theme.normal()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn preview_keeps_literal_evidence_visible_on_a_long_screen_line() {
        let text = format!("{} connection refused {}", "x".repeat(200), "y".repeat(80));
        let line = preview_line(&text, &["connection refused".into()], 40, Theme::new(true));
        assert!(line.to_string().contains("connection refused"));
        assert!(line.to_string().starts_with('…'));
        assert!(line.width() <= 40);
        assert!(line.spans.iter().any(|span| {
            span.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        }));
    }

    #[test]
    fn clips_cells_without_splitting_graphemes() {
        const WIDE_PAIR: &str = "\u{d55c}\u{ae00}";

        assert_eq!(
            clip(&format!("{WIDE_PAIR}🙂abcd"), 5),
            format!("{WIDE_PAIR}…")
        );
        assert!(width(&middle(&format!("{WIDE_PAIR} reviewer"), 10)) <= 10);
        assert_eq!(clip("e\u{301}🙂", 1), "…");
    }

    #[test]
    fn selected_titles_wrap_by_cells_and_keep_the_final_wide_grapheme() {
        // The final two-cell grapheme must survive after a full 34-cell row.
        const FINAL_WIDE_GRAPHEME: &str = "\u{b9ac}";
        let title = format!("{}{FINAL_WIDE_GRAPHEME}", "x".repeat(34));
        let lines = wrapped_cells(&title, 34, 3);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.concat(), title);
        assert_eq!(lines.last().unwrap(), FINAL_WIDE_GRAPHEME);
        assert!(lines.iter().all(|line| width(line) <= 34));

        let unicode = "e\u{301}👩‍💻\u{d55c}\u{ae00}";
        assert_eq!(wrapped_cells(unicode, 4, 3).concat(), unicode);
        assert_eq!(wrapped_cells(&title, 0, 3), vec![String::new()]);
    }

    #[test]
    fn snippet_shows_the_latest_match_with_context() {
        let value = format!("deploy started {} deploy failed: timeout", "z".repeat(120));
        let ranges = crate::search::highlight_spans(&value, "deploy");
        let line = snippet(&value, &ranges, 50, Theme::new(true));
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(plain.contains("deploy failed: timeout"));
        assert!(!plain.contains("deploy started"));
        assert!(line.spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        }));
    }

    #[test]
    fn all_known_badges_fit_without_losing_their_closing_bracket() {
        for provider in provider::catalog() {
            let row = Row {
                kind: RowKind::Session,
                session: Some(crate::model::Session {
                    kind: provider.kind.into(),
                    display_kind: "Custom display".into(),
                    title: "Task".into(),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let line = session_line(&row, 30, Theme::new(true));
            assert!(line.to_string().contains(&format!("[{}]", provider.label)));
            assert!(line.width() <= 30);
        }

        let row = Row {
            kind: RowKind::Session,
            session: Some(crate::model::Session {
                kind: "custom-agent".into(),
                display_kind: "A long custom provider label".into(),
                title: "Task".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let line = session_line(&row, 30, Theme::new(true));
        assert!(line.to_string().contains("…] "));
        assert!(line.width() <= 30);
    }
}
