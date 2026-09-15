mod cli;
mod config;
mod pipeline;
mod report;
mod stages;

use std::fs;

use anyhow::{Context as _, Result};
use clap::Parser;
use lopdf::Document;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::report::Report;

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let config = cli.config();
    let output = cli.output_path();
    tracing::debug!(?config, "effective configuration");

    let input_bytes = fs::metadata(&cli.input)
        .with_context(|| format!("reading {}", cli.input.display()))?
        .len() as usize;
    let mut doc =
        Document::load(&cli.input).with_context(|| format!("parsing {}", cli.input.display()))?;

    let mut report = Report::new(input_bytes);
    pipeline::run(&mut doc, &config, &mut report)?;

    let mut buf = Vec::with_capacity(input_bytes);
    doc.save_modern(&mut buf).context("serializing output")?;
    report.output_bytes = buf.len();

    if cli.dry_run {
        print!("{report}");
        return Ok(());
    }

    fs::write(&output, &buf).with_context(|| format!("writing {}", output.display()))?;
    print!("{report}");
    println!("wrote {}", output.display());
    Ok(())
}

fn init_logging(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
