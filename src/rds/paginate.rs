//! Auto-pagination around the Data API ~1 MB result-size cap.
//!
//! `ExecuteStatement` rejects any result larger than ~1 MB with a
//! `BadRequestException` ("Database response exceeded size limit") — there is
//! no cursor or `NextToken` to resume from, the whole call just fails. This
//! module makes that transparent: a query is run normally first (zero overhead
//! for the common case), and only on the size-limit error is it re-run in
//! `LIMIT/OFFSET` windows that are concatenated back into one [`ExecuteOutput`].
//!
//! ## Snapshot safety
//! Paging across separate autocommit `ExecuteStatement` calls would take a
//! fresh MVCC snapshot per page, so concurrent writes could duplicate or skip
//! rows — the exact "silent wrong results" the design deferred this feature
//! over. When the caller is not already inside a transaction we open our own
//! `REPEATABLE READ` transaction so every page reads one frozen snapshot, then
//! commit it. When the caller *is* in a transaction we page inside it and
//! inherit its isolation (the client's responsibility, matching pg semantics).
//!
//! ## Limits
//! - Ordering: `OFFSET` is only meaningful for a stable row order. Within a
//!   single snapshot with no `ORDER BY` PostgreSQL repeats the same scan order,
//!   so pages don't overlap; an explicit `ORDER BY` in the query is honoured.
//! - A single row larger than ~1 MB cannot be paginated (shrinks to `LIMIT 1`
//!   and still fails) — that surfaces as an error, same as without this module.

use aws_sdk_rdsdata::types::SqlParameter;
use tracing::{info, warn};

use crate::rds::{ExecuteOutput, RdsClient, RdsError};

/// Initial page size. The Data API historically also capped result sets at
/// 1000 records, so this never makes a page that would have succeeded smaller.
const INITIAL_PAGE_ROWS: i64 = 1000;

/// Run a statement, transparently paginating if the result exceeds the Data
/// API size limit. Behaviour is identical to a bare `execute_statement` for
/// any query that fits — the paging path is entered only on the size-limit
/// error.
pub async fn execute_paginated(
    rds: &dyn RdsClient,
    sql: &str,
    parameters: Vec<SqlParameter>,
    transaction_id: Option<&str>,
) -> Result<ExecuteOutput, RdsError> {
    match rds
        .execute_statement(sql, parameters.clone(), transaction_id)
        .await
    {
        Ok(out) => Ok(out),
        Err(e) if is_size_limit_error(&e) && is_wrappable(sql) => {
            info!("Data API result exceeded size limit; auto-paginating");
            paginate(rds, sql, parameters, transaction_id).await
        }
        Err(e) => Err(e),
    }
}

/// Only row-set-producing queries can be wrapped in `SELECT * FROM (...)` for
/// `LIMIT/OFFSET` paging. Anything else (DML, `EXPLAIN`, `SHOW`) gets the
/// original error verbatim — those don't return result sets large enough to
/// hit the cap anyway, and wrapping them would be invalid SQL.
fn is_wrappable(sql: &str) -> bool {
    matches!(
        crate::intercept::leading_verb(sql),
        Some("SELECT") // covers SELECT, WITH, VALUES, TABLE (all normalised to "SELECT")
    )
}

/// Page through `sql` in windows, holding a single snapshot. Opens (and later
/// commits) an own `REPEATABLE READ` transaction only when the caller is not
/// already inside one.
async fn paginate(
    rds: &dyn RdsClient,
    sql: &str,
    parameters: Vec<SqlParameter>,
    transaction_id: Option<&str>,
) -> Result<ExecuteOutput, RdsError> {
    let own_txn_id = match transaction_id {
        Some(_) => None,
        None => {
            let id = rds.begin_transaction().await?;
            // Must be set before the first query so the snapshot is taken once
            // and reused across every page.
            rds.execute_statement(
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                vec![],
                Some(&id),
            )
            .await?;
            Some(id)
        }
    };
    let txn = transaction_id.or(own_txn_id.as_deref());

    let result = paginate_loop(rds, sql, &parameters, txn).await;

    // Close our snapshot txn. Read-only, so the outcome of commit/rollback does
    // not change results; we just avoid leaving a transaction dangling.
    if let Some(id) = &own_txn_id {
        let _ = match &result {
            Ok(_) => rds.commit_transaction(id).await,
            Err(_) => rds.rollback_transaction(id).await,
        };
    }
    result
}

async fn paginate_loop(
    rds: &dyn RdsClient,
    sql: &str,
    parameters: &[SqlParameter],
    txn: Option<&str>,
) -> Result<ExecuteOutput, RdsError> {
    let inner = sql.trim_end().trim_end_matches(';');

    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut offset: i64 = 0;
    let mut limit: i64 = INITIAL_PAGE_ROWS;

    loop {
        let paged_sql =
            format!("SELECT * FROM ({inner}) AS _pgea_page LIMIT {limit} OFFSET {offset}");
        match rds
            .execute_statement(&paged_sql, parameters.to_vec(), txn)
            .await
        {
            Ok(page) => {
                if columns.is_empty() && !page.columns.is_empty() {
                    columns = page.columns;
                }
                let n = page.rows.len() as i64;
                rows.extend(page.rows);
                if n < limit {
                    break; // short page → last page
                }
                offset += n;
                // ponytail: keep the shrunk page size; don't grow back. Costs a
                // few extra round-trips on wide tables, never wrong results.
            }
            Err(e) if is_size_limit_error(&e) => {
                if limit <= 1 {
                    warn!("single row exceeds Data API size limit; cannot paginate");
                    return Err(e);
                }
                limit = (limit / 2).max(1);
                // retry the same offset with a smaller window
            }
            Err(e) => return Err(e),
        }
    }

    Ok(ExecuteOutput {
        columns,
        rows,
        rows_affected: 0,
    })
}

/// True if the error is the Data API's result-size-limit rejection.
///
/// Aurora surfaces this as `UnsupportedResultException: The result exceeds the
/// size limit 1 MB.` — the *same* exception class pgea already special-cases
/// for unsupported column types, so we discriminate on the `"size limit"` text,
/// not the class. `"exceed"` (stem) matches both `exceeds`/`exceeded` wording.
/// Verified against live Aurora UAT (2026-06-26).
fn is_size_limit_error(e: &RdsError) -> bool {
    let msg = match e {
        RdsError::Service(s) | RdsError::Sdk(s) => s,
    }
    .to_lowercase();
    msg.contains("size limit") && msg.contains("exceed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rds::{Field, ResultColumn};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Mock that fails the un-paginated call with a size error, then serves a
    /// fixed table of `total` rows through the wrapped `LIMIT/OFFSET` queries.
    struct PagingMock {
        total: i64,
        /// If set, any page requesting more than this many rows fails with a
        /// size error (simulates wide rows), forcing the adaptive shrink.
        max_page_rows: Option<i64>,
        calls: Mutex<Vec<String>>,
        began: Mutex<u32>,
        committed: Mutex<u32>,
    }

    impl PagingMock {
        fn new(total: i64, max_page_rows: Option<i64>) -> Self {
            Self {
                total,
                max_page_rows,
                calls: Mutex::new(Vec::new()),
                began: Mutex::new(0),
                committed: Mutex::new(0),
            }
        }

        fn parse(sql: &str, key: &str) -> i64 {
            sql.split(key)
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        }
    }

    fn size_err() -> RdsError {
        // Exact wording observed from live Aurora (2026-06-26).
        RdsError::Service(
            "UnsupportedResultException: The result exceeds the size limit 1 MB.".into(),
        )
    }

    #[async_trait]
    impl RdsClient for PagingMock {
        async fn execute_statement(
            &self,
            sql: &str,
            _parameters: Vec<SqlParameter>,
            _transaction_id: Option<&str>,
        ) -> Result<ExecuteOutput, RdsError> {
            self.calls.lock().unwrap().push(sql.to_string());

            if sql.starts_with("SET TRANSACTION") {
                return Ok(ExecuteOutput::default());
            }
            // The original (un-paginated) statement triggers the size error.
            if !sql.contains("_pgea_page") {
                return Err(size_err());
            }

            let limit = Self::parse(sql, "LIMIT");
            let offset = Self::parse(sql, "OFFSET");

            if let Some(max) = self.max_page_rows {
                if limit > max {
                    return Err(size_err()); // page too wide → caller shrinks
                }
            }

            let end = (offset + limit).min(self.total);
            let start = offset.min(self.total);
            let rows: Vec<Vec<Field>> = (start..end).map(|i| vec![Field::Long(i)]).collect();
            Ok(ExecuteOutput {
                columns: vec![ResultColumn {
                    name: "n".into(),
                    type_name: "int8".into(),
                    nullable: false,
                }],
                rows,
                rows_affected: 0,
            })
        }

        async fn begin_transaction(&self) -> Result<String, RdsError> {
            *self.began.lock().unwrap() += 1;
            Ok("tx-page".into())
        }
        async fn commit_transaction(&self, _tx: &str) -> Result<(), RdsError> {
            *self.committed.lock().unwrap() += 1;
            Ok(())
        }
        async fn rollback_transaction(&self, _tx: &str) -> Result<(), RdsError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn small_result_is_not_paginated() {
        // Mock that always succeeds — the happy path must make exactly one call.
        struct Ok1;
        #[async_trait]
        impl RdsClient for Ok1 {
            async fn execute_statement(
                &self,
                _sql: &str,
                _p: Vec<SqlParameter>,
                _t: Option<&str>,
            ) -> Result<ExecuteOutput, RdsError> {
                Ok(ExecuteOutput {
                    rows: vec![vec![Field::Long(1)]],
                    ..ExecuteOutput::default()
                })
            }
            async fn begin_transaction(&self) -> Result<String, RdsError> {
                panic!("must not begin a txn for a fitting result")
            }
            async fn commit_transaction(&self, _t: &str) -> Result<(), RdsError> {
                Ok(())
            }
            async fn rollback_transaction(&self, _t: &str) -> Result<(), RdsError> {
                Ok(())
            }
        }
        let out = execute_paginated(&Ok1, "SELECT 1", vec![], None)
            .await
            .unwrap();
        assert_eq!(out.rows.len(), 1);
    }

    #[tokio::test]
    async fn paginates_across_pages_in_own_snapshot_txn() {
        // 2500 rows, page 1000 → pages of 1000, 1000, 500.
        let mock = PagingMock::new(2500, None);
        let out = execute_paginated(&mock, "SELECT n FROM big", vec![], None)
            .await
            .unwrap();
        assert_eq!(out.rows.len(), 2500);
        assert_eq!(out.columns.len(), 1, "schema carried from first page");
        // Rows are in order across pages.
        let long = |f: &Field| match f {
            Field::Long(n) => *n,
            other => panic!("expected Long, got {other:?}"),
        };
        assert_eq!(long(&out.rows.first().unwrap()[0]), 0);
        assert_eq!(long(&out.rows.last().unwrap()[0]), 2499);
        assert_eq!(*mock.began.lock().unwrap(), 1, "one snapshot txn opened");
        assert_eq!(*mock.committed.lock().unwrap(), 1, "snapshot txn committed");
    }

    #[tokio::test]
    async fn adaptive_shrink_on_wide_rows() {
        // Any page wider than 250 rows fails; loop must shrink 1000→500→250.
        let mock = PagingMock::new(600, Some(250));
        let out = execute_paginated(&mock, "SELECT n FROM wide", vec![], None)
            .await
            .unwrap();
        assert_eq!(out.rows.len(), 600);
        let calls = mock.calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.contains("LIMIT 250")),
            "expected an adaptively shrunk page, calls: {calls:?}"
        );
    }

    #[tokio::test]
    async fn reuses_caller_transaction_without_opening_one() {
        let mock = PagingMock::new(1500, None);
        let out = execute_paginated(&mock, "SELECT n FROM big", vec![], Some("tx-outer"))
            .await
            .unwrap();
        assert_eq!(out.rows.len(), 1500);
        assert_eq!(
            *mock.began.lock().unwrap(),
            0,
            "must page inside the caller's txn"
        );
    }

    #[tokio::test]
    async fn single_oversized_row_surfaces_error() {
        // Even LIMIT 1 fails → the original error is returned, not a silent empty.
        let mock = PagingMock::new(10, Some(0));
        let err = execute_paginated(&mock, "SELECT n FROM huge", vec![], None)
            .await
            .unwrap_err();
        assert!(is_size_limit_error(&err));
    }
}
