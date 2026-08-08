# Session Relay

Session Relay is a cross-session, cross-project, cross-tool message bus for Claude Code and Codex. It provides hooks, MCP tools, a CLI, and an agent skill.

## Supported platforms

Session Relay supports Linux only, on x86-64 and arm64. It does not support macOS or Windows.

## Install from GitHub Releases

Download one binary from the [latest release](https://github.com/DocksDocks/session-relay/releases/latest):

- `session-relay-x86_64-unknown-linux-musl` for x86-64
- `session-relay-aarch64-unknown-linux-musl` for arm64

Set `target` for the current machine. Then download and verify the selected binary.

```bash
target=x86_64-unknown-linux-musl
# Use target=aarch64-unknown-linux-musl on arm64.
curl -fLO "https://github.com/DocksDocks/session-relay/releases/latest/download/session-relay-$target"
curl -fLO "https://github.com/DocksDocks/session-relay/releases/latest/download/SHA256SUMS"
sha256sum --ignore-missing --check SHA256SUMS
chmod +x "session-relay-$target"
install -Dm755 "session-relay-$target" "$HOME/.local/bin/session-relay"
```

Each release publishes `SHA256SUMS` alongside both binaries.

## Add the marketplace

Add the Claude Code marketplace. Then install the plugin from its verified `session-relay` marketplace entry.

```text
/plugin marketplace add DocksDocks/session-relay
/plugin install session-relay@session-relay
```

The repository ships its Codex catalog at `.agents/plugins/marketplace.json`.

The plugin also remains reachable through the docks Claude marketplace by redirect.
