mod cli;

use std::fs;

use anyhow::{Context as _, Result};
use clap::Parser;
use lopdf::Document;
use tracing_subscriber::EnvFilter;

use compress_pdf::{pipeline, report::Report};

use crate::cli::Cli;

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let config = cli.config();
    let output = cli.output_path();
    tracing::debug!(?config, "effective configuration");

    let input = fs::read(&cli.input).with_context(|| format!("reading {}", cli.input.display()))?;
    let mut doc =
        Document::load_mem(&input).with_context(|| format!("parsing {}", cli.input.display()))?;

    let mut report = Report::new(input.len());
    pipeline::run(&mut doc, &config, &mut report)?;
    let buf = pipeline::serialize(&mut doc, &input, &mut report)?;

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
