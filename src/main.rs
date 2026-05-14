use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use pg_rds_connector::config::Config;
use pg_rds_connector::pg::server;

#[derive(Debug, Parser)]
#[command(
    name = "pg-rds-connector",
    about = "Local PostgreSQL wire-protocol proxy for AWS RDS Data API",
    version
)]
struct Cli {
    /// Path to TOML config file. Defaults to ~/.config/pg-rds-connector/config.toml.
    #[arg(short, long, env = "PG_RDS_CONNECTOR_CONFIG")]
    config: Option<PathBuf>,

    /// Override the listen address (e.g. 127.0.0.1:5433).
    #[arg(short, long)]
    listen: Option<String>,

    /// Override log level (e.g. info, debug, trace).
    #[arg(long, env = "PG_RDS_CONNECTOR_LOG")]
    log_level: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(cli.log_level.clone().unwrap_or_else(|| "info".to_string()))
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config_path = match cli.config.clone().or_else(default_config_path) {
        Some(path) => path,
        None => {
            error!("could not determine config path; pass --config");
            return ExitCode::from(2);
        }
    };

    let config = match Config::load(&config_path) {
        Ok(mut cfg) => {
            if let Some(listen) = cli.listen.as_ref() {
                cfg.listen = listen.clone();
            }
            cfg
        }
        Err(err) => {
            error!(path = %config_path.display(), %err, "failed to load config");
            return ExitCode::from(1);
        }
    };

    info!(
        path = %config_path.display(),
        listen = %config.listen,
        targets = config.targets.len(),
        "config loaded"
    );

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            error!(%e, "failed to start tokio runtime");
            return ExitCode::from(1);
        }
    };

    let result = runtime.block_on(server::run(Arc::new(config)));

    if let Err(e) = result {
        error!(%e, "server exited with error");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

fn default_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".config/pg-rds-connector/config.toml");
    Some(p)
}
