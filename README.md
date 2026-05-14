# pg-rds-connector

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

`pg-rds-connector` is a single binary that:

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
- Auto-pagination around the Data API 1 MB result cap
- TLS to the client (loopback only)
- Prepared-statement caching across sessions

Adding any of these requires updating the spec first.

## Install

```sh
cargo build --release
# binary lands in target/release/pg-rds-connector
```

## Configure

Create `~/.config/pg-rds-connector/config.toml`:

```toml
listen        = "127.0.0.1:15000"
default_profile = "default"

[[targets]]
name        = "playground_uat"
profile     = "uat"
region      = "us-east-1"
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:my-cluster"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:my-cluster-credentials-XXXXXX"
database    = "appdb"
```

Validation at startup is structural only (TOML, ARN regex, duplicate names,
listen parses). Credentials are resolved lazily on first connection so a
config with stale SSO sessions for nine of ten targets still serves the
fresh one.

## Run

```sh
./target/release/pg-rds-connector
# or with a custom config / listen address
./target/release/pg-rds-connector --config ./config.toml --listen 127.0.0.1:15000
```

Logs at `info` cover connection events, target resolution, profile used, and
Data API call counts. `RUST_LOG=pg_rds_connector=debug` adds SQL bodies.

## Connect

```sh
psql -h 127.0.0.1 -p 15000 -d playground_uat -U postgres -W
```

Or in a GUI: host `127.0.0.1`, port `15000`, database `playground_uat`, user
anything, password either empty (uses target's `profile`) or an AWS profile
name.

For DBeaver / DataGrip:

- Disable SSL (loopback, no TLS in v1).
- Use the standard PostgreSQL JDBC driver.
- Database = the target name from `config.toml`.

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

- **1 MB result cap.** The Data API returns at most 1 MB per
  `ExecuteStatement`. Large `SELECT *` against wide tables fails. Use
  `LIMIT`, narrow projections, or expect to add pagination yourself.
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

E2E tests against a real cluster are gated by an env var so they don't run
by default.

## License

MIT — see [`LICENSE`](LICENSE).
