//! Integration tests that run the proxy against a *real* local Postgres
//! (via `testcontainers`) instead of the canned `RecordingMock`. This proves
//! genuine SQL/transaction semantics — in particular real ROLLBACK behavior
//! — that a mock can't verify.
//!
//! Gated behind `PGEA_LOCAL_PG_TESTS=1` (mirrors the `[skip]` idiom in
//! `tests/e2e_aurora.rs`) so `cargo test` stays green without Docker.

mod support;

use std::env;
use std::sync::Arc;
use std::time::Duration;

use pgea::config::Config;
use pgea::pg::server::run_with_listener;
use pgea::rds::RdsClient;
use support::PgRdsClient;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tokio::net::TcpListener;

const ENV_GATE: &str = "PGEA_LOCAL_PG_TESTS";

/// Returns `true` when the gate is set, otherwise prints a `[skip]` hint and
/// returns `false` so callers can no-op cleanly under plain `cargo test`.
fn gate_or_skip(test_name: &str) -> bool {
    if env::var(ENV_GATE).ok().as_deref() != Some("1") {
        eprintln!("[skip] {test_name}: set {ENV_GATE}=1 (requires Docker) to run");
        return false;
    }
    true
}

fn config_for_target(name: &str, listen: &str) -> Arc<Config> {
    let toml = format!(
        r#"
listen = "{listen}"
default_profile = "default"

[targets.{name}]
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:test-c"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:test-s"
database    = "appdb"
region      = "us-east-1"
profile     = "test"
"#
    );
    Arc::new(Config::parse(&toml).expect("config parses"))
}

async fn spawn_proxy(target: &str, client: Arc<dyn RdsClient>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let config = config_for_target(target, &addr.to_string());
    tokio::spawn(async move {
        let _ = run_with_listener(listener, config, Some(client)).await;
    });
    // Tiny pause so the accept loop is hot before the client tries to dial.
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

async fn connect(addr: &str, dbname: &str) -> tokio_postgres::Client {
    let host = addr.split(':').next().unwrap().to_owned();
    let port: u16 = addr.split(':').nth(1).unwrap().parse().unwrap();
    let mut config = tokio_postgres::config::Config::new();
    config
        .host(&host)
        .port(port)
        .user("postgres")
        .password("")
        .dbname(dbname);
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("pg connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Start a postgres container and a `PgRdsClient` pointed at it. Returns the
/// container too so the caller keeps it alive for the test's duration (drop
/// stops the container).
async fn start_backend() -> (ContainerAsync<Postgres>, Arc<PgRdsClient>) {
    // `with_host_auth()` switches the container to trust auth so
    // `PgRdsClient::connect` (which sends no password) can connect —
    // avoids threading a password through the RdsClient trait for a
    // test-only double.
    let container = Postgres::default()
        .with_host_auth()
        .start()
        .await
        .expect("start postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("mapped port");
    let client = PgRdsClient::connect("127.0.0.1", port, "postgres", "postgres").await;
    (container, Arc::new(client))
}

#[tokio::test]
async fn simple_select_against_real_postgres() {
    if !gate_or_skip("simple_select_against_real_postgres") {
        return;
    }
    let (_container, backend) = start_backend().await;
    let addr = spawn_proxy("dev", backend.clone()).await;
    let client = connect(&addr, "dev").await;

    client
        .simple_query("CREATE TABLE widgets (id INT, name TEXT, active BOOL)")
        .await
        .expect("create table");
    client
        .simple_query("INSERT INTO widgets VALUES (1, 'foo', true), (2, 'bar', false)")
        .await
        .expect("seed rows");

    let rows = client
        .simple_query("SELECT id, name, active FROM widgets ORDER BY id")
        .await
        .expect("select");
    let data: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0].get("id"), Some("1"));
    assert_eq!(data[0].get("name"), Some("foo"));
    assert_eq!(data[0].get("active"), Some("t"));
    assert_eq!(data[1].get("id"), Some("2"));
    assert_eq!(data[1].get("active"), Some("f"));
}

#[tokio::test]
async fn extended_query_insert_and_select_real_row() {
    if !gate_or_skip("extended_query_insert_and_select_real_row") {
        return;
    }
    let (_container, backend) = start_backend().await;
    let addr = spawn_proxy("dev", backend.clone()).await;
    let client = connect(&addr, "dev").await;

    client
        .simple_query("CREATE TABLE accounts (id INT, label TEXT)")
        .await
        .expect("create table");

    use tokio_postgres::types::Type as PgType;
    let stmt = client
        .prepare_typed(
            "INSERT INTO accounts(id, label) VALUES ($1, $2)",
            &[PgType::INT4, PgType::TEXT],
        )
        .await
        .expect("prepare_typed");
    let affected = client
        .execute(&stmt, &[&42_i32, &"widget"])
        .await
        .expect("extended insert");
    assert_eq!(affected, 1);

    let rows = client
        .simple_query("SELECT id, label FROM accounts")
        .await
        .expect("select");
    let data: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].get("id"), Some("42"));
    assert_eq!(data[0].get("label"), Some("widget"));
}

#[tokio::test]
async fn transaction_rollback_discards_insert() {
    if !gate_or_skip("transaction_rollback_discards_insert") {
        return;
    }
    let (_container, backend) = start_backend().await;
    let addr = spawn_proxy("dev", backend.clone()).await;
    let client = connect(&addr, "dev").await;

    client
        .simple_query("CREATE TABLE ledger (id INT)")
        .await
        .expect("create table");

    // BEGIN / INSERT / ROLLBACK: proves real transaction semantics, the main
    // value-add over the canned mock.
    client.simple_query("BEGIN").await.expect("BEGIN");
    client
        .simple_query("INSERT INTO ledger VALUES (1)")
        .await
        .expect("INSERT inside txn");
    client.simple_query("ROLLBACK").await.expect("ROLLBACK");

    let rows = client
        .simple_query("SELECT id FROM ledger")
        .await
        .expect("select after rollback");
    let data: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert!(data.is_empty(), "row must not persist after ROLLBACK");
}

#[tokio::test]
async fn transaction_commit_persists_insert() {
    if !gate_or_skip("transaction_commit_persists_insert") {
        return;
    }
    let (_container, backend) = start_backend().await;
    let addr = spawn_proxy("dev", backend.clone()).await;
    let client = connect(&addr, "dev").await;

    client
        .simple_query("CREATE TABLE ledger (id INT)")
        .await
        .expect("create table");

    // Companion to `transaction_rollback_discards_insert`: without this, a
    // regression that silently turned ROLLBACK into a no-op could still pass
    // that test (Postgres auto-aborts an in-progress transaction when its
    // connection closes, whether or not an explicit ROLLBACK ran). Asserting
    // the row *does* persist after COMMIT catches the mirror-image bug — a
    // COMMIT that never actually runs would also lose the row.
    client.simple_query("BEGIN").await.expect("BEGIN");
    client
        .simple_query("INSERT INTO ledger VALUES (1)")
        .await
        .expect("INSERT inside txn");
    client.simple_query("COMMIT").await.expect("COMMIT");

    let rows = client
        .simple_query("SELECT id FROM ledger")
        .await
        .expect("select after commit");
    let data: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(data.len(), 1, "row must persist after COMMIT");
    assert_eq!(data[0].get("id"), Some("1"));
}

#[tokio::test]
async fn rollback_removes_transaction_from_map() {
    if !gate_or_skip("rollback_removes_transaction_from_map") {
        return;
    }
    let (_container, backend) = start_backend().await;

    // Drives `PgRdsClient` directly (bypassing the pg-wire proxy) so the
    // test controls the exact transaction id and can prove the map entry is
    // actually removed on rollback — a wire-protocol test can't distinguish
    // "ROLLBACK ran" from "the connection merely closed", since both produce
    // the same "row not visible" outcome.
    let txn_id = backend.begin_transaction().await.expect("begin");
    backend
        .rollback_transaction(&txn_id)
        .await
        .expect("rollback");

    let reused = backend
        .execute_statement("SELECT 1", vec![], Some(&txn_id))
        .await;
    assert!(
        reused.is_err(),
        "transaction id must be rejected after rollback removed it from the map"
    );
}
