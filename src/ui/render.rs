use chrono::Local;
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;

use crate::{
    provider,
    search::highlight_spans_terms,
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
    let mut parts = Vec::new();
    if cells >= 80 && session.split_count > 1 {
        parts.push(format!("{}/{}", session.split_index, session.split_count));
    }
    if cells >= 80 && !session.name.is_empty() && session.name != session.title {
        parts.push(session.name.clone());
    }
    if cells >= 100
        && let Some(at) = row.activity_at
    {
        parts.push(
            at.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        );
    }
    let context = parts.join("  ");
    let (title, context) = if context.is_empty() || available < 12 {
        (middle(&session.title, available), String::new())
    } else {
        let title_budget = width(&session.title).min((available * 2 / 3).max(5));
        let context_budget = width(&context).min(available.saturating_sub(title_budget + 2).max(5));
        (
            middle(&session.title, available.saturating_sub(context_budget + 2)),
            middle(&context, context_budget),
        )
    };
    spans.extend(styled(
        title.clone(),
        &row.query_terms,
        theme.normal(),
        theme,
    ));
    if !context.is_empty() {
        spans.push(Span::raw(
            " ".repeat(
                available
                    .saturating_sub(width(&title) + width(&context))
                    .max(2),
            ),
        ));
        spans.extend(styled(context, &row.query_terms, theme.muted(), theme));
    }
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
                text: Line::from(styled(text, &row.query_terms, base, theme)),
                is_excerpt: false,
            });
        } else {
            lines.push(VisualLine {
                key: Some(row.key.clone()),
                text: session_line(row, cells, theme),
                is_excerpt: false,
            });
            if cells < 100
                && let Some(at) = row.activity_at
            {
                let text = format!(
                    "      {}",
                    at.with_timezone(&Local).format("%Y-%m-%d %H:%M")
                );
                lines.push(VisualLine {
                    key: Some(row.key.clone()),
                    text: Line::styled(clip(&text, cells), theme.muted()),
                    is_excerpt: false,
                });
            }
            for (index, text) in row.snippets.iter().take(2).enumerate() {
                let ranges = row.snippet_spans.get(index).cloned().unwrap_or_default();
                let indent = (cells / 4).min(8);
                let bar = crop("│ ", cells.saturating_sub(indent));
                let budget = cells.saturating_sub(indent + width(&bar));
                let mut line = snippet(text, &ranges, budget, theme);
                let body_style = theme.excerpt(false);
                for span in &mut line.spans {
                    span.style.bg = body_style.bg;
                }
                line.spans.push(Span::styled(
                    " ".repeat(budget.saturating_sub(line.width())),
                    body_style,
                ));
                line.spans.insert(0, Span::styled(bar, body_style));
                line.spans
                    .insert(0, Span::styled(" ".repeat(indent), theme.normal()));
                lines.push(VisualLine {
                    key: Some(row.key.clone()),
                    text: line,
                    is_excerpt: true,
                });
            }
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
