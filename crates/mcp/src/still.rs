//! One frame of the agent's frame copy, saved as a PNG. Windows screenshots are taken this
//! way: the agent copies the game's own backbuffer, so a covered window still captures.

use std::{fs, path::Path, time::Duration};

use protocol::frames::format;

use crate::{
    clock::{Clock, poll},
    error::{Error, Result, bail},
    frame_file::FrameFile,
};

const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// An 8-bit RGBA frame.
#[derive(Debug, PartialEq)]
pub struct Still {
    pub width: u32,
    pub height: u32,
    rgba: Vec<u8>,
}

/// Wait for the agent to publish a frame in the frame file at `frames` and copy the newest.
pub fn grab(frames: &Path, clock: &dyn Clock) -> Result<Still> {
    let file = poll(clock, FRAME_TIMEOUT, Duration::from_millis(20), || {
        FrameFile::open(frames)
    })?
    .ok_or_else(|| Error::msg("The game presented no frame within 5 s; is it rendering?"))?;
    from_file(&file, clock)
}

/// The newest frame in `file`, waiting for the first one.
pub fn from_file(file: &FrameFile, clock: &dyn Clock) -> Result<Still> {
    let mut pixels = Vec::new();
    poll(clock, FRAME_TIMEOUT, Duration::from_millis(10), || {
        Ok(file
            .written()
            .checked_sub(1)
            .and_then(|newest| file.read(newest, &mut pixels)))
    })?
    .ok_or_else(|| Error::msg("The game presented no frame within 5 s; is it rendering?"))?;
    Ok(Still {
        width: file.width,
        height: file.height,
        rgba: to_rgba(file.format, &pixels)?,
    })
}

/// Opaque RGBA from the agent's pixel `format`. Backbuffer alpha is not coverage, so it is
/// dropped.
pub fn to_rgba(pixel_format: u32, pixels: &[u8]) -> Result<Vec<u8>> {
    let pixels = pixels.as_chunks::<4>().0.iter();
    Ok(match pixel_format {
        format::BGRA8 => pixels.flat_map(|p| [p[2], p[1], p[0], 255]).collect(),
        format::RGBA8 => pixels.flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        format::RGB10A2 => pixels
            .flat_map(|p| {
                let value = u32::from_le_bytes(*p);
                let eight = |shift: u32| (((value >> shift) & 0x3ff) >> 2) as u8;
                [eight(0), eight(10), eight(20), 255]
            })
            .collect(),
        other => bail!("The agent sent frames in unknown format {other}."),
    })
}

impl Still {
    /// Area-average this frame down to `width` pixels wide, keeping its aspect ratio.
    pub fn scaled(&self, width: u32) -> Self {
        if width >= self.width || width == 0 {
            return Self {
                width: self.width,
                height: self.height,
                rgba: self.rgba.clone(),
            };
        }
        let height = ((u64::from(self.height) * u64::from(width) + u64::from(self.width) / 2)
            / u64::from(self.width))
        .max(1) as u32;
        let span = |out: u32, of: u32, from: u32| {
            let start = (u64::from(out) * u64::from(from) / u64::from(of)) as usize;
            let end = (u64::from(out + 1) * u64::from(from) / u64::from(of)) as usize;
            start..end.max(start + 1)
        };
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for oy in 0..height {
            let rows = span(oy, height, self.height);
            for ox in 0..width {
                let columns = span(ox, width, self.width);
                let mut sum = [0u32; 4];
                for y in rows.clone() {
                    for x in columns.clone() {
                        let at = (y * self.width as usize + x) * 4;
                        for (total, value) in sum.iter_mut().zip(&self.rgba[at..at + 4]) {
                            *total += u32::from(*value);
                        }
                    }
                }
                let count = (rows.len() * columns.len()) as u32;
                rgba.extend(sum.map(|total| ((total + count / 2) / count) as u8));
            }
        }
        Self {
            width,
            height,
            rgba,
        }
    }

    pub fn png(&self) -> Result<Vec<u8>> {
        ocr::Image::new(self.width, self.height, self.rgba.clone())
            .and_then(|image| image.to_png())
            .map_err(|error| Error::msg(error.to_string()))
    }

    /// Write the frame to `path` at full size; return an inline PNG at most `max_width`
    /// pixels wide when asked.
    pub fn save(&self, path: &Path, max_width: Option<u32>) -> Result<Option<Vec<u8>>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let full = self.png()?;
        fs::write(path, &full)?;
        Ok(match max_width {
            None => None,
            Some(width) if width >= self.width => Some(full),
            Some(width) => Some(self.scaled(width).png()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::fake::FakeClock, frame_file::tests::Writer, testdir::TestDir};

    #[test]
    fn grabs_take_the_newest_published_frame() {
        let dir = TestDir::new("grab");
        let path = dir.path().join("frames");
        let clock = FakeClock::new();
        let writer = Writer::create(&path, 2, 1);
        assert!(grab(&path, &clock).is_err(), "no frame published yet");
        writer.publish(0, 10, 1);
        writer.publish(1, 20, 2);
        let still = grab(&path, &clock).unwrap();
        assert_eq!((still.width, still.height), (2, 1));
        assert_eq!(still.rgba, [20, 20, 20, 255].repeat(2));
        drop(writer);
    }

    #[test]
    fn agent_formats_become_opaque_rgba() {
        assert_eq!(
            to_rgba(format::BGRA8, &[1, 2, 3, 0]).unwrap(),
            [3, 2, 1, 255]
        );
        assert_eq!(
            to_rgba(format::RGBA8, &[1, 2, 3, 0]).unwrap(),
            [1, 2, 3, 255]
        );
        // Red 1023, green 512, blue 4 in 10 bits each.
        let packed = (1023u32 | 512 << 10 | 4 << 20).to_le_bytes();
        assert_eq!(
            to_rgba(format::RGB10A2, &packed).unwrap(),
            [255, 128, 1, 255]
        );
        assert!(to_rgba(9, &[0; 4]).is_err());
    }

    #[test]
    fn inline_copies_average_down_to_the_maximum_width() {
        let still = Still {
            width: 4,
            height: 2,
            rgba: [[0, 0, 0, 255], [100, 100, 100, 255]].repeat(4).concat(),
        };
        let half = still.scaled(2);
        assert_eq!((half.width, half.height), (2, 1));
        assert_eq!(half.rgba, [50, 50, 50, 255].repeat(2));

        let dir = TestDir::new("still");
        let path = dir.path().join("shots/a.png");
        let inline = still.save(&path, Some(2)).unwrap().unwrap();
        let saved = ocr::Image::open(&path).unwrap();
        assert_eq!((saved.width(), saved.height()), (4, 2));
        let inline = ocr::Image::from_png(&inline).unwrap();
        assert_eq!((inline.width(), inline.height()), (2, 1));
        assert!(still.save(&path, None).unwrap().is_none());
    }
}
