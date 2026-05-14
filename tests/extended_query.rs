//! Integration tests: drive the proxy via `tokio-postgres` and assert the
//! mock `RdsClient` saw the expected SQL / parameters / transaction calls.
//!
//! Each test binds an ephemeral 127.0.0.1 port, spawns the proxy with an
//! injected mock client, connects via tokio-postgres, exercises one path
//! (Simple Query / Extended Query / transaction verbs), and checks both
//! the rows the client received and the calls the mock recorded.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_rdsdata::types::SqlParameter;
use pg_rds_connector::config::Config;
use pg_rds_connector::pg::server::run_with_listener;
use pg_rds_connector::rds::{ExecuteOutput, Field, RdsClient, RdsError, ResultColumn};
use tokio::net::TcpListener;

/// Test double that records every Data API call and returns canned responses
/// keyed by SQL substring. Substring matching keeps tests insensitive to the
/// proxy's catalog rewrites.
#[derive(Default)]
struct RecordingMock {
    state: Mutex<MockState>,
}

#[derive(Default)]
struct MockState {
    executes: Vec<(String, Vec<SqlParameter>, Option<String>)>,
    begin_calls: u32,
    commit_calls: Vec<String>,
    rollback_calls: Vec<String>,
    /// (sql_substring → output) — first match wins.
    canned: Vec<(String, ExecuteOutput)>,
    txn_id: String,
}

impl RecordingMock {
    fn new(txn_id: &str) -> Self {
        Self {
            state: Mutex::new(MockState {
                txn_id: txn_id.to_owned(),
                ..MockState::default()
            }),
        }
    }

    fn with_response(self, sql_substring: &str, output: ExecuteOutput) -> Self {
        self.state
            .lock()
            .unwrap()
            .canned
            .push((sql_substring.to_owned(), output));
        self
    }
}

#[async_trait]
impl RdsClient for RecordingMock {
    async fn execute_statement(
        &self,
        sql: &str,
        parameters: Vec<SqlParameter>,
        transaction_id: Option<&str>,
    ) -> Result<ExecuteOutput, RdsError> {
        let mut s = self.state.lock().unwrap();
        s.executes.push((
            sql.to_owned(),
            parameters,
            transaction_id.map(str::to_owned),
        ));
        for (needle, out) in &s.canned {
            if sql.contains(needle) {
                return Ok(out.clone());
            }
        }
        Ok(ExecuteOutput::default())
    }

    async fn begin_transaction(&self) -> Result<String, RdsError> {
        let mut s = self.state.lock().unwrap();
        s.begin_calls += 1;
        Ok(s.txn_id.clone())
    }

    async fn commit_transaction(&self, tx: &str) -> Result<(), RdsError> {
        self.state.lock().unwrap().commit_calls.push(tx.to_owned());
        Ok(())
    }

    async fn rollback_transaction(&self, tx: &str) -> Result<(), RdsError> {
        self.state
            .lock()
            .unwrap()
            .rollback_calls
            .push(tx.to_owned());
        Ok(())
    }
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

/// Bind 127.0.0.1:0, spawn the proxy, return the bound address. Server runs
/// for the lifetime of the test process; a new ephemeral port is bound per
/// test to avoid cross-test interference.
async fn spawn_proxy(target: &str, mock: Arc<dyn RdsClient>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let config = config_for_target(target, &addr.to_string());
    tokio::spawn(async move {
        let _ = run_with_listener(listener, config, Some(mock)).await;
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

#[tokio::test]
async fn simple_select_returns_rows() {
    let output = ExecuteOutput {
        columns: vec![
            ResultColumn {
                name: "id".into(),
                type_name: "int4".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                type_name: "text".into(),
                nullable: true,
            },
        ],
        rows: vec![
            vec![Field::Long(1), Field::String("alice".into())],
            vec![Field::Long(2), Field::String("bob".into())],
        ],
        rows_affected: 0,
    };
    let mock = Arc::new(RecordingMock::new("tx-simple").with_response("FROM users", output));
    let addr = spawn_proxy("dev", mock.clone()).await;
    let client = connect(&addr, "dev").await;

    let rows = client
        .simple_query("SELECT id, name FROM users")
        .await
        .expect("simple_query");
    let data_rows: Vec<_> = rows
        .into_iter()
        .filter_map(|r| match r {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(data_rows.len(), 2);
    assert_eq!(data_rows[0].get("id"), Some("1"));
    assert_eq!(data_rows[0].get("name"), Some("alice"));
    assert_eq!(data_rows[1].get("id"), Some("2"));
    assert_eq!(data_rows[1].get("name"), Some("bob"));

    let s = mock.state.lock().unwrap();
    assert_eq!(s.executes.len(), 1);
    assert!(s.executes[0].0.contains("FROM users"));
    assert!(s.executes[0].2.is_none(), "no transaction expected");
}

#[tokio::test]
async fn extended_query_rewrites_params() {
    // Use INSERT (no result columns) so we can exercise the Extended Query
    // path without needing the proxy to emit a result-row schema at Describe
    // time. Aurora's Data API has no prepare/describe phase, so the proxy
    // can only know column types after execution; that's fine for clients
    // that read schema from Describe(Portal), but tokio-postgres' typed
    // query flow expects schema from Describe(Statement). INSERT side-steps
    // both — we just want to assert the SQL parameter rewrite.
    let mock = Arc::new(RecordingMock::new("tx-ext").with_response(
        "INSERT INTO accounts",
        ExecuteOutput {
            rows_affected: 1,
            ..ExecuteOutput::default()
        },
    ));
    let addr = spawn_proxy("dev", mock.clone()).await;
    let client = connect(&addr, "dev").await;

    use tokio_postgres::types::Type as PgType;
    let stmt = client
        .prepare_typed("INSERT INTO accounts(id) VALUES ($1)", &[PgType::INT4])
        .await
        .expect("prepare_typed");
    let affected = client
        .execute(&stmt, &[&42_i32])
        .await
        .expect("extended insert");
    assert_eq!(affected, 1);

    let s = mock.state.lock().unwrap();
    assert_eq!(s.executes.len(), 1);
    let (sql, params, txn) = &s.executes[0];
    assert!(
        sql.contains(":p1"),
        "expected $1 to be rewritten to :p1, got: {sql}"
    );
    assert!(!sql.contains("$1"), "raw $1 must not reach the Data API");
    assert!(txn.is_none());
    assert_eq!(params.len(), 1);
    assert_eq!(params[0].name(), Some("p1"));
}

#[tokio::test]
async fn explicit_transaction_routes_to_begin_commit() {
    let mock = Arc::new(RecordingMock::new("tx-abcdef"));
    let addr = spawn_proxy("dev", mock.clone()).await;
    let client = connect(&addr, "dev").await;

    client.simple_query("BEGIN").await.expect("BEGIN ok");
    client
        .simple_query("INSERT INTO t VALUES (1)")
        .await
        .expect("INSERT ok");
    client.simple_query("COMMIT").await.expect("COMMIT ok");

    let s = mock.state.lock().unwrap();
    assert_eq!(s.begin_calls, 1);
    assert_eq!(s.commit_calls, vec!["tx-abcdef".to_owned()]);
    assert!(s.rollback_calls.is_empty());
    assert_eq!(
        s.executes.len(),
        1,
        "INSERT should be the only ExecuteStatement"
    );
    assert_eq!(s.executes[0].2.as_deref(), Some("tx-abcdef"));
}

#[tokio::test]
async fn unknown_database_rejected_with_3d000() {
    let mock = Arc::new(RecordingMock::new("tx-x"));
    let addr = spawn_proxy("dev", mock.clone()).await;
    let host = addr.split(':').next().unwrap().to_owned();
    let port: u16 = addr.split(':').nth(1).unwrap().parse().unwrap();
    let mut config = tokio_postgres::config::Config::new();
    config
        .host(&host)
        .port(port)
        .user("postgres")
        .password("")
        .dbname("nope");
    let err = match config.connect(tokio_postgres::NoTls).await {
        Ok(_) => panic!("connect should fail with 3D000 for unknown database"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        msg.contains("3D000") || msg.contains("not configured"),
        "expected 3D000 / not-configured error, got: {msg}"
    );
}
