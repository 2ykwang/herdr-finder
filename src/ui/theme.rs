//! ANSI accents with neutral shades derived from terminal foreground and background.

use super::terminal_colors::TerminalColors;
use crate::provider::{self, ColorRole};
use ratatui::style::{Color, Modifier, Style};
use std::ffi::OsStr;

type Rgb = [u8; 3];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shades {
    foreground: Rgb,
    text: Rgb,
    muted: Rgb,
    separator: Rgb,
    selected: Rgb,
    excerpt: Rgb,
    selected_excerpt: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    colors_enabled: bool,
    shades: Option<Shades>,
}

impl Theme {
    pub const fn new(colors_enabled: bool) -> Self {
        Self {
            colors_enabled,
            shades: None,
        }
    }

    pub fn from_env() -> Self {
        Self::new(colors_allowed(std::env::var_os("NO_COLOR").as_deref()))
    }

    pub const fn colors_enabled(self) -> bool {
        self.colors_enabled
    }

    pub fn with_terminal_colors(mut self, colors: TerminalColors) -> Self {
        if self.colors_enabled {
            let fg = colors.foreground;
            let bg = colors.background;
            if contrast(fg, bg) < 4.5 {
                return self;
            }
            let selected = surface(fg, bg, 7);
            let excerpt = surface(fg, bg, 4);
            let selected_excerpt = surface(fg, bg, 10);
            let backgrounds = [bg, selected, excerpt, selected_excerpt];
            self.shades = Some(Shades {
                foreground: fg,
                text: readable_mix(fg, bg, 86, 5.0, &backgrounds),
                muted: readable_mix(fg, bg, 62, 4.5, &backgrounds),
                separator: mix(fg, bg, 35),
                selected,
                excerpt,
                selected_excerpt,
            });
        }
        self
    }

    pub fn normal(self) -> Style {
        Style::new()
            .fg(self.shades.map_or(Color::Reset, |s| rgb(s.text)))
            .bg(Color::Reset)
    }

    fn color(self, color: Color) -> Style {
        self.normal().fg(if self.colors_enabled {
            color
        } else {
            Color::Reset
        })
    }

    pub fn accent(self) -> Style {
        self.color(Color::Cyan)
    }

    pub fn separator(self) -> Style {
        self.shades
            .map_or_else(|| self.muted(), |s| self.normal().fg(rgb(s.separator)))
    }

    pub fn workspace(self) -> Style {
        self.normal().add_modifier(Modifier::BOLD)
    }

    pub fn tab(self) -> Style {
        self.muted()
    }

    pub fn muted(self) -> Style {
        self.shades.map_or_else(
            || self.normal().add_modifier(Modifier::DIM),
            |s| self.normal().fg(rgb(s.muted)),
        )
    }

    pub fn selected(self) -> Style {
        self.normal()
            .bg(self.shades.map_or(Color::Reset, |s| rgb(s.selected)))
            .remove_modifier(Modifier::REVERSED)
    }

    pub fn highlight(self) -> Style {
        self.normal()
            .fg(self.shades.map_or(Color::Reset, |s| rgb(s.foreground)))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }

    pub fn excerpt(self, selected: bool) -> Style {
        self.muted().bg(self.shades.map_or(Color::Reset, |s| {
            rgb(if selected {
                s.selected_excerpt
            } else {
                s.excerpt
            })
        }))
    }

    pub fn status(self, status: &str) -> Style {
        let color = match status {
            "working" => Color::Yellow,
            "blocked" => Color::Red,
            "done" => Color::Cyan,
            "idle" => Color::Green,
            _ => return self.muted(),
        };
        self.color(color)
    }

    pub fn provider(self, kind: &str) -> Style {
        if provider::is_terminal(kind) {
            return self.muted();
        }
        let role = provider::lookup(kind).map_or(ColorRole::Text, |provider| provider.color);
        // ANSI has no orange slot. Warm brand families share the terminal's yellow.
        let color = match role {
            ColorRole::Text => return self.muted(),
            ColorRole::Peach | ColorRole::Yellow => Color::Yellow,
            ColorRole::Blue => Color::Blue,
            ColorRole::Green => Color::Green,
            ColorRole::Mauve => Color::Magenta,
        };
        self.color(color)
    }
}

fn rgb([r, g, b]: Rgb) -> Color {
    Color::Rgb(r, g, b)
}

fn mix(foreground: Rgb, background: Rgb, percent: u16) -> Rgb {
    std::array::from_fn(|i| {
        ((u16::from(foreground[i]) * percent + u16::from(background[i]) * (100 - percent) + 50)
            / 100) as u8
    })
}

fn surface(fg: Rgb, bg: Rgb, amount: u16) -> Rgb {
    (0..=amount)
        .rev()
        .map(|percent| mix(fg, bg, percent))
        .find(|background| contrast(fg, *background) >= 4.5)
        .unwrap_or(bg)
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let luminance = |color: Rgb| {
        let channel = |value: u8| {
            let v = f64::from(value) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color[0]) + 0.7152 * channel(color[1]) + 0.0722 * channel(color[2])
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn readable_mix(fg: Rgb, bg: Rgb, start: u16, minimum: f64, backgrounds: &[Rgb]) -> Rgb {
    // Do not reduce an already-low contrast chosen by the terminal's owner.
    (start..=100)
        .map(|amount| mix(fg, bg, amount))
        .find(|color| {
            backgrounds
                .iter()
                .all(|background| contrast(*color, *background) >= minimum)
        })
        .unwrap_or(fg)
}

fn colors_allowed(no_color: Option<&OsStr>) -> bool {
    no_color.is_none_or(OsStr::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_provider_accents_survive_without_bold_labels_or_rgb_queries() {
        let theme = Theme::new(true);
        for (kind, color) in [
            ("claude", Color::Yellow),
            ("gemini", Color::Blue),
            ("kiro", Color::Magenta),
            ("mastracode", Color::Green),
            ("codex", Color::Reset),
        ] {
            let style = theme.provider(kind);
            assert_eq!(style.fg, Some(color));
            assert!(!style.add_modifier.contains(Modifier::BOLD));
        }
        assert!(!theme.normal().add_modifier.contains(Modifier::BOLD));
        assert!(!theme.selected().add_modifier.contains(Modifier::BOLD));
        assert!(theme.workspace().add_modifier.contains(Modifier::BOLD));
        assert!(
            theme
                .highlight()
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(theme.selected().bg, Some(Color::Reset));
    }

    #[test]
    fn light_dark_and_tinted_terminals_derive_readable_subtle_shades() {
        for (foreground, background) in [
            ([255, 255, 255], [0, 0, 0]),
            ([0, 0, 0], [255, 255, 255]),
            ([171, 178, 191], [40, 44, 52]),
            ([40, 48, 64], [250, 246, 230]),
        ] {
            let theme = Theme::new(true).with_terminal_colors(TerminalColors {
                foreground,
                background,
            });
            let s = theme.shades.unwrap();
            assert_eq!(theme.normal().bg, Some(Color::Reset));
            assert_ne!(s.text, foreground);
            assert_ne!(s.muted, s.text);
            for bg in [background, s.selected, s.excerpt, s.selected_excerpt] {
                assert!(contrast(s.text, bg) >= 5.0);
                assert!(contrast(s.muted, bg) >= 4.5);
            }
            assert_ne!(s.excerpt, background);
            assert_ne!(s.selected_excerpt, s.excerpt);
            assert!(!theme.selected().add_modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn low_contrast_terminal_text_is_not_dimmed_further() {
        let fg = [120, 120, 120];
        let bg = [110, 110, 110];
        let theme = Theme::new(true).with_terminal_colors(TerminalColors {
            foreground: fg,
            background: bg,
        });
        assert_eq!(theme, Theme::new(true));
    }

    #[test]
    fn no_color_does_not_query_or_apply_rgb_and_keeps_structural_emphasis() {
        let theme = Theme::new(false).with_terminal_colors(TerminalColors {
            foreground: [255; 3],
            background: [0; 3],
        });
        assert!(!theme.colors_enabled());
        assert!(theme.shades.is_none());
        for style in [
            theme.normal(),
            theme.muted(),
            theme.selected(),
            theme.highlight(),
            theme.excerpt(true),
            theme.status("working"),
        ]
        .into_iter()
        .chain(provider::catalog().map(|p| theme.provider(p.kind)))
        {
            assert_eq!(style.fg, Some(Color::Reset));
            assert_eq!(style.bg, Some(Color::Reset));
        }
        assert!(theme.muted().add_modifier.contains(Modifier::DIM));
        assert!(theme.workspace().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn nonempty_no_color_disables_accents() {
        assert!(colors_allowed(None));
        assert!(colors_allowed(Some(OsStr::new(""))));
        for value in ["1", "0", "false", "disabled"] {
            assert!(!colors_allowed(Some(OsStr::new(value))));
        }
    }
}
