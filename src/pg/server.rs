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

use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use futures::{stream, Sink, SinkExt};
use pgwire::api::auth::{
    self, DefaultServerParameterProvider, StartupHandler,
};
use pgwire::api::query::SimpleQueryHandler;
use pgwire::api::results::{
    DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag,
};
use pgwire::api::{
    ClientInfo, ClientPortalStore, PgWireConnectionState, PgWireServerHandlers, Type,
    METADATA_DATABASE,
};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::startup::Authentication;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, warn};

use crate::config::{Config, Target};
use crate::intercept::{self, Action};
use crate::rds::client::AwsRdsClient;
use crate::rds::txn::{TxnState, TxnStatus};
use crate::rds::{ExecuteOutput, Field, RdsClient, RdsError};
use crate::types::{encode_bytea_hex, oid_for_type_name};

/// One pg connection's session state. Shared (as `Arc<Connection>`) between
/// the [`StartupHandler`] and [`SimpleQueryHandler`] impls of this type.
pub struct Connection {
    config: Arc<Config>,
    /// Set by `StartupHandler::on_startup` once the target and profile have
    /// been resolved and the AWS SDK client built.
    rds: Mutex<Option<Arc<dyn RdsClient>>>,
    txn: Mutex<TxnState>,
    /// Test seam: when set, `build_rds_client` returns this client instead of
    /// constructing an `AwsRdsClient`. Lets us drive the StartupHandler in
    /// unit tests without invoking the AWS credential chain.
    test_client: Mutex<Option<Arc<dyn RdsClient>>>,
}

impl Connection {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            rds: Mutex::new(None),
            txn: Mutex::new(TxnState::default()),
            test_client: Mutex::new(None),
        }
    }

    /// Construct an [`AwsRdsClient`] for the given target and profile, or
    /// return an injected test client when one was supplied.
    async fn build_rds_client(
        &self,
        target: &Target,
        profile: Option<&str>,
    ) -> Arc<dyn RdsClient> {
        if let Some(c) = self.test_client.lock().await.clone() {
            return c;
        }
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
        ))
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
                        format!(
                            "database {db_str:?} not configured as a target in pg-rds-connector"
                        ),
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
                let target = self
                    .config
                    .target(&db)
                    .cloned()
                    .ok_or_else(|| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "FATAL".into(),
                            "3D000".into(),
                            format!(
                                "database {db:?} not configured as a target in pg-rds-connector"
                            ),
                        )))
                    })?;

                let profile_override = if password_text.is_empty() {
                    None
                } else {
                    Some(password_text.as_str())
                };
                let resolved_profile =
                    self.config.resolve_profile(&target, profile_override);

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
        C: ClientInfo
            + ClientPortalStore
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

        match rds.execute_statement(sql, vec![], txn_id.as_deref()).await {
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
    info!(addr = %config.listen, targets = config.targets.len(), "pg-rds-connector listening");

    loop {
        let (socket, peer) = listener.accept().await?;
        debug!(?peer, "accepted connection");

        let connection = Arc::new(Connection::new(config.clone()));
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
        assert_eq!(
            cfg.resolve_profile(prod, None).as_deref(),
            Some("fallback"),
        );

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
        let conn = Connection::new(cfg.clone());

        let mock: Arc<dyn RdsClient> = Arc::new(MockRdsClient::default());
        *conn.test_client.lock().await = Some(mock.clone());

        let target = cfg.target("dev").unwrap().clone();
        let returned = conn.build_rds_client(&target, Some("ignored")).await;

        // The same Arc instance should be handed back unchanged.
        assert!(Arc::ptr_eq(&returned, &mock));
    }
}

