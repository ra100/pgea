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

- [ ] `cc:TODO` Init Cargo crate (`cargo init --bin`), set edition 2021, add deps: `tokio`, `pgwire`, `aws-sdk-rdsdata`, `aws-config`, `tracing`, `tracing-subscriber`, `clap`, `serde`, `toml`, `phf`, `regex`, `thiserror`.
- [ ] `cc:TODO` Wire `clap` CLI: `--config`, `--listen`, `--log-level`.
- [ ] `cc:TODO` Set up `tracing` subscriber + `main.rs` bootstrap (load config → spawn server).

### M2 — Config

- [ ] `cc:TODO` `config.rs`: TOML schema + `serde` derive.
- [ ] `cc:TODO` Structural validation (ARN regex, dup target names, `listen` parse).
- [ ] `cc:TODO` Lazy profile resolution (resolve on first connection, cache per target).
- [ ] `cc:TODO` Unit tests: parse valid + invalid configs.

### M3 — Pg wire scaffold

- [ ] `cc:TODO` TCP accept loop (`server.rs`) — bind `127.0.0.1:5433`, spawn per-conn task.
- [ ] `cc:TODO` `pg::startup` — handle StartupMessage, cleartext-password auth, send `AuthenticationOk` + `ParameterStatus` + `BackendKeyData` + `ReadyForQuery`.
- [ ] `cc:TODO` Resolve target from `dbname`; resolve profile from password (or fall back to target/default).

### M4 — RDS client + translation

- [ ] `cc:TODO` `rds::client` — build `aws_sdk_rdsdata::Client` per profile.
- [ ] `cc:TODO` `rds::txn` — `BeginTransaction` / `CommitTransaction` / `RollbackTransaction` wrappers.
- [ ] `cc:TODO` `pg::types` — static `phf` typeName→OID map; value formatter for each Data API field variant; pg array literal encoder.
- [ ] `cc:TODO` Unit tests: type map, value formatter, array literal edge cases.

### M5 — Simple Query path

- [ ] `cc:TODO` `intercept.rs` — regex-based rejection (SAVEPOINT/COPY/CURSOR/LISTEN/NOTIFY/FETCH/MOVE).
- [ ] `cc:TODO` `pg::simple_query` — handle `Q`: route txn verbs vs `ExecuteStatement`.
- [ ] `cc:TODO` Response translation: `RowDescription` + `DataRow`s + `CommandComplete` (verb-tagged) + `ReadyForQuery`.
- [ ] `cc:TODO` Error mapping: Data API errors → pg `ErrorResponse`; in-txn error → `E` state.
- [ ] `cc:TODO` Integration test (mocked SDK): `SELECT 1`, error path, `BEGIN; SELECT 1; COMMIT`.

### M6 — Extended Query path

- [ ] `cc:TODO` Param rewriter `$N` → `:pN` lexer (skip strings, `--` and `/* */` comments, dollar-quoted blocks). Unit-tested heavily.
- [ ] `cc:TODO` `pg::extended` — handle Parse / Bind / Describe / Execute / Sync; portal & statement maps per session.
- [ ] `cc:TODO` Param value decode (pg text/binary by OID) → Data API `SqlParameter` (`stringValue`/`longValue`/`doubleValue`/`booleanValue`/`isNull`/`blobValue`).
- [ ] `cc:TODO` Integration test (mocked SDK): parameterised query via `tokio_postgres`.

### M7 — E2E + ops

- [ ] `cc:TODO` E2E test gated by env var (real Aurora cluster). Smoke: connect, `SELECT 1`, txn, intercepted op error.
- [ ] `cc:TODO` Manual DBeaver smoke (document connection settings in README).
- [ ] `cc:TODO` Release: `cargo install` instructions + GH Actions for prebuilt macOS/Linux binaries.

## Out of scope (v1)

COPY, server-side cursors, LISTEN/NOTIFY, SAVEPOINT, prepared-statement caching, multi-statement Q, auto-pagination, TLS to client. Updating spec required before adding.

## Archive

(empty)
