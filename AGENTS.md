# Session Relay repository

This repository holds the Session Relay Rust crate, its Node harness, and its release infrastructure. The separate `plugin/` directory contains only the shipped payload.

## Commands

Run setup once. Then run the repository gate.

```bash
corepack enable && pnpm install --frozen-lockfile
node scripts/gate.mjs
```

The gate is authoritative.

## Repository scope

The tracked top-level inventory and current working tree define this layout:

```text
.
├── Cargo.toml                         Rust package and explicit integration targets
├── Cargo.lock                         locked Rust dependencies
├── rust-toolchain.toml                pinned Rust toolchain
├── src/                               crate sources
│   └── tests/                         13 explicit [[test]] integration targets
├── test/                              Node scenario and contract harness
├── plugin/                            shipped plugin payload
├── .claude-plugin/marketplace.json    Claude Code marketplace catalog
├── .agents/plugins/marketplace.json   Codex marketplace catalog
├── .github/workflows/                 CI and release workflows
├── docs/crate-map.md                  crate map
├── scripts/gate.mjs                   authoritative repository gate
├── package.json                       Node commands and tool versions
├── biome.json                         JavaScript formatting and lint rules
├── README.md                          installation guide
├── AGENTS.md                          repository rules
├── CLAUDE.md                          Claude Code context-tree import
└── LICENSE                            repository license
```

`Cargo.toml` sets `autotests = false`. It declares all 13 integration targets with explicit `[[test]]` entries under `src/tests/`.

## Payload boundary

Consumers receive `plugin/`. Claude Code copies the plugin directory named by marketplace `source`, not the repository.

Only these first-level entries are allowed under `plugin/`:

- `.claude-plugin`
- `.codex-plugin`
- `skills`
- `hooks`
- `commands`
- `agents`
- `bin`
- `README.md`
- `AGENTS.md`
- `CLAUDE.md`
- `LICENSE`

`test/distribution-contract.mjs` enforces this boundary with `git ls-files`. It provides no exemption mechanism.

## Version

Pin the version in exactly four files:

- `Cargo.toml`
- `plugin/.claude-plugin/plugin.json`
- `plugin/.codex-plugin/plugin.json`
- `.claude-plugin/marketplace.json`

Move all four pins together. `test/distribution-contract.mjs` asserts their lockstep.

## Release

Push a `v<X.Y.Z>` tag. `.github/workflows/release.yml` builds these Linux musl targets:

- `x86_64-unknown-linux-musl` as `session-relay-x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl` as `session-relay-aarch64-unknown-linux-musl`

The workflow aggregates attestations and `SHA256SUMS`. It publishes exactly three assets: two binaries and the checksum file.

The release has no prerelease step. It has no promotion step.

## Platform support

Session Relay supports Linux only, on x86-64 and arm64. Managed workspace custody requires cgroup v2, pidfd, Landlock, and seccomp.

## Provenance

This repository came from `git subtree split --prefix=plugins/session-relay` in `DocksDocks/docks` at commit `2e973cbf08e6be28da260f1f0f48643afb58ac42`.

The split branch head was `161d045f6d0e3ec36f692bcf662444b3506a28ae`. Per-file history before the split remains preserved, so both histories remain joinable.

## Context tree

The repository has two context nodes:

- Root `AGENTS.md` with root `CLAUDE.md`
- `plugin/AGENTS.md` with `plugin/CLAUDE.md`

The `CLAUDE.md` files exist because Claude Code descends `CLAUDE.md`, not `AGENTS.md`.
