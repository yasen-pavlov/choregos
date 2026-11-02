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
        #[arg(required = true, num_args = 1..)]
        fixture: Vec<PathBuf>,
        #[arg(long, default_value = "choregos.toml")]
        config: PathBuf,
        #[arg(long)]
        since: Option<chrono::DateTime<chrono::Utc>>,
        #[arg(long)]
        until: Option<chrono::DateTime<chrono::Utc>>,
    },
    Extract {
        #[arg(required = true, num_args = 1..)]
        trace: Vec<PathBuf>,
        #[arg(long)]
        since: chrono::DateTime<chrono::Utc>,
        #[arg(long)]
        until: chrono::DateTime<chrono::Utc>,
        #[arg(long = "rename", value_parser = cli::parse_rename)]
        renames: Vec<(String, String)>,
        #[arg(long, value_parser = cli::parse_node)]
        node: Option<String>,
        #[arg(short, long)]
        output: PathBuf,
    },
    Healthz {
        #[arg(long, default_value = "http://127.0.0.1:8095/healthz")]
        url: String,
    },
}
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let writer = if matches!(&cli.command, CliCommand::Run { .. }) {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stdout)
    } else {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr)
    };
    tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "choregos=info".into()),
        )
        .init();
    match execute(cli.command).await {
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
        CliCommand::Replay {
            fixture,
            config,
            since,
            until,
        } => cli::replay_files(&config, &fixture, since, until, &mut std::io::stdout())?,
        CliCommand::Extract {
            trace,
            since,
            until,
            renames,
            node,
            output,
        } => cli::extract_to_path(&trace, since, until, &renames, node.as_deref(), &output)?,
        CliCommand::Healthz { url } => cli::healthz(&url).await?,
    }
    Ok(())
}
