//! Abstraction over the RDS Data API.
//!
//! The trait `RdsClient` is the seam that lets us mock the AWS SDK in tests
//! without spinning up real infrastructure. The production implementation
//! `AwsRdsClient` wraps `aws_sdk_rdsdata::Client`. The Data API result shape is
//! flattened here into our own types so the rest of the proxy never imports
//! `aws_sdk_rdsdata` directly — keeps the boundary in `mod.rs` honest.

use async_trait::async_trait;
use aws_sdk_rdsdata::types::{Field as AwsField, SqlParameter};

/// Errors surfaced by the RDS layer. `Service` carries the message produced
/// by AWS so it can be relayed verbatim into a pg `ErrorResponse`.
#[derive(Debug, thiserror::Error)]
pub enum RdsError {
    #[error("AWS service error: {0}")]
    Service(String),

    #[error("SDK error: {0}")]
    Sdk(String),
}

/// One column of a result set, as returned by the Data API.
/// `type_name` is what `oid_for_type_name` was built to consume.
#[derive(Debug, Clone)]
pub struct ResultColumn {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

/// One field of one row. Mirrors `aws_sdk_rdsdata::types::Field` minus the
/// `ArrayValue` recursion (we serialise arrays inline at the boundary).
#[derive(Debug, Clone)]
pub enum Field {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    String(String),
    Blob(Vec<u8>),
    /// Pre-serialised pg array literal text (`{a,b,"c d"}`). The Data API's
    /// `ArrayValue` is recursive and tagged; we collapse it into a text literal
    /// at the point of conversion so downstream encoding stays trivial.
    ArrayLiteral(String),
}

/// Result of a single `ExecuteStatement` call.
#[derive(Debug, Clone, Default)]
pub struct ExecuteOutput {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Field>>,
    pub rows_affected: i64,
}

/// What the proxy needs from the Data API. Kept narrow on purpose.
#[async_trait]
pub trait RdsClient: Send + Sync {
    async fn execute_statement(
        &self,
        sql: &str,
        parameters: Vec<SqlParameter>,
        transaction_id: Option<&str>,
    ) -> Result<ExecuteOutput, RdsError>;

    async fn begin_transaction(&self) -> Result<String, RdsError>;

    async fn commit_transaction(&self, transaction_id: &str) -> Result<(), RdsError>;

    async fn rollback_transaction(&self, transaction_id: &str) -> Result<(), RdsError>;
}

/// Production implementation backed by `aws_sdk_rdsdata::Client`.
pub struct AwsRdsClient {
    client: aws_sdk_rdsdata::Client,
    cluster_arn: String,
    secret_arn: String,
    database: String,
}

impl AwsRdsClient {
    pub fn new(
        client: aws_sdk_rdsdata::Client,
        cluster_arn: String,
        secret_arn: String,
        database: String,
    ) -> Self {
        Self {
            client,
            cluster_arn,
            secret_arn,
            database,
        }
    }
}

#[async_trait]
impl RdsClient for AwsRdsClient {
    async fn execute_statement(
        &self,
        sql: &str,
        parameters: Vec<SqlParameter>,
        transaction_id: Option<&str>,
    ) -> Result<ExecuteOutput, RdsError> {
        let mut req = self
            .client
            .execute_statement()
            .resource_arn(&self.cluster_arn)
            .secret_arn(&self.secret_arn)
            .database(&self.database)
            .sql(sql)
            .include_result_metadata(true);

        if !parameters.is_empty() {
            req = req.set_parameters(Some(parameters));
        }
        if let Some(tx) = transaction_id {
            req = req.transaction_id(tx);
        }

        let resp = req.send().await.map_err(sdk_error)?;

        let columns: Vec<ResultColumn> = resp
            .column_metadata()
            .iter()
            .map(|c| ResultColumn {
                name: c.name().unwrap_or("").to_string(),
                type_name: c.type_name().unwrap_or("text").to_string(),
                nullable: c.nullable() != 0,
            })
            .collect();

        let rows: Vec<Vec<Field>> = resp
            .records()
            .iter()
            .map(|row| row.iter().map(field_from_aws).collect())
            .collect();

        Ok(ExecuteOutput {
            columns,
            rows,
            rows_affected: resp.number_of_records_updated(),
        })
    }

    async fn begin_transaction(&self) -> Result<String, RdsError> {
        let resp = self
            .client
            .begin_transaction()
            .resource_arn(&self.cluster_arn)
            .secret_arn(&self.secret_arn)
            .database(&self.database)
            .send()
            .await
            .map_err(sdk_error)?;
        resp.transaction_id()
            .map(|s| s.to_string())
            .ok_or_else(|| RdsError::Service("BeginTransaction returned no transactionId".into()))
    }

    async fn commit_transaction(&self, transaction_id: &str) -> Result<(), RdsError> {
        self.client
            .commit_transaction()
            .resource_arn(&self.cluster_arn)
            .secret_arn(&self.secret_arn)
            .transaction_id(transaction_id)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    async fn rollback_transaction(&self, transaction_id: &str) -> Result<(), RdsError> {
        self.client
            .rollback_transaction()
            .resource_arn(&self.cluster_arn)
            .secret_arn(&self.secret_arn)
            .transaction_id(transaction_id)
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }
}

/// Convert the SDK's `Field` enum into our flattened version. Arrays are
/// serialised inline with the helpers in `crate::types`.
fn field_from_aws(f: &AwsField) -> Field {
    match f {
        AwsField::IsNull(true) => Field::Null,
        AwsField::IsNull(false) => Field::Null, // defensive
        AwsField::BooleanValue(b) => Field::Bool(*b),
        AwsField::LongValue(n) => Field::Long(*n),
        AwsField::DoubleValue(d) => Field::Double(*d),
        AwsField::StringValue(s) => Field::String(s.clone()),
        AwsField::BlobValue(b) => Field::Blob(b.as_ref().to_vec()),
        AwsField::ArrayValue(a) => Field::ArrayLiteral(format_array_value(a)),
        _ => Field::Null,
    }
}

/// Convert a Data API `ArrayValue` to a pg text array literal.
fn format_array_value(a: &aws_sdk_rdsdata::types::ArrayValue) -> String {
    use crate::types::format_array_literal;

    if let Ok(vs) = a.as_string_values() {
        let elements: Vec<Option<String>> = vs.iter().map(|s| Some(s.clone())).collect();
        return format_array_literal(&elements);
    }
    if let Ok(vs) = a.as_long_values() {
        let elements: Vec<Option<String>> = vs.iter().map(|n| Some(n.to_string())).collect();
        return format_array_literal(&elements);
    }
    if let Ok(vs) = a.as_double_values() {
        let elements: Vec<Option<String>> = vs.iter().map(|n| Some(n.to_string())).collect();
        return format_array_literal(&elements);
    }
    if let Ok(vs) = a.as_boolean_values() {
        let elements: Vec<Option<String>> = vs
            .iter()
            .map(|b| Some(if *b { "t".to_string() } else { "f".to_string() }))
            .collect();
        return format_array_literal(&elements);
    }
    if let Ok(vs) = a.as_array_values() {
        // Nested array: recurse and emit children inline (pg parses brace structure).
        let mut out = String::from("{");
        for (i, child) in vs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format_array_value(child));
        }
        out.push('}');
        return out;
    }
    "{}".to_string()
}

fn sdk_error<E: std::fmt::Display>(e: E) -> RdsError {
    RdsError::Sdk(e.to_string())
}

#[cfg(test)]
pub mod mock {
    //! Test double — records calls and returns canned responses.

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockRdsClient {
        pub state: Mutex<MockState>,
    }

    #[derive(Default)]
    pub struct MockState {
        pub executes: Vec<(String, Vec<SqlParameter>, Option<String>)>,
        pub begin_calls: u32,
        pub commit_calls: Vec<String>,
        pub rollback_calls: Vec<String>,
        pub canned_execute: Option<ExecuteOutput>,
        pub canned_execute_err: Option<String>,
        pub canned_txn_id: Option<String>,
    }

    impl MockRdsClient {
        pub fn with_execute(output: ExecuteOutput) -> Self {
            let m = Self::default();
            m.state.lock().unwrap().canned_execute = Some(output);
            m
        }
        pub fn with_txn_id(self, id: &str) -> Self {
            self.state.lock().unwrap().canned_txn_id = Some(id.to_string());
            self
        }
        pub fn with_execute_err(self, msg: &str) -> Self {
            self.state.lock().unwrap().canned_execute_err = Some(msg.to_string());
            self
        }
    }

    #[async_trait]
    impl RdsClient for MockRdsClient {
        async fn execute_statement(
            &self,
            sql: &str,
            parameters: Vec<SqlParameter>,
            transaction_id: Option<&str>,
        ) -> Result<ExecuteOutput, RdsError> {
            let mut s = self.state.lock().unwrap();
            s.executes
                .push((sql.to_string(), parameters, transaction_id.map(str::to_owned)));
            if let Some(msg) = s.canned_execute_err.clone() {
                return Err(RdsError::Service(msg));
            }
            Ok(s.canned_execute.clone().unwrap_or_default())
        }

        async fn begin_transaction(&self) -> Result<String, RdsError> {
            let mut s = self.state.lock().unwrap();
            s.begin_calls += 1;
            Ok(s.canned_txn_id.clone().unwrap_or_else(|| "tx-1".to_string()))
        }

        async fn commit_transaction(&self, tx: &str) -> Result<(), RdsError> {
            self.state.lock().unwrap().commit_calls.push(tx.to_string());
            Ok(())
        }

        async fn rollback_transaction(&self, tx: &str) -> Result<(), RdsError> {
            self.state
                .lock()
                .unwrap()
                .rollback_calls
                .push(tx.to_string());
            Ok(())
        }
    }
}
