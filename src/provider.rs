// SPDX-License-Identifier: Apache-2.0
// Native kinds, aliases, and executable normalization follow Herdr v0.9.5,
// src/detect/mod.rs at commit 2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01.
// Modified into a presentation catalog shared by search and rendering.
// Native session identity is never rewritten.
// See third-party/herdr-LICENSE.

use crate::model::clean_text;

pub const MAX_LABEL_CELLS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorRole {
    Text,
    Peach,
    Blue,
    Green,
    Mauve,
    Yellow,
}

#[derive(Debug)]
pub struct Provider {
    pub kind: &'static str,
    pub aliases: &'static [&'static str],
    pub label: &'static str,
    pub color: ColorRole,
}

// Brand families use approximate ANSI slots, never fixed product RGB values.
// Text is an explicit neutral choice for monochrome or unconfirmed branding.
pub const NATIVE_CATALOG: &[Provider] = &[
    Provider {
        kind: "pi",
        aliases: &[],
        label: "Pi",
        color: ColorRole::Text,
    },
    Provider {
        kind: "claude",
        aliases: &["claude-code"],
        label: "Claude",
        color: ColorRole::Peach,
    },
    Provider {
        kind: "codex",
        aliases: &[],
        label: "Codex",
        color: ColorRole::Text,
    },
    Provider {
        kind: "gemini",
        aliases: &[],
        label: "Gemini",
        color: ColorRole::Blue,
    },
    Provider {
        kind: "cursor",
        aliases: &["cursor-agent"],
        label: "Cursor",
        color: ColorRole::Text,
    },
    Provider {
        kind: "devin",
        aliases: &["devin-cli", "devin cli"],
        label: "Devin",
        color: ColorRole::Text,
    },
    Provider {
        kind: "agy",
        aliases: &["antigravity", "antigravity-cli"],
        label: "Antigravity",
        color: ColorRole::Text,
    },
    Provider {
        kind: "cline",
        aliases: &[".cline"],
        label: "Cline",
        color: ColorRole::Text,
    },
    Provider {
        kind: "omp",
        aliases: &[],
        label: "Oh My Pi",
        color: ColorRole::Peach,
    },
    Provider {
        kind: "mastracode",
        aliases: &["mastra-code", "mastra code"],
        label: "Mastra Code",
        color: ColorRole::Green,
    },
    Provider {
        kind: "opencode",
        aliases: &["opencode2", "open-code"],
        label: "OpenCode",
        color: ColorRole::Text,
    },
    Provider {
        kind: "copilot",
        aliases: &["github-copilot", "ghcs"],
        label: "Copilot",
        color: ColorRole::Text,
    },
    Provider {
        kind: "kimi",
        aliases: &["kimi-code", "kimi code"],
        label: "Kimi",
        color: ColorRole::Blue,
    },
    Provider {
        kind: "kiro",
        aliases: &["kiro-cli"],
        label: "Kiro",
        color: ColorRole::Mauve,
    },
    Provider {
        kind: "droid",
        aliases: &[],
        label: "Droid",
        color: ColorRole::Text,
    },
    Provider {
        kind: "amp",
        aliases: &["amp-local"],
        label: "Amp",
        color: ColorRole::Text,
    },
    Provider {
        kind: "grok",
        aliases: &["grok-build"],
        label: "Grok Build",
        color: ColorRole::Text,
    },
    Provider {
        kind: "hermes",
        aliases: &["hermes-agent"],
        label: "Hermes",
        color: ColorRole::Yellow,
    },
    Provider {
        kind: "kilo",
        aliases: &["kilo-code", "kilo code"],
        label: "Kilo Code",
        color: ColorRole::Yellow,
    },
    Provider {
        kind: "qodercli",
        aliases: &["qoderclicn", "qoder", "qodercn"],
        label: "Qoder CLI",
        color: ColorRole::Text,
    },
    Provider {
        kind: "qwen",
        aliases: &["qwen-code", "qwen code"],
        label: "Qwen Code",
        color: ColorRole::Blue,
    },
    Provider {
        kind: "letta",
        aliases: &["letta-code", "letta code"],
        label: "Letta Code",
        color: ColorRole::Mauve,
    },
    Provider {
        kind: "maki",
        aliases: &[],
        label: "Maki",
        color: ColorRole::Peach,
    },
    Provider {
        kind: "muse",
        aliases: &["muse-code", "muse-cli"],
        label: "Muse Code",
        color: ColorRole::Text,
    },
];

// Vendor reporters use exact wire labels, not native executable aliases.
// Verified in charmbracelet/crush@8f1f0295 internal/herdr/client.go and
// PrimeIntellect-ai/prime-agent@74591cdd crates/pa-daemon/src/herdr.rs.
// Prime Agent's source "herdr:pi" does not make its agent label a Pi alias.
pub const REPORTED_CATALOG: &[Provider] = &[
    Provider {
        kind: "crush",
        aliases: &[],
        label: "Crush",
        color: ColorRole::Mauve,
    },
    Provider {
        kind: "prime-agent",
        aliases: &[],
        label: "Prime Agent",
        color: ColorRole::Mauve,
    },
];

pub fn catalog() -> impl Iterator<Item = &'static Provider> {
    REPORTED_CATALOG.iter().chain(NATIVE_CATALOG.iter())
}

pub fn lookup(kind: &str) -> Option<&'static Provider> {
    if let Some(provider) = catalog().find(|provider| provider.kind == kind) {
        return Some(provider);
    }

    let mut normalized = kind.trim().to_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js"] {
        if normalized.ends_with(suffix) {
            normalized.truncate(normalized.len() - suffix.len());
            break;
        }
    }
    let basename = normalized
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(&normalized);

    // Native normalization must not invent aliases for vendor-reported labels.
    NATIVE_CATALOG
        .iter()
        .find(|provider| provider.kind == basename || provider.aliases.contains(&basename))
        .or_else(|| {
            // Muse launchers exec versioned binaries. A digit excludes unrelated muse-* tools.
            basename
                .strip_prefix("muse-bin-")
                .is_some_and(|version| version.starts_with(|ch: char| ch.is_ascii_digit()))
                .then(|| {
                    NATIVE_CATALOG
                        .iter()
                        .find(|provider| provider.kind == "muse")
                })
                .flatten()
        })
}

pub fn is_terminal(kind: &str) -> bool {
    kind.trim().eq_ignore_ascii_case("terminal")
}

pub fn label(kind: &str, display: &str) -> String {
    if is_terminal(kind) {
        return "Terminal".to_owned();
    }
    if let Some(provider) = lookup(kind) {
        return provider.label.to_owned();
    }

    let display = clean_text(display, false);
    if !display.is_empty() {
        return display;
    }
    let kind = clean_text(kind, false);
    if kind.is_empty() {
        "Agent".to_owned()
    } else {
        kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use unicode_width::UnicodeWidthStr;

    // Agent::ALL and agent_label in Herdr v0.9.5 define the native inventory.
    const NATIVE_KINDS: &str = "pi claude codex gemini cursor devin agy cline omp mastracode opencode copilot kimi kiro droid amp grok hermes kilo qodercli qwen letta maki muse";

    #[test]
    fn catalog_covers_the_complete_native_inventory_without_ambiguous_names() {
        let inventory: Vec<_> = NATIVE_KINDS.split_whitespace().collect();
        assert_eq!(inventory.len(), 24);
        assert_eq!(NATIVE_CATALOG.len(), inventory.len());
        for kind in inventory {
            assert_eq!(lookup(kind).map(|provider| provider.kind), Some(kind));
        }

        let mut identifiers = HashSet::new();
        for provider in catalog() {
            assert!(identifiers.insert(provider.kind));
            assert!(!provider.label.is_empty());
            assert_eq!(clean_text(provider.label, false), provider.label);
            assert!(UnicodeWidthStr::width(provider.label) <= MAX_LABEL_CELLS);
            for &alias in provider.aliases {
                assert!(identifiers.insert(alias));
                assert_eq!(lookup(alias).map(|entry| entry.kind), Some(provider.kind));
            }
        }
    }

    #[test]
    fn native_aliases_keep_their_identity_through_case_paths_and_one_suffix() {
        for provider in NATIVE_CATALOG {
            for name in std::iter::once(provider.kind).chain(provider.aliases.iter().copied()) {
                for suffix in ["", ".exe", ".cmd", ".bat", ".ps1", ".js"] {
                    let path = format!(" C:\\Tools\\{}{suffix} ", name.to_uppercase());
                    assert_eq!(lookup(&path).map(|entry| entry.kind), Some(provider.kind));
                }
                let path = format!("/opt/agents/{name}");
                assert_eq!(lookup(&path).map(|entry| entry.kind), Some(provider.kind));
            }
        }
        assert!(lookup("claude-code.exe.cmd").is_none());
        assert!(lookup("grok-cli").is_none());
    }

    #[test]
    fn muse_versioned_binary_rule_requires_an_immediate_ascii_digit() {
        for name in [
            "muse-bin-0.1.0-R708.1",
            "/opt/agents/muse-bin-1.2.3",
            r"C:\Agents\MUSE-BIN-0.2.1.EXE",
        ] {
            assert_eq!(lookup(name).map(|provider| provider.kind), Some("muse"));
        }
        for name in [
            "muse-bin",
            "muse-bin-",
            "muse-binary",
            "muse-bin-release-1",
            "muse-bin-\u{661}.0",
            "musescore",
            "muse-helper",
        ] {
            assert!(lookup(name).is_none(), "{name}");
        }
    }

    #[test]
    fn known_labels_are_stable_and_unknown_labels_remain_literal() {
        assert_eq!(label("claude-code", "Personal display"), "Claude");
        assert_eq!(label("terminal", "Shell label"), "Terminal");
        assert_eq!(
            label("custom-agent", "[bold]Custom[/bold]"),
            "[bold]Custom[/bold]"
        );
        assert_eq!(label("CustomAgent", ""), "CustomAgent");
        assert_eq!(label("", ""), "Agent");
        assert_eq!(label("custom", "\u{1b}[31mLabel\u{1b}[0m\u{202e}"), "Label");
        assert!(lookup("my-claude-helper").is_none());
    }

    #[test]
    fn reported_kinds_are_exact_and_display_overrides_do_not_establish_identity() {
        assert_eq!(REPORTED_CATALOG.len(), 2);
        for provider in REPORTED_CATALOG {
            assert!(provider.aliases.is_empty());
            assert_eq!(
                lookup(provider.kind).map(|entry| entry.kind),
                Some(provider.kind)
            );
        }
        for kind in [
            "Crush",
            "CRUSH",
            " crush",
            "crush ",
            "/opt/crush",
            "crush.exe",
            "Prime-Agent",
            "PRIME-AGENT",
            "prime-agent.cmd",
            "herdr:pi",
            "command-code",
            "commandcode",
            "cmd",
            "cmdc",
        ] {
            assert!(lookup(kind).is_none(), "{kind}");
        }
        assert_ne!(
            lookup("prime-agent").unwrap().kind,
            lookup("pi").unwrap().kind
        );
        assert_eq!(label("pi", "Crush"), "Pi");
        assert_eq!(label("unregistered", "Command Code"), "Command Code");
    }
}
