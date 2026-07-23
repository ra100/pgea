//! Statement classification for the proxy.
//!
//! The intercept layer answers two questions before a statement is sent to
//! the RDS Data API:
//!
//! 1. Should this statement be rejected outright? (SAVEPOINT, COPY, cursors,
//!    LISTEN/NOTIFY — none are supported by Data API or by this proxy.)
//! 2. Is this a transaction control verb that must be mapped to a dedicated
//!    Data API call (BeginTransaction / CommitTransaction / RollbackTransaction)
//!    rather than to ExecuteStatement?
//!
//! This module is deliberately a single static regex per concern — SQL parsing
//! belongs in the rewriter, not here.

use once_cell::sync::Lazy;
use regex::Regex;

/// What the proxy should do with a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send to ExecuteStatement (possibly inside a transaction).
    Execute,
    /// Map to BeginTransaction.
    Begin,
    /// Map to CommitTransaction.
    Commit,
    /// Map to RollbackTransaction.
    Rollback,
    /// Reject with a clean pg ErrorResponse. The string is the unsupported op name.
    Reject(&'static str),
}

static REJECT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^\s*(SAVEPOINT|RELEASE\s+SAVEPOINT|LISTEN|NOTIFY|UNLISTEN|COPY|DECLARE\s+[A-Za-z_][A-Za-z0-9_]*\s+(?:BINARY\s+|INSENSITIVE\s+|SCROLL\s+|NO\s+SCROLL\s+|ASENSITIVE\s+)?CURSOR|FETCH(?:\s+\w+)?\s+(?:FROM|IN)|MOVE(?:\s+\w+)?\s+(?:FROM|IN))\b",
    )
    .unwrap()
});

static BEGIN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*(BEGIN|START\s+TRANSACTION)\b").unwrap());

static COMMIT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*(COMMIT|END(?:\s+TRANSACTION)?)\b").unwrap());

static ROLLBACK: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^\s*ROLLBACK\b").unwrap());

/// Classify a single SQL statement. The input is taken as-is (no comment stripping)
/// because pg drivers send canonical control statements without leading comments,
/// and a comment-stripping pass at this layer would be a foot-gun for user SQL.
pub fn classify(sql: &str) -> Action {
    if let Some(m) = REJECT.captures(sql) {
        // Extract a stable label for the rejection message.
        let raw = m
            .get(1)
            .map(|m| m.as_str().to_uppercase())
            .unwrap_or_default();
        let op: &'static str = if raw.starts_with("SAVEPOINT") {
            "SAVEPOINT"
        } else if raw.starts_with("RELEASE") {
            "RELEASE SAVEPOINT"
        } else if raw.starts_with("LISTEN") {
            "LISTEN"
        } else if raw.starts_with("UNLISTEN") {
            "UNLISTEN"
        } else if raw.starts_with("NOTIFY") {
            "NOTIFY"
        } else if raw.starts_with("COPY") {
            "COPY"
        } else if raw.starts_with("DECLARE") {
            "DECLARE CURSOR"
        } else if raw.starts_with("FETCH") {
            "FETCH"
        } else if raw.starts_with("MOVE") {
            "MOVE"
        } else {
            "unsupported statement"
        };
        return Action::Reject(op);
    }
    if BEGIN.is_match(sql) {
        return Action::Begin;
    }
    if COMMIT.is_match(sql) {
        return Action::Commit;
    }
    if ROLLBACK.is_match(sql) {
        return Action::Rollback;
    }
    Action::Execute
}

/// Read verbs a read-only target may run. Everything else that classifies as
/// `Execute` (DML, DDL, GRANT/REVOKE, VACUUM/ANALYZE/REINDEX/REFRESH, CALL) is
/// blocked. `SET`/`SHOW`/`RESET` are allowed because GUI clients emit them on
/// connect and they cannot mutate table data; transaction verbs are handled by
/// `classify` before this check runs.
static READ_VERB: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^\s*(SELECT|WITH|VALUES|TABLE|EXPLAIN|SHOW|SET|RESET)\b").unwrap()
});

/// `EXPLAIN ... ANALYZE` actually *executes* the analyzed statement, so
/// `EXPLAIN ANALYZE INSERT ...` is a write. Match `EXPLAIN` followed anywhere
/// (before the wrapped statement) by the `ANALYZE` option, in both the bare
/// (`EXPLAIN ANALYZE ...`) and parenthesized (`EXPLAIN (ANALYZE, ...) ...`)
/// forms. Plain `EXPLAIN`/`EXPLAIN VERBOSE` (planning only, no execution) is
/// not matched and stays allowed.
static EXPLAIN_ANALYZE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*EXPLAIN\b[\s\S]*?\bANALYZE\b").unwrap());

/// A data-modifying statement inside a CTE (`WITH x AS (INSERT ...) ...`) runs
/// the write even though the outer statement leads with `WITH`. Match a `WITH`
/// whose first parenthesized body opens with a write verb.
static WRITABLE_CTE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*WITH\b[\s\S]*?\(\s*(INSERT|UPDATE|DELETE|MERGE)\b").unwrap());

/// Like [`classify`], but for a target configured `read_only`. This is a
/// *fast-reject* layer: it turns the obvious write shapes into a clean pg error
/// without a Data API round-trip. It is NOT the security boundary — that is the
/// engine-enforced `SET TRANSACTION READ ONLY` wrap in the server (see
/// `TxnState::begin_read_only`), which also catches writes this regex cannot
/// see (volatile writing functions in a `SELECT`, etc). Transaction control and
/// unsupported-op rejections behave exactly as in `classify`.
pub fn classify_read_only(sql: &str) -> Action {
    match classify(sql) {
        Action::Execute
            if !READ_VERB.is_match(sql)
                || EXPLAIN_ANALYZE.is_match(sql)
                || WRITABLE_CTE.is_match(sql) =>
        {
            Action::Reject("write statement on read-only target")
        }
        other => other,
    }
}

/// Detect the leading SQL verb for the pg `CommandComplete` tag.
/// Returns None if the verb is not one we tag specially (caller falls back to "SELECT 0" etc.).
pub fn leading_verb(sql: &str) -> Option<&'static str> {
    static VERB: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?i)^\s*(SELECT|INSERT|UPDATE|DELETE|MERGE|WITH|VALUES|TABLE|CALL|EXPLAIN|TRUNCATE|CREATE|DROP|ALTER|GRANT|REVOKE|VACUUM|ANALYZE|REINDEX|REFRESH|SET|SHOW|RESET)\b",
        )
        .unwrap()
    });
    VERB.captures(sql)
        .and_then(|c| c.get(1))
        .map(|m| match m.as_str().to_uppercase().as_str() {
            "SELECT" => "SELECT",
            "INSERT" => "INSERT",
            "UPDATE" => "UPDATE",
            "DELETE" => "DELETE",
            "MERGE" => "MERGE",
            "WITH" => "SELECT", // CTE — assume read; CommandComplete is best-effort
            "VALUES" => "SELECT",
            "TABLE" => "SELECT",
            "CALL" => "CALL",
            "EXPLAIN" => "EXPLAIN",
            "TRUNCATE" => "TRUNCATE",
            "CREATE" => "CREATE",
            "DROP" => "DROP",
            "ALTER" => "ALTER",
            "GRANT" => "GRANT",
            "REVOKE" => "REVOKE",
            "VACUUM" => "VACUUM",
            "ANALYZE" => "ANALYZE",
            "REINDEX" => "REINDEX",
            "REFRESH" => "REFRESH",
            "SET" => "SET",
            "SHOW" => "SHOW",
            "RESET" => "RESET",
            _ => "SELECT",
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_savepoint() {
        assert_eq!(classify("SAVEPOINT foo"), Action::Reject("SAVEPOINT"));
        assert_eq!(classify("  savepoint bar"), Action::Reject("SAVEPOINT"));
        assert_eq!(
            classify("RELEASE SAVEPOINT foo"),
            Action::Reject("RELEASE SAVEPOINT")
        );
    }

    #[test]
    fn classifies_listen_notify() {
        assert_eq!(classify("LISTEN ch"), Action::Reject("LISTEN"));
        assert_eq!(classify("NOTIFY ch, 'x'"), Action::Reject("NOTIFY"));
        assert_eq!(classify("UNLISTEN *"), Action::Reject("UNLISTEN"));
    }

    #[test]
    fn classifies_copy() {
        assert_eq!(classify("COPY users TO STDOUT"), Action::Reject("COPY"));
    }

    #[test]
    fn classifies_cursor_ops() {
        assert_eq!(
            classify("DECLARE c CURSOR FOR SELECT 1"),
            Action::Reject("DECLARE CURSOR")
        );
        assert_eq!(
            classify("DECLARE c BINARY CURSOR FOR SELECT 1"),
            Action::Reject("DECLARE CURSOR")
        );
        assert_eq!(classify("FETCH 10 FROM c"), Action::Reject("FETCH"));
        assert_eq!(classify("MOVE 5 IN c"), Action::Reject("MOVE"));
    }

    #[test]
    fn classifies_txn_verbs() {
        assert_eq!(classify("BEGIN"), Action::Begin);
        assert_eq!(classify("begin"), Action::Begin);
        assert_eq!(classify("START TRANSACTION READ ONLY"), Action::Begin);
        assert_eq!(classify("COMMIT"), Action::Commit);
        assert_eq!(classify("END"), Action::Commit);
        assert_eq!(classify("END TRANSACTION"), Action::Commit);
        assert_eq!(classify("ROLLBACK"), Action::Rollback);
    }

    #[test]
    fn classifies_normal_statements_as_execute() {
        assert_eq!(classify("SELECT 1"), Action::Execute);
        assert_eq!(classify("UPDATE t SET a = 1"), Action::Execute);
        assert_eq!(classify("INSERT INTO t (a) VALUES (1)"), Action::Execute);
        assert_eq!(
            classify("WITH x AS (SELECT 1) SELECT * FROM x"),
            Action::Execute
        );
    }

    #[test]
    fn read_only_allows_reads_and_session_verbs() {
        for sql in [
            "SELECT 1",
            "  select * from t",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "VALUES (1)",
            "TABLE users",
            "EXPLAIN SELECT 1",
            "SHOW server_version",
            "SET search_path = public",
            "RESET search_path",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
        ] {
            assert_ne!(
                classify_read_only(sql),
                Action::Reject("write statement on read-only target"),
                "expected {sql:?} to be allowed on a read-only target"
            );
        }
    }

    #[test]
    fn read_only_blocks_writes() {
        let w = Action::Reject("write statement on read-only target");
        assert_eq!(classify_read_only("INSERT INTO t (a) VALUES (1)"), w);
        assert_eq!(classify_read_only("UPDATE t SET a = 1"), w);
        assert_eq!(classify_read_only("DELETE FROM t"), w);
        assert_eq!(classify_read_only("MERGE INTO t ..."), w);
        assert_eq!(classify_read_only("TRUNCATE t"), w);
        assert_eq!(classify_read_only("CREATE TABLE t (a int)"), w);
        assert_eq!(classify_read_only("DROP TABLE t"), w);
        assert_eq!(classify_read_only("ALTER TABLE t ADD b int"), w);
        assert_eq!(classify_read_only("GRANT ALL ON t TO r"), w);
        assert_eq!(classify_read_only("CALL do_write()"), w);
        assert_eq!(classify_read_only("VACUUM t"), w);
    }

    #[test]
    fn read_only_blocks_explain_analyze_and_writable_cte() {
        let w = Action::Reject("write statement on read-only target");
        // EXPLAIN ANALYZE executes the analyzed statement — a write.
        assert_eq!(
            classify_read_only("EXPLAIN ANALYZE INSERT INTO t VALUES (1)"),
            w
        );
        assert_eq!(
            classify_read_only("EXPLAIN (ANALYZE, BUFFERS) DELETE FROM t"),
            w
        );
        assert_eq!(
            classify_read_only("  explain   analyze\tupdate t set a=1"),
            w
        );
        // Writable CTE runs the write inside the WITH.
        assert_eq!(
            classify_read_only("WITH x AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM x"),
            w
        );
        // Plain EXPLAIN (planning only) and harmless CTE stay allowed.
        assert_ne!(classify_read_only("EXPLAIN SELECT 1"), w);
        assert_ne!(classify_read_only("EXPLAIN VERBOSE SELECT 1"), w);
        assert_ne!(
            classify_read_only("WITH x AS (SELECT 1) SELECT * FROM x"),
            w
        );
    }

    #[test]
    fn read_only_still_rejects_unsupported_ops() {
        // Unsupported-op rejections keep their own label, not the write label.
        assert_eq!(
            classify_read_only("COPY users TO STDOUT"),
            Action::Reject("COPY")
        );
        assert_eq!(classify_read_only("LISTEN ch"), Action::Reject("LISTEN"));
    }

    #[test]
    fn does_not_misclassify_identifiers_starting_with_keywords() {
        // "SELECT * FROM listen" uses listen as a table name, not a LISTEN statement.        assert_eq!(classify("SELECT * FROM listen"), Action::Execute);
        // A column called "begin"
        assert_eq!(classify("SELECT begin FROM events"), Action::Execute);
    }

    #[test]
    fn leading_verb_detection() {
        assert_eq!(leading_verb("SELECT 1"), Some("SELECT"));
        assert_eq!(leading_verb("  insert into t values (1)"), Some("INSERT"));
        assert_eq!(leading_verb("UPDATE t SET x=1"), Some("UPDATE"));
        assert_eq!(leading_verb("DELETE FROM t"), Some("DELETE"));
        assert_eq!(
            leading_verb("WITH x AS (SELECT 1) SELECT 1"),
            Some("SELECT")
        );
        assert_eq!(leading_verb("SET search_path = public"), Some("SET"));
        assert_eq!(leading_verb("SHOW server_version"), Some("SHOW"));
        assert_eq!(leading_verb("not a sql verb at all"), None);
    }
}
