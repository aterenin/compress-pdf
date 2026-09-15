//! Stage 1: find every placement of every image and compute its effective
//! resolution and visible area.
//!
//! Walks each page's content tracking the CTM and clip through `q`/`Q`/`cm`
//! and path clipping, descends into form XObjects with their `/Matrix` and
//! `/BBox`, tiling patterns, and annotation appearance streams, and at every
//! image `Do` records the rendered size in points and the visible fraction
//! of the image. Effective DPI = pixel width / (rendered width / 72). An
//! image used in several places keeps the *minimum*, since that is the
//! placement that would suffer first from downsampling; its crop box is the
//! union of the visible fractions.
//!
//! The same walk records, per font object, every string shown with it
//! (`Tj`, `TJ`, `'`, `"`), so the font stage knows which codes are used.
//!
//! Images and fonts reached only through paths the walker does not follow
//! (inline images, Type 3 glyph procedures, shading dictionaries, form
//! field default appearances) get no entry; the image stage reports such
//! images as unknown and the font stage leaves such fonts alone. Nothing
//! here mutates the document.

mod geometry;
mod walker;

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use lopdf::{Document, ObjectId};

use crate::config::Config;
use crate::pipeline::{Context, Stage};
pub use geometry::Rect;

#[derive(Debug, Clone, Copy)]
pub struct Placement {
    /// Rendered width and height in points.
    pub width_pt: f32,
    pub height_pt: f32,
    /// Visible fraction of the image's unit square under the clip in effect,
    /// or `None` when it is fully visible.
    pub crop: Option<Rect>,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub placements: Vec<Placement>,
    /// Pixel dimensions, copied here so consumers do not re-read the dictionary.
    pub pixels: (u32, u32),
}

impl Usage {
    /// Minimum effective resolution over all placements, on the tighter axis.
    pub fn min_dpi(&self) -> Option<f32> {
        let (w, h) = self.pixels;
        self.placements
            .iter()
            .map(|p| {
                let dx = w as f32 / (p.width_pt / 72.0);
                let dy = h as f32 / (p.height_pt / 72.0);
                dx.min(dy)
            })
            .fold(None, |acc, d| Some(acc.map_or(d, |a: f32| a.min(d))))
    }

    /// Union of the visible fractions over all placements, or `None` when
    /// any placement shows the whole image.
    pub fn crop_box(&self) -> Option<Rect> {
        let mut acc: Option<Rect> = None;
        for p in &self.placements {
            let r = p.crop?;
            acc = Some(acc.map_or(r, |a| a.union(r)));
        }
        acc
    }
}

/// The strings a font was shown with, as raw code bytes.
#[derive(Debug, Clone, Default)]
pub struct TextUsage {
    pub strings: BTreeSet<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct ImageUsage {
    pub by_object: HashMap<ObjectId, Usage>,
    /// Per font object: what was shown with it. A font that is used but
    /// never appears here was reached through a path the walker does not
    /// follow.
    pub fonts: HashMap<ObjectId, TextUsage>,
}

pub struct AnalyzeUsage;

impl Stage for AnalyzeUsage {
    fn name(&self) -> &'static str {
        "usage"
    }

    fn enabled(&self, config: &Config) -> bool {
        config.bitonal_dpi.enabled()
            || config.gray_dpi.enabled()
            || config.color_dpi.enabled()
            || config.clip_images
            || config.subset_fonts
            || config.merge_fonts
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        let mut usage = ImageUsage::default();
        let mut walker = walker::Walker::new(doc, &mut usage);
        for page_id in doc.page_iter() {
            walker.walk_page(page_id);
        }
        tracing::debug!(
            images = usage.by_object.len(),
            fonts = usage.fonts.len(),
            "usage analysis"
        );
        ctx.usage = usage;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;
    use crate::config::Preset;
    use crate::report::Report;

    /// One page, 200x200 pt, with a 600x300 pixel image `Im1` and the given
    /// content. Returns (doc, image id).
    fn doc_with_image(content: &[u8]) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let image = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 600, "Height" => 300,
                          "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
            vec![0],
        ));
        let contents = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => contents,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im1" => image } },
        });
        doc.objects.insert(
            pages_id,
            lopdf::Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        (doc, image)
    }

    fn analyze(doc: &mut Document) -> ImageUsage {
        let config = Config::preset(Preset::Standard);
        let mut report = Report::new(0);
        let mut ctx = Context {
            config: &config,
            report: &mut report,
            usage: ImageUsage::default(),
        };
        AnalyzeUsage.run(doc, &mut ctx).unwrap();
        ctx.usage
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn strings_are_recorded_per_font_through_q_and_forms() {
        let (mut doc, _image) = doc_with_image(
            b"BT /F1 12 Tf (ab) Tj q /F2 10 Tf [(c) -20 (d)] TJ Q (e) ' ET q /Fm1 Do Q",
        );
        let f1 = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
        );
        let f2 = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier" },
        );
        let form = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => f2 } } },
            b"BT /F1 8 Tf (z) Tj ET".to_vec(),
        ));
        let page_id = doc.page_iter().next().unwrap();
        let page = doc.get_dictionary_mut(page_id).unwrap();
        page.set(
            "Resources",
            dictionary! {
                "Font" => dictionary! { "F1" => f1, "F2" => f2 },
                "XObject" => dictionary! { "Fm1" => form },
            },
        );
        let usage = analyze(&mut doc);
        let shown =
            |id: ObjectId| -> Vec<Vec<u8>> { usage.fonts[&id].strings.iter().cloned().collect() };
        // After Q the font reverts to F1, so "e" belongs to it; inside the
        // form the name F1 resolves to the form's own F2.
        assert_eq!(shown(f1), vec![b"ab".to_vec(), b"e".to_vec()]);
        assert_eq!(shown(f2), vec![b"c".to_vec(), b"d".to_vec(), b"z".to_vec()]);
    }

    #[test]
    fn two_placements_keep_the_minimum_dpi() {
        // 600 px over 144 pt (2 in) = 300 dpi; over 288 pt (4 in) = 150 dpi.
        let (mut doc, image) =
            doc_with_image(b"q 144 0 0 72 0 0 cm /Im1 Do Q q 288 0 0 144 0 0 cm /Im1 Do Q");
        let usage = analyze(&mut doc);
        let u = &usage.by_object[&image];
        assert_eq!(u.placements.len(), 2);
        assert_eq!(u.pixels, (600, 300));
        assert!(close(u.min_dpi().unwrap(), 150.0));
        assert!(u.crop_box().is_none(), "no clip means fully visible");
    }

    #[test]
    fn rectangular_clip_yields_a_crop_box() {
        // Image fills 100x100 pt at the origin; clip keeps the lower-left quarter.
        let (mut doc, image) = doc_with_image(b"q 0 0 50 50 re W n 100 0 0 100 0 0 cm /Im1 Do Q");
        let usage = analyze(&mut doc);
        let crop = usage.by_object[&image].crop_box().unwrap();
        assert!(close(crop.x0, 0.0) && close(crop.y0, 0.0));
        assert!(close(crop.x1, 0.5) && close(crop.y1, 0.5), "{crop:?}");
    }

    #[test]
    fn clip_is_restored_by_q() {
        let (mut doc, image) =
            doc_with_image(b"q 0 0 50 50 re W n Q q 100 0 0 100 0 0 cm /Im1 Do Q");
        let usage = analyze(&mut doc);
        assert!(usage.by_object[&image].crop_box().is_none());
    }

    #[test]
    fn form_matrix_scales_placements() {
        let (mut doc, image) = doc_with_image(b"q /Fm1 Do Q");
        // A form that draws the image at 72x36 pt, itself scaled by 2.
        let form = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Matrix" => vec![2.into(), 0.into(), 0.into(), 2.into(), 0.into(), 0.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im1" => image } } },
            b"q 72 0 0 36 0 0 cm /Im1 Do Q".to_vec(),
        ));
        let page_id = doc.page_iter().next().unwrap();
        let page = doc.get_dictionary_mut(page_id).unwrap();
        let res = page.get_mut(b"Resources").unwrap().as_dict_mut().unwrap();
        res.set("XObject", dictionary! { "Fm1" => form });
        let usage = analyze(&mut doc);
        let p = usage.by_object[&image].placements[0];
        assert!(
            close(p.width_pt, 144.0) && close(p.height_pt, 72.0),
            "{p:?}"
        );
    }

    #[test]
    fn undrawn_images_get_no_entry() {
        let (mut doc, image) = doc_with_image(b"0 0 m 10 10 l S");
        let usage = analyze(&mut doc);
        assert!(!usage.by_object.contains_key(&image));
    }
}
