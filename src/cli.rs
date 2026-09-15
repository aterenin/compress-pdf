//! Command-line surface. Everything here is translated into a [`Config`]
//! before any PDF work happens, so the pipeline never sees clap types.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use compress_pdf::config::{ColorConversion, Config, Dpi, Preset};

#[derive(Debug, Parser)]
#[command(
    name = "compress-pdf",
    version,
    about = "Shrink PDFs by recompressing images, subsetting fonts and stripping dead weight"
)]
pub struct Cli {
    /// Input PDF.
    pub input: PathBuf,

    /// Output PDF. Defaults to `<input>-compressed.pdf` next to the input.
    pub output: Option<PathBuf>,

    /// Preset to start from. Individual flags below override preset values.
    #[arg(short, long, value_enum, default_value_t = PresetArg::Standard)]
    pub preset: PresetArg,

    /// Target resolution after downsampling, applied to all image classes.
    #[arg(long, value_name = "DPI")]
    pub dpi: Option<f32>,

    /// Only downsample images above this resolution. Negative disables downsampling.
    #[arg(long, value_name = "DPI")]
    pub threshold_dpi: Option<f32>,

    /// JPEG quality, 1-100.
    #[arg(long, value_name = "Q", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: Option<u8>,

    /// Convert all color images to grayscale.
    #[arg(long)]
    pub grayscale: bool,

    /// Run the full pipeline and print the report, but do not write the output file.
    #[arg(long)]
    pub dry_run: bool,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace). RUST_LOG overrides.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PresetArg {
    /// Mild: keep resolution up to 400 dpi, JPEG q75, no stripping.
    Less,
    /// Balanced: 150 dpi, JPEG q60, strip non-visual data. The default.
    Standard,
    /// Aggressive: 72 dpi, JPEG q60, force RGB, strip structure tree too.
    More,
}

impl From<PresetArg> for Preset {
    fn from(p: PresetArg) -> Self {
        match p {
            PresetArg::Less => Preset::Less,
            PresetArg::Standard => Preset::Standard,
            PresetArg::More => Preset::More,
        }
    }
}

impl Cli {
    /// Preset plus command-line overrides.
    pub fn config(&self) -> Config {
        let mut cfg = Config::preset(self.preset.into());
        if let Some(dpi) = self.dpi {
            for d in [&mut cfg.bitonal_dpi, &mut cfg.gray_dpi, &mut cfg.color_dpi] {
                d.target = dpi;
            }
        }
        if let Some(t) = self.threshold_dpi {
            for d in [&mut cfg.bitonal_dpi, &mut cfg.gray_dpi, &mut cfg.color_dpi] {
                d.threshold = t;
            }
        }
        if let Some(q) = self.quality {
            cfg.jpeg_quality = q;
        }
        if self.grayscale {
            cfg.color_conversion = ColorConversion::Gray;
        }
        debug_assert!(Dpi::is_sane(&cfg.color_dpi));
        cfg
    }

    pub fn output_path(&self) -> PathBuf {
        self.output.clone().unwrap_or_else(|| {
            let stem = self
                .input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "output".into());
            self.input.with_file_name(format!("{stem}-compressed.pdf"))
        })
    }
}
