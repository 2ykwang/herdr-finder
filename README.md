# Herdr Finder

Find running agents and terminals by name or visible output. Keep workspace and
tab groups while searching, preview the selected screen, then jump to its session.

- **Search by task or output.** Use fuzzy task names, literal screen text, and
  field filters. Every condition must match. Results stay grouped and rank by
  their strongest match.
- **Preview before opening.** Read the selected terminal's visible screen without
  changing its focus. The preview shows its path, status, and search evidence.
- **Scan a compact tree.** Each session occupies one line. Workspace and tab
  context appears in group headers.
- **Jump to a session.** Press Enter or double-click to open its terminal.

## Install

Herdr 0.9.5+ on macOS 11+ (Apple Silicon/Intel) or Linux (x64/ARM64).
Requires `curl`, `tar`, and `sha256sum` or `shasum`.

```sh
herdr plugin install 2ykwang/herdr-finder
```

Add the shortcut to `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+shift+f"
type = "plugin_action"
command = "herdr-finder.open"
description = "Find a running session"
```

```sh
herdr config check
herdr server reload-config
```

Press your Herdr prefix, then **Shift+F**.
Update with `herdr plugin update herdr-finder`.

## Controls

| Key | Action |
| --- | --- |
| Up / Down | Select a row |
| Left / Right | Jump between tabs; move the text cursor while searching |
| Alt+Left / Alt+Right | Jump between workspaces while browsing |
| `/` | Start searching |
| Esc | Return to browsing, then close |
| Enter while searching | Keep results and return to browsing |
| Enter while browsing | Open a session or toggle a group |
| Click / double-click | Select / open a session |
| Tab | Show or hide screen preview |
| Alt+Up / Alt+Down | Scroll the preview |
| Ctrl+R | Refresh sessions and screen text |
| a / b / w / d / i | Show all / needs input / working / unseen done / idle |
| F1 | Help |

The preview appears beside the tree on wide terminals and below it on narrower
terminals. Small terminals show selected-session details until more space is
available. Mouse scrolling over the preview scrolls its content.

Workspace headings use a cyan accent and matching text uses yellow. Palette and
foreground/background colors are queried through OSC when supported, with readable
fallbacks for light and dark terminals. `NO_COLOR` keeps bold and underline without
color accents.

## Search

Press `/` to search. Bare words fuzzy-match titles, agent names, workspace labels,
and tab labels. Paths, agent kinds, statuses, and screen text use normalized literal
matching. Metadata literal matches rank above screen matches, then fuzzy matches.
Within each group, stronger matches come first. Searching orders matching groups
by their best result. Browsing puts the current workspace first, then workspaces
by their most recent focus. Workspaces without recorded visits keep Herdr's order.
Tabs and panels keep their native order. Browse order stays fixed while Finder is
open, and new workspaces appear at the end.

The installed plugin records workspace focus events locally in its state directory,
separately for each Herdr session socket. Visit history starts when the plugin is
enabled; terminal output and agent status changes don't affect it.

| Query | Matches |
| --- | --- |
| `pymnt` | Task names such as Payment |
| `agent:claude status:blocked` | Claude agents waiting for input |
| `ws:commerce path:/api` | A workspace and directory |
| `title:payment` | Literal title text and title aliases |
| `screen:"connection refused"` | A phrase on the visible screen |
| `!payment` | Sessions whose metadata and successfully read screen lack that literal text |

Double quotes make a literal phrase. Known field prefixes are lowercase. Unknown
prefixes and URLs are treated as literal text. OR operators, regular expressions,
and quote escaping aren't supported. An unfinished quote or empty field value
shows an error until corrected.

Finder searches visible screens, up to 300 lines and 64 KiB per session. It doesn't
search full conversation history. Selected-screen previews refresh about every two
seconds and report their age or read errors. `done` is Herdr's unseen-completion
state, not a success verdict or permanent completion history.

## Develop

Install Rust and a C toolchain, then build the local checkout:

```sh
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
sh scripts/build.sh
herdr plugin link .
```

Unlink your local checkout before switching to a GitHub-managed installation.

MIT and Apache-2.0. See [LICENSE](LICENSE) and [third-party notices](THIRD_PARTY.md).
