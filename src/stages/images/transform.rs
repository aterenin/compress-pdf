//! Raster transforms: downsampling and color conversion (cropping and
//! color complexity reduction are still to come).
//!
//! Color conversion to RGB:
//!
//! DeviceCMYK converts through the Neugebauer (Demichel) mixing model with
//! the published default coefficients: each of the 16 ink overprint
//! primaries has an RGB color, and a pixel with ink amounts c, m, y, k is
//! the sum of the primaries weighted by the product, over the four inks, of
//! the amount if the primary contains the ink and one minus the amount
//! otherwise. The model is multilinear, so it reproduces the reference's
//! 3-point-grid ICC profile exactly.
//!
//! Images with an embedded ICC profile convert through the profile with
//! `moxcms`. Gray images are left alone: converting them to RGB would only
//! triple their size.

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use moxcms::{ColorProfile, Layout, TransformOptions};

use super::raster::{Format, Raster};

/// Resample to `width` x `height`. Continuous-tone images use Lanczos3;
/// palette indices use nearest neighbour; bitonal images are resampled as
/// gray and thresholded at mid-gray (the provisional choice in CLAUDE.md).
pub fn downsample(raster: &Raster, width: u32, height: u32) -> Option<Raster> {
    if width == 0 || height == 0 || (width, height) == (raster.width, raster.height) {
        return None;
    }
    match raster.format {
        Format::Gray1 => {
            let gray = resize(&raster.gray1_to_gray8(), width, height, lanczos())?;
            Some(gray.gray8_to_gray1())
        }
        Format::Indexed8 => resize(raster, width, height, ResizeAlg::Nearest),
        _ => resize(raster, width, height, lanczos()),
    }
}

fn lanczos() -> ResizeAlg {
    ResizeAlg::Convolution(FilterType::Lanczos3)
}

fn resize(raster: &Raster, width: u32, height: u32, alg: ResizeAlg) -> Option<Raster> {
    let pixel_type = match raster.format {
        Format::Gray8 | Format::Indexed8 => PixelType::U8,
        Format::Rgb8 => PixelType::U8x3,
        Format::Cmyk8 => PixelType::U8x4,
        Format::Gray1 => return None,
    };
    let src =
        Image::from_vec_u8(raster.width, raster.height, raster.data.clone(), pixel_type).ok()?;
    let mut dst = Image::new(width, height, pixel_type);
    let options = ResizeOptions::new().resize_alg(alg);
    Resizer::new().resize(&src, &mut dst, &options).ok()?;
    Raster::new(width, height, raster.format, dst.into_vec())
}

/// Target size for a downsample by `factor` (< 1), never below one pixel.
pub fn scaled_size(width: u32, height: u32, factor: f32) -> (u32, u32) {
    let w = ((width as f32 * factor).round() as u32).max(1);
    let h = ((height as f32 * factor).round() as u32).max(1);
    (w, h)
}

// ---------------------------------------------------------- color

/// (R, G, B) of the 16 primaries, indexed by a bitmask of inks present:
/// bit 0 = C, bit 1 = M, bit 2 = Y, bit 3 = K.
const PRIMARIES: [[f32; 3]; 16] = [
    [0.996078, 0.996078, 0.996078], // white
    [0.000000, 0.686275, 0.937255], // C
    [0.925490, 0.149020, 0.560784], // M
    [0.243137, 0.247059, 0.584314], // CM
    [1.000000, 0.949020, 0.066667], // Y
    [0.000000, 0.658824, 0.349020], // CY
    [0.929412, 0.196078, 0.215686], // MY
    [0.266667, 0.266667, 0.274510], // CMY
    [0.215686, 0.203922, 0.207843], // K
    [0.066667, 0.176471, 0.215686], // CK
    [0.215686, 0.101961, 0.141176], // MK
    [0.133333, 0.098039, 0.160784], // CMK
    [0.200000, 0.196078, 0.125490], // YK
    [0.074510, 0.180392, 0.133333], // CYK
    [0.215686, 0.121569, 0.113725], // MYK
    [0.125490, 0.121569, 0.121569], // CMYK
];

/// One CMYK pixel (0..255 ink amounts) to RGB by the Neugebauer model.
pub fn neugebauer(cmyk: [u8; 4]) -> [u8; 3] {
    let ink: [f32; 4] = cmyk.map(|v| f32::from(v) / 255.0);
    let mut rgb = [0.0f32; 3];
    for (mask, primary) in PRIMARIES.iter().enumerate() {
        let mut weight = 1.0;
        for (bit, amount) in ink.iter().enumerate() {
            weight *= if mask & (1 << bit) != 0 {
                *amount
            } else {
                1.0 - amount
            };
        }
        for (acc, p) in rgb.iter_mut().zip(primary) {
            *acc += weight * p;
        }
    }
    rgb.map(|v| (v * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// DeviceCMYK raster to RGB.
pub fn cmyk_to_rgb(raster: &Raster) -> Option<Raster> {
    if raster.format != Format::Cmyk8 {
        return None;
    }
    let data: Vec<u8> = raster
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&px| neugebauer(px))
        .collect();
    Raster::new(raster.width, raster.height, Format::Rgb8, data)
}

/// Convert through an embedded ICC profile to sRGB. Gray, RGB and CMYK
/// sources; `None` when the profile does not parse or does not match.
pub fn icc_to_rgb(raster: &Raster, profile: &[u8]) -> Option<Raster> {
    let layout = match raster.format {
        Format::Gray8 => Layout::Gray,
        Format::Rgb8 => Layout::Rgb,
        Format::Cmyk8 => Layout::Rgba, // moxcms: CMYK8 shares the 4-channel layout
        _ => return None,
    };
    let src = ColorProfile::new_from_slice(profile).ok()?;
    let transform = src
        .create_transform_8bit(
            layout,
            &ColorProfile::new_srgb(),
            Layout::Rgb,
            TransformOptions::default(),
        )
        .ok()?;
    let pixels = raster.width as usize * raster.height as usize;
    let mut out = vec![0u8; pixels * 3];
    let in_row = raster.format.row_bytes(raster.width);
    for (src_row, dst_row) in raster
        .data
        .chunks(in_row)
        .zip(out.chunks_mut(raster.width as usize * 3))
    {
        transform.transform(src_row, dst_row).ok()?;
    }
    Raster::new(raster.width, raster.height, Format::Rgb8, out)
}

// ------------------------------------------------------------- crop

/// A pixel rectangle: left, top, width, height (rows counted from the top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Pixel rectangle covering the unit-square fraction `x0..x1` by `y0..y1`
/// (PDF image space, y up) of a `width` x `height` image, rounded outward so
/// nothing visible is lost. `None` when it would keep the whole image or
/// nothing at all.
pub fn crop_pixels(width: u32, height: u32, unit: [f32; 4]) -> Option<PixelRect> {
    let [x0, y0, x1, y1] = unit;
    let left = (x0.max(0.0) * width as f32).floor() as u32;
    let right = ((x1.min(1.0) * width as f32).ceil() as u32).min(width);
    let top = ((1.0 - y1.min(1.0)) * height as f32).floor() as u32;
    let bottom = (((1.0 - y0.max(0.0)) * height as f32).ceil() as u32).min(height);
    if right <= left || bottom <= top {
        return None;
    }
    let rect = PixelRect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    };
    (rect.w < width || rect.h < height).then_some(rect)
}

/// The unit-square rectangle a pixel rectangle covers, for the wrapper form.
pub fn unit_of(width: u32, height: u32, r: PixelRect) -> [f32; 4] {
    let (w, h) = (width as f32, height as f32);
    [
        r.x as f32 / w,
        1.0 - (r.y + r.h) as f32 / h,
        (r.x + r.w) as f32 / w,
        1.0 - r.y as f32 / h,
    ]
}

pub fn crop(raster: &Raster, r: PixelRect) -> Option<Raster> {
    if r.x + r.w > raster.width || r.y + r.h > raster.height {
        return None;
    }
    if raster.format == Format::Gray1 {
        return crop(&raster.gray1_to_gray8(), r).map(|g| g.gray8_to_gray1());
    }
    let n = raster.format.samples_per_pixel();
    let stride = raster.format.row_bytes(raster.width);
    let mut out = Vec::with_capacity(r.w as usize * r.h as usize * n);
    for row in r.y..r.y + r.h {
        let start = row as usize * stride + r.x as usize * n;
        out.extend_from_slice(&raster.data[start..start + r.w as usize * n]);
    }
    Raster::new(r.w, r.h, raster.format, out)
}

// ------------------------------------------------------- complexity

/// Color complexity reduction: a flat image becomes one pixel; RGB or CMYK
/// whose pixels are all gray becomes gray; gray with only black and white
/// becomes bitonal. Returns `None` when nothing applies.
pub fn reduce(raster: &Raster) -> Option<Raster> {
    let mut current = raster.clone();
    let mut changed = false;
    for step in [flatten, gray_if_gray, bitonal_if_two_level] {
        if let Some(next) = step(&current) {
            current = next;
            changed = true;
        }
    }
    changed.then_some(current)
}

fn flatten(raster: &Raster) -> Option<Raster> {
    if raster.width as u64 * raster.height as u64 <= 1 {
        return None;
    }
    let unpacked = if raster.format == Format::Gray1 {
        raster.gray1_to_gray8()
    } else {
        raster.clone()
    };
    let n = unpacked.format.samples_per_pixel();
    let first = &unpacked.data[..n];
    if !unpacked.data.chunks(n).all(|px| px == first) {
        return None;
    }
    let one = Raster::new(1, 1, unpacked.format, first.to_vec())?;
    Some(if raster.format == Format::Gray1 {
        one.gray8_to_gray1()
    } else {
        one
    })
}

fn gray_if_gray(raster: &Raster) -> Option<Raster> {
    let data: Vec<u8> = match raster.format {
        Format::Rgb8 => {
            let px = raster.data.as_chunks::<3>().0;
            if !px.iter().all(|p| p[0] == p[1] && p[1] == p[2]) {
                return None;
            }
            px.iter().map(|p| p[0]).collect()
        }
        Format::Cmyk8 => {
            let px = raster.data.as_chunks::<4>().0;
            if !px.iter().all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0) {
                return None;
            }
            px.iter().map(|p| 255 - p[3]).collect()
        }
        _ => return None,
    };
    Raster::new(raster.width, raster.height, Format::Gray8, data)
}

fn bitonal_if_two_level(raster: &Raster) -> Option<Raster> {
    if raster.format != Format::Gray8 || !raster.data.iter().all(|&v| v == 0 || v == 255) {
        return None;
    }
    Some(raster.gray8_to_gray1())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_downsample_averages() {
        let data: Vec<u8> = (0..16).map(|i| if i % 2 == 0 { 0 } else { 200 }).collect();
        let r = Raster::new(4, 4, Format::Gray8, data).unwrap();
        let d = downsample(&r, 2, 2).unwrap();
        assert_eq!((d.width, d.height), (2, 2));
        assert!(d.data.iter().all(|&v| v > 60 && v < 140), "{:?}", d.data);
    }

    #[test]
    fn bitonal_round_trips_through_gray() {
        // 8x2 all white -> 4x1 all white.
        let r = Raster::new(8, 2, Format::Gray1, vec![0xFF, 0xFF]).unwrap();
        let d = downsample(&r, 4, 1).unwrap();
        assert_eq!(d.format, Format::Gray1);
        assert_eq!(d.data, vec![0xF0]);
    }

    #[test]
    fn reduction_rules() {
        // Gray RGB becomes gray, then two-level gray becomes bitonal.
        let rgb = Raster::new(2, 1, Format::Rgb8, vec![0, 0, 0, 255, 255, 255]).unwrap();
        let out = reduce(&rgb).unwrap();
        assert_eq!(out.format, Format::Gray1);
        assert_eq!(out.data, vec![0b0100_0000]);
        // K-only CMYK becomes gray.
        let cmyk = Raster::new(1, 1, Format::Cmyk8, vec![0, 0, 0, 55]).unwrap();
        assert_eq!(reduce(&cmyk).unwrap().data, vec![200]);
        // Flat color image collapses to one pixel.
        let flat = Raster::new(4, 4, Format::Rgb8, [10u8, 20, 30].repeat(16)).unwrap();
        let out = reduce(&flat).unwrap();
        assert_eq!(
            (out.width, out.height, out.data.as_slice()),
            (1, 1, &[10u8, 20, 30][..])
        );
        // Nothing applies to a real color image.
        let color = Raster::new(2, 1, Format::Rgb8, vec![1, 2, 3, 4, 5, 6]).unwrap();
        assert!(reduce(&color).is_none());
    }

    #[test]
    fn crop_rounds_outward_and_maps_back() {
        // Bottom-left quarter of a 10x10 image is rows 5..10, columns 0..5.
        let r = crop_pixels(10, 10, [0.0, 0.0, 0.5, 0.5]).unwrap();
        assert_eq!(
            r,
            PixelRect {
                x: 0,
                y: 5,
                w: 5,
                h: 5
            }
        );
        assert_eq!(unit_of(10, 10, r), [0.0, 0.0, 0.5, 0.5]);
        // Fractions round outward.
        let r = crop_pixels(10, 10, [0.31, 0.0, 0.69, 1.0]).unwrap();
        assert_eq!((r.x, r.w), (3, 4));
        assert!(crop_pixels(10, 10, [0.0, 0.0, 1.0, 1.0]).is_none());
        assert!(crop_pixels(10, 10, [0.5, 0.5, 0.5, 0.5]).is_none());
    }

    #[test]
    fn crop_extracts_the_window() {
        let data: Vec<u8> = (0..16).collect();
        let r = Raster::new(4, 4, Format::Gray8, data).unwrap();
        let c = crop(
            &r,
            PixelRect {
                x: 1,
                y: 2,
                w: 2,
                h: 2,
            },
        )
        .unwrap();
        assert_eq!(c.data, vec![9, 10, 13, 14]);
        let bits = Raster::new(8, 2, Format::Gray1, vec![0b1010_1010, 0b0101_0101]).unwrap();
        let c = crop(
            &bits,
            PixelRect {
                x: 1,
                y: 0,
                w: 3,
                h: 2,
            },
        )
        .unwrap();
        assert_eq!(c.data, vec![0b0100_0000, 0b1010_0000]);
    }

    #[test]
    fn same_size_is_a_no_op() {
        let r = Raster::new(2, 2, Format::Gray8, vec![0; 4]).unwrap();
        assert!(downsample(&r, 2, 2).is_none());
        assert_eq!(scaled_size(1000, 10, 0.05), (50, 1));
    }
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn primaries_reproduce_the_table() {
        assert_eq!(neugebauer([0, 0, 0, 0]), [254, 254, 254]);
        assert_eq!(neugebauer([255, 0, 0, 0]), [0, 175, 239]);
        assert_eq!(neugebauer([0, 255, 0, 0]), [236, 38, 143]);
        assert_eq!(neugebauer([0, 0, 255, 0]), [255, 242, 17]);
        assert_eq!(neugebauer([255, 255, 255, 255]), [32, 31, 31]);
    }

    #[test]
    fn half_cyan_is_halfway() {
        // Multilinear: 50% C is the midpoint of white and C.
        let [r, g, b] = neugebauer([128, 0, 0, 0]);
        assert!(
            (120..=135).contains(&r) && (210..=218).contains(&g) && (244..=250).contains(&b),
            "{r} {g} {b}"
        );
    }

    #[test]
    fn srgb_profile_round_trips_through_moxcms() {
        // sRGB -> sRGB through the profile machinery is the identity.
        let srgb = ColorProfile::new_srgb();
        let bytes = srgb.encode().unwrap();
        let r = Raster::new(2, 1, Format::Rgb8, vec![10, 200, 30, 255, 0, 128]).unwrap();
        let out = icc_to_rgb(&r, &bytes).unwrap();
        for (a, b) in out.data.iter().zip(&r.data) {
            assert!((i16::from(*a) - i16::from(*b)).abs() <= 1, "{:?}", out.data);
        }
    }

    #[test]
    fn raster_conversion_changes_format() {
        let r = Raster::new(2, 1, Format::Cmyk8, vec![0, 0, 0, 0, 0, 0, 0, 255]).unwrap();
        let rgb = cmyk_to_rgb(&r).unwrap();
        assert_eq!(rgb.format, Format::Rgb8);
        assert_eq!(rgb.data, vec![254, 254, 254, 55, 52, 53]);
    }
}
