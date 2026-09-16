mod cli;

use std::fs;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use lopdf::Document;
use tracing_subscriber::EnvFilter;

use compress_pdf::{pipeline, report::Report, verify};

use crate::cli::{Cli, VerifyArg};

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let config = cli.config();
    let output = cli.output_path();
    tracing::debug!(?config, "effective configuration");

    let input = fs::read(&cli.input).with_context(|| format!("reading {}", cli.input.display()))?;
    let mut doc =
        Document::load_mem(&input).with_context(|| format!("parsing {}", cli.input.display()))?;

    let pages_in = doc.get_pages().len();
    let mut report = Report::new(input.len());
    pipeline::run(&mut doc, &config, &mut report)?;
    let buf = pipeline::serialize(&mut doc, &input, &mut report)?;

    // Nothing to verify when the output is the input unchanged.
    if buf != input {
        let verification = verify::verify(&buf, pages_in);
        report.note(verification.to_string());
        if !verification.is_ok() {
            // Problems the input already had are warnings; new ones are bugs.
            let baseline = verify::verify(&input, pages_in);
            let regressions = verification.regressions_from(&baseline);
            if regressions.is_empty() {
                report.note("warning: the input already had these problems; output written anyway");
            } else {
                print!("{report}");
                bail!(
                    "output failed verification with new problems {regressions:?}; nothing written (this is a bug, please report it)"
                );
            }
        }
        if cli.verify == VerifyArg::Render {
            verify_render(&cli, &input, &buf, &mut report)?;
        }
    }

    if cli.dry_run {
        print!("{report}");
        return Ok(());
    }

    fs::write(&output, &buf).with_context(|| format!("writing {}", output.display()))?;
    print!("{report}");
    println!("wrote {}", output.display());
    Ok(())
}

/// The visual level: every page rendered before and after and compared.
/// A page under the preset's floor is a warning, or with `--strict` a
/// failure that leaves nothing written.
fn verify_render(cli: &Cli, input: &[u8], output: &[u8], report: &mut Report) -> Result<()> {
    match verify::render::compare(input, output, cli.preset.into()) {
        Ok(comparison) => {
            report.note(comparison.to_string());
            if comparison.below_floor().is_empty() {
                return Ok(());
            }
            if cli.strict {
                print!("{report}");
                bail!("pages below the similarity floor; nothing written (--strict)");
            }
            report.note("warning: pages below the similarity floor; output written anyway");
            Ok(())
        }
        Err(e) => {
            report.note(format!("render: skipped, {e}"));
            Ok(())
        }
    }
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
