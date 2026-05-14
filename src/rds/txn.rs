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
