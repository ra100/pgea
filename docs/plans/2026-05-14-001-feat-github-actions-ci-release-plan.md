---
title: "feat: GitHub Actions CI + release pipeline"
status: active
created: 2026-05-14
type: feat
depth: lightweight
origin: Plans.md M7 ("E2E + ops" — release: cargo install instructions + GH Actions for prebuilt macOS/Linux binaries)
---

# feat: GitHub Actions CI + release pipeline

## Summary

Add two GitHub Actions workflows:

1. **`ci.yml`** — runs on every push and PR. Lints (`cargo fmt --check`, `cargo clippy -D warnings`), builds, and tests on Linux. Fast feedback for the whole project.
2. **`release.yml`** — runs on git tag push matching `v*`. Cross-compiles release binaries for **`aarch64-apple-darwin`** and **`x86_64-unknown-linux-musl`**, packages them as `.tar.gz`, and uploads them to a GitHub Release.

Closes the "GH Actions for prebuilt macOS/Linux binaries" item in Plans.md M7.

---

## Problem Frame

Today the repo has zero CI: regressions can slip into `main` and there is no published binary. Users following the README must build from source. Plans.md M7 calls for prebuilt macOS + Linux binaries on release.

We want:

- Every PR/push validated (fmt, clippy, test) before merge.
- Tagging `v0.1.0` produces downloadable binaries on the GitHub Release page.

---

## Scope Boundaries

### In scope

- `.github/workflows/ci.yml` — Linux-only build/lint/test on every push + PR.
- `.github/workflows/release.yml` — tag-triggered cross-compile + artifact upload.
- Two release targets: `aarch64-apple-darwin` (macOS Apple Silicon), `x86_64-unknown-linux-musl` (portable Linux).
- `.github/dependabot.yml` for `cargo` + `github-actions` ecosystems.
- README CI badge + install snippet.

### Deferred to Follow-Up Work

- macOS Intel (`x86_64-apple-darwin`) build.
- Linux glibc target alongside musl.
- Windows target.
- Code signing / notarization.
- Codecov / coverage.
- `cargo publish` to crates.io.
- Homebrew tap.

### Out of scope (v1)

- Running the M7 E2E test (real Aurora cluster) in CI. E2E remains opt-in via env var locally; CI runs `cargo test` (which excludes `#[ignore]`d tests by default). Wiring AWS secrets into a public repo is a separate decision and should follow a spec update.

---

## Key Technical Decisions

### Native crypto for `aws-config` on musl

`aws-config` (and transitively `aws-lc-rs`) defaults to native crypto that needs `cmake` + a C toolchain on musl. Two viable approaches:

- **A.** Use `taiki-e/upload-rust-binary-action` (or `cross`), which bundles a working musl cross-compile environment.
- **B.** Switch `aws-config` features to `rustls` to avoid native crypto entirely.

**Decision: A.** Less invasive — does not change the default crypto stack used by everyone building locally on macOS. `aarch64-apple-darwin` builds natively on `macos-14` (Apple Silicon runner). If the action fails for `aws-lc-rs`, fall back to `use-cross: true`. See Risks.

### Single CI test job, not a matrix

Tests are pure Rust and use `MockRdsClient` — no AWS calls, no network. `ubuntu-latest` stable Rust is enough signal; matrixing across OSes triples runtime for no extra coverage. Cross-OS confidence comes from the release build itself.

### Pinned action versions

Pin third-party actions to a major-version tag (`actions/checkout@v4`, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`, `taiki-e/upload-rust-binary-action@v1`). Dependabot keeps them current.

### Release artifact naming

`pg-rds-connector-${tag}-${target}.tar.gz` containing the `pg-rds-connector` binary plus `LICENSE` and `README.md`. Predictable for scripted installs.

---

## Implementation Units

### U1. CI workflow — fmt, clippy, build, test on push/PR

**Goal:** Catch regressions on every push and PR with the same checks the project runs locally.

**Requirements:** Plans.md M7 (the "run tests" half of the user request).

**Dependencies:** none.

**Files:**

- `.github/workflows/ci.yml` (new)

**Approach:**

- Triggers: `push` to `main`, `pull_request` (any branch).
- Single job `test` on `ubuntu-latest`, stable Rust.
- Steps: checkout → install toolchain (with `rustfmt`, `clippy`) → `Swatinem/rust-cache` → `cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` → `cargo build --locked` → `cargo test --locked`.
- `--locked` so CI fails loudly if `Cargo.lock` is out of sync with `Cargo.toml`.
- Concurrency group keyed on `${{ github.workflow }}-${{ github.ref }}` with `cancel-in-progress: true`.
- Permissions: `contents: read` only.

**Patterns to follow:** mirror the commands listed in `CLAUDE.md` § Commands so local + CI stay identical.

**Test scenarios:**

- Push a branch with a `cargo fmt` violation → CI fails at the fmt step.
- Push a branch with a clippy warning → CI fails (`-D warnings` makes warnings errors).
- Push a branch where a unit test panics → CI fails at the test step.
- A clean PR off `main` → CI passes within ~5 min on a warm cache.
- Two consecutive pushes to the same PR → second run cancels the first via concurrency group.

**Verification:** Open a draft PR with a deliberately-broken commit, confirm red ✗. Push a fix, confirm green ✓.

---

### U2. Release workflow — tag-triggered cross-compile + artifact upload

**Goal:** On `git push --tags` for a `v*` tag, build release binaries for `aarch64-apple-darwin` and `x86_64-unknown-linux-musl` and attach them to a GitHub Release.

**Requirements:** Plans.md M7 ("GH Actions for prebuilt macOS/Linux binaries").

**Dependencies:** none in workflow, but tagging implies CI green on `main`.

**Files:**

- `.github/workflows/release.yml` (new)

**Approach:**

- Trigger: `push` with `tags: ['v*']`.
- Two-job structure:
  - `create-release` (`ubuntu-latest`) — runs once. Creates the GitHub Release via `softprops/action-gh-release` (or `gh release create`), draft until artifacts upload, body sourced from a `RELEASE_NOTES.md` snippet or empty.
  - `build` matrix:
    - `{ target: aarch64-apple-darwin, os: macos-14 }`
    - `{ target: x86_64-unknown-linux-musl, os: ubuntu-latest }`
    - Uses `taiki-e/upload-rust-binary-action@v1` (handles toolchain install, musl setup, tarball packaging, asset upload).
- Permissions on the build job: `contents: write` (needed to upload assets).
- `aarch64-apple-darwin` builds natively on `macos-14` — no cross-compile, no codesigning.
- `x86_64-unknown-linux-musl` uses the action's bundled musl setup; if it fails for `aws-lc-rs`, fall back is in Risks.
- Tarball contents: `pg-rds-connector` binary, `LICENSE`, `README.md`. Name: `pg-rds-connector-${tag}-${target}.tar.gz`.
- After both matrix legs succeed, mark release non-draft.

**Patterns to follow:** `taiki-e/upload-rust-binary-action` README examples. Pin to a major version tag.

**Test scenarios:**

- Push tag `v0.0.0-test1` to a throwaway branch → workflow runs, both jobs succeed, GH Release page shows two `.tar.gz` assets.
- Download the macOS arm64 tarball → `tar -xzf` → `./pg-rds-connector --help` on Apple Silicon → exits 0.
- Download the Linux musl tarball → `tar -xzf` → `./pg-rds-connector --help` on a glibc-only Linux box → exits 0 (proves musl static linkage).
- Push tag `v0.0.0-test2` while `Cargo.lock` is missing or stale → release build fails fast (`--locked`).
- Re-push the same tag (after deletion + recreation) → workflow runs again and replaces previously-uploaded assets without erroring.

**Verification:** Tag `v0.0.0-test1` against an unmerged branch (or a private fork). Confirm both binaries are present, executable, and report a sane `--version`. Delete the test release + tag.

---

### U3. Dependabot config

**Goal:** Keep cargo deps and GH Action versions current with low effort.

**Requirements:** Healthy CI hygiene; pairs with introducing pinned actions in U1/U2.

**Dependencies:** U1, U2.

**Files:**

- `.github/dependabot.yml` (new)

**Approach:**

- Two ecosystems: `cargo` (weekly, `Cargo.toml` + `Cargo.lock`), `github-actions` (weekly, `.github/workflows/*`).
- Open PRs against `main`. No version ignores in v1.

**Test scenarios:**

- Bump a workflow's pinned action major manually → next Dependabot run produces no PR (already on latest).
- Stale a transitive cargo dep (artificial) → Dependabot opens an "update Cargo.lock" PR within a week.

**Verification:** Push, wait one cycle, confirm at least one Dependabot run executed (visible under repo Insights → Dependabot).

---

### U4. README badge + install snippet

**Goal:** Make CI status and release downloads discoverable.

**Requirements:** Cosmetic but expected once CI exists.

**Dependencies:** U1, U2.

**Files:**

- `README.md` (modify)

**Approach:**

- CI badge near the top: `![CI](https://github.com/svarba/pg-rds-connector/actions/workflows/ci.yml/badge.svg)`.
- Short "Install (prebuilt binaries)" section pointing to the Releases page with one-liner `tar -xzf` examples for each target.
- Keep `cargo install --git ...` as the build-from-source alternative.

**Test scenarios:** none (doc change). Manual eyeball.

**Verification:** Render README on GitHub, confirm badge resolves and links go to the right pages.

---

## System-Wide Impact

- New required status check: once `ci.yml` is green on `main`, suggest setting branch protection on `main` to require it. Repo-settings change, out of scope for the workflow file itself.
- Tag pushes now have side effects (binary publication). Document in README so contributors know not to push throwaway tags to the canonical repo.

---

## Risks & Mitigations

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `aws-lc-rs` fails to cross-compile to musl in `taiki-e/upload-rust-binary-action`'s default container | Medium | Set `use-cross: true` on the action, or temporarily switch `aws-config` to `rustls` for the musl target only via `[target.'cfg(target_env = "musl")'.dependencies]` overrides. Decide on first failure. |
| `macos-14` runner queue times spike | Low | Matrix is independent — Linux artifact still uploads. Re-run failed leg only. |
| Tag pushed accidentally publishes a bad binary | Low | Workflow can be re-run after deleting the release + tag; `softprops/action-gh-release` overwrites assets idempotently. |
| GitHub Actions free minutes exhausted | Very low | macOS minutes count 10x; release flow runs only on tag, not every push. CI workflow is Linux-only. |

---

## Out-of-Plan Notes

- An E2E job that exercises a real Aurora cluster is a separate, larger decision (secrets management, cost, ownership). Plans.md M7 leaves it for a future iteration.
- If Windows demand appears, add a third matrix entry to `release.yml` — no architectural change required.
