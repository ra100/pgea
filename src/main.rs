use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use pgea::config::Config;
use pgea::pg::server;

const INSTALL_URL: &str = "https://raw.githubusercontent.com/ra100/pgea/main/install.sh";

#[derive(Debug, Parser)]
#[command(
    name = "pgea",
    about = "Local PostgreSQL wire-protocol proxy for AWS RDS Data API",
    version
)]
struct Cli {
    /// Path to TOML config file. Defaults to ~/.config/pgea/config.toml.
    #[arg(short, long, env = "PG_RDS_CONNECTOR_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Override the listen address (e.g. 127.0.0.1:5433).
    #[arg(short, long, global = true)]
    listen: Option<String>,

    /// Override log level (e.g. info, debug, trace).
    #[arg(long, env = "PG_RDS_CONNECTOR_LOG", global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Update pgea to the latest GitHub release.
    SelfUpdate {
        /// Pin to a specific version tag (e.g. v0.2.0).
        #[arg(long)]
        tag: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(cli.log_level.clone().unwrap_or_else(|| "info".to_string()))
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Some(Commands::SelfUpdate { tag }) = cli.command.as_ref() {
        return run_self_update(tag.as_deref());
    }

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
    p.push(".config/pgea/config.toml");
    Some(p)
}

fn run_self_update(tag: Option<&str>) -> ExitCode {
    let pipeline = match tag {
        Some(t) => format!(
            "curl -fsSL {url} | PGEA_VERSION={tag} bash",
            url = INSTALL_URL,
            tag = shell_escape(t)
        ),
        None => format!("curl -fsSL {url} | bash", url = INSTALL_URL),
    };

    info!("running: {pipeline}");

    let status = Command::new("sh").arg("-c").arg(&pipeline).status();

    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => {
            error!(code = ?s.code(), "self-update failed");
            ExitCode::from(1)
        }
        Err(e) => {
            error!(%e, "failed to spawn installer");
            ExitCode::from(1)
        }
    }
}

fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}
