//! In-memory image representation shared by decode, transform and encode.

/// Sample layout of a [`Raster`]. Everything except `Gray1` is 8 bits per
/// sample, interleaved, row-major, no padding. `Gray1` is packed one bit
/// per pixel, rows padded to a byte, 1 = white unless the image is a
/// stencil mask (where 1 = masked out), exactly as PDF stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Gray1,
    Gray8,
    Rgb8,
    Cmyk8,
    /// 8-bit palette indices; the palette stays in the PDF dictionary.
    Indexed8,
}

impl Format {
    pub fn samples_per_pixel(self) -> usize {
        match self {
            Format::Gray1 | Format::Gray8 | Format::Indexed8 => 1,
            Format::Rgb8 => 3,
            Format::Cmyk8 => 4,
        }
    }

    pub fn bits_per_component(self) -> u8 {
        if self == Format::Gray1 { 1 } else { 8 }
    }

    pub fn row_bytes(self, width: u32) -> usize {
        match self {
            Format::Gray1 => (width as usize).div_ceil(8),
            _ => width as usize * self.samples_per_pixel(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub data: Vec<u8>,
}

impl Raster {
    pub fn new(width: u32, height: u32, format: Format, data: Vec<u8>) -> Option<Raster> {
        let expected = format.row_bytes(width) * height as usize;
        (data.len() == expected && width > 0 && height > 0).then_some(Raster {
            width,
            height,
            format,
            data,
        })
    }

    /// Unpacked 8-bit gray from a packed 1-bit image (1 -> 255).
    pub fn gray1_to_gray8(&self) -> Raster {
        let stride = Format::Gray1.row_bytes(self.width);
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize);
        for row in self.data.chunks(stride) {
            for x in 0..self.width as usize {
                let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                out.push(if bit == 1 { 255 } else { 0 });
            }
        }
        Raster {
            width: self.width,
            height: self.height,
            format: Format::Gray8,
            data: out,
        }
    }

    /// Packed 1-bit from 8-bit gray by thresholding at mid-gray.
    pub fn gray8_to_gray1(&self) -> Raster {
        let stride = Format::Gray1.row_bytes(self.width);
        let mut out = vec![0u8; stride * self.height as usize];
        for (y, row) in self.data.chunks(self.width as usize).enumerate() {
            for (x, &v) in row.iter().enumerate() {
                if v >= 128 {
                    out[y * stride + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        Raster {
            width: self.width,
            height: self.height,
            format: Format::Gray1,
            data: out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_packing_round_trips() {
        let gray = Raster::new(
            10,
            2,
            Format::Gray8,
            vec![
                0, 255, 0, 255, 0, 255, 0, 255, 0, 255, //
                255, 255, 255, 255, 255, 0, 0, 0, 0, 0,
            ],
        )
        .unwrap();
        let packed = gray.gray8_to_gray1();
        assert_eq!(
            packed.data,
            vec![0b0101_0101, 0b0100_0000, 0b1111_1000, 0b0000_0000]
        );
        assert_eq!(packed.gray1_to_gray8(), gray);
    }

    #[test]
    fn size_is_validated() {
        assert!(Raster::new(3, 2, Format::Rgb8, vec![0; 18]).is_some());
        assert!(Raster::new(3, 2, Format::Rgb8, vec![0; 17]).is_none());
        assert!(Raster::new(0, 2, Format::Gray8, vec![]).is_none());
    }
}
