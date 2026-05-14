//! Map RDS Data API `columnMetadata.typeName` values to PostgreSQL type OIDs.
//!
//! All values are sent in pg text format (format code 0), so the OID is used
//! by clients only for display and client-side coercion (sortable numeric
//! columns, date pickers, etc.). Unknown type names fall back to `text`.

/// PostgreSQL type OID. Numeric values are taken from `pg_type.h`.
pub type PgOid = u32;

/// `text` — used as the fallback for any unknown Data API type name.
pub const OID_TEXT: PgOid = 25;

/// Look up a pg OID for a Data API `typeName`. Comparison is case-insensitive
/// because Aurora returns lower-case names today; tolerating mixed case keeps
/// us robust to future changes.
pub fn oid_for_type_name(type_name: &str) -> PgOid {
    let key = type_name.trim().to_ascii_lowercase();
    match key.as_str() {
        "bool" | "boolean" => 16,
        "bytea" => 17,
        "name" => 19,
        "int8" | "bigint" => 20,
        "int2" | "smallint" => 21,
        "int4" | "integer" | "int" => 23,
        "text" => 25,
        "oid" => 26,
        "json" => 114,
        "xml" => 142,
        "float4" | "real" => 700,
        "float8" | "double precision" | "double" => 701,
        "money" => 790,
        "bpchar" | "char" | "character" => 1042,
        "varchar" | "character varying" => 1043,
        "date" => 1082,
        "time" | "time without time zone" => 1083,
        "timestamp" | "timestamp without time zone" => 1114,
        "timestamptz" | "timestamp with time zone" => 1184,
        "interval" => 1186,
        "timetz" | "time with time zone" => 1266,
        "numeric" | "decimal" => 1700,
        "uuid" => 2950,
        "jsonb" => 3802,
        // Common array variants. Aurora reports arrays as either `_int4` (pg style)
        // or `int4[]`; we accept both spellings.
        "_bool" | "bool[]" => 1000,
        "_bytea" | "bytea[]" => 1001,
        "_int2" | "int2[]" | "smallint[]" => 1005,
        "_int4" | "int4[]" | "integer[]" | "int[]" => 1007,
        "_text" | "text[]" => 1009,
        "_int8" | "int8[]" | "bigint[]" => 1016,
        "_float4" | "float4[]" | "real[]" => 1021,
        "_float8" | "float8[]" | "double precision[]" => 1022,
        "_varchar" | "varchar[]" => 1015,
        "_uuid" | "uuid[]" => 2951,
        "_jsonb" | "jsonb[]" => 3807,
        "_numeric" | "numeric[]" | "decimal[]" => 1231,
        "_timestamp" | "timestamp[]" => 1115,
        "_timestamptz" | "timestamptz[]" => 1185,
        "_date" | "date[]" => 1182,
        _ => OID_TEXT,
    }
}

/// Encode a Rust string as a pg array literal element. Quotes the value if it
/// contains characters that would confuse the pg array parser; escapes embedded
/// backslashes and double quotes.
pub fn quote_array_element(s: &str) -> String {
    // pg's rule: quote when the element contains `{`, `}`, `,`, whitespace, `"`, `\`,
    // or is the literal four characters `NULL` (case-insensitive), or is empty.
    let needs_quote = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| {
            matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace()
        });

    if !needs_quote {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Build a pg array literal `{a,b,"with space",NULL}` from typed elements.
/// Each element is `Some(text)` for a value or `None` for SQL NULL.
pub fn format_array_literal(elements: &[Option<String>]) -> String {
    let mut out = String::from("{");
    for (i, el) in elements.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match el {
            None => out.push_str("NULL"),
            Some(s) => out.push_str(&quote_array_element(s)),
        }
    }
    out.push('}');
    out
}

/// Encode bytes as pg `bytea` hex format, e.g. `\x48656c6c6f`.
pub fn encode_bytea_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push('\\');
    out.push('x');
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_scalar_types_map() {
        assert_eq!(oid_for_type_name("bool"), 16);
        assert_eq!(oid_for_type_name("BOOLEAN"), 16);
        assert_eq!(oid_for_type_name("int4"), 23);
        assert_eq!(oid_for_type_name("integer"), 23);
        assert_eq!(oid_for_type_name("int8"), 20);
        assert_eq!(oid_for_type_name("text"), 25);
        assert_eq!(oid_for_type_name("varchar"), 1043);
        assert_eq!(oid_for_type_name("uuid"), 2950);
        assert_eq!(oid_for_type_name("jsonb"), 3802);
        assert_eq!(oid_for_type_name("timestamptz"), 1184);
        assert_eq!(oid_for_type_name("numeric"), 1700);
    }

    #[test]
    fn array_types_map_both_spellings() {
        assert_eq!(oid_for_type_name("_int4"), 1007);
        assert_eq!(oid_for_type_name("int4[]"), 1007);
        assert_eq!(oid_for_type_name("_text"), 1009);
        assert_eq!(oid_for_type_name("uuid[]"), 2951);
    }

    #[test]
    fn unknown_falls_back_to_text() {
        assert_eq!(oid_for_type_name("citext"), OID_TEXT);
        assert_eq!(oid_for_type_name("hstore"), OID_TEXT);
        assert_eq!(oid_for_type_name(""), OID_TEXT);
    }

    #[test]
    fn array_literal_basic() {
        let elements = vec![
            Some("a".to_string()),
            Some("b".to_string()),
            Some("c".to_string()),
        ];
        assert_eq!(format_array_literal(&elements), "{a,b,c}");
    }

    #[test]
    fn array_literal_with_nulls_and_quoting() {
        let elements = vec![
            Some("a".to_string()),
            None,
            Some("with space".to_string()),
            Some("has,comma".to_string()),
            Some(r#"quote"and\back"#.to_string()),
            Some("".to_string()),
            Some("NULL".to_string()),
        ];
        assert_eq!(
            format_array_literal(&elements),
            r#"{a,NULL,"with space","has,comma","quote\"and\\back","","NULL"}"#
        );
    }

    #[test]
    fn bytea_hex_encoding() {
        assert_eq!(encode_bytea_hex(b""), "\\x");
        assert_eq!(encode_bytea_hex(b"\x00\x01\xff"), "\\x0001ff");
        assert_eq!(encode_bytea_hex(b"Hello"), "\\x48656c6c6f");
    }
}
