//! Pg wire server scaffold.
//!
//! Wires together a `pgwire::tokio::process_socket` accept loop with our own
//! `SimpleQueryHandler` that routes statements through the intercept layer
//! and the RDS client. Extended Query is intentionally absent in this slice;
//! it is the next milestone (M6) and requires the full `Parse`/`Bind` plumbing.
//!
//! Authentication: pgwire's `NoopHandler` accepts the StartupMessage without
//! a password challenge. The proxy is loopback-only; repurposing the password
//! field as an AWS profile name override is the M3 follow-up.

use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Sink};
use pgwire::api::query::SimpleQueryHandler;
use pgwire::api::results::{
    DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag,
};
use pgwire::api::{ClientInfo, NoopHandler, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, instrument, warn};

use crate::config::{Config, Target};
use crate::intercept::{self, Action};
use crate::rds::txn::{TxnState, TxnStatus};
use crate::rds::{ExecuteOutput, Field, RdsClient, RdsError};
use crate::types::{encode_bytea_hex, oid_for_type_name};

/// One pg connection's session state. Held inside an `Arc` so the pgwire
/// trait machinery (which sees `&self` only) can call into it from multiple
/// places; mutable state is guarded by `Mutex`.
pub struct Session {
    rds: Arc<dyn RdsClient>,
    txn: Mutex<TxnState>,
}

impl Session {
    pub fn new(rds: Arc<dyn RdsClient>) -> Self {
        Self {
            rds,
            txn: Mutex::new(TxnState::default()),
        }
    }
}

/// Bundle of per-connection handlers passed to `process_socket`. We rely on
/// `pgwire`'s `NoopHandler` defaults for startup, extended-query, copy,
/// error, and cancel.
pub struct ProxyHandlers {
    session: Arc<Session>,
}

impl PgWireServerHandlers for ProxyHandlers {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.session.clone()
    }
}

#[async_trait]
impl SimpleQueryHandler for Session {
    async fn do_query<C>(&self, _client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo
            + pgwire::api::ClientPortalStore
            + Sink<PgWireBackendMessage>
            + Unpin
            + Send
            + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(vec![Response::EmptyQuery]);
        }

        match intercept::classify(trimmed) {
            Action::Reject(op) => Ok(vec![error_response(
                "ERROR",
                "0A000",
                format!("{op} not supported by RDS Data API proxy"),
            )]),

            Action::Begin => match self.txn.lock().await.begin(&self.rds).await {
                Ok(_) => Ok(vec![Response::TransactionStart(Tag::new("BEGIN"))]),
                Err(e) => Ok(vec![error_response("ERROR", "25001", e.to_string())]),
            },

            Action::Commit => match self.txn.lock().await.commit(&self.rds).await {
                Ok(_) => Ok(vec![Response::TransactionEnd(Tag::new("COMMIT"))]),
                Err(e) => Ok(vec![error_response("ERROR", "25P01", e.to_string())]),
            },

            Action::Rollback => match self.txn.lock().await.rollback(&self.rds).await {
                Ok(_) => Ok(vec![Response::TransactionEnd(Tag::new("ROLLBACK"))]),
                Err(e) => Ok(vec![error_response("ERROR", "25P01", e.to_string())]),
            },

            Action::Execute => self.execute_simple(trimmed).await,
        }
    }
}

impl Session {
    async fn execute_simple(&self, sql: &str) -> PgWireResult<Vec<Response>> {
        let txn_id = {
            let s = self.txn.lock().await;
            if s.status() == TxnStatus::Failed {
                return Ok(vec![error_response(
                    "ERROR",
                    "25P02",
                    "current transaction is aborted, commands ignored until end of transaction block".to_string(),
                )]);
            }
            s.transaction_id().map(str::to_owned)
        };

        match self.rds.execute_statement(sql, vec![], txn_id.as_deref()).await {
            Ok(out) => Ok(vec![response_from_output(sql, out)]),
            Err(e) => {
                self.txn.lock().await.mark_failed();
                Ok(vec![error_response("ERROR", "42000", rds_error_msg(&e))])
            }
        }
    }
}

fn rds_error_msg(e: &RdsError) -> String {
    match e {
        RdsError::Service(s) | RdsError::Sdk(s) => s.clone(),
    }
}

fn error_response(severity: &str, code: &str, msg: String) -> Response {
    Response::Error(Box::new(ErrorInfo::new(
        severity.to_string(),
        code.to_string(),
        msg,
    )))
}

/// Build pg `Response::Query` from an `ExecuteOutput`. Always emits text format.
fn response_from_output(sql: &str, out: ExecuteOutput) -> Response {
    if out.columns.is_empty() {
        let verb = intercept::leading_verb(sql).unwrap_or("OK");
        let tag = match verb {
            "INSERT" => Tag::new("INSERT")
                .with_oid(0)
                .with_rows(out.rows_affected as usize),
            other => Tag::new(other).with_rows(out.rows_affected as usize),
        };
        return Response::Execution(tag);
    }

    let schema: Arc<Vec<FieldInfo>> = Arc::new(
        out.columns
            .iter()
            .map(|c| {
                let oid = oid_for_type_name(&c.type_name);
                let pg_type = Type::from_oid(oid).unwrap_or(Type::TEXT);
                FieldInfo::new(c.name.clone(), None, None, pg_type, FieldFormat::Text)
            })
            .collect(),
    );

    let schema_for_rows = schema.clone();
    let rows = out.rows;
    let row_iter = rows.into_iter().map(move |row| {
        let mut enc = DataRowEncoder::new(schema_for_rows.clone());
        for f in row {
            let v: Option<String> = match &f {
                Field::Null => None,
                _ => Some(field_to_text(&f)),
            };
            enc.encode_field(&v)?;
        }
        enc.finish()
    });

    let stream = stream::iter(row_iter);
    let mut q = QueryResponse::new(schema, stream);
    let verb = intercept::leading_verb(sql).unwrap_or("SELECT");
    q.set_command_tag(verb);
    Response::Query(q)
}

/// Render a `Field` as pg text format.
fn field_to_text(f: &Field) -> String {
    match f {
        Field::Null => String::new(),
        Field::Bool(true) => "t".into(),
        Field::Bool(false) => "f".into(),
        Field::Long(n) => n.to_string(),
        Field::Double(d) => d.to_string(),
        Field::String(s) => s.clone(),
        Field::Blob(b) => encode_bytea_hex(b),
        Field::ArrayLiteral(s) => s.clone(),
    }
}

/// Run the accept loop on `config.listen`, building a fresh session per
/// connection. `client_factory` is invoked once per accepted connection to
/// construct the `RdsClient`.
///
/// Target routing from the StartupMessage's `dbname` is the next iteration
/// (custom `StartupHandler`); for now we use the first configured target.
#[instrument(skip(config, client_factory))]
pub async fn run<F>(config: Config, client_factory: F) -> std::io::Result<()>
where
    F: Fn(&Target) -> Arc<dyn RdsClient> + Send + Sync + 'static,
{
    let listener = TcpListener::bind(&config.listen).await?;
    info!(addr = %config.listen, targets = config.targets.len(), "pg-rds-connector listening");

    let (default_target_name, default_target) = config
        .targets
        .iter()
        .next()
        .map(|(k, v)| (k.clone(), v.clone()))
        .ok_or_else(|| std::io::Error::other("no targets configured"))?;
    info!(default_target = %default_target_name, "using first configured target as default");

    let factory = Arc::new(client_factory);

    loop {
        let (socket, peer) = listener.accept().await?;
        let client = factory(&default_target);
        let session = Arc::new(Session::new(client));
        let handlers = ProxyHandlers { session };

        tokio::spawn(async move {
            if let Err(e) = process_socket(socket, None, handlers).await {
                warn!(?peer, err = %e, "connection errored");
            }
        });
    }
}

/// Suppress an unused-import lint for `NoopHandler` when default impls
/// are pulled in transitively.
#[allow(dead_code)]
fn _noop_marker(_: NoopHandler) {}
