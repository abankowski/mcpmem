use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use mcpmem_core::relation_integrity::{audit, repair};

#[derive(Parser)]
#[command(name = "mcpmem-maintenance")]
#[command(about = "Offline maintenance for an mcpmem SQLite database")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect physical relation rows and denormalized relation counters.
    RelationAudit {
        #[arg(long)]
        database: PathBuf,
        #[arg(long, value_enum)]
        format: OutputFormat,
    },
    /// Create a verified backup, then repair duplicate legacy relation rows.
    RelationRepair {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        backup: Option<PathBuf>,
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::RelationAudit {
            database,
            format: OutputFormat::Json,
        } => print_json(audit(&database)?),
        Command::RelationRepair {
            database,
            backup,
            confirm,
        } => print_json(repair(&database, backup.as_deref(), confirm)?),
    }
}

fn print_json(value: impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}
