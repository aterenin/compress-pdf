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
