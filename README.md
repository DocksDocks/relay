# Relay

Relay provides durable mail between omp sessions across projects: hold/ack/rollback, correlated request/reply, hook delivery, wake, and watch. It provides an omp extension with a `relay` tool and `/relay` command, a CLI, and an agent skill.

## Supported platforms

Relay distributes Linux x86-64 and arm64 musl binaries.

## Install from GitHub Releases

Download one binary from the [latest release](https://github.com/DocksDocks/relay/releases/latest):

- `relay-x86_64-unknown-linux-musl` for x86-64
- `relay-aarch64-unknown-linux-musl` for arm64

Set `target` for the current machine. Then download and verify the selected binary.

```bash
target=x86_64-unknown-linux-musl
# Use target=aarch64-unknown-linux-musl on arm64.
curl -fLO "https://github.com/DocksDocks/relay/releases/latest/download/relay-$target"
curl -fLO "https://github.com/DocksDocks/relay/releases/latest/download/SHA256SUMS"
sha256sum --ignore-missing --check SHA256SUMS
chmod +x "relay-$target"
install -Dm755 "relay-$target" "$HOME/.local/bin/relay"
```

Each release publishes `SHA256SUMS` alongside both binaries.

## Add the marketplace

Add the omp marketplace from the repository URL or from a local checkout. Then install the plugin into the current project and restart the omp session.

```bash
omp plugin marketplace add https://github.com/DocksDocks/relay.git
# Or: omp plugin marketplace add /path/to/relay
omp plugin install relay@relay --scope project
```

The repository ships its catalog at `.omp-plugin/marketplace.json`. omp copies the directory named by the catalog `source`, which is `plugin/`.
