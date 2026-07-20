//! `RdsClient` backed by a real local Postgres (via `tokio_postgres`) instead
//! of canned responses. Gives integration tests genuine SQL/transaction
//! semantics without touching AWS.
//!
//! Kept in `tests/` (not `src/`) because integration tests link the compiled
//! library normally and cannot see `#[cfg(test)]` items inside it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_rdsdata::types::{Field as AwsField, SqlParameter};
use pgea::rds::{ExecuteOutput, Field, RdsClient, RdsError, ResultColumn};
use tokio::sync::Mutex;
use tokio_postgres::types::{Format, IsNull, ToSql, Type};
use tokio_postgres::Client;

/// `RdsClient` impl that runs SQL against a real Postgres connection.
///
/// One autocommit `Client` handles calls with `transaction_id: None`. Each
/// `begin_transaction` opens a *separate* raw connection (a borrowed
/// `tokio_postgres::Transaction` can't be stored across the `&self` async
/// trait methods) and keys it by an opaque `tx-N` handle in `txns`.
///
/// Transactions are `Arc<Client>` rather than bare `Client` so a query can
/// clone the `Arc` and drop the map lock before awaiting — holding the mutex
/// across a query's `.await` would serialize every in-flight transaction
/// against every other one, and block `begin_transaction`/`commit_transaction`
/// for the duration of an unrelated query.
pub struct PgRdsClient {
    autocommit: Client,
    txns: Mutex<HashMap<String, Arc<Client>>>,
    next_txn: AtomicU64,
    // Kept so `begin_transaction` can open its own separate connection later.
    host: String,
    port: u16,
    user: String,
    dbname: String,
}

impl PgRdsClient {
    /// Connect the autocommit client. Every connection this type opens
    /// (this one and every future transaction connection) needs its
    /// background `Connection` future polled, or the driver deadlocks —
    /// `tokio::spawn` it and let the handle leak for the test process
    /// lifetime (ponytail: no pool needed for a short-lived test binary).
    pub async fn connect(host: &str, port: u16, user: &str, dbname: &str) -> Self {
        let autocommit = connect_raw(host, port, user, dbname).await;
        Self {
            autocommit,
            txns: Mutex::new(HashMap::new()),
            next_txn: AtomicU64::new(1),
            host: host.to_string(),
            port,
            user: user.to_string(),
            dbname: dbname.to_string(),
        }
    }
}

async fn connect_raw(host: &str, port: u16, user: &str, dbname: &str) -> Client {
    let mut config = tokio_postgres::Config::new();
    config.host(host).port(port).user(user).dbname(dbname);
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("connect to test postgres container");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[async_trait]
impl RdsClient for PgRdsClient {
    async fn execute_statement(
        &self,
        sql: &str,
        parameters: Vec<SqlParameter>,
        transaction_id: Option<&str>,
    ) -> Result<ExecuteOutput, RdsError> {
        let translated = translate_params(sql);
        let boxed_params: Vec<Box<dyn ToSql + Sync + Send>> =
            parameters.iter().map(param_to_sql).collect();
        let params: Vec<&(dyn ToSql + Sync)> = boxed_params
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect();

        match transaction_id {
            None => run_query(&self.autocommit, &translated, &params).await,
            Some(id) => {
                // Clone the Arc and drop the lock before the query awaits —
                // see the `txns` field doc for why holding the guard across
                // `.await` here would be a bug, not just a style nit.
                let client = {
                    let txns = self.txns.lock().await;
                    txns.get(id)
                        .cloned()
                        .ok_or_else(|| RdsError::Service(format!("unknown transaction id: {id}")))?
                };
                run_query(&client, &translated, &params).await
            }
        }
    }

    async fn begin_transaction(&self) -> Result<String, RdsError> {
        let client = connect_raw(&self.host, self.port, &self.user, &self.dbname).await;
        client
            .batch_execute("BEGIN")
            .await
            .map_err(|e| RdsError::Service(e.to_string()))?;
        let id = format!("tx-{}", self.next_txn.fetch_add(1, Ordering::SeqCst));
        self.txns.lock().await.insert(id.clone(), Arc::new(client));
        Ok(id)
    }

    async fn commit_transaction(&self, transaction_id: &str) -> Result<(), RdsError> {
        let client = self
            .txns
            .lock()
            .await
            .remove(transaction_id)
            .ok_or_else(|| {
                RdsError::Service(format!("unknown transaction id: {transaction_id}"))
            })?;
        client
            .batch_execute("COMMIT")
            .await
            .map_err(|e| RdsError::Service(e.to_string()))
    }

    async fn rollback_transaction(&self, transaction_id: &str) -> Result<(), RdsError> {
        let client = self
            .txns
            .lock()
            .await
            .remove(transaction_id)
            .ok_or_else(|| {
                RdsError::Service(format!("unknown transaction id: {transaction_id}"))
            })?;
        client
            .batch_execute("ROLLBACK")
            .await
            .map_err(|e| RdsError::Service(e.to_string()))
    }
}

async fn run_query(
    client: &Client,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<ExecuteOutput, RdsError> {
    use futures::TryStreamExt;
    use std::pin::pin;

    // `query_raw` (rather than `query`) is needed to get `rows_affected()`:
    // a plain INSERT/UPDATE/DELETE returns zero *rows* but still reports a
    // real affected-row count in `CommandComplete`, which `query()`'s `Vec<Row>`
    // return type has no way to surface.
    let stream = client
        .query_raw(sql, params.iter().map(|p| *p as &(dyn ToSql + Sync)))
        .await
        .map_err(|e| RdsError::Service(e.to_string()))?;
    let mut stream = pin!(stream);

    let mut columns: Vec<ResultColumn> = Vec::new();
    let mut out_rows: Vec<Vec<Field>> = Vec::new();
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| RdsError::Service(e.to_string()))?
    {
        if columns.is_empty() {
            columns = row
                .columns()
                .iter()
                .map(|c| ResultColumn {
                    name: c.name().to_string(),
                    type_name: c.type_().name().to_string(),
                    nullable: true, // tokio_postgres doesn't expose not-null info here.
                })
                .collect();
        }
        out_rows.push(row_to_fields(&row));
    }
    let rows_affected = stream.rows_affected().unwrap_or(0) as i64;

    Ok(ExecuteOutput {
        columns,
        rows: out_rows,
        rows_affected,
    })
}

/// Convert one `tokio_postgres::Row` into our flattened `Field` vector.
/// NUMERIC and anything else not explicitly handled falls back to `Field::Null`
/// with a warning rather than pulling in a decimal dependency — this is a
/// test-only double, narrower coverage than production `AwsRdsClient` is fine.
fn row_to_fields(row: &tokio_postgres::Row) -> Vec<Field> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, col)| match col.type_().name() {
            "bool" => row
                .get::<_, Option<bool>>(i)
                .map_or(Field::Null, Field::Bool),
            "int2" => row
                .get::<_, Option<i16>>(i)
                .map_or(Field::Null, |n| Field::Long(n as i64)),
            "int4" => row
                .get::<_, Option<i32>>(i)
                .map_or(Field::Null, |n| Field::Long(n as i64)),
            "int8" => row
                .get::<_, Option<i64>>(i)
                .map_or(Field::Null, Field::Long),
            "float4" => row
                .get::<_, Option<f32>>(i)
                .map_or(Field::Null, |f| Field::Double(f as f64)),
            "float8" => row
                .get::<_, Option<f64>>(i)
                .map_or(Field::Null, Field::Double),
            "text" | "varchar" | "bpchar" | "name" => row
                .get::<_, Option<String>>(i)
                .map_or(Field::Null, Field::String),
            "bytea" => row
                .get::<_, Option<Vec<u8>>>(i)
                .map_or(Field::Null, Field::Blob),
            other => {
                tracing::warn!(
                    column = col.name(),
                    type_name = other,
                    "PgRdsClient: no conversion for column type, returning NULL (known gap)"
                );
                Field::Null
            }
        })
        .collect()
}

/// Convert an AWS SDK `SqlParameter` into a boxed `ToSql`. pgea's bind
/// params only ever produce IsNull/BooleanValue/LongValue/DoubleValue/
/// StringValue/BlobValue — never ArrayValue — so that's all we handle.
///
/// In practice `src/pg/server.rs::decode_bind_param` renders every scalar
/// bind param (int, bool, etc) to its *text* representation and wraps it in
/// `StringValue` — mirroring how the real Data API accepts params as text
/// and implicitly casts them server-side. So `StringValue` must bind as a
/// text-format literal (`TextParam`, `accepts()` true for any type) rather
/// than a plain `String` (whose `ToSql` only accepts TEXT/VARCHAR-like
/// types) — otherwise binding "42" against an `INT4` column fails
/// `accepts()` before the query ever runs. `BooleanValue`/`LongValue`/
/// `DoubleValue` are handled defensively for completeness even though
/// today's server.rs never actually produces them for bind params.
fn param_to_sql(p: &SqlParameter) -> Box<dyn ToSql + Sync + Send> {
    match p.value() {
        None | Some(AwsField::IsNull(true)) => Box::new(TextParam(None)),
        Some(AwsField::IsNull(false)) => Box::new(TextParam(None)), // defensive, mirrors src/rds/client.rs
        Some(AwsField::BooleanValue(b)) => Box::new(TextParam(Some(b.to_string()))),
        Some(AwsField::LongValue(n)) => Box::new(TextParam(Some(n.to_string()))),
        Some(AwsField::DoubleValue(d)) => Box::new(TextParam(Some(d.to_string()))),
        Some(AwsField::StringValue(s)) => Box::new(TextParam(Some(s.clone()))),
        Some(AwsField::BlobValue(b)) => Box::new(b.as_ref().to_vec()),
        _ => Box::new(TextParam(None)),
    }
}

/// A bind param sent in pg *text* format, accepted by any column type —
/// same trick the pg wire protocol itself uses for text-format binds, and
/// what lets a `StringValue("42")` land in an `INT4` column without the
/// caller having to know the target type ahead of time.
#[derive(Debug)]
struct TextParam(Option<String>);

impl ToSql for TextParam {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> Format {
        Format::Text
    }

    tokio_postgres::types::to_sql_checked!();
}

/// pgea always names bind params `p1`, `p2`, ... sequentially by bind order,
/// and the SQL handed to `execute_statement` already has `:p1`, `:p2` (from
/// `src/rewriter.rs`). Since param N is always literally named `pN`, this is
/// a direct order-preserving `:pN` -> `$N` substitution — no remapping.
/// Skips single-quoted strings, `"quoted identifiers"`, and `--`/`/* */`
/// comments (mirroring `src/rewriter.rs`'s handling in the opposite
/// direction) so a literal or identifier containing `:p1`-looking text is
/// never rewritten. Dollar-quoted blocks aren't handled (not needed for this
/// direction per spec). Operates on `&str` slices rather than casting bytes
/// to `char`, so multi-byte UTF-8 (e.g. accented literals) passes through
/// unmangled.
fn translate_params(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            let end = bytes[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| i + p + 1)
                .unwrap_or(bytes.len());
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let end = sql[i + 2..]
                .find("*/")
                .map(|p| i + 2 + p + 2)
                .unwrap_or(bytes.len());
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }

        if b == b'\'' || b == b'"' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 2; // doubled-quote escape, still inside the literal/identifier
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(&sql[start..i]);
            continue;
        }

        if b == b':' && bytes.get(i + 1) == Some(&b'p') {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2 {
                out.push('$');
                out.push_str(&sql[i + 2..j]); // longest match: all digits after "p"
                i = j;
                continue;
            }
        }

        // Advance by one full UTF-8 char (not one byte) so multi-byte
        // sequences copy through intact instead of being mangled.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&sql[i..i + ch_len]);
        i += ch_len;
    }

    out
}

/// Length in bytes of the UTF-8 sequence starting with `first_byte`.
fn utf8_char_len(first_byte: u8) -> usize {
    match first_byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_simple_param() {
        assert_eq!(
            translate_params("SELECT * FROM t WHERE id = :p1"),
            "SELECT * FROM t WHERE id = $1"
        );
    }

    #[test]
    fn translates_multi_digit_param_longest_match() {
        assert_eq!(
            translate_params("INSERT INTO t VALUES (:p1, :p10)"),
            "INSERT INTO t VALUES ($1, $10)"
        );
    }

    #[test]
    fn skips_string_literal() {
        assert_eq!(
            translate_params("SELECT ':p1 is not a param', :p2"),
            "SELECT ':p1 is not a param', $2"
        );
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            translate_params("SELECT :p1 -- :p2 in a comment\n, :p3"),
            "SELECT $1 -- :p2 in a comment\n, $3"
        );
    }

    #[test]
    fn skips_quoted_identifier() {
        assert_eq!(
            translate_params(r#"SELECT "col:p1" FROM t WHERE id = :p2"#),
            r#"SELECT "col:p1" FROM t WHERE id = $2"#
        );
    }

    #[test]
    fn preserves_multibyte_utf8() {
        assert_eq!(
            translate_params("SELECT 'héllo', :p1"),
            "SELECT 'héllo', $1"
        );
    }
}
