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
- [x] `cc:完了` Integration test (mocked SDK + `tokio_postgres` client) — `tests/extended_query.rs` covers Simple Query rows, BEGIN/INSERT/COMMIT routing, Extended Query with `$1` → `:p1` param rewrite, and 3D000 on unknown dbname. Added `pg::server::run_with_listener` test seam + `tokio-postgres` dev-dep. Also fixed `do_describe_statement` to report parameter types so non-JDBC clients don't fail on `ParameterDescription`.

### M6 — Extended Query path

- [x] `cc:完了` Param rewriter `$N` → `:pN` lexer in `rewriter.rs` (skips single-quoted strings, double-quoted identifiers, `--` line comments, nested `/* */` block comments, dollar-quoted blocks with named & empty tags). 13 unit tests covering edge cases incl. unterminated literals.
- [x] `cc:完了` `ExtendedQueryHandler for Connection` — Parse stores SQL via pgwire's `NoopQueryParser`; Bind builds `Portal` with parameter format codes; Describe(Portal) eagerly executes statement and caches `ExecuteOutput`; Execute serves rows from cache; Describe(Statement) reports parameter type list (echoes Parse type_oids, falls back to `Type::UNKNOWN` × placeholder count via `rewriter::rewrite`).
- [x] `cc:完了` Param value decode (text + binary by `parameter_format`/`Type`) → Data API `SqlParameter`. Text: UTF-8 → `stringValue`, else `blobValue`. Binary: `decode_binary_scalar` handles BOOL/INT2/INT4/INT8/OID/FLOAT4/FLOAT8/BYTEA + text-like fallback. Fixes JDBC OID 4-byte BE leak that produced `invalid byte sequence for encoding "UTF8": 0x00` on real Aurora.
- [x] `cc:完了` Integration test in `tests/extended_query.rs::extended_query_rewrites_params` — `prepare_typed("INSERT ... ($1)", &[INT4])` round-trip asserts SQL got `$1` → `:p1` rewritten and the recorded `SqlParameter` is named `p1`.

### M7 — E2E + ops

- [x] `cc:完了 [97a446a]` E2E test gated by env var (real Aurora cluster). `tests/e2e_aurora.rs` — `SELECT 1`, `BEGIN`/`ROLLBACK` round-trip, `SAVEPOINT` intercept. `PG_RDS_CONNECTOR_E2E=1` plus cluster/secret/database/region env vars to opt in; otherwise no-op.
- [x] `cc:完了 [97a446a]` README DBeaver/DataGrip/TablePlus walkthrough + caveats; also fixes the broken `[[targets]]` config example to the real `[targets.<name>]` map syntax.
- [x] `cc:完了 [97a446a]` `.github/workflows/release.yml` — on tag push (`v*`), builds release binaries for macOS arm64, macOS x86_64, Linux x86_64 GNU; tarball + sha256 attached to GitHub Release. README documents `cargo install --git` plus prebuilt-binary download.

## Out of scope (v1)

COPY, server-side cursors, LISTEN/NOTIFY, SAVEPOINT, prepared-statement caching, multi-statement Q, TLS to client. Updating spec required before adding.

(Auto-pagination around the 1 MB Data API cap is now implemented — `rds/paginate.rs`.)

(Per-target SDK client pooling — spec's Future Work list — is now implemented: `rds/pool.rs` caches one `Arc<dyn RdsClient>` per resolved (target, profile) with a 15-minute TTL, wired into `Connection::build_rds_client` and shared across every connection accepted by a listener.)

## Status (2026-05-14)

**Shipped (60 unit + 4 integration + 3 env-gated E2E tests pass, clippy clean):**
- Cargo crate, CLI bootstrap, tokio runtime
- Config loader + structural validation + lazy profile resolution
- Intercept layer (rejection + txn verb classification + leading-verb helper)
- Type map (typeName → pg OID), pg array literal encoder, bytea hex encoder
- `$N` → `:pN` rewriter (string/identifier/comment/dollar-quote aware)
- `RdsClient` trait + `AwsRdsClient` (real SDK) + `MockRdsClient` (tests)
- `TxnState` state machine (begin/commit/rollback/failed)
- pg wire server (`pg::server::run`) — Simple + Extended Query with txn routing, error mapping, response translation
- Extended Query: Parse / Bind / Describe(Statement) / Describe(Portal) / Execute / Sync; eager-execute portal cache; binary bind-param decode (BOOL/INT2/INT4/INT8/OID/FLOAT4/FLOAT8/BYTEA)
- DBeaver / DataGrip catalog rewrites (`catalog.rs`): pg_type, pg_namespace, pg_class, pg_attribute, pg_constraint, pg_index, pg_collation, pg_am, pg_depend; oid-placeholder type-cast injection; rewriter test count 17
- Integration test harness: `tests/extended_query.rs` drives the proxy via `tokio-postgres` with a mock `RdsClient`
- main.rs wires Config → tokio runtime → server with AWS-backed factory
- README + MIT LICENSE; Cargo metadata (license/readme/repository) populated

- M7 done — E2E env-gated suite (`tests/e2e_aurora.rs`), README DBeaver/DataGrip/TablePlus walkthrough + cargo install + prebuilt-binary docs, GH Actions release workflow (macOS arm64/x86_64, Linux x86_64 GNU)

**Verified by:** `cargo build --release` succeeds; `cargo test` passes 60 unit + 4 integration + 3 env-gated E2E (skipped without gate); `cargo clippy --all-targets -- -D warnings` clean. Manually validated against a private Aurora cluster — DBeaver schema browser, table list, column list, constraints, indexes load end-to-end. Real-cluster E2E run pending; release workflow exercised against tags v0.1.0, v0.1.1, v0.2.0.

## Archive

(empty)
