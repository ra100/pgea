//! End-to-end smoke tests against a real Aurora cluster.
//!
//! These tests are gated by `PG_RDS_CONNECTOR_E2E=1` and additional env vars
//! that point at a real cluster. Without the gate they no-op and `cargo test`
//! stays green. With the gate they spin up an in-process proxy backed by a
//! real `AwsRdsClient`, connect via `tokio-postgres`, and exercise the
//! Simple Query path, transaction verbs, and the unsupported-op intercept.
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

use pg_rds_connector::config::Config;
use pg_rds_connector::pg::server::run_with_listener;
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
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("savepoint") || msg.contains("not supported") || msg.contains("0a000"),
        "unexpected error message: {msg}"
    );
}
