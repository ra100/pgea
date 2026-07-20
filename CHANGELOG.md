# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/ra100/pgea/compare/v0.2.0...v0.2.1) - 2026-07-20

### Other

- fix stale claims about module status and release workflow

## [0.2.0] - 2026-06-26

### Added

- Auto-pagination around the RDS Data API ~1 MB result cap. A `SELECT` that
  overflows the limit is transparently re-run in `LIMIT/OFFSET` windows held in
  a single `REPEATABLE READ` snapshot and reassembled into one result set — no
  client change required. Page size halves automatically for very wide rows; a
  single row over 1 MB still surfaces the error. (`rds/paginate.rs`)

## [0.1.1] - 2026-05-15

### Changed

- Default config path renamed from `~/.config/pg-rds-connector/config.toml`
  to `~/.config/pgea/config.toml`. Existing users must move their config
  file (or pass `--config <path>` / set `PG_RDS_CONNECTOR_CONFIG`).
- Release workflow no longer builds `x86_64-apple-darwin`. Intel Macs run the
  `aarch64-apple-darwin` binary under Rosetta 2, or build from source.
- `install.sh` resolves `releases/latest` more robustly: falls back to the
  GitHub API when no Release is published yet, and fails with an actionable
  message instead of `tag must start with 'v' (got: releases)`.

## [0.1.0] - 2026-05-15

Initial release of `pgea` — a local TCP proxy that speaks the PostgreSQL wire
protocol on the client side and translates queries into AWS RDS Data API calls
on the backend, so that ordinary pg GUIs (DBeaver, DataGrip, TablePlus, psql)
can be used against Aurora PostgreSQL clusters that live in private VPCs.

### Added

- PostgreSQL wire protocol front-end (Simple + Extended Query) over loopback
  using the `pgwire` crate. Always serves values in text format.
- AWS RDS Data API back-end via `aws-sdk-rdsdata`, with credentials resolved
  from the standard chain (`AWS_PROFILE`, env, SSO, IMDS).
- Per-connection target routing: pg `dbname` selects a named target from
  `~/.config/pg-rds-connector/config.toml`; the pg `password` field is
  repurposed as an AWS profile-name override (empty falls back to the target's
  `profile`, then `default_profile`).
- Transaction support: `BEGIN` / `COMMIT` / `ROLLBACK` map to
  `BeginTransaction` / `CommitTransaction` / `RollbackTransaction`. Errors
  inside a transaction set the txn status to `E` until the client `ROLLBACK`s.
- Extended Query path: `Parse` / `Bind` / `Describe` / `Execute` / `Sync`
  with `$N` → `:pN` rewriting (string-, comment-, and dollar-quote-aware
  lexer) and per-type binary-format parameter decoding.
- Type mapping: static `typeName` → pg OID table (~40 entries) covering the
  common scalar and array types. Unknown types fall back to `text` (OID 25)
  and log a warning.
- Catalog-query rewriting: targeted casts for the GUI introspection probes
  that the Data API refuses to return (`pg_type`, `pg_class`, `pg_namespace`,
  `pg_attribute`, `pg_collation`, `pg_constraint`, `pg_index`, `pg_depend`,
  `pg_am`), keeping DBeaver / DataGrip / `psql \d` working.
- Intercept layer that rejects unsupported ops (`SAVEPOINT`, `COPY`, cursors,
  `LISTEN` / `NOTIFY`) with a clean pg `ErrorResponse`.
- `pgea self-update` subcommand and `install.sh` one-liner installer that
  fetch the latest release archive from GitHub and place the binary on
  `$PATH`.
- GitHub Actions CI (fmt + clippy + test) and a release workflow that builds
  release artifacts for `aarch64-apple-darwin`, `x86_64-apple-darwin`, and
  `x86_64-unknown-linux-gnu` and publishes them to a tag-driven GitHub
  Release.
- Integration test suite (`tests/extended_query.rs`) that drives the proxy
  via `tokio-postgres` against a `MockRdsClient`, plus an env-gated end-to-end
  suite (`tests/e2e_aurora.rs`) for real Aurora clusters.

### Documentation

- README with install / configuration / GUI client setup walkthroughs.
- Full design spec under `docs/superpowers/specs/`.
- `Plans.md` milestone breakdown (M1–M7).

[Unreleased]: https://github.com/ra100/pgea/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/ra100/pgea/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ra100/pgea/releases/tag/v0.1.0
