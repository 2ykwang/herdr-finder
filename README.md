# Herdr Finder

Find running agents and terminals by name or visible output. Browse workspace and
tab groups, then jump to the matching session.

![Browsing, searching terminal text, and opening the matching session](assets/demo.gif)

- **Search terminal text.** Search names, agents, paths, and the current screen.
  All words must match. Matching lines show highlighted context.
- **Browse by workspace.** Sessions are grouped by workspace and tab, with agent
  labels and status.
- **Jump to a session.** Press Enter or double-click to open its terminal.

![Terminal text search with highlighted context](assets/search.png)

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
| Enter | Open a session or toggle a group |
| Click / double-click | Select / open a session |
| F1 | Help |

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
