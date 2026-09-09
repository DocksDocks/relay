# Session Relay repository

This repository holds the Session Relay Rust crate for durable mail between omp sessions, its Node harness, and its release infrastructure. The separate `plugin/` directory contains only the shipped payload.

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
- After you add or remove a test, run `node test/rust-test-inventory.mjs --generate`. Commit the fixture.
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

- `x86_64-unknown-linux-musl` as `session-relay-x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl` as `session-relay-aarch64-unknown-linux-musl`

The workflow aggregates attestations and `SHA256SUMS`. It publishes exactly three assets: two binaries and the checksum file.

The release has no prerelease step. It has no promotion step.

## Platform support

Session Relay distributes Linux x86-64 and arm64 musl binaries.

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

Use direct implementation for one clear, reversible, low-risk local diff with one
bounded acceptance path; it creates no plan issue, reviewer, or automatic
commit. Use a canonical plan for explicit planning, multi-commit or
cross-repository work, cold handoff, an unresolved decision, a cross-subsystem or
public-contract change, security-sensitive or destructive work, or any
non-`local` effect.

<constraint>
The plan record is a GitHub issue. Its body starts with
`<!-- plan-contract: v3 -->`, then a blank line and the exact eight `##`
sections; it has no frontmatter. GitHub owns title, open-work phase, owner,
timestamps, and completion, and no plan markdown is tracked in the repository.
Exactly three skills own the workflow: `plan-workspace` maintains the workspace;
main-context `plan-manager` runs six phases - decide, draft, research, plan
review, implement, code review - with bounded repair and fresh re-review in both
review phases, then archives; internal `plan-reviewer` returns one readable
pre-implementation verdict block per round. Two read-only reviewer wrappers
ship, `plan-reviewer` and `code-reviewer`, and nothing else in the lifecycle has
a wrapper.
</constraint>

After the marker and blank line, the record carries exactly `## Goal`,
`## Research`, `## Steps`, `## Acceptance`, `## Do not touch`,
`## Open questions`, `## Review`, and `## Verification Results`, in that order
and once each. `## Goal` carries exactly one mode line. Open-work phase is one
of `drafting`, `planned`, `ongoing`, or `blocked` in a `plan:<phase>` label; a
blocked plan starts `## Open questions` with `Blocked: <one-line reason>`.
Closed completion derives from GitHub `state` and `stateReason`. `## Review`
contains exactly `_Review records are stored in issue comments._`. Each reviewer
returns one markdown block, and the manager posts that whole block as one issue
comment. The latest trusted well-formed record per review kind wins; its author
must equal the plan's sole assignee. A legacy body verdict is consulted only
when no trusted comment record exists for that kind. Both review phases use
fresh inputs and run at most five rounds, stopping on pass, no progress, a
finding surviving its fix, or `repair` or `fixes-required` in round five. A
plan-review `blocked` verdict always routes its user-only decision through
`## Open questions` and `ask`.

The record carries no hash, permit, run identity, lock, or bundle, and the
`plan.mjs` shipped inside the installed `plan-lifecycle` plugin is the only
lifecycle tool. An `export` writes the sha256 of the body it copied beside the
copy so a stale copy cannot revert the record; that digest detects staleness and
authorizes nothing. Routine plan issue publication, implement-start linked
branch creation, commits, normal pushes, and the closing pull request carry the
settled mode's authorization and need no repeated prompt. Before any branch
checkout, including `gh issue develop --checkout`, require
`git status --porcelain` to be empty. If it is dirty, never stash, move, or
commit ambient work; set the plan `blocked` and name the dirty paths, or use an
authorized clean worktree.
Immediately after setting the plan `ongoing`, every `gh issue develop` call uses
`--repo`; the manager reuses a linked branch or creates one with
`--base <default> --checkout`, then re-lists and recovers after failure.
Implementation stops when no linked branch can be verified; there is no local
fallback. After the checks policy passes, the manager asks immediately before
merge. Without a fresh `Merge now` answer, it leaves the pull request and issue
open. `plan.mjs archive` verifies the latest trusted code-review result and
merged closing pull request after landing.

Every Steps row carries an `Effect` of exactly
`local|probe|production_access|publish|push|release|deploy`. A step whose
`Effect` is not `local` requires an in-session `ask` confirmation immediately
before it runs; when `ask` is unavailable the step is set `blocked` and the plan
reason becomes the first `## Open questions` line, `Blocked: <reason>`.
Persisted effects record intent only. Routine issue publication and landing
actions are outside the Steps table.

Render a plan body verbatim only when the user names that plan and asks to see it. After a write, report the one-line header strip and the changed lines only; a write never re-renders the body.

`docs/plans/finished/` is frozen pre-GitHub history. Humans may read it as
history, but it is not a source of truth. No lifecycle command or workspace
migration operation opens or inventories it. The complete contract lives in
`docs/PLAN.md`; `docs/AGENTS.md` routes to it and `docs/CLAUDE.md` contains only
`@AGENTS.md`.
