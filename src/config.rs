//! Optimization settings expressed as plain data.
//!
//! Every knob is a field on [`Config`] so that presets are values rather than
//! code paths. See CLAUDE.md, section "Presets".

use std::fmt;
use std::ops::BitOr;

/// Set of codecs a stage may *try* for an image class. The image stage encodes
/// with every member and keeps the smallest result. `SOURCE` means "the
/// original bytes are also a candidate", which is what makes the pipeline
/// safe: with `SOURCE` present an image can never grow.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Codecs(u16);

impl Codecs {
    pub const NONE: Codecs = Codecs(0);
    /// DCT (lossy), continuous-tone images.
    pub const JPEG: Codecs = Codecs(1 << 0);
    /// Flate with per-image predictor choice (lossless).
    pub const FLATE: Codecs = Codecs(1 << 1);
    /// CCITT Group 4, bitonal images.
    pub const G4: Codecs = Codecs(1 << 2);
    /// JBIG2 symbol mode with shared globals, bitonal images.
    pub const JBIG2: Codecs = Codecs(1 << 3);
    /// The original bytes as a candidate.
    pub const SOURCE: Codecs = Codecs(1 << 4);

    pub const fn contains(self, other: Codecs) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const NAMES: [(Codecs, &'static str); 5] = [
        (Codecs::JPEG, "jpeg"),
        (Codecs::FLATE, "flate"),
        (Codecs::G4, "g4"),
        (Codecs::JBIG2, "jbig2"),
        (Codecs::SOURCE, "source"),
    ];
}

impl BitOr for Codecs {
    type Output = Codecs;
    fn bitor(self, rhs: Codecs) -> Codecs {
        Codecs(self.0 | rhs.0)
    }
}

impl fmt::Debug for Codecs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return write!(f, "none");
        }
        let names: Vec<&str> = Codecs::NAMES
            .iter()
            .filter(|(c, _)| self.contains(*c))
            .map(|(_, n)| *n)
            .collect();
        write!(f, "{}", names.join("|"))
    }
}

/// Non-visual document parts that may be removed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Strip(u16);

impl Strip {
    pub const NONE: Strip = Strip(0);
    pub const THREADS: Strip = Strip(1 << 0);
    pub const METADATA: Strip = Strip(1 << 1);
    pub const PIECE_INFO: Strip = Strip(1 << 2);
    pub const STRUCT_TREE: Strip = Strip(1 << 3);
    pub const THUMBNAILS: Strip = Strip(1 << 4);
    pub const SPIDER: Strip = Strip(1 << 5);
    pub const ALTERNATES: Strip = Strip(1 << 6);
    pub const OUTPUT_INTENTS: Strip = Strip(1 << 7);

    pub const fn contains(self, other: Strip) -> bool {
        self.0 & other.0 == other.0
    }

    const NAMES: [(Strip, &'static str); 8] = [
        (Strip::THREADS, "threads"),
        (Strip::METADATA, "metadata"),
        (Strip::PIECE_INFO, "piece-info"),
        (Strip::STRUCT_TREE, "struct-tree"),
        (Strip::THUMBNAILS, "thumbnails"),
        (Strip::SPIDER, "spider"),
        (Strip::ALTERNATES, "alternates"),
        (Strip::OUTPUT_INTENTS, "output-intents"),
    ];
}

impl BitOr for Strip {
    type Output = Strip;
    fn bitor(self, rhs: Strip) -> Strip {
        Strip(self.0 | rhs.0)
    }
}

impl fmt::Debug for Strip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return write!(f, "none");
        }
        let names: Vec<&str> = Strip::NAMES
            .iter()
            .filter(|(s, _)| self.contains(*s))
            .map(|(_, n)| *n)
            .collect();
        write!(f, "{}", names.join("|"))
    }
}

/// Target color space for images. Only what a preset uses exists here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorConversion {
    None,
    /// Convert every color image to RGB through ICC profiles.
    Rgb,
}

/// Downsampling rule for one image class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dpi {
    /// Resolution to resample to.
    pub target: f32,
    /// Only images whose effective resolution exceeds this are touched.
    /// Negative disables downsampling for the class.
    pub threshold: f32,
}

impl Dpi {
    pub const fn new(target: f32, threshold: f32) -> Dpi {
        Dpi { target, threshold }
    }

    pub const fn disabled(target: f32) -> Dpi {
        Dpi {
            target,
            threshold: -1.0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.threshold >= 0.0
    }

    pub fn is_sane(&self) -> bool {
        self.target > 0.0 && (!self.enabled() || self.threshold >= self.target)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // Image classes and the codecs to try for each.
    pub bitonal: Codecs,
    pub continuous: Codecs,
    pub indexed: Codecs,

    pub bitonal_dpi: Dpi,
    pub gray_dpi: Dpi,
    pub color_dpi: Dpi,

    /// 1-100, JPEG quality index.
    pub jpeg_quality: u8,
    pub color_conversion: ColorConversion,
    /// Crop image pixels that lie outside their clip path before recompressing.
    pub clip_images: bool,
    /// RGB-that-is-gray to gray, two-level gray to bitonal, flat images to 1x1,
    /// opaque soft masks removed.
    pub reduce_color_complexity: bool,

    // Fonts.
    pub subset_fonts: bool,
    pub merge_fonts: bool,
    pub remove_standard_fonts: bool,
    pub convert_to_cff: bool,

    // Structure.
    pub optimize_resources: bool,
    pub remove_redundant_objects: bool,
    pub strip: Strip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Less,
    Standard,
    More,
}

impl Config {
    /// Engine defaults before any profile is applied: nothing is processed.
    fn baseline() -> Config {
        Config {
            bitonal: Codecs::NONE,
            continuous: Codecs::NONE,
            indexed: Codecs::NONE,
            bitonal_dpi: Dpi::disabled(200.0),
            gray_dpi: Dpi::disabled(150.0),
            color_dpi: Dpi::disabled(150.0),
            jpeg_quality: 75,
            color_conversion: ColorConversion::None,
            clip_images: false,
            reduce_color_complexity: false,
            subset_fonts: false,
            merge_fonts: false,
            remove_standard_fonts: false,
            convert_to_cff: false,
            optimize_resources: false,
            remove_redundant_objects: false,
            strip: Strip::NONE,
        }
    }

    /// Shared base for the `Standard` and `More` presets: lossless passes
    /// on, image recompression on, no downsampling until a preset sets DPI.
    fn heavy_base() -> Config {
        Config {
            bitonal: Codecs::G4 | Codecs::SOURCE,
            continuous: Codecs::JPEG | Codecs::FLATE | Codecs::SOURCE,
            indexed: Codecs::FLATE | Codecs::SOURCE,
            jpeg_quality: 80,
            clip_images: true,
            reduce_color_complexity: true,
            subset_fonts: true,
            merge_fonts: true,
            convert_to_cff: true,
            optimize_resources: true,
            remove_redundant_objects: true,
            strip: Strip::THREADS
                | Strip::PIECE_INFO
                | Strip::STRUCT_TREE
                | Strip::THUMBNAILS
                | Strip::SPIDER,
            ..Config::baseline()
        }
    }

    pub fn preset(p: Preset) -> Config {
        match p {
            Preset::Less => Config {
                bitonal: Codecs::JBIG2 | Codecs::SOURCE,
                continuous: Codecs::JPEG | Codecs::SOURCE,
                indexed: Codecs::NONE,
                bitonal_dpi: Dpi::new(200.0, 400.0),
                gray_dpi: Dpi::new(200.0, 400.0),
                color_dpi: Dpi::new(200.0, 400.0),
                jpeg_quality: 75,
                optimize_resources: true,
                remove_redundant_objects: true,
                remove_standard_fonts: true,
                subset_fonts: true,
                ..Config::baseline()
            },
            Preset::Standard => Config {
                bitonal_dpi: Dpi::new(150.0, 150.0),
                gray_dpi: Dpi::new(150.0, 150.0),
                color_dpi: Dpi::new(150.0, 150.0),
                jpeg_quality: 60,
                strip: Strip::THREADS
                    | Strip::METADATA
                    | Strip::PIECE_INFO
                    | Strip::THUMBNAILS
                    | Strip::SPIDER
                    | Strip::ALTERNATES
                    | Strip::OUTPUT_INTENTS,
                ..Config::heavy_base()
            },
            Preset::More => Config {
                bitonal: Codecs::JBIG2 | Codecs::SOURCE,
                continuous: Codecs::JPEG | Codecs::SOURCE,
                bitonal_dpi: Dpi::new(72.0, 110.0),
                gray_dpi: Dpi::new(72.0, 110.0),
                color_dpi: Dpi::new(72.0, 110.0),
                jpeg_quality: 60,
                color_conversion: ColorConversion::Rgb,
                remove_standard_fonts: true,
                strip: Strip::THREADS
                    | Strip::METADATA
                    | Strip::PIECE_INFO
                    | Strip::STRUCT_TREE
                    | Strip::THUMBNAILS
                    | Strip::SPIDER
                    | Strip::ALTERNATES,
                ..Config::heavy_base()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_sane() {
        for p in [Preset::Less, Preset::Standard, Preset::More] {
            let c = Config::preset(p);
            assert!(c.bitonal_dpi.is_sane(), "{p:?} bitonal dpi");
            assert!(c.gray_dpi.is_sane(), "{p:?} gray dpi");
            assert!(c.color_dpi.is_sane(), "{p:?} color dpi");
            assert!((1..=100).contains(&c.jpeg_quality));
            // Every preset must keep the never-grow guarantee.
            assert!(c.bitonal.is_empty() || c.bitonal.contains(Codecs::SOURCE));
            assert!(c.continuous.is_empty() || c.continuous.contains(Codecs::SOURCE));
            assert!(c.indexed.is_empty() || c.indexed.contains(Codecs::SOURCE));
        }
    }

    #[test]
    fn standard_does_not_convert_color() {
        assert_eq!(
            Config::preset(Preset::Standard).color_conversion,
            ColorConversion::None
        );
    }

    #[test]
    fn flag_debug_is_readable() {
        assert_eq!(
            format!("{:?}", Codecs::JPEG | Codecs::SOURCE),
            "jpeg|source"
        );
        assert_eq!(format!("{:?}", Strip::NONE), "none");
    }
}
