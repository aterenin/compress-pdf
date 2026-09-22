//! Command-line surface. Everything here is translated into a [`Config`]
//! before any PDF work happens, so the pipeline never sees clap types.

use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};

use compress_pdf::config::{Codecs, ColorConversion, Config, Dpi, Preset, Strip};

#[derive(Debug, Parser)]
#[command(
    name = "compress-pdf",
    version,
    about = "Shrink PDFs by recompressing images, subsetting fonts and stripping dead weight"
)]
pub struct Cli {
    /// Input PDFs, compressed one after another.
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,

    /// Output file, or output directory when it is one or there are several
    /// inputs. Defaults to `<input>-compressed.pdf` next to each input.
    #[arg(short, long)]
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

    /// Codecs to try for bitonal images: a comma-separated list drawn from
    /// g4, jbig2, source; `none` leaves the class untouched.
    #[arg(long, value_name = "LIST")]
    pub bitonal_codecs: Option<Codecs>,

    /// Codecs to try for gray and color images, from jpeg, flate, source.
    #[arg(long, value_name = "LIST")]
    pub continuous_codecs: Option<Codecs>,

    /// Codecs to try for indexed images, from flate, source.
    #[arg(long, value_name = "LIST")]
    pub indexed_codecs: Option<Codecs>,

    /// Color conversion applied to color images.
    #[arg(long, value_enum, value_name = "TARGET")]
    pub color_conversion: Option<ColorArg>,

    /// Crop images to the part their clip paths leave visible.
    #[arg(long, value_name = "BOOL")]
    pub clip_images: Option<bool>,

    /// Turn gray RGB into gray, two-level gray into bitonal, flat images
    /// into one pixel, and drop opaque soft masks.
    #[arg(long, value_name = "BOOL")]
    pub reduce_color_complexity: Option<bool>,

    /// Subset embedded font programs to the glyphs in use.
    #[arg(long, value_name = "BOOL")]
    pub subset_fonts: Option<bool>,

    /// Merge byte-identical embedded font programs.
    #[arg(long, value_name = "BOOL")]
    pub merge_fonts: Option<bool>,

    /// Drop the programs of embedded standard-14 fonts.
    #[arg(long, value_name = "BOOL")]
    pub unembed_standard_fonts: Option<bool>,

    /// Convert Type 1 font programs to CFF.
    #[arg(long, value_name = "BOOL")]
    pub convert_to_cff: Option<bool>,

    /// Remove unused entries from resource dictionaries.
    #[arg(long, value_name = "BOOL")]
    pub optimize_resources: Option<bool>,

    /// Merge duplicate objects.
    #[arg(long, value_name = "BOOL")]
    pub dedupe: Option<bool>,

    /// Rewrite content streams in canonical form where smaller.
    #[arg(long, value_name = "BOOL")]
    pub rebuild_content_streams: Option<bool>,

    /// Non-visual parts to remove: a comma-separated list drawn from threads,
    /// metadata, piece-info, struct-tree, thumbnails, spider, alternates,
    /// output-intents; `none` keeps everything.
    #[arg(long, value_name = "LIST")]
    pub strip: Option<Strip>,

    /// Run the full pipeline and print the report, but do not write the output file.
    #[arg(long)]
    pub dry_run: bool,

    /// Verification level: `structural` re-parses the output; `render` also
    /// rasterizes every page of input and output and compares them.
    #[arg(long, value_enum, default_value_t = VerifyArg::Structural)]
    pub verify: VerifyArg,

    /// Treat a page below the preset's similarity floor as a failure
    /// (nothing is written) instead of a warning.
    #[arg(long)]
    pub strict: bool,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace). RUST_LOG overrides.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VerifyArg {
    /// Re-parse the output with an independent reader (always on).
    Structural,
    /// Structural, plus rendering and SSIM comparison of every page.
    Render,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorArg {
    /// Leave color spaces alone.
    None,
    /// Convert color images to RGB through ICC profiles.
    Rgb,
}

impl From<ColorArg> for ColorConversion {
    fn from(c: ColorArg) -> Self {
        match c {
            ColorArg::None => ColorConversion::None,
            ColorArg::Rgb => ColorConversion::Rgb,
        }
    }
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

/// Replace `dst` when the flag was given.
fn set<T>(dst: &mut T, flag: Option<T>) {
    if let Some(v) = flag {
        *dst = v;
    }
}

impl Cli {
    /// Preset plus command-line overrides.
    pub fn config(&self) -> Config {
        let mut cfg = Config::preset(self.preset.into());
        for d in [&mut cfg.bitonal_dpi, &mut cfg.gray_dpi, &mut cfg.color_dpi] {
            set(&mut d.target, self.dpi);
            set(&mut d.threshold, self.threshold_dpi);
        }
        set(&mut cfg.jpeg_quality, self.quality);
        set(&mut cfg.bitonal, self.bitonal_codecs);
        set(&mut cfg.continuous, self.continuous_codecs);
        set(&mut cfg.indexed, self.indexed_codecs);
        set(
            &mut cfg.color_conversion,
            self.color_conversion.map(Into::into),
        );
        set(&mut cfg.clip_images, self.clip_images);
        set(
            &mut cfg.reduce_color_complexity,
            self.reduce_color_complexity,
        );
        set(&mut cfg.subset_fonts, self.subset_fonts);
        set(&mut cfg.merge_fonts, self.merge_fonts);
        set(&mut cfg.remove_standard_fonts, self.unembed_standard_fonts);
        set(&mut cfg.convert_to_cff, self.convert_to_cff);
        set(&mut cfg.optimize_resources, self.optimize_resources);
        set(&mut cfg.remove_redundant_objects, self.dedupe);
        set(
            &mut cfg.rebuild_content_streams,
            self.rebuild_content_streams,
        );
        set(&mut cfg.strip, self.strip);
        debug_assert!(Dpi::is_sane(&cfg.color_dpi));
        cfg
    }

    /// Where `input`'s output goes: the named file, or inside the named
    /// directory, or next to the input with `-compressed` added.
    pub fn output_path(&self, input: &Path) -> PathBuf {
        let stem = input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        let name = format!("{stem}-compressed.pdf");
        match &self.output {
            Some(dir) if dir.is_dir() || self.inputs.len() > 1 => dir.join(name),
            Some(file) => file.clone(),
            None => input.with_file_name(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(["compress-pdf", "in.pdf"].iter().chain(args)).expect("parses")
    }

    #[test]
    fn output_paths_follow_the_inputs() {
        let one = Cli::try_parse_from(["compress-pdf", "a/x.pdf"]).unwrap();
        assert_eq!(
            one.output_path(Path::new("a/x.pdf")),
            Path::new("a/x-compressed.pdf")
        );
        let named = Cli::try_parse_from(["compress-pdf", "a/x.pdf", "-o", "y.pdf"]).unwrap();
        assert_eq!(named.output_path(Path::new("a/x.pdf")), Path::new("y.pdf"));
        let many =
            Cli::try_parse_from(["compress-pdf", "a/x.pdf", "b/z.pdf", "-o", "out"]).unwrap();
        assert_eq!(many.inputs.len(), 2);
        assert_eq!(
            many.output_path(Path::new("b/z.pdf")),
            Path::new("out/z-compressed.pdf")
        );
        assert!(
            Cli::try_parse_from(["compress-pdf"]).is_err(),
            "an input is required"
        );
    }

    #[test]
    fn flags_override_preset_fields() {
        let cfg = parse(&[
            "--preset",
            "less",
            "--continuous-codecs",
            "jpeg,flate,source",
            "--indexed-codecs",
            "none",
            "--color-conversion",
            "rgb",
            "--clip-images",
            "true",
            "--subset-fonts",
            "false",
            "--strip",
            "metadata,thumbnails",
            "--dpi",
            "100",
            "--threshold-dpi",
            "120",
        ])
        .config();
        assert_eq!(
            cfg.continuous,
            Codecs::JPEG | Codecs::FLATE | Codecs::SOURCE
        );
        assert_eq!(cfg.indexed, Codecs::NONE);
        assert_eq!(cfg.color_conversion, ColorConversion::Rgb);
        assert!(cfg.clip_images && !cfg.subset_fonts);
        assert_eq!(cfg.strip, Strip::METADATA | Strip::THUMBNAILS);
        assert_eq!(
            (cfg.gray_dpi.target, cfg.gray_dpi.threshold),
            (100.0, 120.0)
        );
        // Untouched fields keep the preset's values.
        assert_eq!(cfg.bitonal, Config::preset(Preset::Less).bitonal);
    }

    #[test]
    fn unknown_names_are_rejected() {
        let err =
            Cli::try_parse_from(["compress-pdf", "in.pdf", "--strip", "threads,metadata,foo"])
                .unwrap_err()
                .to_string();
        assert!(err.contains("unknown name `foo`"), "{err}");
        assert!(
            Cli::try_parse_from(["compress-pdf", "in.pdf", "--bitonal-codecs", "png"]).is_err()
        );
    }
}
