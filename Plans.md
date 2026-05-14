# Plans — pg-rds-connector

Spec: `docs/superpowers/specs/2026-05-14-pg-rds-connector-design.md`

## Marker Legend

| Marker | State | Meaning |
|--------|-------|---------|
| `cc:TODO` | not started | Impl (Claude Code) will do |
| `cc:WIP` | in progress | Impl is working |
| `cc:blocked` | blocked | Waiting on dependency |
| `pm:依頼中` | requested | PM-requested (2-agent) |

## Milestones

### M1 — Scaffolding

- [x] `cc:完了` Init Cargo crate (`cargo init --bin`), edition 2021, deps: `clap`, `serde`, `toml`, `tracing`, `tracing-subscriber`, `thiserror`, `regex`, `once_cell` (tokio/pgwire/aws SDKs deferred to M3/M4 — need verified API references first).
- [x] `cc:完了` Wire `clap` CLI: `--config`, `--listen`, `--log-level`.
- [x] `cc:完了` `tracing` subscriber + `main.rs` bootstrap (loads config; server spawn deferred).

### M2 — Config

- [x] `cc:完了` `config.rs`: TOML schema + `serde` derive.
- [x] `cc:完了` Structural validation (ARN regex, listen parse, missing field detection).
- [x] `cc:完了` Lazy profile resolution helper (`Config::resolve_profile`).
- [x] `cc:完了` Unit tests: 7 cases covering valid/invalid configs + profile precedence.

### M3 — Pg wire scaffold

- [ ] `cc:TODO` TCP accept loop (`server.rs`) — bind `127.0.0.1:5433`, spawn per-conn task.
- [ ] `cc:TODO` `pg::startup` — handle StartupMessage, cleartext-password auth, send `AuthenticationOk` + `ParameterStatus` + `BackendKeyData` + `ReadyForQuery`.
- [ ] `cc:TODO` Resolve target from `dbname`; resolve profile from password (or fall back to target/default).

### M4 — RDS client + translation

- [ ] `cc:TODO` `rds::client` — build `aws_sdk_rdsdata::Client` per profile.
- [ ] `cc:TODO` `rds::txn` — `BeginTransaction` / `CommitTransaction` / `RollbackTransaction` wrappers.
- [x] `cc:完了` `types.rs` — typeName→OID match (used `match` not `phf` — simpler, equivalent perf for ~40 entries); pg array literal encoder + bytea hex encoder. 6 unit tests.
- [x] `cc:完了` Unit tests: type map, value formatter, array literal edge cases.

### M5 — Simple Query path

- [x] `cc:完了` `intercept.rs` — regex-based rejection (SAVEPOINT/COPY/CURSOR/LISTEN/NOTIFY/FETCH/MOVE) + txn verb classification + `leading_verb()` helper. 9 unit tests.
- [ ] `cc:TODO` `pg::simple_query` — handle `Q`: route txn verbs vs `ExecuteStatement`.
- [ ] `cc:TODO` Response translation: `RowDescription` + `DataRow`s + `CommandComplete` (verb-tagged) + `ReadyForQuery`.
- [ ] `cc:TODO` Error mapping: Data API errors → pg `ErrorResponse`; in-txn error → `E` state.
- [ ] `cc:TODO` Integration test (mocked SDK): `SELECT 1`, error path, `BEGIN; SELECT 1; COMMIT`.

### M6 — Extended Query path

- [x] `cc:完了` Param rewriter `$N` → `:pN` lexer in `rewriter.rs` (skips single-quoted strings, double-quoted identifiers, `--` line comments, nested `/* */` block comments, dollar-quoted blocks with named & empty tags). 13 unit tests covering edge cases incl. unterminated literals.
- [ ] `cc:TODO` `pg::extended` — handle Parse / Bind / Describe / Execute / Sync; portal & statement maps per session.
- [ ] `cc:TODO` Param value decode (pg text/binary by OID) → Data API `SqlParameter` (`stringValue`/`longValue`/`doubleValue`/`booleanValue`/`isNull`/`blobValue`).
- [ ] `cc:TODO` Integration test (mocked SDK): parameterised query via `tokio_postgres`.

### M7 — E2E + ops

- [ ] `cc:TODO` E2E test gated by env var (real Aurora cluster). Smoke: connect, `SELECT 1`, txn, intercepted op error.
- [ ] `cc:TODO` Manual DBeaver smoke (document connection settings in README).
- [ ] `cc:TODO` Release: `cargo install` instructions + GH Actions for prebuilt macOS/Linux binaries.

## Out of scope (v1)

COPY, server-side cursors, LISTEN/NOTIFY, SAVEPOINT, prepared-statement caching, multi-statement Q, auto-pagination, TLS to client. Updating spec required before adding.

## Status (2026-05-14)

**Shipped:** Cargo crate, CLI bootstrap, config loader+validator, intercept layer, type map, value formatters, $N→:pN rewriter. **35 unit tests pass, clippy clean.**

**Halted before:** pgwire wire codec, AWS SDK integration, server accept loop, transaction state machine, response translation, end-to-end integration.

**Why halted:** Requires verified API surface for `pgwire` and `aws-sdk-rdsdata` crates. context7 was unavailable when checked. Continuing without docs would mean guessing API signatures and shipping non-compiling code. Resume with: `cargo doc --open` of those crates, or context7 lookup, before writing M3/M4.

## Archive

(empty)
