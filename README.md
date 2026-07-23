# pgea

[![CI](https://github.com/ra100/pgea/actions/workflows/ci.yml/badge.svg)](https://github.com/ra100/pgea/actions/workflows/ci.yml)
[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/D4I523F8EW)

Local PostgreSQL wire-protocol proxy that translates queries into [AWS RDS
Data API](https://docs.aws.amazon.com/rdsdataservice/latest/APIReference/Welcome.html)
calls.

Use ordinary PostgreSQL GUIs (DBeaver, DataGrip, TablePlus, `psql`) against
Aurora PostgreSQL clusters that live in private VPCs and are not directly
reachable from your laptop. AWS credentials come from the standard credential
chain (`AWS_PROFILE`, env vars, SSO, IMDS).

## Why

Aurora clusters in private VPCs typically need either a bastion host, a VPN,
or a VPC endpoint to reach. None of these are convenient for ad-hoc
exploration, and most pg GUIs don't speak the RDS Data API natively.

`pgea` is a single binary that:

- Listens on `127.0.0.1:<port>` and speaks the standard pg wire protocol.
- Translates each query into `ExecuteStatement` (and `BeginTransaction` /
  `CommitTransaction` / `RollbackTransaction`) calls against the Data API.
- Picks the AWS profile, cluster ARN, secret ARN, database, and region from
  named targets in a TOML config file.

You connect with `psql -h 127.0.0.1 -p <port> -d <target> -U postgres -W`.
The `dbname` selects the target. The `password` field is repurposed as an
optional AWS profile override.

## Status

Early. v1 covers connection routing, Simple + Extended Query, transaction
verbs, and a small set of catalog-query rewrites needed for DBeaver
compatibility against Aurora's stricter Data API result-type allowlist. See
`docs/superpowers/specs/2026-05-14-pg-rds-connector-design.md` for the full
design and what is explicitly out of scope.

Not yet supported:

- `COPY`, server-side cursors, `LISTEN` / `NOTIFY`, `SAVEPOINT`
- Multi-statement Simple Query
- TLS to the client (loopback only)
- Prepared-statement caching across sessions

Adding any of these requires updating the spec first.

## Install

One-liner (macOS arm64, Linux x86_64; Intel Macs run the arm64 binary under Rosetta 2 or build from source):

```sh
curl -fsSL https://raw.githubusercontent.com/ra100/pgea/main/install.sh | bash
```

The installer downloads the latest tagged release, verifies the SHA-256
checksum, and installs `pgea` into `$HOME/.local/bin` (override with
`PGEA_BIN_DIR`). Pin a version with `PGEA_VERSION=v0.2.0`.

Update later with either:

```sh
pgea self-update                     # in-place upgrade
curl -fsSL https://raw.githubusercontent.com/ra100/pgea/main/install.sh | bash
```

From source via cargo:

```sh
cargo install --git https://github.com/ra100/pgea
# binary lands in ~/.cargo/bin/pgea
```

Or build a local checkout:

```sh
cargo build --release
# binary lands in target/release/pgea
```

Prebuilt macOS (arm64) and Linux (x86_64) binaries are attached to each
tagged GitHub release. Intel Mac users can run the arm64 binary under
Rosetta 2 or build from source via `cargo install --git`.

### Releasing (maintainers)

Releases are automated end to end. `release-plz` watches conventional-commit
PRs merged to `main` and keeps an up-to-date "Release" PR that bumps `version`
in `Cargo.toml` and updates `CHANGELOG.md`. Merging that PR publishes the
crate to crates.io and pushes the version bump to `main`; the `autotag`
workflow then creates and pushes the matching `vX.Y.Z` tag, and `release`
builds the binaries and publishes the GitHub Release (marked latest, which is
what `pgea self-update` pulls). No manual version bumping or tagging needed.

## Configure

Create `~/.config/pgea/config.toml`:

```toml
listen          = "127.0.0.1:15000"
default_profile = "default"

[targets.playground_uat]
profile     = "uat"
region      = "us-east-1"
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:my-cluster"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:my-cluster-credentials-XXXXXX"
database    = "appdb"

[targets.playground_uat_ro]
profile     = "uat"
region      = "us-east-1"
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:my-cluster"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:my-cluster-credentials-XXXXXX"
database    = "appdb"
read_only   = true
```

Targets are a map keyed by name; the section header (`[targets.<name>]`)
sets the name. The pg `dbname` field selects the target.

Set `read_only = true` on a target to make it a hard read-only boundary.
Every statement then runs inside an engine-enforced `SET TRANSACTION READ
ONLY` transaction, so PostgreSQL itself rejects any write — including ones a
leading-keyword check cannot see: writable CTEs (`WITH ... AS (INSERT ...)`),
`SELECT`s that call volatile writing functions, and `EXPLAIN ANALYZE` of a
write. A fast-reject layer additionally turns the obvious write shapes (DML,
DDL, GRANT/REVOKE, VACUUM/…, CALL, `EXPLAIN ANALYZE`, writable CTEs) into a
clean pg error with no Data API round-trip. Reads, transaction verbs, and
harmless session verbs (`SET`/`SHOW`/`RESET`) still work, so GUI clients
connect normally. Defaults to `false`.

Cost: each statement on a read-only target is three Data API calls
(begin/set-read-only/commit) instead of one, so reads are slower and cost
more. That is the price of an engine-enforced guarantee.

Validation at startup is structural only (TOML, ARN regex, duplicate names,
listen parses). Credentials are resolved lazily on first connection so a
config with stale SSO sessions for nine of ten targets still serves the
fresh one.

## Run

```sh
./target/release/pgea
# or with a custom config / listen address
./target/release/pgea --config ./config.toml --listen 127.0.0.1:15000
```

Logs at `info` cover connection events, target resolution, profile used, and
Data API call counts. `RUST_LOG=pgea=debug` adds SQL bodies.

## Connect

```sh
psql -h 127.0.0.1 -p 15000 -d playground_uat -U postgres -W
```

Or in a GUI: host `127.0.0.1`, port `15000`, database `playground_uat`, user
anything, password either empty (uses target's `profile`) or an AWS profile
name.

### DBeaver

1. New Connection → PostgreSQL.
2. **Main** tab:
   - Host: `127.0.0.1`
   - Port: the `listen` port from `config.toml` (e.g. `15000`)
   - Database: the target name (e.g. `playground_uat`)
   - Username: any non-empty value (DBeaver requires one; the proxy
     ignores it on loopback)
   - Password: leave empty to use the target's `profile`, or set it to an
     AWS profile name to override
   - Save password locally: optional
3. **Driver properties**: defaults are fine.
4. **SSL** tab: `Use SSL` → off. v1 has no TLS to the client; the listener
   is bound to loopback only.
5. Test Connection. The proxy log should show
   `pg startup` and the resolved profile.

Schema browser, table list, columns, constraints, and indexes load
end-to-end against Aurora. The Data API 1 MB cap still applies — wide
`SELECT *` over large tables fails; `LIMIT` or narrower projections work.

### DataGrip

1. New Data Source → PostgreSQL.
2. **General** tab:
   - Host: `127.0.0.1`, Port: `15000`, Database: target name.
   - User: any value. Password: empty or AWS profile name.
   - Authentication: `User & Password`.
3. **SSH/SSL** tab: `Use SSL` → off.
4. Use the bundled PostgreSQL JDBC driver. Test Connection.

### TablePlus

1. Create connection → PostgreSQL.
2. Host: `127.0.0.1`, Port: `15000`, Database: target name.
3. User: any value. Password: empty or AWS profile name.
4. SSL mode: `disable`.

### Caveats specific to GUIs

- The proxy rewrites a known set of `pg_catalog.*` probes that hit Data
  API result-type restrictions (`bpchar`, `oid[]`, `regproc`, …). Your
  own queries that touch those types will still fail; cast to `text` in
  the SQL.
- No multi-statement `Q` support. Batch script execution that sends
  several statements separated by `;` in a single Simple Query message
  errors out. Run them one at a time.
- No `LISTEN` / `NOTIFY`, no server-side cursors, no `SAVEPOINT`. The
  intercept layer rejects these with a clean pg `ErrorResponse` so the
  GUI sees a normal SQL error rather than a connection drop.

## Architecture

Two strict module boundaries:

- `pg::*` — speaks pg wire only. No AWS knowledge.
- `rds::*` — speaks RDS Data API only. No pg-wire knowledge.
- `server.rs` orchestrates a pg session against an rds session. The only
  place where the two halves meet.

Per-pg-connection state lives in a Tokio task and holds: resolved target
(cluster ARN, secret ARN, database, region, profile), an
`aws_sdk_rdsdata::Client` initialised for that profile, an optional active
`transactionId`, and prepared-statement / portal maps for the Extended Query
protocol.

## Constraints worth knowing

- **1 MB result cap (auto-paginated).** The Data API returns at most 1 MB per
  `ExecuteStatement`. When a `SELECT` overflows it, the proxy transparently
  re-runs the query in `LIMIT/OFFSET` windows inside a single `REPEATABLE READ`
  snapshot and stitches the pages back into one result — no client change
  needed. The page size halves automatically if a window is still too wide; a
  single row larger than 1 MB still fails. Non-`SELECT` statements are not
  wrapped. Only the original query needs a stable order for `OFFSET` to be
  meaningful; add an `ORDER BY` if row order matters to you.
- **Type allowlist.** Aurora's Data API refuses to return rows containing
  certain Postgres types (`CHAR`/`bpchar`, `regproc`, `pg_node_tree`,
  `aclitem`, `xid`, `int2vector`, `oidvector`, `oid[]`, ...). The proxy
  rewrites a small set of catalog queries used by GUIs to cast or NULL these
  out; user queries that hit them still fail. Cast to `text` in your SQL.
- **No streaming.** Each statement is one round-trip; no server-side
  cursors.
- **Loopback only.** Binding non-loopback would let any local user supply
  someone else's AWS profile name in the password field and ride their
  credentials. The listener is constrained at config-validation time.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The crate is structured so each pure-logic unit has its own unit tests:

- `catalog.rs` — targeted catalog query rewrites for DBeaver / DataGrip
  introspection
- `config.rs` — TOML config + validation
- `intercept.rs` — verb classification (transaction control, rejected ops,
  passthrough)
- `rewriter.rs` — `$N` → `:pN` parameter rewriting (lexer respects strings,
  comments, dollar-quoted blocks)
- `types.rs` — Postgres `typeName` → OID mapping for response translation
- `pg::server` — wire protocol handlers
- `rds::client` — SDK wrapper + a mock for tests
- `rds::txn` — transaction state machine

E2E tests against a real cluster live in `tests/e2e_aurora.rs` and are
gated behind `PG_RDS_CONNECTOR_E2E=1`. Without the gate they no-op and
`cargo test` stays green. To run them:

```sh
PG_RDS_CONNECTOR_E2E=1 \
  PG_RDS_CONNECTOR_E2E_CLUSTER_ARN=arn:aws:rds:us-east-1:123456789012:cluster:... \
  PG_RDS_CONNECTOR_E2E_SECRET_ARN=arn:aws:secretsmanager:us-east-1:123456789012:secret:... \
  PG_RDS_CONNECTOR_E2E_DATABASE=appdb \
  PG_RDS_CONNECTOR_E2E_REGION=us-east-1 \
  PG_RDS_CONNECTOR_E2E_PROFILE=my-sso-profile \
  cargo test --test e2e_aurora -- --nocapture --test-threads=1
```

The smoke covers `SELECT 1`, a `BEGIN ... ROLLBACK` round-trip, and the
`SAVEPOINT` intercept path. AWS credentials come from the standard chain;
`PG_RDS_CONNECTOR_E2E_PROFILE` is optional.

## License

MIT — see [`LICENSE`](LICENSE).
