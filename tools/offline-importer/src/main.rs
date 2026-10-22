use anyhow::Result;
use clap::{Parser, Subcommand};
use pantheon_offline_importer::{
    plan,
    replay::{self, Config},
    seal::Export,
    storage,
};
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    about = "Standalone offline synthetic-only native-memory importer; no actions or real inference"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Plan {
        #[arg(long)]
        export: PathBuf,
    },
    ImportFake {
        #[arg(long)]
        export: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        policy: Option<PathBuf>,
        #[arg(long, hide = true)]
        stop_after: Option<u64>,
    },
    Verify {
        #[arg(long)]
        output: PathBuf,
    },
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Plan { export } => {
            let c = Config::default();
            let e = Export::load(&export, c.max_source_bytes, c.max_records)?;
            let p = plan::build(&e)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"records":e.seal.count,"pages":p.spans.last().map(|s|s.end),"repairs":p.repairs,"iterations":p.iterations})
                )?
            );
        }
        Command::ImportFake {
            export,
            output,
            policy,
            stop_after,
        } => {
            let c = if let Some(p) = policy {
                storage::read_json(&p)?
            } else {
                Config::default()
            };
            if let Some(r) = replay::run(&export, &output, c, stop_after)? {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("Stopped at coherent checkpoint; no ready output published.");
            }
        }
        Command::Verify { output } => {
            let r = replay::validate_ready(&output)?;
            println!(
                "verified {} originals, {} native pages; FAKE summaries only",
                r.records, r.pages
            );
        }
    }
    Ok(())
}
