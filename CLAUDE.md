# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`pg-rds-connector` is a local TCP proxy that speaks the PostgreSQL wire protocol on the client side and translates queries into AWS RDS Data API calls on the backend. It exists so developers can use ordinary pg GUIs (DBeaver, DataGrip, TablePlus, psql) against Aurora PostgreSQL clusters that live in private VPCs and are not directly reachable. AWS credentials come from the standard credential chain (`AWS_PROFILE`, env, SSO, IMDS).

The full design lives in `docs/superpowers/specs/2026-05-14-pg-rds-connector-design.md`. Read it before making non-trivial changes — it defines the boundaries, what is and is not in scope for v1, and the rationale behind decisions.

## Stack

- Rust, Tokio async runtime, single binary.
- Pg wire framing via the `pgwire` crate.
- AWS via `aws-sdk-rdsdata` and `aws-config`.
- Logging via `tracing`. CLI via `clap`. Config via `serde` + `toml`.

## Commands

The crate is not yet scaffolded. Once `Cargo.toml` exists, the conventional commands are:

- Build: `cargo build` (release: `cargo build --release`)
- Run: `cargo run -- --config <path> --listen 127.0.0.1:5433`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Format check: `cargo fmt --check`
- Tests: `cargo test`
- Single test: `cargo test <test_name>` or `cargo test --test <integration_file> <test_name>`
- E2E tests against a real cluster are gated by an env var (TBD when implemented; the spec calls for it to be opt-in).

## Architecture

The proxy is structured around two strict module boundaries:

- `pg::*` — speaks pg wire only. Knows nothing of AWS.
- `rds::*` — speaks RDS Data API only. Knows nothing of pg wire.
- `server.rs` — orchestrates a pg session against an rds session. The only place where the two halves meet.

Per-pg-connection state lives in a Tokio task and holds: resolved target (cluster ARN, secret ARN, database, region, profile), an `aws_sdk_rdsdata::Client` initialised for that profile, an optional active `transactionId`, and prepared-statement / portal maps for the Extended Query protocol.

### Translation flow

- Simple Query (`Q`): intercept layer rejects unsupported ops (`SAVEPOINT`, `COPY`, cursors, `LISTEN`/`NOTIFY`) with a clean pg error; transaction verbs map to `BeginTransaction` / `CommitTransaction` / `RollbackTransaction`; everything else is `ExecuteStatement`.
- Extended Query (`P`/`B`/`D`/`E`/`S`): `Parse` stores SQL+param OIDs, `Bind` rewrites `$N` → `:pN` (lexer skips strings, comments, dollar-quoted blocks) and converts pg-encoded params to Data API `SqlParameter`s, `Execute` runs `ExecuteStatement` and emits `RowDescription` + `DataRow`s + `CommandComplete`, `Sync` emits `ReadyForQuery` with txn status.
- Response translation: `columnMetadata.typeName` is mapped to pg OIDs via a static `phf` table; values are always sent in pg text format. Data API errors become pg `ErrorResponse`; errors inside a transaction set the txn status to `E` until the client `ROLLBACK`s.

### Configuration

Config file at `~/.config/pg-rds-connector/config.toml` (override via `--config`). Targets are named; pg `dbname` selects the target. The pg `password` field is repurposed as an AWS profile-name override (empty falls back to the target's `profile`, then `default_profile`). Validation at startup is structural only (TOML, ARN regex, dup names, `listen` parses) — credentials are resolved lazily on first connection so a config with stale SSO sessions for nine of ten targets still serves the fresh one.

## Conventions

- Loopback only (`127.0.0.1`). No TLS to clients in v1.
- Always send pg values in text format (format code 0). The static type map is the single source of truth for OIDs; unknown `typeName`s fall back to `text` (OID 25) and log at warn.
- No SQL parsing beyond what the spec calls for: a leading-keyword regex for verb detection / intercept, and a parameter-rewriting lexer that respects strings, comments, and dollar-quoted blocks.
- Out of scope for v1: `COPY`, server-side cursors, `LISTEN`/`NOTIFY`, `SAVEPOINT`, prepared-statement caching across sessions, multi-statement Simple Query, auto-pagination, TLS to client. Adding any of these requires updating the spec first.
- Logging: connection events, target resolution, profile used, and Data API call counts at `info`; SQL bodies only at `debug`.
