//! Config file: TOML schema, structural validation, lookup helpers.
//!
//! Intentionally performs no AWS calls. Profile resolution happens lazily
//! when a pg client first connects to a target so that one stale SSO
//! session does not prevent the proxy from serving healthy targets.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("could not parse TOML: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("invalid listen address {addr:?}: {source}")]
    Listen {
        addr: String,
        #[source]
        source: std::net::AddrParseError,
    },

    #[error("target {target:?}: invalid {field} ARN {value:?}")]
    InvalidArn {
        target: String,
        field: &'static str,
        value: String,
    },

    #[error("target {target:?}: missing {field}")]
    MissingField { target: String, field: &'static str },

    #[error("no targets configured")]
    NoTargets,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,

    #[serde(default = "default_log_level")]
    pub log_level: String,

    #[serde(default)]
    pub default_profile: Option<String>,

    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub cluster_arn: String,
    pub secret_arn: String,
    pub database: String,
    pub region: String,
    #[serde(default)]
    pub profile: Option<String>,
    /// When true, the proxy rejects any write statement (DML, DDL,
    /// GRANT/REVOKE, VACUUM/ANALYZE/REINDEX/REFRESH, CALL) against this
    /// target with a clean pg error. Read statements, transaction control,
    /// and harmless session verbs (SET/SHOW/RESET) are still allowed so GUI
    /// clients connect. Defaults to false (writes allowed).
    #[serde(default)]
    pub read_only: bool,
}

fn default_listen() -> String {
    "127.0.0.1:5433".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

static CLUSTER_ARN_RE: Lazy<Regex> = Lazy::new(|| {
    // arn:aws:rds:<region>:<account>:cluster:<name>
    Regex::new(r"^arn:aws:rds:[a-z0-9-]+:\d+:cluster:[A-Za-z0-9_.\-/]+$").unwrap()
});

static SECRET_ARN_RE: Lazy<Regex> = Lazy::new(|| {
    // arn:aws:secretsmanager:<region>:<account>:secret:<name>
    // Secrets Manager allows letters, digits, and /_+=.@!- in a secret name
    // (AWS's own cluster-managed secrets use the `!` form, e.g.
    // `rds!cluster-<uuid>-<suffix>`).
    Regex::new(r"^arn:aws:secretsmanager:[a-z0-9-]+:\d+:secret:[A-Za-z0-9_.\-/+=@!]+$").unwrap()
});

impl Config {
    /// Load and structurally validate a config file. Performs no AWS calls.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse from a TOML string (testing convenience).
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(s)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        // listen must parse as a SocketAddr.
        self.listen
            .parse::<SocketAddr>()
            .map_err(|source| ConfigError::Listen {
                addr: self.listen.clone(),
                source,
            })?;

        if self.targets.is_empty() {
            return Err(ConfigError::NoTargets);
        }

        for (name, target) in &self.targets {
            if target.cluster_arn.is_empty() {
                return Err(ConfigError::MissingField {
                    target: name.clone(),
                    field: "cluster_arn",
                });
            }
            if target.secret_arn.is_empty() {
                return Err(ConfigError::MissingField {
                    target: name.clone(),
                    field: "secret_arn",
                });
            }
            if target.database.is_empty() {
                return Err(ConfigError::MissingField {
                    target: name.clone(),
                    field: "database",
                });
            }
            if target.region.is_empty() {
                return Err(ConfigError::MissingField {
                    target: name.clone(),
                    field: "region",
                });
            }
            if !CLUSTER_ARN_RE.is_match(&target.cluster_arn) {
                return Err(ConfigError::InvalidArn {
                    target: name.clone(),
                    field: "cluster_arn",
                    value: target.cluster_arn.clone(),
                });
            }
            if !SECRET_ARN_RE.is_match(&target.secret_arn) {
                return Err(ConfigError::InvalidArn {
                    target: name.clone(),
                    field: "secret_arn",
                    value: target.secret_arn.clone(),
                });
            }
        }

        Ok(())
    }

    /// Look up a target by name (used for the pg `dbname` field).
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }

    /// Resolve which AWS profile to use for a connection.
    /// Precedence: per-connection override > target.profile > default_profile > None (default chain).
    pub fn resolve_profile(
        &self,
        target: &Target,
        override_profile: Option<&str>,
    ) -> Option<String> {
        override_profile
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| target.profile.clone())
            .or_else(|| self.default_profile.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
listen = "127.0.0.1:5433"
log_level = "debug"
default_profile = "default"

[targets.dev]
cluster_arn = "arn:aws:rds:eu-west-1:123456789012:cluster:dev-analytics"
secret_arn  = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:dev-secret-AbC123"
database    = "analytics"
region      = "eu-west-1"
profile     = "dev-profile"

[targets.prod]
cluster_arn = "arn:aws:rds:eu-west-1:123456789012:cluster:prod-analytics"
secret_arn  = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:prod-secret-XyZ789"
database    = "analytics"
region      = "eu-west-1"
"#;

    #[test]
    fn parses_valid_config() {
        let cfg = Config::parse(VALID_TOML).expect("valid config");
        assert_eq!(cfg.listen, "127.0.0.1:5433");
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.default_profile.as_deref(), Some("default"));
        assert_eq!(cfg.targets.len(), 2);
    }

    #[test]
    fn defaults_apply_when_omitted() {
        let cfg = Config::parse(
            r#"
[targets.dev]
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:foo"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:bar"
database    = "x"
region      = "us-east-1"
"#,
        )
        .unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:5433");
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn rejects_invalid_listen() {
        let err = Config::parse(
            r#"
listen = "not a socket"
[targets.dev]
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:foo"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:bar"
database    = "x"
region      = "us-east-1"
"#,
        )
        .unwrap_err();
        matches!(err, ConfigError::Listen { .. });
    }

    #[test]
    fn rejects_invalid_cluster_arn() {
        let err = Config::parse(
            r#"
[targets.dev]
cluster_arn = "not-an-arn"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:bar"
database    = "x"
region      = "us-east-1"
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::InvalidArn { field, .. } => assert_eq!(field, "cluster_arn"),
            other => panic!("expected InvalidArn, got {other:?}"),
        }
    }

    #[test]
    fn accepts_aws_managed_secret_arn_with_bang() {
        let cfg = Config::parse(
            r#"
[targets.dev]
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:foo"
secret_arn  = "arn:aws:secretsmanager:us-east-1:123456789012:secret:rds!cluster-8faa94cd-70eb-464e-ba98-3e70e07e15a0-sk0gOX"
database    = "x"
region      = "us-east-1"
"#,
        )
        .expect("AWS-managed secret ARNs use `rds!cluster-<uuid>-<suffix>` names");
        assert!(cfg.targets["dev"].secret_arn.contains('!'));
    }

    #[test]
    fn rejects_invalid_secret_arn() {
        let err = Config::parse(
            r#"
[targets.dev]
cluster_arn = "arn:aws:rds:us-east-1:123456789012:cluster:foo"
secret_arn  = "arn:aws:rds:us-east-1:123456789012:cluster:not-a-secret"
database    = "x"
region      = "us-east-1"
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::InvalidArn { field, .. } => assert_eq!(field, "secret_arn"),
            other => panic!("expected InvalidArn, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_targets() {
        let err = Config::parse(r#"listen = "127.0.0.1:5433""#).unwrap_err();
        matches!(err, ConfigError::NoTargets);
    }

    #[test]
    fn resolve_profile_precedence() {
        let cfg = Config::parse(VALID_TOML).unwrap();
        let dev = cfg.target("dev").unwrap();
        let prod = cfg.target("prod").unwrap();

        // Override wins.
        assert_eq!(
            cfg.resolve_profile(dev, Some("override")),
            Some("override".to_string())
        );
        // Empty override falls through.
        assert_eq!(
            cfg.resolve_profile(dev, Some("")),
            Some("dev-profile".to_string())
        );
        // No override, target has profile.
        assert_eq!(
            cfg.resolve_profile(dev, None),
            Some("dev-profile".to_string())
        );
        // No override, target has no profile, falls back to default.
        assert_eq!(cfg.resolve_profile(prod, None), Some("default".to_string()));
    }
}
