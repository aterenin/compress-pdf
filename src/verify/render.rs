//! Output verification, visual level (AGENTS.md, "Output verification").
//!
//! Every page of the input and of the output is rasterized with `hayro`
//! at a fixed low resolution, converted to gray, and compared with SSIM
//! (structural similarity: means, variances and covariance over blocks,
//! which forgives a re-encoded photo's noise but not a missing image, a
//! shifted glyph or a lost page). A page under the preset's floor is a
//! warning by default; callers may make it a failure.

use std::fmt;

use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::page::Page;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, hayro_interpret::InterpreterSettings};

use crate::config::Preset;

/// Rendering resolution. Low, so a full document renders in seconds and
/// the comparison judges layout and content rather than pixel noise.
pub const DPI: f32 = 72.0;

/// A page whose shorter side would render below this many pixels is
/// scaled up to it, so a stamp-sized page still yields enough blocks for
/// the comparison to mean something.
const MIN_SIDE: f32 = 128.0;

/// Pixels a rendered page may have at most; a page with an enormous media
/// box is scaled down to fit, since the comparison is about layout and
/// content, not resolution, and both documents render at the same scale.
const MAX_PIXELS: f32 = 4_000_000.0;

/// SSIM a page must reach for a preset's output to count as faithful.
/// Provisional: set from the corpus and probes, to be revisited against
/// reference outputs.
pub fn floor(preset: Preset) -> f32 {
    match preset {
        Preset::Less => 0.95,
        Preset::Standard => 0.93,
        Preset::More => 0.90,
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PageScore {
    /// Zero-based page index.
    pub page: usize,
    pub ssim: f32,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Comparison {
    pub pages: Vec<PageScore>,
    pub floor: f32,
}

impl Comparison {
    pub fn below_floor(&self) -> Vec<&PageScore> {
        self.pages.iter().filter(|p| p.ssim < self.floor).collect()
    }

    pub fn min(&self) -> Option<f32> {
        self.pages.iter().map(|p| p.ssim).reduce(f32::min)
    }
}

impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "render: {} pages compared at {DPI:.0} dpi, min SSIM {:.4} (floor {:.2})",
            self.pages.len(),
            self.min().unwrap_or(1.0),
            self.floor
        )?;
        for p in self.below_floor() {
            write!(
                f,
                "\n  - page {}: SSIM {:.4} below floor",
                p.page + 1,
                p.ssim
            )?;
        }
        Ok(())
    }
}

/// Render both documents and score every page. Errors name what could
/// not be rendered; a page count mismatch is scored as zero for the
/// missing pages rather than an error, so it shows up as a failure.
pub fn compare(input: &[u8], output: &[u8], preset: Preset) -> Result<Comparison, String> {
    let before = Pdf::new(input.to_vec())
        .map_err(|e| format!("input does not load for rendering: {e:?}"))?;
    let after = Pdf::new(output.to_vec())
        .map_err(|e| format!("output does not load for rendering: {e:?}"))?;
    let (pages_before, pages_after) = (before.pages(), after.pages());
    let count = pages_before.len().max(pages_after.len());
    let mut pages = Vec::with_capacity(count);
    for i in 0..count {
        let ssim = match (pages_before.get(i), pages_after.get(i)) {
            (Some(a), Some(b)) => ssim_gray(&render_gray(a), &render_gray(b)),
            _ => 0.0,
        };
        pages.push(PageScore { page: i, ssim });
    }
    Ok(Comparison {
        pages,
        floor: floor(preset),
    })
}

/// A page as 8-bit gray on white, rendered at [`DPI`] and averaged over
/// 2x2 pixels: a re-encoded or downsampled image lands on a different
/// pixel grid when rasterized, and the averaging keeps that from reading
/// as a structural change while a missing or shifted element still does.
fn render_gray(page: &Page<'_>) -> Gray {
    let (w, h) = page.render_dimensions();
    let scale = (DPI / 72.0)
        .max(MIN_SIDE / w.min(h).max(1.0))
        .min((MAX_PIXELS / (w * h).max(1.0)).sqrt());
    let settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: WHITE,
        ..RenderSettings::default()
    };
    let pixmap = hayro::render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &settings,
    );
    let (width, height) = (usize::from(pixmap.width()), usize::from(pixmap.height()));
    let data = pixmap.data_as_u8_slice();
    let mut gray = Vec::with_capacity(width * height);
    for px in data.as_chunks::<4>().0 {
        // Premultiplied over white: alpha is 1 after the background fill.
        gray.push(
            ((u32::from(px[0]) * 299 + u32::from(px[1]) * 587 + u32::from(px[2]) * 114) / 1000)
                as u8,
        );
    }
    halve(&Gray {
        width,
        height,
        data: gray,
    })
}

fn halve(g: &Gray) -> Gray {
    let (w, h) = (g.width / 2, g.height / 2);
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let at = |dx: usize, dy: usize| u32::from(g.data[(2 * y + dy) * g.width + 2 * x + dx]);
            data.push(((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1)) / 4) as u8);
        }
    }
    Gray {
        width: w,
        height: h,
        data,
    }
}

struct Gray {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

const BLOCK: usize = 8;

/// Mean SSIM over 8x8 blocks with the usual constants; 1.0 for identical
/// images, 0 when the sizes differ.
fn ssim_gray(a: &Gray, b: &Gray) -> f32 {
    if a.width != b.width || a.height != b.height || a.width == 0 || a.height == 0 {
        return 0.0;
    }
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut sum, mut blocks) = (0.0f64, 0usize);
    for by in (0..a.height).step_by(BLOCK) {
        for bx in (0..a.width).step_by(BLOCK) {
            let (mut ma, mut mb, mut va, mut vb, mut cov, mut n) =
                (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0);
            for y in by..(by + BLOCK).min(a.height) {
                for x in bx..(bx + BLOCK).min(a.width) {
                    let (pa, pb) = (
                        f64::from(a.data[y * a.width + x]),
                        f64::from(b.data[y * b.width + x]),
                    );
                    ma += pa;
                    mb += pb;
                    va += pa * pa;
                    vb += pb * pb;
                    cov += pa * pb;
                    n += 1.0;
                }
            }
            ma /= n;
            mb /= n;
            va = va / n - ma * ma;
            vb = vb / n - mb * mb;
            cov = cov / n - ma * mb;
            sum += ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            blocks += 1;
        }
    }
    (sum / blocks as f64) as f32
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object, Stream, dictionary};

    use super::*;

    fn page_with(content: &[u8]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let contents = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => contents,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    fn identical_pages_score_one_and_changed_pages_less() {
        let a = page_with(b"0 g 20 20 100 100 re f");
        let same = compare(&a, &a, Preset::Standard).unwrap();
        assert_eq!(same.pages.len(), 1);
        assert!((same.pages[0].ssim - 1.0).abs() < 1e-6, "{same}");
        assert!(same.below_floor().is_empty());
        let b = page_with(b"0 g 60 60 100 100 re f");
        let moved = compare(&a, &b, Preset::Standard).unwrap();
        assert!(moved.pages[0].ssim < 0.95, "{moved}");
        assert_eq!(moved.below_floor().len(), 1);
        let empty = page_with(b"");
        let gone = compare(&a, &empty, Preset::More).unwrap();
        assert!(gone.pages[0].ssim < 0.9, "{gone}");
    }

    #[test]
    fn ssim_handles_size_mismatch_and_flat_images() {
        let flat = Gray {
            width: 16,
            height: 16,
            data: vec![200; 256],
        };
        assert!((ssim_gray(&flat, &flat) - 1.0).abs() < 1e-6);
        let other = Gray {
            width: 8,
            height: 8,
            data: vec![0; 64],
        };
        assert_eq!(ssim_gray(&flat, &other), 0.0);
        assert!(compare(b"not a pdf", b"not a pdf", Preset::Less).is_err());
    }
}
