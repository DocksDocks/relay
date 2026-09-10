# Relay repository

This repository holds the Relay Rust crate for durable mail between omp sessions, its Node harness, and its release infrastructure. The separate `plugin/` directory contains only the shipped payload.

## Commands

Run setup once. Then run the repository gate.

```bash
corepack enable && pnpm install --frozen-lockfile
rustup toolchain install
sh scripts/install-cargo-deny.sh
node scripts/gate.mjs
```

`rustup toolchain install` reads `rust-toolchain.toml`. It installs `rustfmt`, `clippy`, and `rust-analyzer` for the pinned channel.

`scripts/install-cargo-deny.sh` installs the pinned `cargo-deny` release:

- It selects the x86-64 or arm64 tarball.
- It verifies the pinned SHA-256.
- It copies the binary into the cargo bin directory.

`.github/workflows/ci.yml` calls the same script. The gate refuses any other `cargo-deny` version.

`.omp/lsp.json` starts `rust-analyzer` from the cargo bin directory. The harness `lsp` tool then serves Rust even when the cargo bin directory is not on `PATH`.

The gate is authoritative.

## Repository scope

The tracked top-level inventory and current working tree define this layout:

```text
.
├── Cargo.toml                         Rust package, lint policy, explicit integration targets
├── Cargo.lock                         locked Rust dependencies
├── rust-toolchain.toml                pinned Rust toolchain and components
├── clippy.toml                        clippy test exemptions
├── .omp/lsp.json                      rust-analyzer launch for the harness lsp tool
├── deny.toml                          cargo-deny supply-chain policy
├── src/                               crate sources
│   └── tests/                         five explicit [[test]] integration targets
├── test/                              Node scenario and contract harness
├── plugin/                            shipped plugin payload
├── .omp-plugin/marketplace.json       omp marketplace catalog
├── .github/workflows/                 CI and release workflows
├── docs/                              plan record standard, routing node, crate map
├── scripts/gate.mjs                   authoritative repository gate
├── scripts/install-cargo-deny.sh      pinned cargo-deny installer
├── package.json                       Node commands and tool versions
├── biome.json                         JavaScript formatting and lint rules
├── README.md                          installation guide
├── AGENTS.md                          repository rules
└── LICENSE                            repository license
```

`Cargo.toml` sets `autotests = false`. It declares five integration targets with explicit `[[test]]` entries under `src/tests/`: `bus_smoke`, `protocol`, `lock_race`, `holds`, and `watch`.


## Rust code

The lint policy is machine-enforced:

- `Cargo.toml` `[lints]` sets the lint levels.
- `clippy.toml` exempts tests from the panic-family lints.
- The gate runs clippy with `-D warnings`.

When a rule below has a lint, the lint is the rule. The text explains the intent.

### Toolchain and dependencies

- Keep the toolchain and the crate `rust-version` on one channel. Move `rust-toolchain.toml` and `Cargo.toml` `rust-version` together.
- The crate has three runtime dependencies: `tinyjson`, `libc`, and `rustix`.
- Before you add a dependency, get an `ask` decision.
- A new dependency must pass `cargo deny --locked --offline check`. The policy allows MIT or Apache-2.0 licenses, crates.io only, no wildcard versions, and no duplicate versions.

### Suppressions

- Do not write `#[allow(...)]`.
- The five integration test roots under `src/tests/` carry the only crate-level `allow`.
- For a justified exception, use `#[expect(lint, reason = "...")]`. The compiler reports the attribute when it becomes unnecessary.

### Panics

- Do not call `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`, or `unimplemented!` outside tests.
- Library code returns `Result`. The CLI exits through the `die` helpers with a user-facing message.
- Tests may panic. A failed assertion is the intended signal.
- One exception exists. `store::uuid_v4` keeps two `expect` calls under `#[expect(clippy::expect_used, reason = ...)]`, because a failed entropy read is unrecoverable and the abort is intentional.
- A new exception needs the same shape: an unrecoverable condition, one function, and a `reason` that names the invariant.

### Unsafe and casts

- Every `unsafe` block holds one operation.
- Put a `// SAFETY:` comment directly above the block. The comment names the invariant.
- The crate has two such blocks. Both are `libc` calls.
- Numeric casts state their contract. Use `T::try_from(x)` with an error path or a commented saturating fallback.
- Clamp a float before you narrow it. Add `#[expect(clippy::cast_possible_truncation, reason = "...")]` only on that clamped line.

### Signatures and tools

- Borrow what you do not consume. Take `&T` unless the function stores or moves the value.
- Use the `lsp` tool for definitions, references, and renames. Text search misses shadowed and re-exported symbols.
- Tests live in the five explicit `[[test]]` targets and in inline `#[cfg(test)]` modules.
- After you add or remove a test in an integration target, run `node test/rust-test-inventory.mjs --generate`. Commit the fixture. Inline unit tests are discovered live and need no fixture change.
- Run `cargo fmt` before the gate. The gate runs `cargo fmt --check` first and fails on any difference.

## Payload boundary

Consumers receive `plugin/`. omp copies the plugin directory named by the catalog `source`, not the repository.

Only these first-level entries are allowed under `plugin/`:

- `package.json`
- `extension`
- `skills`
- `bin`
- `README.md`
- `AGENTS.md`
- `LICENSE`

`test/distribution-contract.mjs` enforces this boundary with `git ls-files`. It provides no exemption mechanism.

## Version

Pin the version in exactly three files:

- `Cargo.toml`
- `plugin/package.json`
- `.omp-plugin/marketplace.json`

Move all three pins together. `test/distribution-contract.mjs` asserts their lockstep.

## Release

Push a `v<X.Y.Z>` tag. `.github/workflows/release.yml` builds these Linux musl targets:

- `x86_64-unknown-linux-musl` as `relay-x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl` as `relay-aarch64-unknown-linux-musl`

The workflow aggregates attestations and `SHA256SUMS`. It publishes exactly three assets: two binaries and the checksum file.

The release has no prerelease step. It has no promotion step.

## Platform support

Relay distributes Linux x86-64 and arm64 musl binaries.

## Provenance

This repository came from `git subtree split --prefix=plugins/session-relay` in `DocksDocks/docks` at commit `2e973cbf08e6be28da260f1f0f48643afb58ac42`.

The split branch head was `161d045f6d0e3ec36f692bcf662444b3506a28ae`. Per-file history before the split remains preserved, so both histories remain joinable.

## Context tree

The repository has three context nodes:

- Root `AGENTS.md` with root `CLAUDE.md`
- `plugin/AGENTS.md`
- `docs/AGENTS.md` with `docs/CLAUDE.md`

The `CLAUDE.md` files exist because Claude Code descends `CLAUDE.md`, not `AGENTS.md`. The payload ships no `CLAUDE.md`.

## Plans

Read `docs/PLAN.md` before plan work. The installed `plan-lifecycle` plugin's
`plan-manager/references/plan-contract.md` is the canonical contract.
Use `plan-workspace` for setup, main-context `plan-manager` for the six phases,
and read-only `plan-reviewer` and `code-reviewer` wrappers for review.
Use direct implementation only for a clear, reversible, low-risk local diff
with one bounded acceptance path. Otherwise follow the plan-manager decision.
Plan issues are the live records. Leave `docs/plans/finished/` frozen.
