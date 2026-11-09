//! RGBA images and rectangles.

use std::{io::Cursor, path::Path};

use serde::{Deserialize, Serialize};

use crate::Error;

/// A rectangle in image pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// This rectangle clipped to `width`×`height`.
    pub fn clamp(self, width: u32, height: u32) -> Self {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Self {
            x,
            y,
            width: self.width.min(width - x),
            height: self.height.min(height - y),
        }
    }
}

/// An 8-bit RGBA image.
#[derive(Clone, Debug)]
pub struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, Error> {
        if rgba.len() != width as usize * height as usize * 4 {
            return Err(Error(format!(
                "{} bytes do not make a {width}x{height} RGBA image",
                rgba.len()
            )));
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// Decode a PNG of any colour type into RGBA.
    pub fn from_png(bytes: &[u8]) -> Result<Self, Error> {
        let error = |error: png::DecodingError| Error(format!("cannot decode PNG: {error}"));
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().map_err(error)?;
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| Error("PNG too large".into()))?;
        let mut buffer = vec![0; size];
        let info = reader.next_frame(&mut buffer).map_err(error)?;
        buffer.truncate(info.line_size * info.height as usize);
        let pixels = (info.width * info.height) as usize;
        let rgba = match info.color_type {
            png::ColorType::Rgba => buffer,
            png::ColorType::Rgb => buffer
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => buffer
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::Indexed => {
                return Err(Error("PNG palette was not expanded".into()));
            }
        };
        debug_assert_eq!(rgba.len(), pixels * 4);
        Self::new(info.width, info.height, rgba)
    }

    pub fn open(path: &Path) -> Result<Self, Error> {
        let bytes = std::fs::read(path)
            .map_err(|error| Error(format!("cannot read {}: {error}", path.display())))?;
        Self::from_png(&bytes)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Rec. 601 luma of `rect` (assumed inside the image), row-major.
    pub(crate) fn luma(&self, rect: Rect) -> Vec<u8> {
        let mut out = Vec::with_capacity(rect.width as usize * rect.height as usize);
        for y in rect.y..rect.y + rect.height {
            let start = (y as usize * self.width as usize + rect.x as usize) * 4;
            let row = &self.rgba[start..start + rect.width as usize * 4];
            out.extend(row.as_chunks::<4>().0.iter().map(|p| {
                ((u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114 + 500)
                    / 1000) as u8
            }));
        }
        out
    }

    /// `rect` (assumed inside the image) as a new image.
    pub fn crop(&self, rect: Rect) -> Self {
        let mut rgba = Vec::with_capacity(rect.width as usize * rect.height as usize * 4);
        for y in rect.y..rect.y + rect.height {
            let start = (y as usize * self.width as usize + rect.x as usize) * 4;
            rgba.extend_from_slice(&self.rgba[start..start + rect.width as usize * 4]);
        }
        Self {
            width: rect.width,
            height: rect.height,
            rgba,
        }
    }

    pub fn to_png(&self) -> Result<Vec<u8>, Error> {
        let error = |error: png::EncodingError| Error(format!("cannot encode PNG: {error}"));
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(error)?;
        writer.write_image_data(&self.rgba).map_err(error)?;
        writer.finish().map_err(error)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trips() {
        let rgba: Vec<u8> = (0..2 * 3 * 4).map(|i| (i * 10) as u8).collect();
        let image = Image::new(2, 3, rgba.clone()).unwrap();
        let back = Image::from_png(&image.to_png().unwrap()).unwrap();
        assert_eq!((back.width(), back.height()), (2, 3));
        assert_eq!(back.rgba(), rgba);
    }

    #[test]
    fn rects_clamp_to_the_image() {
        assert_eq!(Rect::new(5, 5, 10, 10).clamp(8, 20), Rect::new(5, 5, 3, 10));
        assert_eq!(
            Rect::new(30, 0, 10, 10).clamp(8, 20),
            Rect::new(8, 0, 0, 10)
        );
    }
}
