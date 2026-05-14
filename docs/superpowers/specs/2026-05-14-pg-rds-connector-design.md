# pg-rds-connector — Design

**Date:** 2026-05-14
**Status:** Approved (pending user review of written spec)

## Problem

Aurora PostgreSQL clusters in private VPCs are not reachable from developer laptops. Developers currently issue queries via `aws rds-data execute-statement`, which is awkward and lacks the ergonomics of a SQL GUI (DBeaver, DataGrip, TablePlus, psql).

A local proxy that speaks the PostgreSQL wire protocol on one side and the AWS RDS Data API on the other lets any standard pg client connect as if to a local Postgres, while traffic is tunnelled through AWS using the developer's existing AWS credentials. No VPC, no bastion, no SSH tunnel.

## Goals

- Run a local TCP listener that accepts standard pg wire connections.
- Translate pg queries into RDS Data API calls (`ExecuteStatement`, `BeginTransaction`, `CommitTransaction`, `RollbackTransaction`).
- Authenticate to AWS via the standard credential chain (`AWS_PROFILE`, env, SSO, IMDS).
- Serve enough of the pg protocol that mainstream GUIs (DBeaver, DataGrip, TablePlus) work for everyday querying and editing.
- Keep configuration declarative and per-target, so one proxy can serve many clusters.

## Non-Goals (v1)

- `COPY`, server-side cursors, `LISTEN`/`NOTIFY`, `SAVEPOINT`, prepared-statement caching across sessions.
- TLS between client and proxy (loopback only).
- Multi-statement queries in a single Simple Query message.
- Auto-pagination of large result sets (deferred).
- A graphical configuration UI.

## Architecture

```
┌──────────┐  pg wire  ┌─────────────────────────┐  HTTPS  ┌──────────────┐
│ DBeaver  │──────────▶│ pg-rds-connector (Rust) │────────▶│ RDS Data API │
│ psql etc │           │  127.0.0.1:5433         │         │ (AWS)        │
└──────────┘           └─────────────────────────┘         └──────────────┘
                              │
                              └─ ~/.config/pg-rds-connector/config.toml
```

- Single Rust binary, Tokio async runtime.
- Listens on configurable local address (default `127.0.0.1:5433`).
- Per-pg-connection task. Each task holds:
  - Resolved target (cluster ARN, secret ARN, database, region, AWS profile).
  - AWS SDK `aws_sdk_rdsdata::Client` initialised with that profile.
  - Optional active `transactionId`.
  - Prepared-statement and portal maps for the Extended Query protocol.
- Translates pg wire frames to Data API calls and back.

## Module Layout

```
src/
├── main.rs              # CLI parse, config load, listener bootstrap
├── config.rs            # TOML schema, load, structural validation
├── server.rs            # TCP accept loop, per-connection task spawn
├── pg/
│   ├── mod.rs
│   ├── startup.rs       # StartupMessage, cleartext-password auth
│   ├── codec.rs         # pg wire frame encode/decode (via `pgwire` crate)
│   ├── simple_query.rs  # `Query` (`Q`) message handler
│   ├── extended.rs      # Parse / Bind / Describe / Execute / Sync handler
│   └── types.rs         # OID ↔ Data API typeName map; value formatting
├── rds/
│   ├── mod.rs
│   ├── client.rs        # AWS SDK wrapper, profile selection, retries
│   ├── translate.rs     # SQL + params → ExecuteStatement input
│   └── txn.rs           # Transaction state machine (Begin / Commit / Rollback)
├── intercept.rs         # SAVEPOINT / LISTEN / COPY / CURSOR rejection layer
└── error.rs             # Internal error → pg ErrorResponse mapping
```

Boundary rules:

- `pg::*` knows nothing of AWS.
- `rds::*` knows nothing of pg wire.
- `server.rs` orchestrates a pg session and an rds session.

External crates:

- `pgwire` — pg wire protocol framing and message types.
- `aws-sdk-rdsdata` — official AWS SDK.
- `aws-config` — credential chain and profile resolution.
- `tokio` — async runtime.
- `tracing` + `tracing-subscriber` — logging.
- `clap` — CLI parsing.
- `serde` + `toml` — config parsing.
- `phf` — static type map.
- `regex` — intercept and verb detection.

## Configuration

File location: `~/.config/pg-rds-connector/config.toml` (override via `--config <path>`).

```toml
listen          = "127.0.0.1:5433"
log_level       = "info"
default_profile = "default"

[targets.dev-analytics]
cluster_arn = "arn:aws:rds:eu-west-1:123:cluster:dev-analytics"
secret_arn  = "arn:aws:secretsmanager:eu-west-1:123:secret:dev-analytics-xyz"
database    = "analytics"
region      = "eu-west-1"
profile     = "dev"

[targets.prod-analytics]
cluster_arn = "arn:aws:rds:eu-west-1:123:cluster:prod-analytics"
secret_arn  = "arn:aws:secretsmanager:eu-west-1:123:secret:prod-analytics-abc"
database    = "analytics"
region      = "eu-west-1"
profile     = "prod-readonly"
```

Connection convention from a pg client:

- Host `127.0.0.1`, port from `listen`.
- `dbname` selects a target by name.
- `user` is ignored (logged for traceability).
- `password` empty → use `target.profile`, falling back to `default_profile`.
- `password` non-empty → override profile name (string-typed, not a real password).

### Validation strategy

Startup performs **structural validation only** (no AWS calls):

- TOML parses.
- ARN strings match expected regex.
- No duplicate target names.
- `listen` parses as `SocketAddr`.

Profile resolution and AWS reachability are checked **lazily, on first connection to a target**. A stale or expired SSO profile produces a clean error returned to the pg client (`ERROR: AWS auth failed for profile 'foo': <sdk error>`); the user runs `aws sso login --profile foo` and retries without restarting the proxy. A config with ten targets where only one has fresh credentials still starts and serves the fresh one.

## Authentication

### Client → proxy

- Bind to `127.0.0.1` only.
- Pg auth method: cleartext password.
- Password is interpreted as an AWS profile name override; empty means use the target's configured profile or the default credential chain.
- No TLS; loopback only.

### Proxy → AWS

- Standard AWS credential chain via `aws-config`, scoped to the resolved profile.
- Each pg connection gets its own SDK client bound to that profile.

## Protocol Coverage

### Simple Query (`Q`)

1. Receive SQL text.
2. Intercept layer: if the SQL matches `SAVEPOINT|RELEASE|LISTEN|NOTIFY|COPY|DECLARE\s+CURSOR|FETCH|MOVE`, return `ErrorResponse`:
   `ERROR: <op> not supported by RDS Data API proxy`.
3. If `BEGIN|START\s+TRANSACTION`: call `BeginTransaction`, store `transactionId`, send `CommandComplete("BEGIN")`.
4. If `COMMIT`: call `CommitTransaction`, clear `transactionId`, send `CommandComplete("COMMIT")`.
5. If `ROLLBACK`: call `RollbackTransaction`, clear `transactionId`, send `CommandComplete("ROLLBACK")`.
6. Otherwise: call `ExecuteStatement { sql, transactionId?, includeResultMetadata: true }` and emit pg messages (see Response Translation).

### Extended Query (`P`/`B`/`D`/`E`/`S`)

- `Parse`: store SQL and parameter type OIDs in the session. No Data API call (Data API has no prepare).
- `Bind`: store parameter values bound to a portal. Convert pg-encoded params to Data API `SqlParameter` (named: `p1..pN`). Rewrite SQL `$N` → `:pN`.
- `Describe(Statement)`: return empty `ParameterDescription` and `NoData` (column types are unknown until execution).
- `Describe(Portal)`: return `NoData`; `Execute` will produce `RowDescription`.
- `Execute`: build `ExecuteStatement` with rewritten SQL and `parameters[]`. Emit `RowDescription` (from response metadata) + `DataRow`s + `CommandComplete`.
- `Sync`: emit `ReadyForQuery` with txn status (`I` idle / `T` in-txn / `E` failed).

Out of scope: COPY, cursors, LISTEN/NOTIFY, SAVEPOINT — all rejected by the intercept layer with a clean pg error.

### Parameter rewriting

- pg uses `$1, $2`. Data API supports named `:foo` parameters.
- A simple lexer rewrites `$N` → `:pN`, skipping content inside string literals (`'...'`), identifier quotes (`"..."`), line comments (`-- ...`), block comments (`/* ... */`), and dollar-quoted strings (`$tag$ ... $tag$`).
- pg parameter values arrive as text or binary. Each is decoded to a Rust value per its declared OID, then encoded as a Data API `Field` (`stringValue`, `longValue`, `doubleValue`, `booleanValue`, `isNull`, `blobValue`).

### Transactions

- One `transactionId` per pg connection.
- Nested `BEGIN` is rejected (matches pg).
- `SAVEPOINT` / `RELEASE` are rejected with a clear error.
- The Data API auto-rolls back idle transactions after 24 hours; this matches pg's idle-in-transaction semantics for our purposes.
- Errors inside a transaction set the txn status to `E`; the next `ReadyForQuery` reports `E`, and the only accepted next statement is `ROLLBACK`.

## Response Translation

A Data API response has `columnMetadata`, `records`, `numberOfRecordsUpdated`, and `generatedFields`. Pg messages are emitted as follows:

1. **`RowDescription`** — one field per `columnMetadata` entry:
   - `name` from `columnMetadata[i].name`
   - `tableOid = 0`, `columnAttrNum = 0` (not available)
   - `dataTypeOid` from the type map
   - `dataTypeSize = -1`, `typeModifier = -1`
   - `format = 0` (text)
   - Skipped if the statement returns no rows (e.g. `INSERT` without `RETURNING`).

2. **`DataRow`** per record. Each field encoded as text bytes:
   - `longValue: 42` → `"42"`
   - `doubleValue: 3.14` → `"3.14"`
   - `booleanValue: true` → `"t"`
   - `stringValue: "x"` → `"x"`
   - `blobValue` (bytes) → pg bytea hex format `"\\x..."`
   - `isNull: true` → -1 length marker
   - `arrayValue` → recursive pg array literal `{a,b,c}` with proper quoting and null handling

3. **`CommandComplete`** tag, derived from a cheap regex on the leading SQL keyword:
   - `SELECT N` (N = `records.len()`)
   - `INSERT 0 N`, `UPDATE N`, `DELETE N` (N = `numberOfRecordsUpdated`)
   - `BEGIN`, `COMMIT`, `ROLLBACK` for txn ops

4. **`ReadyForQuery`** with current txn status flag.

### Errors

Data API errors (`BadRequestException`, `StatementTimeoutException`, etc.) become pg `ErrorResponse`:

- `Severity: ERROR`
- `Code: 42000` (or a more specific SQLSTATE when mappable)
- `Message: <Data API message>`

Inside a transaction, an error sets txn status to `E`; the client must `ROLLBACK` to recover.

## Type Map

Static `phf` map from `columnMetadata.typeName` to pg OID:

| typeName | pg OID | pg name |
|---|---|---|
| `bool` | 16 | bool |
| `bytea` | 17 | bytea |
| `int2` / `smallint` | 21 | int2 |
| `int4` / `integer` | 23 | int4 |
| `int8` / `bigint` | 20 | int8 |
| `float4` / `real` | 700 | float4 |
| `float8` / `double precision` | 701 | float8 |
| `numeric` / `decimal` | 1700 | numeric |
| `text` | 25 | text |
| `varchar` | 1043 | varchar |
| `bpchar` / `char` | 1042 | bpchar |
| `name` | 19 | name |
| `uuid` | 2950 | uuid |
| `json` | 114 | json |
| `jsonb` | 3802 | jsonb |
| `date` | 1082 | date |
| `time` | 1083 | time |
| `timestamp` | 1114 | timestamp |
| `timestamptz` | 1184 | timestamptz |
| `interval` | 1186 | interval |
| `_text` | 1009 | text[] |
| `_int4` | 1007 | int4[] |
| (unknown) | 25 | text (fallback, logged at warn) |

All values sent in pg text format (format code 0).

## Result Set Size

`ExecuteStatement` is capped at ~1 MB / 1000 rows by AWS. v1 returns the Data API error verbatim as a pg `ERROR`, prompting the user to add `LIMIT`. A future `--auto-paginate` flag may rewrite SELECTs into windowed `LIMIT/OFFSET` loops; not in v1 because rewriting risks silent wrong results on unstable orderings.

## Introspection Queries

Pg GUIs fire many catalog queries on connect (`SELECT version()`, `SHOW search_path`, queries against `pg_catalog.*`). These are passed through to the cluster as-is. Aurora PostgreSQL answers them correctly. The Data API rate limit (~1000 req/s) comfortably handles connect storms; if pain emerges, intercept-and-cache is a future option.

## Testing

- **Unit tests:**
  - Parameter rewriter (`$N` → `:pN`) including edge cases: dollar-quoted strings, comments, escaped quotes.
  - Type map lookup including arrays and unknowns.
  - Value formatter for each Data API field variant.
  - Intercept regex (true/false positives for SAVEPOINT, COPY, etc.).
  - Verb detection for `CommandComplete` tags.
- **Integration tests:**
  - Mock the AWS SDK client via a trait object; drive pg sessions with `tokio_postgres` and assert wire output.
  - Cover: simple query, extended query with params, transaction Begin/Commit/Rollback, error inside txn, intercepted statements.
- **End-to-end (optional, gated by env var):**
  - Real cluster in a test AWS account.
  - Smoke: connect, `SELECT 1`, `BEGIN; SELECT 1; COMMIT`, error path.

## Operations

- CLI: `pg-rds-connector --config <path> --listen 127.0.0.1:5433 --log-level debug`.
- Logging via `tracing`, structured. Connect events, target resolution, profile used, and Data API call counts at info; SQL bodies at debug only.
- Distribution: `cargo install pg-rds-connector` and prebuilt binaries (macOS, Linux) attached to GitHub Releases.
- No metrics in v1.

## Future Work

- `--auto-paginate` for large SELECTs.
- TLS between client and proxy if a non-loopback bind becomes useful.
- Catalog-query interception cache.
- Per-target connection pooling of SDK clients (currently per-pg-connection).
- Optional `psql`-style introspection shortcuts (`\d`).
