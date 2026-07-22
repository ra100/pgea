//! Pg wire server.
//!
//! One pg client connection maps to one [`Connection`] which implements both
//! [`StartupHandler`] (target + profile resolution from the StartupMessage)
//! and [`SimpleQueryHandler`] (query routing). Sharing one struct between the
//! two handlers means the RDS client built during startup can be picked up
//! by query execution without an external session map.
//!
//! The proxy is **loopback-only**. The `password` field of the pg connection
//! string is repurposed as an AWS profile-name override; we do not validate
//! it. On a non-loopback bind any user could supply someone else's profile
//! name and ride their credentials, so the listener address is constrained
//! at config validation time.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use futures::{stream, Sink, SinkExt};
use pgwire::api::auth::{self, DefaultServerParameterProvider, StartupHandler};
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::{
    ClientInfo, ClientPortalStore, PgWireConnectionState, PgWireServerHandlers, Type,
    METADATA_DATABASE,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use pgwire::types::format::FormatOptions;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, warn};

use crate::config::{Config, Target};
use crate::intercept::{self, Action};
use crate::rds::client::AwsRdsClient;
use crate::rds::txn::{TxnState, TxnStatus};
use crate::rds::{ExecuteOutput, Field, RdsClient, RdsClientPool, RdsError};
use crate::rewriter;
use crate::types::{encode_bytea_hex, oid_for_type_name};
use aws_sdk_rdsdata::types::{Field as AwsField, SqlParameter};

/// One pg connection's session state. Shared (as `Arc<Connection>`) between
/// the [`StartupHandler`], [`SimpleQueryHandler`], and [`ExtendedQueryHandler`]
/// impls of this type.
pub struct Connection {
    config: Arc<Config>,
    /// Shared across every `Connection` accepted by this listener (see
    /// `accept_loop`) so a second pg connection to the same target+profile
    /// reuses the first one's resolved AWS credentials + SDK client instead
    /// of re-running credential resolution from scratch. `None` when
    /// `test_client` is set, since that short-circuits `build_rds_client`
    /// before the pool is ever touched.
    pool: Option<Arc<RdsClientPool>>,
    /// Set by `StartupHandler::on_startup` once the target and profile have
    /// been resolved and the AWS SDK client built.
    rds: Mutex<Option<Arc<dyn RdsClient>>>,
    txn: Mutex<TxnState>,
    /// Test seam: when set, `build_rds_client` returns this client instead of
    /// constructing an `AwsRdsClient`. Lets us drive the StartupHandler in
    /// unit tests without invoking the AWS credential chain.
    test_client: Mutex<Option<Arc<dyn RdsClient>>>,
    /// Per-portal cache of pre-executed results. The Extended Query
    /// `Describe(Portal)` step demands a `RowDescription` before `Execute`,
    /// so we eagerly run the statement during Describe, cache the output,
    /// and serve `Execute` from the cache. Keyed by portal name.
    portal_cache: Mutex<HashMap<String, ExecuteOutput>>,
}

impl Connection {
    pub fn new(config: Arc<Config>, pool: Arc<RdsClientPool>) -> Self {
        Self {
            config,
            pool: Some(pool),
            rds: Mutex::new(None),
            txn: Mutex::new(TxnState::default()),
            test_client: Mutex::new(None),
            portal_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Construct a [`Connection`] with a pre-installed RDS client. Used by
    /// integration tests so the StartupHandler skips real AWS credential
    /// resolution and reuses the supplied client for every per-connection
    /// session. No pool is allocated: `build_rds_client` returns
    /// `test_client` before it would ever consult `pool`.
    pub fn with_test_client(config: Arc<Config>, test_client: Arc<dyn RdsClient>) -> Self {
        Self {
            config,
            pool: None,
            rds: Mutex::new(None),
            txn: Mutex::new(TxnState::default()),
            test_client: Mutex::new(Some(test_client)),
            portal_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Return a pooled [`AwsRdsClient`] for the given target and profile
    /// (building + caching it on first use), or the injected test client
    /// when one was supplied.
    async fn build_rds_client(&self, target: &Target, profile: Option<&str>) -> Arc<dyn RdsClient> {
        if let Some(c) = self.test_client.lock().await.clone() {
            return c;
        }
        self.pool
            .as_ref()
            .expect("pool is always set when test_client is unset")
            .get_or_build(target, profile, || async {
                let mut loader = aws_config::defaults(BehaviorVersion::latest())
                    .region(aws_config::Region::new(target.region.clone()));
                if let Some(p) = profile {
                    loader = loader.profile_name(p);
                }
                let sdk_config = loader.load().await;
                let client = aws_sdk_rdsdata::Client::new(&sdk_config);
                Arc::new(AwsRdsClient::new(
                    client,
                    target.cluster_arn.clone(),
                    target.secret_arn.clone(),
                    target.database.clone(),
                )) as Arc<dyn RdsClient>
            })
            .await
    }
}

/// Bundle of per-connection handlers passed to `process_socket`.
pub struct ProxyHandlers {
    connection: Arc<Connection>,
}

impl PgWireServerHandlers for ProxyHandlers {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.connection.clone()
    }
    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.connection.clone()
    }
    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.connection.clone()
    }
}

// ===== Startup =====

#[async_trait]
impl StartupHandler for Connection {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                auth::protocol_negotiation(client, startup).await?;
                auth::save_startup_parameters_to_metadata(client, startup);

                // Validate the target now so we fail fast before prompting for
                // a password. The target is also re-resolved on the password
                // message because metadata is the source of truth there.
                let db = client.metadata().get(METADATA_DATABASE).cloned();
                let db_str = db.as_deref().unwrap_or("");
                if self.config.target(db_str).is_none() {
                    return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                        "FATAL".into(),
                        "3D000".into(),
                        format!("database {db_str:?} not configured as a target in pgea"),
                    ))));
                }

                client.set_state(PgWireConnectionState::AuthenticationInProgress);
                client
                    .send(PgWireBackendMessage::Authentication(
                        Authentication::CleartextPassword,
                    ))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(pwd) => {
                let pwd = pwd.into_password()?;
                let password_text = pwd.password;

                let db = client
                    .metadata()
                    .get(METADATA_DATABASE)
                    .cloned()
                    .unwrap_or_default();
                let target = self.config.target(&db).cloned().ok_or_else(|| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "FATAL".into(),
                        "3D000".into(),
                        format!("database {db:?} not configured as a target in pgea"),
                    )))
                })?;

                let profile_override = if password_text.is_empty() {
                    None
                } else {
                    Some(password_text.as_str())
                };
                let resolved_profile = self.config.resolve_profile(&target, profile_override);

                info!(
                    user = client.metadata().get("user").map(|s| s.as_str()).unwrap_or(""),
                    database = %db,
                    profile = resolved_profile.as_deref().unwrap_or("<default chain>"),
                    password_supplied = !password_text.is_empty(),
                    "resolving target and profile",
                );

                let rds = self
                    .build_rds_client(&target, resolved_profile.as_deref())
                    .await;
                *self.rds.lock().await = Some(rds);

                let provider = DefaultServerParameterProvider::default();
                auth::finish_authentication(client, &provider).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

// ===== Simple Query =====

#[async_trait]
impl SimpleQueryHandler for Connection {
    async fn do_query<C>(&self, _client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(vec![Response::EmptyQuery]);
        }

        let rds = match self.rds.lock().await.clone() {
            Some(c) => c,
            None => {
                return Ok(vec![error_response(
                    "ERROR",
                    "08P01",
                    "internal error: no RDS client bound to this connection".to_string(),
                )]);
            }
        };

        match intercept::classify(trimmed) {
            Action::Reject(op) => Ok(vec![error_response(
                "ERROR",
                "0A000",
                format!("{op} not supported by RDS Data API proxy"),
            )]),

            Action::Begin => match self.txn.lock().await.begin(&rds).await {
                Ok(_) => Ok(vec![Response::TransactionStart(Tag::new("BEGIN"))]),
                Err(e) => Ok(vec![error_response("ERROR", "25001", e.to_string())]),
            },

            Action::Commit => match self.txn.lock().await.commit(&rds).await {
                Ok(_) => Ok(vec![Response::TransactionEnd(Tag::new("COMMIT"))]),
                Err(e) => Ok(vec![error_response("ERROR", "25P01", e.to_string())]),
            },

            Action::Rollback => match self.txn.lock().await.rollback(&rds).await {
                Ok(_) => Ok(vec![Response::TransactionEnd(Tag::new("ROLLBACK"))]),
                Err(e) => Ok(vec![error_response("ERROR", "25P01", e.to_string())]),
            },

            Action::Execute => self.execute_simple(&rds, trimmed).await,
        }
    }
}

impl Connection {
    async fn execute_simple(
        &self,
        rds: &Arc<dyn RdsClient>,
        sql: &str,
    ) -> PgWireResult<Vec<Response>> {
        match self.run_sql(rds, sql, vec![]).await {
            Ok(resp) => Ok(vec![resp]),
            Err(resp) => Ok(vec![resp]),
        }
    }

    /// Run an arbitrary SQL string against the RDS Data API, threading the
    /// per-connection transaction state. Returns either a successful pg
    /// `Response` or an error `Response` already wrapped — the caller decides
    /// whether to put it into a `Vec` (Simple Query) or return it directly
    /// (Extended Query).
    async fn run_sql(
        &self,
        rds: &Arc<dyn RdsClient>,
        sql: &str,
        parameters: Vec<SqlParameter>,
    ) -> Result<Response, Response> {
        // Apply catalog rewrites for queries known to trip
        // RDS Data API's UnsupportedResultException (CHAR / TIME / INTERVAL
        // columns in catalog reads). Falls back to the input on no match.
        let rewritten;
        let sql = match crate::catalog::maybe_rewrite(sql) {
            Some(r) => {
                tracing::debug!(original = sql, rewritten = %r, "catalog rewrite applied");
                rewritten = r;
                rewritten.as_str()
            }
            None => sql,
        };
        tracing::debug!(sql, params = parameters.len(), "ExecuteStatement");
        let txn_id = {
            let s = self.txn.lock().await;
            if s.status() == TxnStatus::Failed {
                return Err(error_response(
                    "ERROR",
                    "25P02",
                    "current transaction is aborted, commands ignored until end of transaction block".to_string(),
                ));
            }
            s.transaction_id().map(str::to_owned)
        };

        match crate::rds::execute_paginated(rds.as_ref(), sql, parameters, txn_id.as_deref()).await
        {
            Ok(out) => Ok(response_from_output(sql, out)),
            Err(e) => {
                let msg = rds_error_msg(&e);
                tracing::warn!(sql, %msg, "ExecuteStatement failed");
                // Aurora's Data API refuses to return values of certain
                // PG types (CHAR/bpchar, TIME, INTERVAL, etc) with
                // UnsupportedResultException. GUI clients fail on connect
                // because their catalog probes hit these types. Surface a
                // pg ErrorResponse that does NOT abort the transaction so
                // the client can keep going with the next probe.
                if is_unsupported_type_error(&msg) {
                    return Err(error_response("ERROR", "0A000", msg));
                }
                self.txn.lock().await.mark_failed();
                Err(error_response("ERROR", "42000", msg))
            }
        }
    }
}

fn is_unsupported_type_error(msg: &str) -> bool {
    msg.contains("UnsupportedResultException")
}

/// Decode a pg binary-format bind parameter for a curated set of scalar
/// types to the Data API `Field` variant matching its actual pg type.
/// Returns `None` if we don't know how to decode the type (caller falls
/// back).
///
/// Data API validates parameter types strictly server-side: sending an
/// `INT4` column's value as `Field::StringValue("42")` fails with
/// `column "id" is of type integer but expression is of type text`, it does
/// *not* implicitly cast the way a raw SQL text literal would. So numeric
/// and boolean types must use their native `Field` variant
/// (`LongValue`/`DoubleValue`/`BooleanValue`), not a stringified fallback.
fn decode_binary_scalar(ty: &Type, bytes: &[u8]) -> Option<AwsField> {
    match *ty {
        Type::BOOL if bytes.len() == 1 => Some(AwsField::BooleanValue(bytes[0] != 0)),
        Type::INT2 if bytes.len() == 2 => {
            Some(AwsField::LongValue(
                i16::from_be_bytes([bytes[0], bytes[1]]) as i64,
            ))
        }
        Type::INT4 if bytes.len() == 4 => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(AwsField::LongValue(i32::from_be_bytes(arr) as i64))
        }
        Type::OID if bytes.len() == 4 => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            // OID has no dedicated Field variant; Data API accepts an OID
            // column's value as a stringified integer (unlike INT4/INT8,
            // which reject StringValue -- OID's implicit text->oid cast is
            // permitted server-side).
            Some(AwsField::StringValue(u32::from_be_bytes(arr).to_string()))
        }
        Type::INT8 if bytes.len() == 8 => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            Some(AwsField::LongValue(i64::from_be_bytes(arr)))
        }
        Type::FLOAT4 if bytes.len() == 4 => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(AwsField::DoubleValue(f32::from_be_bytes(arr) as f64))
        }
        Type::FLOAT8 if bytes.len() == 8 => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            Some(AwsField::DoubleValue(f64::from_be_bytes(arr)))
        }
        Type::BYTEA => Some(AwsField::BlobValue(aws_sdk_rdsdata::primitives::Blob::new(
            bytes.to_vec(),
        ))),
        // Text-like types: pg binary format is identical to text bytes.
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            std::str::from_utf8(bytes)
                .ok()
                .map(|s| AwsField::StringValue(s.to_owned()))
        }
        _ => None,
    }
}

/// Map a text-format bind parameter to a Data API `Field`, using the
/// declared pg type the same way `decode_binary_scalar` does for binary
/// format -- Data API rejects `StringValue` for numeric/boolean columns
/// regardless of which wire format the client used to send the value.
/// Falls back to `StringValue` when the type is unknown or the text fails
/// to parse as its declared type (lets the Data API surface its own error
/// rather than us guessing).
fn field_for_text_scalar(ty: Option<&Type>, text: &str) -> AwsField {
    match ty {
        Some(&Type::BOOL) => match text {
            "t" | "true" | "TRUE" | "1" => AwsField::BooleanValue(true),
            "f" | "false" | "FALSE" | "0" => AwsField::BooleanValue(false),
            _ => AwsField::StringValue(text.to_owned()),
        },
        Some(&Type::INT2) | Some(&Type::INT4) | Some(&Type::INT8) => text
            .parse::<i64>()
            .map(AwsField::LongValue)
            .unwrap_or_else(|_| AwsField::StringValue(text.to_owned())),
        Some(&Type::FLOAT4) | Some(&Type::FLOAT8) => text
            .parse::<f64>()
            .map(AwsField::DoubleValue)
            .unwrap_or_else(|_| AwsField::StringValue(text.to_owned())),
        _ => AwsField::StringValue(text.to_owned()),
    }
}

// ===== Extended Query =====
//
// JDBC's Extended Query flow is Parse -> Bind -> Describe(Portal) -> Execute.
// The client *requires* a real RowDescription from Describe(Portal); if it
// gets `NoData` and then DataRows from Execute it raises
// "Received resultset tuples, but no field structure for them".
//
// The Data API has no `prepare`/`describe`, so we cannot get a schema without
// running the statement. To stay compatible we eagerly execute the statement
// inside `do_describe_portal`, cache the result keyed by portal name, and
// have `do_query` return the cached rows for matching portals.

impl Connection {
    /// Convert a `Portal`'s pg-encoded params into Data API SqlParameters.
    ///
    /// Bind parameters arrive in either text (format=0) or binary (format=1).
    /// Drivers like the PG JDBC routinely send oid/int parameters as binary
    /// — for OID that's a 4-byte big-endian integer whose bytes can include
    /// 0x00. Blindly stuffing those bytes into `stringValue` produces invalid
    /// UTF-8 / NUL on the wire and Aurora returns
    /// `invalid byte sequence for encoding "UTF8": 0x00`. Decode binary
    /// scalars to their text representation per parameter type.
    fn sql_parameters_from_portal(portal: &Portal<String>) -> Vec<SqlParameter> {
        portal
            .parameters
            .iter()
            .enumerate()
            .map(|(idx, raw)| {
                let value = match raw {
                    None => AwsField::IsNull(true),
                    Some(bytes) => Self::decode_bind_param(idx, bytes.as_ref(), portal),
                };
                SqlParameter::builder()
                    .name(format!("p{}", idx + 1))
                    .value(value)
                    .build()
            })
            .collect()
    }

    /// Decode a single bind parameter to a Data API `Field`. Honors the
    /// per-parameter format code (text vs binary). Both paths need the
    /// declared pg type to pick a `Field` variant Data API's strict
    /// server-side type checking will accept for that column (see
    /// `decode_binary_scalar` and `field_for_text_scalar`).
    fn decode_bind_param(idx: usize, bytes: &[u8], portal: &Portal<String>) -> AwsField {
        let pg_type = portal
            .statement
            .parameter_types
            .get(idx)
            .and_then(|t| t.as_ref());
        let is_binary = portal.parameter_format.is_binary(idx);

        if !is_binary {
            return match std::str::from_utf8(bytes) {
                Ok(s) => field_for_text_scalar(pg_type, s),
                Err(_) => {
                    AwsField::BlobValue(aws_sdk_rdsdata::primitives::Blob::new(bytes.to_vec()))
                }
            };
        }

        if let Some(decoded) = pg_type.and_then(|t| decode_binary_scalar(t, bytes)) {
            return decoded;
        }

        // Unknown/unsupported binary type. Fall back: try UTF-8, else blob.
        match std::str::from_utf8(bytes) {
            Ok(s) if !s.contains('\0') => AwsField::StringValue(s.to_owned()),
            _ => AwsField::BlobValue(aws_sdk_rdsdata::primitives::Blob::new(bytes.to_vec())),
        }
    }

    /// Execute the portal's statement against the Data API and stash the
    /// result in `portal_cache`. No-op for txn verbs and intercepted
    /// statements: those don't need a result-row cache. Errors are stored as
    /// `Err` in the cache so `do_query` can replay them.
    async fn ensure_portal_executed(&self, portal: &Portal<String>) -> PortalState {
        let raw_sql = portal.statement.statement.as_str();
        if raw_sql.trim().is_empty() {
            return PortalState::Empty;
        }

        let sql = rewriter::rewrite(raw_sql).sql;

        match intercept::classify(&sql) {
            Action::Reject(op) => PortalState::Reject(op),
            Action::Begin => PortalState::Begin,
            Action::Commit => PortalState::Commit,
            Action::Rollback => PortalState::Rollback,
            Action::Execute => {
                if self.portal_cache.lock().await.contains_key(&portal.name) {
                    return PortalState::Executed;
                }

                let rds = match self.rds.lock().await.clone() {
                    Some(c) => c,
                    None => {
                        return PortalState::Error(
                            "internal error: no RDS client bound to this connection".to_string(),
                        )
                    }
                };

                let params = Self::sql_parameters_from_portal(portal);
                let txn_id = {
                    let s = self.txn.lock().await;
                    if s.status() == TxnStatus::Failed {
                        return PortalState::Error(
                            "current transaction is aborted, commands ignored until end of transaction block".to_string(),
                        );
                    }
                    s.transaction_id().map(str::to_owned)
                };

                let sql_for_log = sql.clone();
                let sql_for_exec = match crate::catalog::maybe_rewrite(&sql) {
                    Some(r) => {
                        debug!(original = sql_for_log, rewritten = %r, "catalog rewrite applied");
                        r
                    }
                    None => sql.clone(),
                };
                debug!(sql = %sql_for_exec, params = params.len(), "ExecuteStatement (extended)");

                match crate::rds::execute_paginated(
                    rds.as_ref(),
                    &sql_for_exec,
                    params,
                    txn_id.as_deref(),
                )
                .await
                {
                    Ok(out) => {
                        self.portal_cache
                            .lock()
                            .await
                            .insert(portal.name.clone(), out);
                        PortalState::Executed
                    }
                    Err(e) => {
                        let msg = rds_error_msg(&e);
                        warn!(sql = %sql_for_exec, %msg, "ExecuteStatement failed");
                        if !is_unsupported_type_error(&msg) {
                            self.txn.lock().await.mark_failed();
                        }
                        PortalState::Error(msg)
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum PortalState {
    Empty,
    Executed,
    Reject(&'static str),
    Begin,
    Commit,
    Rollback,
    Error(String),
}

#[async_trait]
impl ExtendedQueryHandler for Connection {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        Arc::new(NoopQueryParser)
    }

    async fn do_query<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let state = self.ensure_portal_executed(portal).await;
        let sql = rewriter::rewrite(portal.statement.statement.as_str()).sql;

        match state {
            PortalState::Empty => Ok(Response::EmptyQuery),
            PortalState::Reject(op) => Ok(error_response(
                "ERROR",
                "0A000",
                format!("{op} not supported by RDS Data API proxy"),
            )),
            PortalState::Begin => {
                let rds = match self.rds.lock().await.clone() {
                    Some(c) => c,
                    None => {
                        return Ok(error_response(
                            "ERROR",
                            "08P01",
                            "internal error: no RDS client bound to this connection".to_string(),
                        ))
                    }
                };
                match self.txn.lock().await.begin(&rds).await {
                    Ok(_) => Ok(Response::TransactionStart(Tag::new("BEGIN"))),
                    Err(e) => Ok(error_response("ERROR", "25001", e.to_string())),
                }
            }
            PortalState::Commit => {
                let rds = match self.rds.lock().await.clone() {
                    Some(c) => c,
                    None => {
                        return Ok(error_response(
                            "ERROR",
                            "08P01",
                            "internal error: no RDS client bound to this connection".to_string(),
                        ))
                    }
                };
                match self.txn.lock().await.commit(&rds).await {
                    Ok(_) => Ok(Response::TransactionEnd(Tag::new("COMMIT"))),
                    Err(e) => Ok(error_response("ERROR", "25P01", e.to_string())),
                }
            }
            PortalState::Rollback => {
                let rds = match self.rds.lock().await.clone() {
                    Some(c) => c,
                    None => {
                        return Ok(error_response(
                            "ERROR",
                            "08P01",
                            "internal error: no RDS client bound to this connection".to_string(),
                        ))
                    }
                };
                match self.txn.lock().await.rollback(&rds).await {
                    Ok(_) => Ok(Response::TransactionEnd(Tag::new("ROLLBACK"))),
                    Err(e) => Ok(error_response("ERROR", "25P01", e.to_string())),
                }
            }
            PortalState::Error(msg) => Ok(error_response("ERROR", "42000", msg)),
            PortalState::Executed => {
                let out = self
                    .portal_cache
                    .lock()
                    .await
                    .remove(&portal.name)
                    .unwrap_or_default();
                Ok(response_from_output(&sql, out))
            }
        }
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        statement: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        // No prepare-time introspection of result columns on the Data API,
        // so the row-description list stays empty here — the real schema is
        // delivered by Describe(Portal). But the parameter type list must
        // be non-empty when the SQL contains placeholders: tokio-postgres
        // checks `Bind` parameter count against this `ParameterDescription`
        // and aborts with `Parameters(N, 0)` on mismatch. JDBC tolerates
        // the empty form; tokio-postgres does not.
        //
        // If the client's Parse declared parameter types, echo them back.
        // Otherwise count `$N` placeholders in the original SQL via the
        // rewriter and report each as `Type::UNKNOWN` — Postgres' standard
        // way to ask the server to pick a binding-time type, which is the
        // closest we can do without a live db to do real inference.
        let param_types: Vec<Type> = if statement.parameter_types.is_empty() {
            let rewritten = rewriter::rewrite(statement.statement.as_str());
            vec![Type::UNKNOWN; rewritten.params.len()]
        } else {
            statement
                .parameter_types
                .iter()
                .map(|t| t.clone().unwrap_or(Type::UNKNOWN))
                .collect()
        };
        Ok(DescribeStatementResponse::new(param_types, vec![]))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let state = self.ensure_portal_executed(portal).await;
        let cache = self.portal_cache.lock().await;
        let columns = match (&state, cache.get(&portal.name)) {
            (PortalState::Executed, Some(out)) => out
                .columns
                .iter()
                .map(|c| {
                    let oid = oid_for_type_name(&c.type_name);
                    let pg_type = Type::from_oid(oid).unwrap_or(Type::TEXT);
                    FieldInfo::new(c.name.clone(), None, None, pg_type, FieldFormat::Text)
                })
                .collect(),
            _ => vec![],
        };
        Ok(DescribePortalResponse::new(columns))
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
    let verb = intercept::leading_verb(sql).unwrap_or("OK");

    // For row-returning verbs we MUST emit Response::Query — even when the
    // result set is empty — because JDBC clients expect a RowDescription on
    // any SELECT. Aurora occasionally omits columnMetadata when the planner
    // proves the query produces no rows (e.g. WHERE 1<>1); in that case we
    // synthesise a single-column placeholder schema so JDBC sees a valid
    // RowDescription with zero rows. DML/DDL stays on the Execution path
    // so psql shows e.g. `UPDATE 5`.
    let row_returning = matches!(
        verb,
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "EXPLAIN" | "SHOW"
    );

    if out.columns.is_empty() && !row_returning {
        let tag = match verb {
            "INSERT" => Tag::new("INSERT")
                .with_oid(0)
                .with_rows(out.rows_affected as usize),
            other => Tag::new(other).with_rows(out.rows_affected as usize),
        };
        return Response::Execution(tag);
    }

    if out.columns.is_empty() && row_returning {
        // Empty schema would be illegal in pg wire (RowDescription must
        // declare at least one field). Synthesise a placeholder column.
        let schema: Arc<Vec<FieldInfo>> = Arc::new(vec![FieldInfo::new(
            "?column?".to_string(),
            None,
            None,
            Type::TEXT,
            FieldFormat::Text,
        )]);
        let stream = stream::iter(std::iter::empty::<PgWireResult<_>>());
        let mut q = QueryResponse::new(schema, stream);
        q.set_command_tag(verb);
        return Response::Query(q);
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
            // Every value is a fully pre-formatted pg text literal (array
            // literals included -- see field_to_text), so it must be
            // encoded as plain TEXT regardless of the column's declared
            // OID. `encode_field` would use the schema's real OID
            // instead, and pgwire's array-aware `ToSqlText` assumes it's
            // being handed one unquoted *element*, not a finished `{...}`
            // literal -- it would wrap our already-built literal in an
            // extra layer of quotes because it contains `{`/`}`.
            enc.encode_field_with_type_and_format(
                &v,
                &Type::TEXT,
                FieldFormat::Text,
                &FormatOptions::default(),
            )?;
        }
        Ok(enc.take_row())
    });

    let stream = stream::iter(row_iter);
    let mut q = QueryResponse::new(schema, stream);
    q.set_command_tag(verb);
    Response::Query(q)
}

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

/// Run the accept loop. The proxy is loopback-only by policy; callers should
/// pass a `127.0.0.1` or `::1` listen address.
#[instrument(skip(config))]
pub async fn run(config: Arc<Config>) -> std::io::Result<()> {
    let listener = TcpListener::bind(&config.listen).await?;
    info!(addr = %config.listen, targets = config.targets.len(), "pgea listening");
    accept_loop(listener, config, None).await
}

/// Run the accept loop on a pre-bound `TcpListener`. Useful for integration
/// tests that need to bind an ephemeral port and inject a mock RDS client
/// without going through the AWS credential chain.
pub async fn run_with_listener(
    listener: TcpListener,
    config: Arc<Config>,
    test_client: Option<Arc<dyn RdsClient>>,
) -> std::io::Result<()> {
    accept_loop(listener, config, test_client).await
}

async fn accept_loop(
    listener: TcpListener,
    config: Arc<Config>,
    test_client: Option<Arc<dyn RdsClient>>,
) -> std::io::Result<()> {
    // One pool for the life of the listener, shared by every accepted
    // connection — this is what lets a second connection to the same
    // target+profile skip AWS credential resolution.
    let pool = Arc::new(RdsClientPool::default());

    loop {
        let (socket, peer) = listener.accept().await?;
        debug!(?peer, "accepted connection");

        let connection = Arc::new(match test_client.clone() {
            Some(c) => Connection::with_test_client(config.clone(), c),
            None => Connection::new(config.clone(), pool.clone()),
        });
        let handlers = ProxyHandlers { connection };

        tokio::spawn(async move {
            if let Err(e) = process_socket(socket, None, handlers).await {
                warn!(?peer, err = %e, "connection errored");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rds::client::mock::MockRdsClient;

    fn config_two_targets() -> Arc<Config> {
        Arc::new(
            Config::parse(
                r#"
listen = "127.0.0.1:5433"
default_profile = "fallback"

[targets.dev]
cluster_arn = "arn:aws:rds:eu-west-1:123456789012:cluster:dev-c"
secret_arn  = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:dev-s"
database    = "analytics"
region      = "eu-west-1"
profile     = "dev-profile"

[targets.prod]
cluster_arn = "arn:aws:rds:eu-west-1:123456789012:cluster:prod-c"
secret_arn  = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:prod-s"
database    = "analytics"
region      = "eu-west-1"
"#,
            )
            .unwrap(),
        )
    }

    #[test]
    fn unknown_dbname_is_rejected_at_lookup_time() {
        let cfg = config_two_targets();
        assert!(cfg.target("does-not-exist").is_none());
        assert!(cfg.target("dev").is_some());
    }

    #[test]
    fn profile_resolution_order_matches_design() {
        let cfg = config_two_targets();
        let dev = cfg.target("dev").unwrap();
        let prod = cfg.target("prod").unwrap();

        // 1. password override wins
        assert_eq!(
            cfg.resolve_profile(dev, Some("override")).as_deref(),
            Some("override"),
        );

        // 2. empty password falls through to target.profile
        assert_eq!(
            cfg.resolve_profile(dev, None).as_deref(),
            Some("dev-profile"),
        );

        // 3. no target.profile → default_profile
        assert_eq!(cfg.resolve_profile(prod, None).as_deref(), Some("fallback"),);

        // 4. an empty override is treated as "use default" (string filter
        //    trims false-truthy empty strings inside resolve_profile).
        assert_eq!(
            cfg.resolve_profile(dev, Some("")).as_deref(),
            Some("dev-profile"),
        );
    }

    #[tokio::test]
    async fn build_rds_client_returns_test_client_when_injected() {
        let cfg = config_two_targets();
        let conn = Connection::new(cfg.clone(), Arc::new(RdsClientPool::default()));

        let mock: Arc<dyn RdsClient> = Arc::new(MockRdsClient::default());
        *conn.test_client.lock().await = Some(mock.clone());

        let target = cfg.target("dev").unwrap().clone();
        let returned = conn.build_rds_client(&target, Some("ignored")).await;

        // The same Arc instance should be handed back unchanged.
        assert!(Arc::ptr_eq(&returned, &mock));
    }

    #[tokio::test]
    async fn build_rds_client_reuses_pool_when_no_test_client_is_set() {
        let cfg = config_two_targets();
        let pool = Arc::new(RdsClientPool::default());
        let conn = Connection::new(cfg.clone(), pool);

        let target = cfg.target("dev").unwrap().clone();
        let first = conn.build_rds_client(&target, Some("dev-profile")).await;
        let second = conn.build_rds_client(&target, Some("dev-profile")).await;
        assert!(
            Arc::ptr_eq(&first, &second),
            "same target+profile must reuse the pooled client"
        );

        let third = conn.build_rds_client(&target, Some("other-profile")).await;
        assert!(
            !Arc::ptr_eq(&first, &third),
            "distinct profile must build a distinct client"
        );
    }

    // Regression coverage for a bug only a real Data API run surfaced: numeric
    // and boolean bind params were always wrapped in `Field::StringValue`,
    // which Data API's strict server-side type checking rejects for columns
    // whose pg type isn't itself text-like (e.g. "column is of type integer
    // but expression is of type text"). Both the binary-format decode path
    // (`decode_binary_scalar`) and the text-format path
    // (`field_for_text_scalar`) must produce the native `Field` variant.

    #[test]
    fn decode_binary_scalar_uses_native_field_variants() {
        assert_eq!(
            decode_binary_scalar(&Type::INT4, &42_i32.to_be_bytes()),
            Some(AwsField::LongValue(42))
        );
        assert_eq!(
            decode_binary_scalar(&Type::INT8, &42_i64.to_be_bytes()),
            Some(AwsField::LongValue(42))
        );
        assert_eq!(
            decode_binary_scalar(&Type::INT2, &42_i16.to_be_bytes()),
            Some(AwsField::LongValue(42))
        );
        assert_eq!(
            decode_binary_scalar(&Type::BOOL, &[1]),
            Some(AwsField::BooleanValue(true))
        );
        assert_eq!(
            decode_binary_scalar(&Type::FLOAT8, &1.5_f64.to_be_bytes()),
            Some(AwsField::DoubleValue(1.5))
        );
        assert_eq!(
            decode_binary_scalar(&Type::TEXT, b"hello"),
            Some(AwsField::StringValue("hello".to_owned()))
        );
        // OID keeps the stringified form -- Data API permits OID's
        // text->oid implicit cast even though it rejects it for INT4/INT8.
        assert_eq!(
            decode_binary_scalar(&Type::OID, &42_u32.to_be_bytes()),
            Some(AwsField::StringValue("42".to_owned()))
        );
    }

    #[test]
    fn field_for_text_scalar_uses_native_field_variants() {
        assert_eq!(
            field_for_text_scalar(Some(&Type::INT4), "42"),
            AwsField::LongValue(42)
        );
        assert_eq!(
            field_for_text_scalar(Some(&Type::FLOAT8), "1.5"),
            AwsField::DoubleValue(1.5)
        );
        assert_eq!(
            field_for_text_scalar(Some(&Type::BOOL), "t"),
            AwsField::BooleanValue(true)
        );
        assert_eq!(
            field_for_text_scalar(Some(&Type::TEXT), "hello"),
            AwsField::StringValue("hello".to_owned())
        );
        // Unparseable text for a declared numeric type falls back to
        // StringValue rather than panicking -- Data API will report its own
        // clear error instead of us guessing.
        assert_eq!(
            field_for_text_scalar(Some(&Type::INT4), "not-a-number"),
            AwsField::StringValue("not-a-number".to_owned())
        );
        // Unknown/no declared type: text stays a StringValue, unchanged
        // behavior from before this fix.
        assert_eq!(
            field_for_text_scalar(None, "42"),
            AwsField::StringValue("42".to_owned())
        );
    }
}
