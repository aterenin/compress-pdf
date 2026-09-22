mod cli;

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use compress_pdf::compress::{Compressed, Verify, compress};
use compress_pdf::config::Config;

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
    let verify = match cli.verify {
        VerifyArg::Render => Verify::Render {
            preset: cli.preset.into(),
            strict: cli.strict,
        },
        VerifyArg::Structural => Verify::Structural,
    };
    let Compressed {
        output: buf,
        report,
    } = match compress(&input, config, verify) {
        Ok(compressed) => compressed,
        Err(rejected) => {
            print!("{}", rejected.report);
            return Err(rejected.reason);
        }
    };

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
