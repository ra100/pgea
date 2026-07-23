//! Per-pg-connection transaction state machine.
//!
//! Pg clients send `BEGIN` / `COMMIT` / `ROLLBACK` as ordinary statements; the
//! Data API needs separate API calls and a `transactionId` threaded through
//! every `ExecuteStatement` until commit/rollback. This struct owns that state.

use std::sync::Arc;

use crate::rds::RdsClient;

#[derive(Debug, thiserror::Error)]
pub enum TxnError {
    #[error("already inside a transaction")]
    AlreadyInTransaction,

    #[error("not currently in a transaction")]
    NotInTransaction,

    #[error(transparent)]
    Rds(#[from] crate::rds::RdsError),
}

/// pg `ReadyForQuery` status byte: I=idle, T=in-tx, E=failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    Idle,
    InTransaction,
    Failed,
}

impl TxnStatus {
    pub fn as_byte(self) -> u8 {
        match self {
            TxnStatus::Idle => b'I',
            TxnStatus::InTransaction => b'T',
            TxnStatus::Failed => b'E',
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TxnState {
    transaction_id: Option<String>,
    failed: bool,
}

impl TxnState {
    pub fn transaction_id(&self) -> Option<&str> {
        self.transaction_id.as_deref()
    }

    pub fn status(&self) -> TxnStatus {
        if self.failed {
            TxnStatus::Failed
        } else if self.transaction_id.is_some() {
            TxnStatus::InTransaction
        } else {
            TxnStatus::Idle
        }
    }

    /// Mark the current transaction as failed (an error occurred mid-txn).
    /// Subsequent statements other than `ROLLBACK` should be rejected.
    pub fn mark_failed(&mut self) {
        if self.transaction_id.is_some() {
            self.failed = true;
        }
    }

    pub async fn begin(&mut self, client: &Arc<dyn RdsClient>) -> Result<(), TxnError> {
        if self.transaction_id.is_some() {
            return Err(TxnError::AlreadyInTransaction);
        }
        let id = client.begin_transaction().await?;
        self.transaction_id = Some(id);
        self.failed = false;
        Ok(())
    }

    /// Begin a transaction and immediately mark it `READ ONLY`, so PostgreSQL
    /// itself rejects any write — including ones the intercept regex cannot
    /// see (writable CTEs, `SELECT`s that call volatile writing functions,
    /// `EXPLAIN ANALYZE` of a write). This is the load-bearing guarantee for a
    /// `read_only` target; the regex layer is only a fast, friendly reject.
    ///
    /// `SET TRANSACTION READ ONLY` must be the first statement in the txn, so
    /// this runs it before any user SQL. On failure the half-open transaction
    /// is rolled back so the connection is left idle, not stuck in-txn.
    pub async fn begin_read_only(&mut self, client: &Arc<dyn RdsClient>) -> Result<(), TxnError> {
        self.begin(client).await?;
        let id = self
            .transaction_id
            .clone()
            .expect("transaction_id set by begin");
        if let Err(e) = client
            .execute_statement("SET TRANSACTION READ ONLY", vec![], Some(&id))
            .await
        {
            // Best-effort unwind; report the original error.
            let _ = self.rollback(client).await;
            return Err(TxnError::from(e));
        }
        Ok(())
    }

    pub async fn commit(&mut self, client: &Arc<dyn RdsClient>) -> Result<(), TxnError> {
        let id = self
            .transaction_id
            .take()
            .ok_or(TxnError::NotInTransaction)?;
        self.failed = false;
        client.commit_transaction(&id).await?;
        Ok(())
    }

    pub async fn rollback(&mut self, client: &Arc<dyn RdsClient>) -> Result<(), TxnError> {
        let id = self
            .transaction_id
            .take()
            .ok_or(TxnError::NotInTransaction)?;
        self.failed = false;
        client.rollback_transaction(&id).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rds::client::mock::MockRdsClient;

    fn arc_client(c: MockRdsClient) -> Arc<dyn RdsClient> {
        Arc::new(c)
    }

    #[tokio::test]
    async fn begin_read_only_issues_set_transaction_read_only() {
        let mock = Arc::new(MockRdsClient::default());
        mock.state.lock().unwrap().canned_txn_id = Some("tx-ro".into());
        let client: Arc<dyn RdsClient> = mock.clone();

        let mut state = TxnState::default();
        state.begin_read_only(&client).await.unwrap();

        assert_eq!(state.status(), TxnStatus::InTransaction);
        assert_eq!(state.transaction_id(), Some("tx-ro"));

        let s = mock.state.lock().unwrap();
        assert_eq!(s.begin_calls, 1);
        // The first (and only) statement run in the txn is the read-only marker,
        // threaded through the new transaction id.
        assert_eq!(
            s.executes,
            vec![(
                "SET TRANSACTION READ ONLY".to_string(),
                vec![],
                Some("tx-ro".to_string())
            )]
        );
    }

    #[tokio::test]
    async fn begin_read_only_rolls_back_when_set_fails() {
        let mock = Arc::new(MockRdsClient::default());
        {
            let mut s = mock.state.lock().unwrap();
            s.canned_txn_id = Some("tx-bad".into());
            s.canned_execute_err = Some("boom".into());
        }
        let client: Arc<dyn RdsClient> = mock.clone();

        let mut state = TxnState::default();
        let err = state.begin_read_only(&client).await.unwrap_err();
        assert!(matches!(err, TxnError::Rds(_)));
        // Connection is left idle, not stuck in a half-open transaction.
        assert_eq!(state.status(), TxnStatus::Idle);
        assert_eq!(
            mock.state.lock().unwrap().rollback_calls,
            vec!["tx-bad".to_string()]
        );
    }

    #[tokio::test]
    async fn begin_commit_cycle() {
        let mock = MockRdsClient::default();
        mock.state.lock().unwrap().canned_txn_id = Some("tx-42".into());
        let client = arc_client(mock);

        let mut state = TxnState::default();
        assert_eq!(state.status(), TxnStatus::Idle);

        state.begin(&client).await.unwrap();
        assert_eq!(state.status(), TxnStatus::InTransaction);
        assert_eq!(state.transaction_id(), Some("tx-42"));

        state.commit(&client).await.unwrap();
        assert_eq!(state.status(), TxnStatus::Idle);
        assert_eq!(state.transaction_id(), None);
    }

    #[tokio::test]
    async fn nested_begin_rejected() {
        let client = arc_client(MockRdsClient::default());
        let mut state = TxnState::default();
        state.begin(&client).await.unwrap();
        let err = state.begin(&client).await.unwrap_err();
        assert!(matches!(err, TxnError::AlreadyInTransaction));
    }

    #[tokio::test]
    async fn commit_outside_txn_rejected() {
        let client = arc_client(MockRdsClient::default());
        let mut state = TxnState::default();
        let err = state.commit(&client).await.unwrap_err();
        assert!(matches!(err, TxnError::NotInTransaction));
    }

    #[tokio::test]
    async fn failed_state_after_error_then_rollback_clears() {
        let client = arc_client(MockRdsClient::default());
        let mut state = TxnState::default();
        state.begin(&client).await.unwrap();
        state.mark_failed();
        assert_eq!(state.status(), TxnStatus::Failed);
        state.rollback(&client).await.unwrap();
        assert_eq!(state.status(), TxnStatus::Idle);
    }

    #[tokio::test]
    async fn mark_failed_outside_txn_is_noop() {
        let mut state = TxnState::default();
        state.mark_failed();
        assert_eq!(state.status(), TxnStatus::Idle);
    }
}
