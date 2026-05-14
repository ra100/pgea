//! Rewrite pg-style positional parameters (`$1`, `$2`) to RDS Data API named
//! parameters (`:p1`, `:p2`).
//!
//! The rewriter is a small lexer that walks the SQL one byte at a time and
//! switches modes when it sees a string literal, identifier quote, comment,
//! or dollar-quoted block. Inside any of those, a `$N` is left alone. Outside
//! of them, `$<digits>` is rewritten to `:p<digits>`.
//!
//! Why a hand-rolled lexer rather than a regex: pg block comments nest, and
//! dollar-quoted strings use balanced tag pairs (`$foo$ ... $foo$`). Both are
//! awkward in regex; a state machine is more honest about the invariants.

use std::collections::BTreeSet;

/// Result of rewriting a SQL string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    /// The SQL with `$N` placeholders replaced by `:pN`.
    pub sql: String,
    /// The distinct placeholder indices that were rewritten (sorted ascending).
    pub params: Vec<u32>,
}

/// Rewrite `$N` placeholders into `:pN` named parameters.
pub fn rewrite(sql: &str) -> Rewritten {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut params: BTreeSet<u32> = BTreeSet::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // -- single-line comment
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

        // /* block comment */ — pg nests these
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth: u32 = 1;
            out.push_str("/*");
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    out.push_str("/*");
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    out.push_str("*/");
                    i += 2;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
            continue;
        }

        // 'string literal' — pg doubles single quotes to escape (`''`).
        if b == b'\'' {
            out.push('\'');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        out.push_str("''");
                        i += 2;
                        continue;
                    }
                    out.push('\'');
                    i += 1;
                    break;
                }
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // "quoted identifier" — pg doubles double quotes to escape.
        if b == b'"' {
            out.push('"');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        out.push_str("\"\"");
                        i += 2;
                        continue;
                    }
                    out.push('"');
                    i += 1;
                    break;
                }
                out.push(bytes[i] as char);
                i += 1;
            }
            continue;
        }

        // $-prefixed token: either a parameter ($N) or a dollar-quoted block ($tag$ ... $tag$).
        if b == b'$' {
            // Try parameter first: $ followed by one or more digits.
            if matches!(bytes.get(i + 1), Some(c) if c.is_ascii_digit()) {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let n: u32 = sql[i + 1..j].parse().unwrap_or(0);
                if n > 0 {
                    out.push_str(&format!(":p{}", n));
                    params.insert(n);
                    i = j;
                    continue;
                }
            }

            // Try dollar-quoted: $<tag>$ ... $<tag>$ where tag is [A-Za-z_][A-Za-z0-9_]* or empty.
            if let Some((tag_end, tag)) = read_dollar_tag(bytes, i) {
                // Look for the closing `$tag$`.
                let needle = format!("${}$", tag);
                let body_start = tag_end;
                if let Some(rel) = sql[body_start..].find(&needle) {
                    let body_end = body_start + rel;
                    let close_end = body_end + needle.len();
                    out.push_str(&sql[i..close_end]);
                    i = close_end;
                    continue;
                }
                // Unterminated dollar-quote — copy rest verbatim and stop.
                out.push_str(&sql[i..]);
                i = bytes.len();
                continue;
            }

            // Lone `$` not followed by digits or a valid tag: copy as-is.
            out.push('$');
            i += 1;
            continue;
        }

        out.push(b as char);
        i += 1;
    }

    Rewritten {
        sql: out,
        params: params.into_iter().collect(),
    }
}

/// If `bytes[start]` is `$` and the next characters form a valid pg dollar-quote
/// opening tag (`$tag$` where tag is empty or matches `[A-Za-z_][A-Za-z0-9_]*`),
/// returns the byte index immediately after the closing `$` of the opening tag,
/// plus the tag string.
fn read_dollar_tag(bytes: &[u8], start: usize) -> Option<(usize, String)> {
    debug_assert_eq!(bytes.get(start), Some(&b'$'));
    let mut j = start + 1;
    let tag_begin = j;

    // First tag char (if any) must be letter or underscore.
    if let Some(&c) = bytes.get(j) {
        if c == b'$' {
            // Empty tag: $$
            return Some((j + 1, String::new()));
        }
        if !(c.is_ascii_alphabetic() || c == b'_') {
            return None;
        }
        j += 1;
    } else {
        return None;
    }

    while let Some(&c) = bytes.get(j) {
        if c == b'$' {
            let tag = std::str::from_utf8(&bytes[tag_begin..j]).ok()?.to_string();
            return Some((j + 1, tag));
        }
        if !(c.is_ascii_alphanumeric() || c == b'_') {
            return None;
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(s: &str) -> Rewritten {
        rewrite(s)
    }

    #[test]
    fn rewrites_basic_placeholder() {
        let r = rw("SELECT $1, $2 FROM t WHERE id = $1");
        assert_eq!(r.sql, "SELECT :p1, :p2 FROM t WHERE id = :p1");
        assert_eq!(r.params, vec![1, 2]);
    }

    #[test]
    fn handles_no_params() {
        let r = rw("SELECT 1");
        assert_eq!(r.sql, "SELECT 1");
        assert!(r.params.is_empty());
    }

    #[test]
    fn skips_inside_single_quoted_string() {
        let r = rw("SELECT '$1 is not a param', $1");
        assert_eq!(r.sql, "SELECT '$1 is not a param', :p1");
        assert_eq!(r.params, vec![1]);
    }

    #[test]
    fn handles_doubled_single_quote_escape() {
        // In pg, '' inside a string is an escaped single quote.
        let r = rw("SELECT 'it''s $1', $1");
        assert_eq!(r.sql, "SELECT 'it''s $1', :p1");
        assert_eq!(r.params, vec![1]);
    }

    #[test]
    fn skips_inside_quoted_identifier() {
        let r = rw(r#"SELECT "col$1", $1 FROM t"#);
        assert_eq!(r.sql, r#"SELECT "col$1", :p1 FROM t"#);
        assert_eq!(r.params, vec![1]);
    }

    #[test]
    fn skips_inside_line_comment() {
        let r = rw("SELECT $1 -- comment with $2\n, $3");
        assert_eq!(r.sql, "SELECT :p1 -- comment with $2\n, :p3");
        assert_eq!(r.params, vec![1, 3]);
    }

    #[test]
    fn skips_inside_block_comment_with_nesting() {
        let r = rw("SELECT $1 /* outer $2 /* inner $3 */ still outer $4 */ , $5");
        assert_eq!(
            r.sql,
            "SELECT :p1 /* outer $2 /* inner $3 */ still outer $4 */ , :p5"
        );
        assert_eq!(r.params, vec![1, 5]);
    }

    #[test]
    fn skips_inside_dollar_quoted_block() {
        let r = rw("SELECT $tag$ inside $1 outside $tag$, $1");
        assert_eq!(r.sql, "SELECT $tag$ inside $1 outside $tag$, :p1");
        assert_eq!(r.params, vec![1]);
    }

    #[test]
    fn skips_inside_empty_tag_dollar_quote() {
        let r = rw("SELECT $$ inside $7 outside $$, $1");
        assert_eq!(r.sql, "SELECT $$ inside $7 outside $$, :p1");
        assert_eq!(r.params, vec![1]);
    }

    #[test]
    fn dollar_with_no_digits_or_tag_is_passthrough() {
        // A lone `$` followed by something that isn't a digit or valid tag.
        let r = rw("SELECT 'a' || $foo, $1");
        // `$foo` has no closing `$`, so read_dollar_tag returns None, and `$` is
        // copied as-is. The rest of the input proceeds normally.
        assert_eq!(r.sql, "SELECT 'a' || $foo, :p1");
        assert_eq!(r.params, vec![1]);
    }

    #[test]
    fn handles_high_param_numbers() {
        let r = rw("SELECT $10, $11, $100");
        assert_eq!(r.sql, "SELECT :p10, :p11, :p100");
        assert_eq!(r.params, vec![10, 11, 100]);
    }

    #[test]
    fn unterminated_block_comment_is_handled_gracefully() {
        let r = rw("SELECT 1 /* unterminated $1");
        // We bail out at EOF; output is the input unchanged.
        assert_eq!(r.sql, "SELECT 1 /* unterminated $1");
        assert!(r.params.is_empty());
    }

    #[test]
    fn unterminated_string_literal_is_handled_gracefully() {
        let r = rw("SELECT 'oops $1");
        assert_eq!(r.sql, "SELECT 'oops $1");
        assert!(r.params.is_empty());
    }

    #[test]
    fn mixed_real_world_query() {
        let sql = "/* trace */ SELECT id, '$bogus' AS x FROM users WHERE email = $1 AND created > $2 -- $3 trailing";
        let r = rw(sql);
        assert_eq!(
            r.sql,
            "/* trace */ SELECT id, '$bogus' AS x FROM users WHERE email = :p1 AND created > :p2 -- $3 trailing"
        );
        assert_eq!(r.params, vec![1, 2]);
    }
}
