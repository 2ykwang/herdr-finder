use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

use super::{render, theme::Theme};

const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Browse",
        &[
            ("↑ / ↓", "Select a row"),
            ("← / →", "Jump between tabs"),
            ("Alt+← / →", "Jump between workspaces"),
            ("Enter", "Open a session or fold a group"),
            ("a b w d i", "All / needs input / working / done / idle"),
            ("Ctrl+R", "Refresh"),
        ],
    ),
    (
        "Search",
        &[
            ("/", "Search names, paths and screen text"),
            ("← / →", "Move the text cursor"),
            ("Esc", "Back to browsing; Esc again closes Finder"),
        ],
    ),
    (
        "Mouse",
        &[
            ("Click / double-click", "Select / open a session"),
            ("Right-click", "Select the parent group"),
            ("Wheel", "Scroll without changing selection"),
        ],
    ),
];

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        for part in render::wrapped_cells(word, width.max(1), usize::MAX) {
            if !line.is_empty() && render::width(&line) + 1 + render::width(&part) > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&part);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

pub fn lines(width: usize, theme: Theme) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let key_width = SECTIONS
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .map(|(key, _)| render::width(key))
        .max()
        .unwrap_or_default();
    let keys = theme.normal().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    for (section_index, (title, entries)) in SECTIONS.iter().enumerate() {
        if section_index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            render::crop(title, width),
            theme.accent().add_modifier(Modifier::BOLD),
        ));
        for (key, action) in *entries {
            if width >= 58 {
                for (index, text) in wrap_words(action, width - key_width - 2)
                    .into_iter()
                    .enumerate()
                {
                    let key = if index == 0 { *key } else { "" };
                    lines.push(Line::from(vec![
                        Span::styled(key.to_owned(), keys),
                        Span::raw(" ".repeat(key_width + 2 - render::width(key))),
                        Span::styled(text, theme.normal()),
                    ]));
                }
            } else {
                lines.extend(
                    wrap_words(key, width)
                        .into_iter()
                        .map(|text| Line::styled(text, keys)),
                );
                let indent = width.saturating_sub(1).min(2);
                lines.extend(wrap_words(action, width - indent).into_iter().map(|text| {
                    Line::styled(format!("{}{text}", " ".repeat(indent)), theme.normal())
                }));
            }
        }
        if *title == "Search" {
            lines.extend(
                wrap_words("Every word must match. Current screen only.", width)
                    .into_iter()
                    .map(|text| Line::styled(text, theme.muted())),
            );
        }
    }
    lines
}
