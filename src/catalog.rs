//! Targeted rewrites for catalog queries that hit RDS Data API type
//! restrictions. The Data API refuses to return rows containing certain
//! Postgres types (`CHAR`/`bpchar`, `TIME`, `INTERVAL`, etc), so common GUI
//! introspection queries fail unless we cast those columns to `text`.
//!
//! This module is deliberately narrow: it pattern-matches a handful of
//! known queries and rewrites them. We do not attempt general SQL rewriting.

use once_cell::sync::Lazy;
use regex::Regex;

/// Rewrite known catalog queries that would otherwise trip
/// `UnsupportedResultException` or other Aurora type-output errors.
/// Returns the rewritten SQL when a rule matches; returns `None` for
/// SQL we don't touch.
pub fn maybe_rewrite(sql: &str) -> Option<String> {
    if let Some(out) = rewrite_pg_type_star(sql) {
        return Some(out);
    }
    if let Some(out) = rewrite_pg_namespace_star(sql) {
        return Some(out);
    }
    None
}

/// DBeaver schema browser fires
/// `SELECT n.oid, n.*, d.description FROM pg_catalog.pg_namespace n ...`.
/// `n.*` pulls `nspacl` which is `aclitem[]` — Aurora has no binary output
/// function for aclitem and the Data API request fails. Replace `n.*` with
/// an explicit column list that casts `nspacl` to text.
fn rewrite_pg_namespace_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_namespace\s+(?:AS\s+)?n\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bn\.\*").unwrap());

    if !FROM_RE.is_match(sql) || !STAR_RE.is_match(sql) {
        return None;
    }
    let cols = "n.nspname, n.nspowner, n.nspacl::text AS nspacl";
    Some(STAR_RE.replace(sql, cols).into_owned())
}

/// DBeaver / DataGrip ship a metadata reader that does
/// `SELECT t.oid, t.*, ... FROM pg_catalog.pg_type t ...`. The `t.*` star
/// expansion pulls `typcategory`, `typdelim`, `typalign`, `typstorage` —
/// each declared as `char(1)`. The Data API refuses to return them.
///
/// Replace `t.*` with an explicit column list that casts the CHAR columns
/// to text, leaving every other column untouched.
fn rewrite_pg_type_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_type\s+(?:AS\s+)?t\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bt\.\*").unwrap());

    if !FROM_RE.is_match(sql) {
        return None;
    }
    if !STAR_RE.is_match(sql) {
        return None;
    }

    // Explicit column list for pg_type. Casts the four CHAR(1) columns to
    // text. Order matches `pg_type` (PG 14+) but ordering is not load-bearing
    // — DBeaver indexes columns by name from the result metadata.
    // RDS Data API refuses several Postgres types in result rows. Cast every
    // offender in pg_type:
    //   char(1):       typtype, typcategory, typdelim, typalign, typstorage
    //   regproc:       typinput, typoutput, typreceive, typsend, typmodin,
    //                  typmodout, typanalyze, typsubscript
    //   pg_node_tree:  typdefaultbin
    //   aclitem[]:     typacl  (cast to text[] then ::text for safety)
    let cols = "\
t.typname, t.typnamespace, t.typowner, t.typlen, t.typbyval, \
t.typtype::text AS typtype, \
t.typcategory::text AS typcategory, t.typispreferred, t.typisdefined, \
t.typdelim::text AS typdelim, t.typrelid, \
t.typsubscript::text AS typsubscript, t.typelem, t.typarray, \
t.typinput::text AS typinput, t.typoutput::text AS typoutput, \
t.typreceive::text AS typreceive, t.typsend::text AS typsend, \
t.typmodin::text AS typmodin, t.typmodout::text AS typmodout, \
t.typanalyze::text AS typanalyze, t.typalign::text AS typalign, \
t.typstorage::text AS typstorage, t.typnotnull, t.typbasetype, t.typtypmod, \
t.typndims, t.typcollation, t.typdefaultbin::text AS typdefaultbin, \
t.typdefault, t.typacl::text AS typacl";

    let mut out: String = STAR_RE.replace(sql, cols).into_owned();

    // The same DBeaver query projects `c.relkind` from pg_class as a result
    // column (char(1) — refused by Data API). The exact projection token in
    // the upstream query is `,c.relkind,`. Replace that single occurrence;
    // predicate uses (`c.relkind IS NULL`, `c.relkind = 'c'`) keep their
    // unmodified `c.relkind` reference because they don't sit between commas.
    out = out.replacen(
        ",c.relkind,",
        ",c.relkind::text AS relkind,",
        1,
    );

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_pg_type_star() {
        let sql = "SELECT t.oid,t.*,c.relkind,d.description FROM pg_catalog.pg_type t LEFT JOIN pg_catalog.pg_class c ON c.oid=t.typrelid LEFT JOIN pg_catalog.pg_description d ON t.oid=d.objoid";
        let out = maybe_rewrite(sql).expect("should match");
        assert!(out.contains("t.typtype::text AS typtype"));
        assert!(out.contains("t.typcategory::text AS typcategory"));
        assert!(out.contains("t.typdelim::text AS typdelim"));
        assert!(out.contains("t.typalign::text AS typalign"));
        assert!(out.contains("t.typstorage::text AS typstorage"));
        assert!(out.contains("t.typinput::text AS typinput"));
        assert!(out.contains("t.typoutput::text AS typoutput"));
        assert!(out.contains("t.typreceive::text AS typreceive"));
        assert!(out.contains("t.typsend::text AS typsend"));
        assert!(out.contains("t.typmodin::text AS typmodin"));
        assert!(out.contains("t.typmodout::text AS typmodout"));
        assert!(out.contains("t.typanalyze::text AS typanalyze"));
        assert!(out.contains("t.typsubscript::text AS typsubscript"));
        assert!(out.contains("t.typdefaultbin::text AS typdefaultbin"));
        assert!(out.contains("t.typacl::text AS typacl"));
        assert!(!out.contains("t.*"));
        assert!(out.contains("t.oid"));
        // Projection of c.relkind cast to text. Predicate `c.relkind = 'c'`
        // (if present) must stay un-cast.
        assert!(out.contains("c.relkind::text AS relkind"));
    }

    #[test]
    fn dbeaver_query_full_rewrite_preserves_predicate_relkind() {
        let sql = "SELECT t.oid,t.*,c.relkind,format_type(nullif(t.typbasetype, 0), t.typtypmod) as base_type_name, d.description\nFROM pg_catalog.pg_type t\nLEFT OUTER JOIN pg_catalog.pg_type et ON et.oid=t.typelem \nLEFT OUTER JOIN pg_catalog.pg_class c ON c.oid=t.typrelid\nLEFT OUTER JOIN pg_catalog.pg_description d ON t.oid=d.objoid\nWHERE t.typname IS NOT NULL\nAND (c.relkind IS NULL OR c.relkind = 'c') AND (et.typcategory IS NULL OR et.typcategory <> 'C')";
        let out = maybe_rewrite(sql).expect("matches");
        assert!(out.contains("c.relkind::text AS relkind"));
        // Predicate should still read raw c.relkind.
        assert!(out.contains("(c.relkind IS NULL OR c.relkind = 'c')"));
    }

    #[test]
    fn rewrites_pg_namespace_star() {
        let sql = "SELECT n.oid,n.*,d.description FROM pg_catalog.pg_namespace n LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=n.oid AND d.objsubid=0 AND d.classoid='pg_namespace'::regclass ORDER BY nspname";
        let out = maybe_rewrite(sql).expect("matches pg_namespace");
        assert!(out.contains("n.nspacl::text AS nspacl"));
        assert!(!out.contains("n.*"));
        assert!(out.contains("ORDER BY nspname"));
    }

    #[test]
    fn skips_non_pg_type_queries() {
        assert_eq!(maybe_rewrite("SELECT * FROM users"), None);
        // pg_type without `t` alias: not our pattern
        assert_eq!(
            maybe_rewrite("SELECT typname FROM pg_catalog.pg_type"),
            None
        );
    }

    #[test]
    fn skips_pg_type_without_star() {
        assert_eq!(
            maybe_rewrite("SELECT t.oid FROM pg_catalog.pg_type t"),
            None
        );
    }
}
