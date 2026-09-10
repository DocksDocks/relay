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

## Migrate from session-relay

Version 0.19.0 renamed the project from `session-relay` to `relay`. The store under `~/.agent-relay` and the `AGENT_RELAY_HOME` variable did not change, so existing mail and registrations stay in place. Do not keep both plugin identities installed: the two extensions register the same tool and command.

1. Uninstall the old plugin from each scope where it is installed, then remove the old marketplace entry:

   ```bash
   omp plugin uninstall session-relay --scope project
   omp plugin uninstall session-relay
   omp plugin marketplace remove session-relay
   ```

2. Remove the old binary, then install the new one as `~/.local/bin/relay` with the commands above:

   ```bash
   rm -f "$HOME/.local/bin/session-relay"
   ```

3. Replace `SESSION_RELAY_BIN` with `RELAY_BIN` in any launcher override. The `SESSION_RELAY_HOME` alias no longer exists; set `AGENT_RELAY_HOME` when the store must live elsewhere.
4. Add the marketplace at the new URL and install `relay@relay` as shown above. The old marketplace name does not resolve to the new plugin.
