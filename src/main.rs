use choregos::runtime::cli;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "choregos", version, about = "Home-theater conductor")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}
#[derive(Subcommand)]
enum CliCommand {
    Run {
        #[arg(long, default_value = "choregos.toml")]
        config: PathBuf,
    },
    CheckConfig {
        #[arg(long, default_value = "choregos.toml")]
        config: PathBuf,
    },
    Replay {
        fixture: PathBuf,
        #[arg(long, default_value = "choregos.toml")]
        config: PathBuf,
    },
    Healthz {
        #[arg(long, default_value = "http://127.0.0.1:8095/healthz")]
        url: String,
    },
}
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "choregos=info".into()),
        )
        .init();
    match execute(Cli::parse().command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
async fn execute(command: CliCommand) -> anyhow::Result<()> {
    match command {
        CliCommand::Run { config } => cli::run(config).await?,
        CliCommand::CheckConfig { config } => print!("{}", cli::check_config(&config)?),
        CliCommand::Replay { fixture, config } => {
            cli::replay_file(&config, &fixture, &mut std::io::stdout())?
        }
        CliCommand::Healthz { url } => cli::healthz(&url).await?,
    }
    Ok(())
}
