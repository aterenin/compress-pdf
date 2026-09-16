//! Probe tests: each synthetic one-variable PDF from `probes/generators.rs`
//! runs through the pipeline and the design's stated behavior for it is
//! asserted on the report and the document (CLAUDE.md, "Testing").

#[path = "probes/generators.rs"]
mod generators;

use compress_pdf::config::{Config, Preset};
use compress_pdf::pipeline;
use compress_pdf::report::{FontRow, ImageRow, Report};
use lopdf::{Document, Object};

/// Run a probe under a preset; the report and the transformed document.
fn run(mut doc: Document, preset: Preset) -> (Document, Report) {
    let mut report = Report::new(0);
    pipeline::run(&mut doc, &Config::preset(preset), &mut report).expect("pipeline runs");
    (doc, report)
}

fn image_row(report: &Report) -> &ImageRow {
    assert_eq!(report.images.len(), 1, "one image row expected: {report}");
    &report.images[0]
}

fn font_row(report: &Report) -> &FontRow {
    assert_eq!(report.fonts.len(), 1, "one font row expected: {report}");
    &report.fonts[0]
}

fn image_streams(doc: &Document) -> Vec<&lopdf::Stream> {
    doc.objects
        .values()
        .filter_map(|o| match o {
            Object::Stream(s)
                if s.dict.get(b"Subtype").ok().and_then(|n| n.as_name().ok()) == Some(b"Image") =>
            {
                Some(s)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn every_probe_serializes_and_survives_every_preset() {
    for probe in generators::all() {
        let bytes = generators::to_bytes(&mut probe.doc.clone());
        let mut doc = Document::load_mem(&bytes).unwrap_or_else(|e| panic!("{}: {e}", probe.name));
        assert_eq!(doc.get_pages().len(), 1, "{}", probe.name);
        for preset in [Preset::Less, Preset::Standard, Preset::More] {
            let mut report = Report::new(bytes.len());
            pipeline::run(&mut doc, &Config::preset(preset), &mut report)
                .unwrap_or_else(|e| panic!("{} under {preset:?}: {e}", probe.name));
            let out = pipeline::serialize(&mut doc, &bytes, &mut report)
                .unwrap_or_else(|e| panic!("{} under {preset:?}: {e}", probe.name));
            assert!(
                out.len() <= bytes.len(),
                "{} ({}) grew under {preset:?}",
                probe.name,
                probe.about
            );
            let v = compress_pdf::verify::verify(&out, 1);
            assert!(
                v.problems.is_empty(),
                "{} under {preset:?}: {v}",
                probe.name
            );
            let rendered = compress_pdf::verify::render::compare(&bytes, &out, preset)
                .unwrap_or_else(|e| panic!("{} under {preset:?}: {e}", probe.name));
            assert!(
                rendered.below_floor().is_empty(),
                "{} under {preset:?}: {rendered}",
                probe.name
            );
        }
    }
}

#[test]
fn cmyk_is_converted_to_rgb_only_by_more() {
    let (doc, report) = run(generators::cmyk_patch(), Preset::More);
    assert!(image_row(&report).action.contains("rgb"), "{report}");
    let cs = image_streams(&doc)[0].dict.get(b"ColorSpace").unwrap();
    assert_eq!(cs.as_name().unwrap(), b"DeviceRGB");
    let (_, report) = run(generators::cmyk_patch(), Preset::Standard);
    assert!(!image_row(&report).action.contains("rgb"), "{report}");
}

#[test]
fn gray_rgb_image_is_reduced_to_gray_under_standard() {
    let (doc, report) = run(generators::rgb_gray(), Preset::Standard);
    let row = image_row(&report);
    assert!(row.action.contains("reduced"), "{report}");
    assert!(
        row.action.contains("downsampled"),
        "300 dpi over a 150 dpi threshold: {report}"
    );
    let cs = image_streams(&doc)[0].dict.get(b"ColorSpace").unwrap();
    assert_eq!(cs.as_name().unwrap(), b"DeviceGray");
    let (_, report) = run(generators::rgb_gray(), Preset::Less);
    assert!(!image_row(&report).action.contains("reduced"), "{report}");
}

#[test]
fn bitonal_lines_follow_each_presets_dpi_rule_and_codec() {
    let (_, report) = run(generators::bitonal_lines(), Preset::Standard);
    let row = image_row(&report);
    assert!(row.action.contains("downsampled"), "{report}");
    assert_eq!(row.filter_out, "CCITTFaxDecode", "{report}");
    let (_, report) = run(generators::bitonal_lines(), Preset::Less);
    let row = image_row(&report);
    assert!(
        row.action.contains("downsampled"),
        "600 dpi is above the 400 dpi threshold: {report}"
    );
    assert_eq!(row.filter_out, "JBIG2Decode", "{report}");
}

#[test]
fn indexed_image_is_untouched_by_less_and_flate_under_standard() {
    let (_, report) = run(generators::indexed_patch(), Preset::Less);
    assert_eq!(
        image_row(&report).action,
        "kept: class excluded",
        "{report}"
    );
    let (_, report) = run(generators::indexed_patch(), Preset::Standard);
    let row = image_row(&report);
    assert!(row.action.contains("downsampled"), "{report}");
    assert_eq!(row.filter_out, "FlateDecode", "{report}");
}

#[test]
fn jpeg_needing_no_transform_never_grows() {
    for preset in [Preset::Less, Preset::Standard, Preset::More] {
        let (_, report) = run(generators::jpeg_photo(), preset);
        let row = image_row(&report);
        assert!(row.bytes_out <= row.bytes_in, "{preset:?}: {report}");
        assert_eq!(row.filter_out, "DCTDecode", "{preset:?}: {report}");
    }
}

#[test]
fn clipped_image_is_cropped_by_standard_but_not_less() {
    let (doc, report) = run(generators::clipped_image(), Preset::Standard);
    assert!(image_row(&report).action.contains("clipped"), "{report}");
    let cropped = image_streams(&doc)[0];
    assert_eq!(cropped.dict.get(b"Width").unwrap().as_i64().unwrap(), 200);
    let (_, report) = run(generators::clipped_image(), Preset::Less);
    assert!(!image_row(&report).action.contains("clipped"), "{report}");
}

#[test]
fn opaque_soft_mask_is_removed_under_standard() {
    let (doc, report) = run(generators::smask_opaque(), Preset::Standard);
    assert!(
        image_row(&report).action.contains("smask-removed"),
        "{report}"
    );
    assert!(image_streams(&doc).iter().all(|s| !s.dict.has(b"SMask")));
    assert_eq!(image_streams(&doc).len(), 1, "the mask object is collected");
}

#[test]
fn lab_image_lands_in_rgb() {
    let (doc, report) = run(generators::lab_image(), Preset::Standard);
    assert!(image_row(&report).action.contains("alternate"), "{report}");
    let cs = image_streams(&doc)[0].dict.get(b"ColorSpace").unwrap();
    assert_eq!(cs.as_name().unwrap(), b"DeviceRGB");
}

#[test]
fn duplicate_images_are_merged() {
    for preset in [Preset::Less, Preset::Standard] {
        let (doc, _) = run(generators::duplicate_images(), preset);
        assert_eq!(image_streams(&doc).len(), 1, "{preset:?}");
    }
}

#[test]
fn unused_resources_are_dropped_under_standard() {
    let (doc, report) = run(generators::unused_resources(), Preset::Standard);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("removed 2 unused resource entries")),
        "{report}"
    );
    assert_eq!(image_streams(&doc).len(), 1);
    assert!(
        !doc.objects
            .values()
            .any(|o| matches!(o, Object::Dictionary(d) if d.get(b"BaseFont").is_ok()))
    );
}

#[test]
fn type1_font_is_converted_and_subset() {
    let (doc, report) = run(generators::type1_partly_used(), Preset::Standard);
    assert_eq!(
        font_row(&report).action,
        "cff+subset to 3 glyphs",
        "{report}"
    );
    let descriptor = doc
        .objects
        .values()
        .find_map(|o| match o {
            Object::Dictionary(d) if d.has(b"FontFile3") => Some(d),
            _ => None,
        })
        .expect("a descriptor with FontFile3");
    assert!(!descriptor.has(b"FontFile"));
    let (_, report) = run(generators::type1_partly_used(), Preset::Less);
    assert_eq!(
        font_row(&report).action,
        "kept: Type1 programs are not subset",
        "{report}"
    );
}

#[test]
fn embedded_arial_is_unembedded_by_less_only() {
    let (doc, report) = run(generators::arial_embedded(), Preset::Less);
    assert_eq!(
        font_row(&report).action,
        "unembedded as Helvetica",
        "{report}"
    );
    let font = doc
        .objects
        .values()
        .find_map(|o| match o {
            Object::Dictionary(d) if d.has(b"BaseFont") => Some(d),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        font.get(b"BaseFont").unwrap().as_name().unwrap(),
        b"Helvetica"
    );
    let (_, report) = run(generators::arial_embedded(), Preset::Standard);
    assert!(
        !font_row(&report).action.starts_with("unembedded"),
        "{report}"
    );
    let (_, report) = run(generators::arial_embedded(), Preset::More);
    assert_eq!(
        font_row(&report).action,
        "unembedded as Helvetica",
        "{report}"
    );
}

#[test]
fn metadata_and_thumbnail_are_stripped_by_standard_only() {
    let has = |doc: &Document, key: &[u8]| {
        doc.objects
            .values()
            .any(|o| matches!(o, Object::Dictionary(d) if d.has(key)))
    };
    let (doc, _) = run(generators::metadata_thumbnail(), Preset::Standard);
    assert!(!has(&doc, b"Metadata") && !has(&doc, b"Thumb"));
    let (doc, _) = run(generators::metadata_thumbnail(), Preset::Less);
    assert!(has(&doc, b"Metadata") && has(&doc, b"Thumb"));
}

#[test]
fn default_resource_fonts_are_pruned_unless_xfa() {
    let dr_fonts = |doc: &Document| -> Vec<Vec<u8>> {
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let acro = doc.get_dictionary(root).unwrap().get(b"AcroForm").unwrap();
        let acro = doc.dereference(acro).unwrap().1.as_dict().unwrap();
        let dr = acro.get(b"DR").unwrap();
        let dr = doc.dereference(dr).unwrap().1.as_dict().unwrap();
        let fonts = dr.get(b"Font").unwrap();
        let fonts = doc.dereference(fonts).unwrap().1.as_dict().unwrap();
        let mut names: Vec<Vec<u8>> = fonts.iter().map(|(k, _)| k.clone()).collect();
        names.sort();
        names
    };
    let (doc, _) = run(generators::form_default_fonts(false), Preset::Standard);
    assert_eq!(dr_fonts(&doc), vec![b"Helv".to_vec()]);
    let (doc, _) = run(generators::form_default_fonts(true), Preset::Standard);
    assert_eq!(dr_fonts(&doc), vec![b"Cour".to_vec(), b"Helv".to_vec()]);
    let (doc, _) = run(generators::form_shared_fonts(), Preset::Standard);
    assert_eq!(dr_fonts(&doc), vec![b"Cour".to_vec(), b"Helv".to_vec()]);
}
