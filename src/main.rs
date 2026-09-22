mod cli;

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use lopdf::Document;
use tracing_subscriber::EnvFilter;

use compress_pdf::{config::Config, pipeline, report::Report, verify};

use crate::cli::{Cli, VerifyArg};

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let config = cli.config();
    tracing::debug!(?config, "effective configuration");

    if let [input] = cli.inputs.as_slice() {
        return compress_one(&cli, &config, input);
    }
    // Several inputs: one failure does not stop the rest.
    let mut failed = 0;
    for input in &cli.inputs {
        println!("== {}", input.display());
        if let Err(e) = compress_one(&cli, &config, input) {
            eprintln!("error: {}: {e:#}", input.display());
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} of {} files failed", cli.inputs.len());
    }
    Ok(())
}

fn compress_one(cli: &Cli, config: &Config, input_path: &Path) -> Result<()> {
    let output = cli.output_path(input_path);
    let input =
        fs::read(input_path).with_context(|| format!("reading {}", input_path.display()))?;
    let mut doc =
        Document::load_mem(&input).with_context(|| format!("parsing {}", input_path.display()))?;

    let pages_in = doc.get_pages().len();
    let mut report = Report::new(input.len());
    pipeline::run(&mut doc, config, &mut report)?;
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
            verify_render(cli, &input, &buf, &mut report)?;
        }
    }

    if cli.dry_run {
        print!("{report}");
        return Ok(());
    }

    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
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

/// lopdf logs one warning per item it cannot handle (a filtered inline
/// image, a stream it cannot decode), which on some files means hundreds of
/// thousands of lines; what matters reaches the report as a note, so its
/// warnings are shown one verbosity level later than ours.
fn init_logging(verbosity: u8) {
    let (level, lopdf) = match verbosity {
        0 => ("warn", "error"),
        1 => ("info", "warn"),
        2 => ("debug", "info"),
        _ => ("trace", "trace"),
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},lopdf={lopdf}")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
