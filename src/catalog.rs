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
    let mut out = sql.to_string();
    let mut changed = false;

    if let Some(r) = rewrite_pg_type_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = rewrite_pg_namespace_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = rewrite_pg_class_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = rewrite_pg_collation_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = rewrite_pg_attribute_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = rewrite_pg_constraint_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = rewrite_pg_index_star(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = patch_pg_depend_projection(&out) {
        out = r;
        changed = true;
    }
    if let Some(r) = cast_oid_placeholders(&out) {
        out = r;
        changed = true;
    }

    if changed { Some(out) } else { None }
}

/// We pass every Extended Query parameter as `stringValue` (text), which
/// breaks pg comparisons against `oid` columns: `oid = text` has no
/// implicit operator. For known oid columns (qualified or bare), append a
/// `::oid` cast to a placeholder reference of the form `<col> = :pN` /
/// `<col> = $N`. Conservative — only the columns we've seen GUI clients
/// use are listed.
fn cast_oid_placeholders(sql: &str) -> Option<String> {
    static RE: Lazy<Regex> = Lazy::new(|| {
        // Match `<alias>.<col> = :pN` or `<col> = :pN` for a small list of
        // canonical oid-typed catalog columns.
        Regex::new(
            r"(?i)((?:\w+\.)?(?:oid|relnamespace|relfilenode|reltype|reltoastrelid|relowner|relam|reloftype|relrewrite|typrelid|typelem|typarray|typbasetype|typnamespace|typowner|typsubscript|attrelid|atttypid|attcollation|conrelid|contypid|conindid|conparentid|confrelid|indexrelid|indrelid|conkey|confkey|inhparent|inhrelid|nspowner|adrelid|adnum|enumtypid|pronamespace|proowner|prolang|provariadic|proargdefaults|protrftypes|prorettype|proallargtypes|proargtypes|proargmodes|proargnames|prosrc|probin|proconfig|amhandler|opfmethod|opfnamespace|opfowner|opcmethod|opcnamespace|opcowner|opcfamily|opcintype|opckeytype|amopfamily|amoplefttype|amoprighttype|amopopr|amopmethod|amopsortfamily|amprocfamily|amproclefttype|amprocrighttype|amproc|conpfeqop|conppeqop|conffeqop|partrelid|partclass|partcollation|partexprs|classid|objid|refclassid|refobjid|tgrelid|tgfoid|ev_class|ev_qual|ev_action|tgconstrrelid|tgconstrindid|tgconstraint|evtowner)\s*(?:=|<>|!=|<=|>=|<|>)\s*):(p\d+)\b",
        )
        .unwrap()
    });

    if !RE.is_match(sql) {
        return None;
    }
    Some(RE.replace_all(sql, "$1:$2::oid").into_owned())
}

/// `SELECT c.oid, c.*, ... FROM pg_catalog.pg_class c ...` — `c.*` exposes
/// `relkind` (char(1)), `relpersistence`, `relreplident`, `relkind` (char) and
/// `relacl` (aclitem[]), all of which Aurora's Data API refuses. Replace
/// `c.*` with an explicit column list that casts the offenders to text.
fn rewrite_pg_class_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_class\s+(?:AS\s+)?c\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bc\.\*").unwrap());

    if !FROM_RE.is_match(sql) || !STAR_RE.is_match(sql) {
        return None;
    }
    // Columns from pg_class (PG14+). char(1) / aclitem[] cast to text.
    // pg_node_tree (relpartbound) cannot be cast to text safely — Aurora
    // returns the internal node-tree representation which contains NUL
    // bytes that the Data API rejects as invalid UTF-8. Substitute with
    // pg_get_expr(...) so we still get a printable value or NULL.
    //
    // We also strip d.description and the trailing pg_get_partkeydef call
    // because those occasionally yield strings containing 0x00 (the Data
    // API only allows valid UTF-8 with no NULs). DBeaver tolerates NULL
    // in those positions.
    // relacl and reloptions occasionally contain bytes the Data API
    // refuses (we've seen 'invalid byte sequence for encoding UTF8: 0x00'
    // even after ::text casts). Substitute NULL — DBeaver tolerates it.
    // relpartbound also: pg_get_expr over a partition bound can produce
    // strings the Data API rejects with the same 0x00 error. NULL it too.
    let cols = "\
c.relname, c.relnamespace, c.reltype, c.reloftype, c.relowner, c.relam, \
c.relfilenode, c.reltablespace, c.relpages, c.reltuples, c.relallvisible, \
c.reltoastrelid, c.relhasindex, c.relisshared, \
c.relpersistence::text AS relpersistence, \
c.relkind::text AS relkind, \
c.relnatts, c.relchecks, c.relhasrules, c.relhastriggers, c.relhassubclass, \
c.relrowsecurity, c.relforcerowsecurity, c.relispopulated, \
c.relreplident::text AS relreplident, \
c.relispartition, c.relrewrite, \
c.relfrozenxid::text AS relfrozenxid, \
c.relminmxid::text AS relminmxid, \
NULL::text AS relacl, \
NULL::text AS reloptions, \
NULL::text AS relpartbound";
    let mut out = STAR_RE.replace(sql, cols).into_owned();

    // DBeaver also projects d.description and pg_get_partkeydef(c.oid).
    // Both have produced rows with embedded 0x00 in our environment which
    // the Data API rejects with "invalid byte sequence for encoding UTF8".
    // Replace each with NULL so the projection shape is unchanged.
    static D_DESC: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bd\.description\b").unwrap());
    static PARTKEYDEF: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?i)pg_catalog\.pg_get_partkeydef\s*\(\s*c\.oid\s*\)\s*as\s+partition_key",
        )
        .unwrap()
    });
    static PARTEXPR: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?i)pg_catalog\.pg_get_expr\s*\(\s*c\.relpartbound\s*,\s*c\.oid\s*\)\s*as\s+partition_expr",
        )
        .unwrap()
    });
    out = D_DESC.replace_all(&out, "NULL").into_owned();
    out = PARTKEYDEF
        .replace_all(&out, "NULL AS partition_key")
        .into_owned();
    out = PARTEXPR
        .replace_all(&out, "NULL AS partition_expr")
        .into_owned();

    Some(out)
}

/// `SELECT c.* FROM pg_catalog.pg_constraint c ...` — `c.*` exposes
/// `contype`, `confupdtype`, `confdeltype`, `confmatchtype` (all char(1))
/// plus int2/int4 array side-cars (conkey, confkey, conpfeqop, ...) which
/// Aurora may also choke on when transmitted as binary. Cast char(1) cols
/// to text. Leave the int arrays alone — they pass through fine.
fn rewrite_pg_constraint_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_constraint\s+(?:AS\s+)?c\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bc\.\*").unwrap());

    if !FROM_RE.is_match(sql) || !STAR_RE.is_match(sql) {
        return None;
    }
    let cols = "\
c.conname, c.connamespace, \
c.contype::text AS contype, \
c.condeferrable, c.condeferred, c.convalidated, \
c.conrelid, c.contypid, c.conindid, c.conparentid, \
c.confrelid, \
c.confupdtype::text AS confupdtype, \
c.confdeltype::text AS confdeltype, \
c.confmatchtype::text AS confmatchtype, \
c.conislocal, c.coninhcount, c.connoinherit, \
c.conkey::text AS conkey, \
c.confkey::text AS confkey, \
c.conpfeqop::text AS conpfeqop, \
c.conppeqop::text AS conppeqop, \
c.conffeqop::text AS conffeqop, \
c.confdelsetcols::text AS confdelsetcols, \
c.conexclop::text AS conexclop, \
NULL::text AS conbin";
    Some(STAR_RE.replace(sql, cols).into_owned())
}

/// `SELECT i.* FROM pg_catalog.pg_index i ...` — `i.*` exposes
/// `int2vector` columns (`indkey`) and `oidvector` (`indcollation`,
/// `indclass`, `indoption`). Aurora rejects int2vector outright. Cast each
/// to text so the column shape stays usable.
fn rewrite_pg_index_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_index\s+(?:AS\s+)?i\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bi\.\*").unwrap());

    if !FROM_RE.is_match(sql) || !STAR_RE.is_match(sql) {
        return None;
    }
    let cols = "\
i.indexrelid, i.indrelid, i.indnatts, i.indnkeyatts, \
i.indisunique, i.indnullsnotdistinct, i.indisprimary, i.indisexclusion, \
i.indimmediate, i.indisclustered, i.indisvalid, i.indcheckxmin, \
i.indisready, i.indislive, i.indisreplident, \
i.indkey::text AS indkey, \
i.indcollation::text AS indcollation, \
i.indclass::text AS indclass, \
i.indoption::text AS indoption, \
NULL::text AS indexprs, NULL::text AS indpred";
    let mut out = STAR_RE.replace(sql, cols).into_owned();

    // DBeaver also projects `i.indkey as keys` (raw int2vector) alongside
    // the star expansion. Cast that copy too — same Aurora rejection.
    static KEYS_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bi\.indkey\s+as\s+keys\b").unwrap()
    });
    out = KEYS_RE
        .replace_all(&out, "i.indkey::text AS keys")
        .into_owned();

    Some(out)
}

/// DBeaver's pg_depend probe selects `dep.deptype` (char(1)) and `cl.relkind`
/// raw at the head of the projection. Aurora rejects both. Patch the exact
/// upstream projection prefix to cast each to text — leaves the rest of the
/// query untouched.
fn patch_pg_depend_projection(sql: &str) -> Option<String> {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)dep\.deptype, dep\.classid, dep\.objid, cl\.relkind,").unwrap()
    });
    if !RE.is_match(sql) {
        return None;
    }
    Some(
        RE.replace(
            sql,
            "dep.deptype::text AS deptype, dep.classid, dep.objid, cl.relkind::text AS relkind,",
        )
        .into_owned(),
    )
}

/// `SELECT c.oid,c.* FROM pg_catalog.pg_collation c` — `c.*` exposes
/// `collprovider` (char(1)) which Aurora refuses. Cast it; rest pass through.
fn rewrite_pg_collation_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_collation\s+(?:AS\s+)?c\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bc\.\*").unwrap());

    if !FROM_RE.is_match(sql) || !STAR_RE.is_match(sql) {
        return None;
    }
    // pg_collation columns. collprovider char(1) → text.
    // colliculocale (PG15+) and collicurules (PG16+) are version-gated;
    // omit so the projection works against PG14/15 Auroras. DBeaver doesn't
    // require either field for the basic collation list.
    let cols = "\
c.collname, c.collnamespace, c.collowner, \
c.collprovider::text AS collprovider, \
c.collisdeterministic, c.collencoding, c.collcollate, c.collctype, \
c.collversion";
    Some(STAR_RE.replace(sql, cols).into_owned())
}

/// DBeaver column-listing query selects `a.*` from `pg_attribute a`.
/// `attidentity`, `attgenerated`, `attstorage`, `attalign` are char(1) — refused
/// by Aurora Data API. `attacl` is aclitem[]. Cast char(1) to text and NULL
/// out attacl. Other columns left untouched.
fn rewrite_pg_attribute_star(sql: &str) -> Option<String> {
    static FROM_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\bFROM\s+(?:pg_catalog\.)?pg_attribute\s+(?:AS\s+)?a\b").unwrap()
    });
    static STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\ba\.\*").unwrap());

    if !FROM_RE.is_match(sql) || !STAR_RE.is_match(sql) {
        return None;
    }
    // pg_attribute columns (PG14+). Char(1)/aclitem[] cast or NULLed.
    // attmissingval is anyarray (also rejected); NULL it.
    let cols = "\
a.attrelid, a.attname, a.atttypid, a.attstattarget, a.attlen, a.attnum, \
a.attndims, a.attcacheoff, a.atttypmod, a.attbyval, \
a.attalign::text AS attalign, \
a.attstorage::text AS attstorage, \
a.attcompression::text AS attcompression, \
a.attnotnull, a.atthasdef, a.atthasmissing, \
a.attidentity::text AS attidentity, \
a.attgenerated::text AS attgenerated, \
a.attisdropped, a.attislocal, a.attinhcount, a.attcollation, \
NULL::text AS attacl, NULL::text AS attoptions, NULL::text AS attfdwoptions, \
NULL::text AS attmissingval";
    Some(STAR_RE.replace(sql, cols).into_owned())
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
    //   pg_node_tree:  typdefaultbin — use NULL::text instead (raw cast yields
    //                  NUL bytes that fail UTF-8 validation on the wire)
    //   aclitem[]:     typacl
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
t.typndims, t.typcollation, NULL::text AS typdefaultbin, \
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
        assert!(out.contains("NULL::text AS typdefaultbin"));
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
    fn casts_oid_placeholder_after_relnamespace() {
        let sql = "SELECT c.oid FROM pg_class c WHERE c.relnamespace=:p1 AND c.relkind not in ('i')";
        let out = maybe_rewrite(sql).expect("matches oid placeholder");
        assert!(out.contains("c.relnamespace=:p1::oid"));
    }

    #[test]
    fn casts_oid_placeholder_for_bare_oid_column() {
        let sql = "SELECT 1 FROM pg_class c WHERE c.oid=:p1";
        let out = maybe_rewrite(sql).expect("matches");
        assert!(out.contains("c.oid=:p1::oid"));
    }

    #[test]
    fn casts_oid_placeholder_with_spaces() {
        let sql = "SELECT 1 FROM pg_class WHERE relnamespace = :p2";
        let out = maybe_rewrite(sql).expect("matches");
        assert!(out.contains("relnamespace = :p2::oid"));
    }

    #[test]
    fn rewrites_pg_class_star() {
        let sql = "SELECT c.oid,c.*,d.description,pg_catalog.pg_get_expr(c.relpartbound, c.oid) as partition_expr,  pg_catalog.pg_get_partkeydef(c.oid) as partition_key FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_description d ON d.objoid=c.oid WHERE c.relnamespace=:p1 AND c.relkind not in ('i','c')";
        let out = maybe_rewrite(sql).expect("matches pg_class");
        assert!(out.contains("c.relkind::text AS relkind"));
        assert!(out.contains("NULL::text AS relacl"));
        assert!(out.contains("NULL::text AS relpartbound"));
        assert!(out.contains("c.relnamespace=:p1::oid"));
        assert!(!out.contains("c.*"));
        // Description and partition projections elided to dodge UTF-8 NUL.
        assert!(!out.contains("d.description"));
        assert!(out.contains("NULL AS partition_key"));
        assert!(out.contains("NULL AS partition_expr"));
    }

    #[test]
    fn rewrites_pg_constraint_star() {
        let sql = "SELECT c.oid,c.* FROM pg_catalog.pg_constraint c WHERE c.conrelid=:p1";
        let out = maybe_rewrite(sql).expect("matches pg_constraint");
        assert!(out.contains("c.contype::text AS contype"));
        assert!(out.contains("c.confupdtype::text AS confupdtype"));
        assert!(out.contains("c.confmatchtype::text AS confmatchtype"));
        assert!(out.contains("c.conpfeqop::text AS conpfeqop"));
        assert!(out.contains("c.conexclop::text AS conexclop"));
        assert!(out.contains("c.conrelid=:p1::oid"));
        assert!(!out.contains("c.*"));
    }

    #[test]
    fn rewrites_pg_index_star() {
        let sql = "SELECT i.*,c.relname FROM pg_catalog.pg_index i INNER JOIN pg_catalog.pg_class c ON c.oid=i.indexrelid WHERE i.indrelid=:p1";
        let out = maybe_rewrite(sql).expect("matches pg_index");
        assert!(out.contains("i.indkey::text AS indkey"));
        assert!(out.contains("i.indcollation::text AS indcollation"));
        assert!(out.contains("i.indrelid=:p1::oid"));
        assert!(!out.contains("i.*"));
    }

    #[test]
    fn rewrites_pg_index_keys_alias() {
        let sql = "SELECT i.*,i.indkey as keys,c.relname FROM pg_catalog.pg_index i INNER JOIN pg_catalog.pg_class c ON c.oid=i.indexrelid WHERE i.indrelid=:p1";
        let out = maybe_rewrite(sql).expect("matches pg_index keys");
        assert!(out.contains("i.indkey::text AS keys"));
        // No raw int2vector reference remains.
        assert!(!out.contains("i.indkey as keys"));
        assert!(!out.contains("i.indkey AS keys"));
    }

    #[test]
    fn patches_pg_depend_deptype() {
        let sql = "SELECT DISTINCT dep.deptype, dep.classid, dep.objid, cl.relkind, attr.attname FROM pg_depend dep WHERE dep.refobjid=:p1";
        let out = maybe_rewrite(sql).expect("matches pg_depend");
        assert!(out.contains("dep.deptype::text AS deptype"));
        assert!(out.contains("cl.relkind::text AS relkind,"));
        assert!(out.contains("dep.refobjid=:p1::oid"));
    }

    #[test]
    fn casts_oid_placeholder_for_tgrelid() {
        let sql = "SELECT 1 FROM pg_trigger x WHERE x.tgrelid = :p1";
        let out = maybe_rewrite(sql).expect("matches");
        assert!(out.contains("x.tgrelid = :p1::oid"));
    }

    #[test]
    fn rewrites_pg_collation_star() {
        let sql = "SELECT c.oid,c.* FROM pg_catalog.pg_collation c ORDER BY c.oid";
        let out = maybe_rewrite(sql).expect("matches pg_collation");
        assert!(out.contains("c.collprovider::text AS collprovider"));
        assert!(!out.contains("c.*"));
        assert!(out.contains("ORDER BY c.oid"));
    }

    #[test]
    fn rewrites_pg_attribute_star() {
        let sql = "SELECT c.relname,a.*,pg_catalog.pg_get_expr(ad.adbin, ad.adrelid, true) as def_value FROM pg_catalog.pg_attribute a INNER JOIN pg_catalog.pg_class c ON (a.attrelid=c.oid) WHERE c.oid=:p1";
        let out = maybe_rewrite(sql).expect("matches pg_attribute");
        assert!(out.contains("a.attidentity::text AS attidentity"));
        assert!(out.contains("a.attgenerated::text AS attgenerated"));
        assert!(out.contains("a.attstorage::text AS attstorage"));
        assert!(out.contains("a.attalign::text AS attalign"));
        assert!(out.contains("NULL::text AS attacl"));
        assert!(out.contains("NULL::text AS attmissingval"));
        assert!(!out.contains("a.*"));
        assert!(out.contains("c.oid=:p1::oid"));
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
