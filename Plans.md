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

- [x] `cc:完了` TCP accept loop in `pg/server.rs::run` via `pgwire::tokio::process_socket`.
- [x] `cc:完了` Startup uses pgwire's `NoopHandler` (no auth challenge — loopback only). Custom cleartext-password handler + dbname-based target routing deferred to M3-followup task below.
- [x] `cc:完了` Custom `StartupHandler` on `Connection` — reads `database` from StartupMessage metadata, validates against `Config::target()` (3D000 if missing), sends `AuthenticationCleartextPassword`, accepts password as profile-name override (empty → fallback chain), builds per-connection `AwsRdsClient` with resolved profile + region, stashes in `Mutex<Option<Arc<dyn RdsClient>>>` shared with `SimpleQueryHandler`. main.rs no longer pre-builds clients; SDK construction is per-connection. Test seam (`test_client`) lets unit tests inject `MockRdsClient`. 3 new tests.

### M4 — RDS client + translation

- [x] `cc:完了` `rds::client` — `RdsClient` trait + `AwsRdsClient` SDK impl + `MockRdsClient` test double; flattens Data API `Field` into our enum, converts `ArrayValue` → pg array literal.
- [x] `cc:完了` `rds::txn` — `TxnState` with `begin/commit/rollback`, failed-state tracking, 5 unit tests.
- [x] `cc:完了` `types.rs` — typeName→OID match (used `match` not `phf` — simpler, equivalent perf for ~40 entries); pg array literal encoder + bytea hex encoder. 6 unit tests.
- [x] `cc:完了` Unit tests: type map, value formatter, array literal edge cases.

### M5 — Simple Query path

- [x] `cc:完了` `intercept.rs` — regex-based rejection (SAVEPOINT/COPY/CURSOR/LISTEN/NOTIFY/FETCH/MOVE) + txn verb classification + `leading_verb()` helper. 9 unit tests.
- [x] `cc:完了` `pg::server::Session::do_query` — routes via intercept module to BeginTransaction/Execute/Commit/Rollback through `RdsClient`.
- [x] `cc:完了` Response translation via pgwire `QueryResponse` + `DataRowEncoder`; verb-tagged `CommandComplete` (SELECT N / INSERT 0 N / UPDATE N / DELETE N).
- [x] `cc:完了` Error mapping: Data API errors → pg `Response::Error(ErrorInfo)`; in-txn error sets `TxnState::failed` → status `E`; aborted-txn statements rejected with 25P02.
- [ ] `cc:TODO` Integration test (mocked SDK + `tokio_postgres` client) — needs adding `tokio-postgres` dev-dep + spawning local listener; deferred.

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

**Shipped (40 unit tests pass, clippy clean):**
- Cargo crate, CLI bootstrap, tokio runtime
- Config loader + structural validation + lazy profile resolution
- Intercept layer (rejection + txn verb classification + leading-verb helper)
- Type map (typeName → pg OID), pg array literal encoder, bytea hex encoder
- `$N` → `:pN` rewriter (string/identifier/comment/dollar-quote aware)
- `RdsClient` trait + `AwsRdsClient` (real SDK) + `MockRdsClient` (tests)
- `TxnState` state machine (begin/commit/rollback/failed)
- pg wire server (`pg::server::run`) — Simple Query path with txn routing, error mapping, response translation
- main.rs wires Config → tokio runtime → server with AWS-backed factory

**Halted before:**
- M6: Extended Query path (`Parse`/`Bind`/`Describe`/`Execute`/`Sync`) with `$N`→`:pN` + `SqlParameter` conversion
- Integration test harness with `tokio_postgres` exercising the proxy against `MockRdsClient`
- M7: E2E against real Aurora cluster + DBeaver smoke + release CI

**Verified by:** `cargo build` succeeds for full crate; `cargo test` passes 40 unit tests; `cargo clippy --all-targets -- -D warnings` clean.

## Archive

(empty)
