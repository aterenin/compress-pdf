//! Raster transforms: downsampling (and, later, cropping, color reduction
//! and conversion).

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

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
