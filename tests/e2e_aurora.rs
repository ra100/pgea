//! End-to-end smoke tests against a real Aurora cluster.
//!
//! These tests are gated by `PG_RDS_CONNECTOR_E2E=1` and additional env vars
//! that point at a real cluster. Without the gate they no-op and `cargo test`
//! stays green. With the gate they spin up an in-process proxy backed by a
//! real `AwsRdsClient`, connect via `tokio-postgres`, and exercise the
//! Simple Query path, transaction verbs, the unsupported-op intercept, DML
//! command tags, Extended Query parameter binding, and the aborted-txn
//! (25P02) state machine -- all against the real RDS Data API rather than
//! the `MockRdsClient`/testcontainers doubles used elsewhere, so bugs in the
//! actual AWS request/response translation surface here.
//!
//! Tests that create tables use a distinct table name and drop it (`DROP
//! TABLE IF EXISTS ... CASCADE`) both before and after, so they stay
//! parallel-safe against a shared cluster even if a prior run panicked
//! mid-test.
//!
//! Required env vars when `PG_RDS_CONNECTOR_E2E=1`:
//!
//! - `PG_RDS_CONNECTOR_E2E_CLUSTER_ARN` — Aurora cluster ARN
//! - `PG_RDS_CONNECTOR_E2E_SECRET_ARN`  — secret ARN with cluster credentials
//! - `PG_RDS_CONNECTOR_E2E_DATABASE`    — database name on the cluster
//! - `PG_RDS_CONNECTOR_E2E_REGION`      — AWS region (e.g. `us-east-1`)
//! - `PG_RDS_CONNECTOR_E2E_PROFILE`     — optional AWS profile (defaults to
//!   the standard credential chain)
//!
//! Run them with:
//!
//! ```sh
//! PG_RDS_CONNECTOR_E2E=1 \
//!   PG_RDS_CONNECTOR_E2E_CLUSTER_ARN=arn:aws:rds:... \
//!   PG_RDS_CONNECTOR_E2E_SECRET_ARN=arn:aws:secretsmanager:... \
//!   PG_RDS_CONNECTOR_E2E_DATABASE=appdb \
//!   PG_RDS_CONNECTOR_E2E_REGION=us-east-1 \
//!   cargo test --test e2e_aurora -- --nocapture --test-threads=1
//! ```

use std::env;
use std::sync::Arc;
use std::time::Duration;

use pgea::config::Config;
use pgea::pg::server::run_with_listener;
use tokio::net::TcpListener;

const ENV_GATE: &str = "PG_RDS_CONNECTOR_E2E";
const ENV_CLUSTER: &str = "PG_RDS_CONNECTOR_E2E_CLUSTER_ARN";
const ENV_SECRET: &str = "PG_RDS_CONNECTOR_E2E_SECRET_ARN";
const ENV_DATABASE: &str = "PG_RDS_CONNECTOR_E2E_DATABASE";
const ENV_REGION: &str = "PG_RDS_CONNECTOR_E2E_REGION";
const ENV_PROFILE: &str = "PG_RDS_CONNECTOR_E2E_PROFILE";

/// Returns `Some(config)` when the gate is set and all required env vars are
/// present. Returns `None` (and prints a hint) otherwise so the test can
/// no-op cleanly under `cargo test`.
fn e2e_config_or_skip(test_name: &str) -> Option<Arc<Config>> {
    if env::var(ENV_GATE).ok().as_deref() != Some("1") {
        eprintln!(
            "[skip] {test_name}: set {ENV_GATE}=1 plus cluster/secret/database/region env vars to run"
        );
        return None;
    }

    let cluster_arn = match env::var(ENV_CLUSTER) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[skip] {test_name}: missing {ENV_CLUSTER}");
            return None;
        }
    };
    let secret_arn = match env::var(ENV_SECRET) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[skip] {test_name}: missing {ENV_SECRET}");
            return None;
        }
    };
    let database = match env::var(ENV_DATABASE) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[skip] {test_name}: missing {ENV_DATABASE}");
            return None;
        }
    };
    let region = match env::var(ENV_REGION) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[skip] {test_name}: missing {ENV_REGION}");
            return None;
        }
    };
    let profile_line = env::var(ENV_PROFILE)
        .ok()
        .map(|p| format!("profile     = \"{p}\"\n"))
        .unwrap_or_default();

    // Listen address is overridden by `spawn_real_proxy` (it binds an
    // ephemeral port directly), so any valid loopback address is fine here.
    let toml = format!(
        r#"
listen = "127.0.0.1:5433"

[targets.e2e]
cluster_arn = "{cluster_arn}"
secret_arn  = "{secret_arn}"
database    = "{database}"
region      = "{region}"
{profile_line}"#
    );
    let config = Config::parse(&toml).expect("e2e config parses");
    Some(Arc::new(config))
}

/// Bind an ephemeral 127.0.0.1 port and spawn the proxy with `test_client=None`
/// so it constructs a real `AwsRdsClient` via the standard credential chain.
async fn spawn_real_proxy(config: Arc<Config>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = run_with_listener(listener, config, None).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

async fn connect(addr: &str) -> tokio_postgres::Client {
    let host = addr.split(':').next().unwrap().to_owned();
    let port: u16 = addr.split(':').nth(1).unwrap().parse().unwrap();
    let mut config = tokio_postgres::config::Config::new();
    config
        .host(&host)
        .port(port)
        .user("postgres")
        .password("")
        .dbname("e2e");
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("pg connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
async fn e2e_select_one() {
    let Some(config) = e2e_config_or_skip("e2e_select_one") else {
        return;
    };
    let addr = spawn_real_proxy(config).await;
    let client = connect(&addr).await;

    let rows = client
        .simple_query("SELECT 1")
        .await
        .expect("simple_query SELECT 1");
    let data: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(data.len(), 1, "expected one row");
    assert_eq!(data[0].get(0), Some("1"));
}

#[tokio::test]
async fn e2e_transaction_roundtrip() {
    let Some(config) = e2e_config_or_skip("e2e_transaction_roundtrip") else {
        return;
    };
    let addr = spawn_real_proxy(config).await;
    let client = connect(&addr).await;

    // BEGIN ... ROLLBACK is enough to prove the proxy routes BeginTransaction
    // and RollbackTransaction at the Data API layer without mutating data.
    client.simple_query("BEGIN").await.expect("BEGIN");
    let rows = client
        .simple_query("SELECT current_database()")
        .await
        .expect("SELECT current_database()");
    let data: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(data.len(), 1, "expected one row inside txn");
    client.simple_query("ROLLBACK").await.expect("ROLLBACK");
}

#[tokio::test]
async fn e2e_intercepted_op_returns_clean_error() {
    let Some(config) = e2e_config_or_skip("e2e_intercepted_op_returns_clean_error") else {
        return;
    };
    let addr = spawn_real_proxy(config).await;
    let client = connect(&addr).await;

    // SAVEPOINT is on the v1 reject list. The proxy should respond with a
    // pg ErrorResponse (feature_not_supported / 0A000) without ever touching
    // the Data API.
    let res = client.simple_query("SAVEPOINT s1").await;
    let err = res.expect_err("expected SAVEPOINT to be rejected");
    // tokio_postgres::Error's Display just prints "db error"; the actual pg
    // ErrorResponse fields (SQLSTATE + message) live on the wrapped DbError.
    let db_err = err.as_db_error().expect("expected a pg ErrorResponse");
    assert_eq!(db_err.code().code(), "0A000");
    assert!(
        db_err.message().to_lowercase().contains("savepoint"),
        "unexpected error message: {}",
        db_err.message()
    );
}

/// Drops `table` (if present) both before and after the closure runs, so a
/// panic mid-test doesn't leave debris for the next run against the shared
/// cluster.
async fn with_scratch_table<F, Fut>(client: &tokio_postgres::Client, table: &str, body: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let drop_sql = format!("DROP TABLE IF EXISTS {table} CASCADE");
    client.simple_query(&drop_sql).await.expect("pre-drop");
    body().await;
    client.simple_query(&drop_sql).await.expect("post-drop");
}

#[tokio::test]
async fn e2e_dml_command_tags_report_real_row_counts() {
    let Some(config) = e2e_config_or_skip("e2e_dml_command_tags_report_real_row_counts") else {
        return;
    };
    let addr = spawn_real_proxy(config).await;
    let client = connect(&addr).await;

    with_scratch_table(&client, "e2e_dml_widgets", || async {
        client
            .simple_query("CREATE TABLE e2e_dml_widgets (id INT, label TEXT)")
            .await
            .expect("create table");

        let insert = client
            .simple_query("INSERT INTO e2e_dml_widgets VALUES (1, 'a'), (2, 'b'), (3, 'c')")
            .await
            .expect("insert");
        assert_eq!(rows_affected(&insert), Some(3), "INSERT row count");

        let update = client
            .simple_query("UPDATE e2e_dml_widgets SET label = 'z' WHERE id <= 2")
            .await
            .expect("update");
        assert_eq!(rows_affected(&update), Some(2), "UPDATE row count");

        let delete = client
            .simple_query("DELETE FROM e2e_dml_widgets WHERE id = 3")
            .await
            .expect("delete");
        assert_eq!(rows_affected(&delete), Some(1), "DELETE row count");

        let rows = client
            .simple_query("SELECT id, label FROM e2e_dml_widgets ORDER BY id")
            .await
            .expect("select");
        let data = rows_of(rows);
        assert_eq!(data.len(), 2, "expected the 2 updated rows to remain");
        assert_eq!(data[0].get("label"), Some("z"));
    })
    .await;
}

#[tokio::test]
async fn e2e_extended_query_binds_params_through_real_data_api() {
    let Some(config) = e2e_config_or_skip("e2e_extended_query_binds_params_through_real_data_api")
    else {
        return;
    };
    let addr = spawn_real_proxy(config).await;
    let client = connect(&addr).await;

    with_scratch_table(&client, "e2e_extended_accounts", || async {
        client
            .simple_query("CREATE TABLE e2e_extended_accounts (id INT, label TEXT)")
            .await
            .expect("create table");

        use tokio_postgres::types::Type as PgType;
        let stmt = client
            .prepare_typed(
                "INSERT INTO e2e_extended_accounts(id, label) VALUES ($1, $2)",
                &[PgType::INT4, PgType::TEXT],
            )
            .await
            .expect("prepare_typed");
        let affected = client
            .execute(&stmt, &[&42_i32, &"widget"])
            .await
            .expect("extended insert through real Data API");
        assert_eq!(affected, 1);

        // Verify via simple_query, not client.query(): tokio-postgres' typed
        // query() flow demands a RowDescription from Describe(Statement),
        // which runs before Bind -- before the Data API (execute-only, no
        // real prepare) has ever run the statement and can report a schema.
        // The design spec documents this: Describe(Statement) returns NoData
        // on purpose, and Describe(Portal) supplies the schema instead (the
        // path JDBC/DBeaver/DataGrip use). Simple Query sidesteps the
        // mismatch entirely, which is enough to prove the bound param
        // reached the real Data API and matched the inserted row.
        let rows = client
            .simple_query("SELECT label FROM e2e_extended_accounts WHERE id = 42")
            .await
            .expect("select back the row inserted via extended query");
        let data = rows_of(rows);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].get("label"), Some("widget"));
    })
    .await;
}

#[tokio::test]
async fn e2e_error_inside_transaction_aborts_until_rollback() {
    let Some(config) = e2e_config_or_skip("e2e_error_inside_transaction_aborts_until_rollback")
    else {
        return;
    };
    let addr = spawn_real_proxy(config).await;
    let client = connect(&addr).await;

    with_scratch_table(&client, "e2e_txn_abort_ledger", || async {
        client
            .simple_query("CREATE TABLE e2e_txn_abort_ledger (id INT)")
            .await
            .expect("create table");

        client.simple_query("BEGIN").await.expect("BEGIN");

        // A bogus statement makes the Data API return an error mid-txn,
        // which must flip the proxy's txn status to Failed (pg 25P02).
        let bad = client.simple_query("SELECT * FROM no_such_table_xyz").await;
        assert!(bad.is_err(), "querying a nonexistent table must fail");

        // Any further statement, even a trivially valid one, must be
        // rejected with 25P02 while the txn is aborted -- Data API is never
        // consulted for it.
        let res = client
            .simple_query("INSERT INTO e2e_txn_abort_ledger VALUES (1)")
            .await;
        let err = res.expect_err("statement after txn error must be rejected");
        let db_err = err.as_db_error().expect("expected a pg ErrorResponse");
        assert_eq!(db_err.code().code(), "25P02");

        client.simple_query("ROLLBACK").await.expect("ROLLBACK");

        // Once rolled back, the connection is usable again and the failed
        // insert attempt (rejected before ever reaching Data API) left no
        // row behind.
        let rows = client
            .simple_query("SELECT id FROM e2e_txn_abort_ledger")
            .await
            .expect("select after rollback");
        assert!(rows_of(rows).is_empty());
    })
    .await;
}

/// Extracts the affected-row count from a `simple_query` response's
/// `CommandComplete` message, if present.
fn rows_affected(messages: &[tokio_postgres::SimpleQueryMessage]) -> Option<u64> {
    messages.iter().find_map(|m| match m {
        tokio_postgres::SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    })
}

fn rows_of(
    messages: Vec<tokio_postgres::SimpleQueryMessage>,
) -> Vec<tokio_postgres::SimpleQueryRow> {
    messages
        .into_iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect()
}
