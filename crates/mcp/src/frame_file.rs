//! Reading the agent's shared-memory frame file; the layout is [`protocol::frames`].

use std::{
    fs::File,
    path::Path,
    sync::atomic::{AtomicU32, AtomicU64, Ordering, fence},
};

use memmap2::Mmap;
use protocol::frames::{self, Header, PRESENT_RING, Slot};

use crate::error::{Error, Result};

/// A mapped frame file.
pub struct FrameFile {
    /// The start of `_map`. The agent writes into it, so shared fields are read atomically.
    base: *mut u8,
    _map: Mmap,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    slots: u32,
    slot_bytes: u64,
    pixels_offset: u64,
}

// SAFETY: the mapping is read-only here and owned by this value.
unsafe impl Send for FrameFile {}

/// Where a frame came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    /// Which present it was, counted from the start of the capture.
    pub present: u64,
    /// Its present time, in nanoseconds on the agent's monotonic clock.
    pub present_ns: u64,
}

impl FrameFile {
    /// Map the file at `path`; `None` until the agent has published its header.
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let Ok(file) = File::open(path) else {
            return Ok(None);
        };
        let len = file.metadata()?.len() as usize;
        if len < size_of::<Header>() {
            return Ok(None);
        }
        // SAFETY: a fresh shared read-only mapping of an open file. The agent changes it
        // underneath; every read below goes through atomics or rechecks a sequence number.
        let map = unsafe { Mmap::map(&file) }
            .map_err(|error| Error::msg(format!("Cannot map {}: {error}", path.display())))?;
        let mut mapped = Self {
            base: map.as_ptr().cast_mut(),
            _map: map,
            width: 0,
            height: 0,
            format: 0,
            slots: 0,
            slot_bytes: 0,
            pixels_offset: 0,
        };
        let header = mapped.header();
        // SAFETY: the mapping holds a header; the magic is stored last, with Release.
        unsafe {
            if AtomicU32::from_ptr(&raw mut (*header).magic).load(Ordering::Acquire)
                != frames::MAGIC
            {
                return Ok(None);
            }
            if (*header).version != frames::VERSION {
                return Err(Error::msg(format!(
                    "The agent writes frame file version {}, this server reads {}; rebuild both.",
                    (*header).version,
                    frames::VERSION
                )));
            }
            mapped.width = (*header).width;
            mapped.height = (*header).height;
            mapped.format = (*header).format;
            mapped.slots = (*header).slots;
            mapped.slot_bytes = (*header).slot_bytes;
            mapped.pixels_offset = (*header).pixels_offset;
        }
        let needed = mapped.pixels_offset + u64::from(mapped.slots) * mapped.slot_bytes;
        if mapped.slots == 0
            || mapped.slot_bytes < mapped.frame_bytes() as u64
            || (len as u64) < needed
        {
            return Err(Error::msg(format!(
                "The frame file {} is malformed.",
                path.display()
            )));
        }
        Ok(Some(mapped))
    }

    fn header(&self) -> *mut Header {
        self.base.cast()
    }

    fn load(&self, field: *mut u64) -> u64 {
        // SAFETY: `field` points into the live mapping and is 8-byte aligned.
        unsafe { AtomicU64::from_ptr(field) }.load(Ordering::Acquire)
    }

    /// Bytes of one frame: `width * height * 4`.
    pub fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn slots(&self) -> u64 {
        u64::from(self.slots)
    }

    /// Frames published so far.
    pub fn written(&self) -> u64 {
        self.load(unsafe { &raw mut (*self.header()).written })
    }

    /// Frames the game presented while capturing.
    pub fn presents(&self) -> u64 {
        self.load(unsafe { &raw mut (*self.header()).presents })
    }

    /// Frames the agent skipped.
    pub fn dropped(&self) -> u64 {
        self.load(unsafe { &raw mut (*self.header()).dropped })
    }

    pub fn state(&self) -> u32 {
        // SAFETY: the header is mapped.
        unsafe { AtomicU32::from_ptr(&raw mut (*self.header()).state) }.load(Ordering::Acquire)
    }

    /// Present time of present `n`; valid while `n` is within [`PRESENT_RING`] of `presents()`.
    pub fn present_ns(&self, n: u64) -> u64 {
        let at = (n % PRESENT_RING as u64) as usize;
        self.load(unsafe { &raw mut (*self.header()).present_ns[at] })
    }

    /// Copy frame `n` into `out`; `None` when it is not written yet or was overwritten.
    pub fn read(&self, n: u64, out: &mut Vec<u8>) -> Option<FrameInfo> {
        let index = (n % u64::from(self.slots)) as usize;
        // SAFETY: slot headers follow the header; pixel slots lie inside the mapping (checked
        // in `open`).
        unsafe {
            let slot = self.base.add(size_of::<Header>()).cast::<Slot>().add(index);
            let seq = AtomicU64::from_ptr(&raw mut (*slot).seq);
            if seq.load(Ordering::Acquire) != n + 1 {
                return None;
            }
            let pixels = self
                .base
                .add((self.pixels_offset + index as u64 * self.slot_bytes) as usize);
            out.clear();
            out.extend_from_slice(std::slice::from_raw_parts(pixels, self.frame_bytes()));
            let info = FrameInfo {
                present: std::ptr::read_volatile(&raw const (*slot).present),
                present_ns: std::ptr::read_volatile(&raw const (*slot).present_ns),
            };
            fence(Ordering::Acquire);
            (seq.load(Ordering::Relaxed) == n + 1).then_some(info)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::OpenOptions;

    use memmap2::MmapMut;
    use protocol::frames::{SLOTS, WRITING, format, state};

    use super::*;

    /// A writer following the protocol's publication order, for readers to check against.
    pub(crate) struct Writer {
        base: *mut u8,
        _map: MmapMut,
        width: u32,
        height: u32,
    }

    impl Writer {
        pub(crate) fn create(path: &Path, width: u32, height: u32) -> Self {
            let len = frames::file_size(width, height, SLOTS) as usize;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .unwrap();
            file.set_len(len as u64).unwrap();
            let mut map = unsafe { MmapMut::map_mut(&file) }.unwrap();
            let base = map.as_mut_ptr();
            let header = base.cast::<Header>();
            unsafe {
                (*header).version = frames::VERSION;
                (*header).width = width;
                (*header).height = height;
                (*header).format = format::BGRA8;
                (*header).slots = SLOTS;
                (*header).slot_bytes = frames::slot_bytes(width, height);
                (*header).pixels_offset = frames::pixels_offset(SLOTS);
                (*header).state = state::RUNNING;
                AtomicU32::from_ptr(&raw mut (*header).magic)
                    .store(frames::MAGIC, Ordering::Release);
            }
            Self {
                base,
                _map: map,
                width,
                height,
            }
        }

        fn slot(&self, n: u64) -> *mut Slot {
            unsafe {
                self.base
                    .add(size_of::<Header>())
                    .cast::<Slot>()
                    .add((n % u64::from(SLOTS)) as usize)
            }
        }

        pub(crate) fn publish(&self, n: u64, fill: u8, present_ns: u64) {
            unsafe {
                let slot = self.slot(n);
                (*slot).seq = WRITING;
                let pixels = self.base.add(
                    (frames::pixels_offset(SLOTS)
                        + (n % u64::from(SLOTS)) * frames::slot_bytes(self.width, self.height))
                        as usize,
                );
                std::ptr::write_bytes(pixels, fill, (self.width * self.height * 4) as usize);
                (*slot).present = n * 2;
                (*slot).present_ns = present_ns;
                (*slot).seq = n + 1;
                (*self.base.cast::<Header>()).written = n + 1;
            }
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("bedrock-frame-file-{name}-{}", std::process::id()))
    }

    #[test]
    fn files_open_only_once_the_header_is_published() {
        let path = temp("header");
        let _ = std::fs::remove_file(&path);
        assert!(FrameFile::open(&path).unwrap().is_none());
        std::fs::write(&path, vec![0u8; frames::file_size(4, 2, SLOTS) as usize]).unwrap();
        assert!(FrameFile::open(&path).unwrap().is_none(), "no magic yet");
        let writer = Writer::create(&path, 4, 2);
        let file = FrameFile::open(&path).unwrap().unwrap();
        assert_eq!((file.width, file.height, file.frame_bytes()), (4, 2, 32));
        // Windows deletes no file that is still mapped.
        drop((file, writer));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn frames_are_read_until_their_slot_is_reused() {
        let path = temp("slots");
        let writer = Writer::create(&path, 3, 2);
        let file = FrameFile::open(&path).unwrap().unwrap();
        let mut pixels = Vec::new();
        assert!(file.read(0, &mut pixels).is_none(), "not written yet");
        writer.publish(0, 7, 1_000);
        assert_eq!(
            file.read(0, &mut pixels),
            Some(FrameInfo {
                present: 0,
                present_ns: 1_000
            })
        );
        assert_eq!(pixels, vec![7; 24]);
        for n in 1..=u64::from(SLOTS) {
            writer.publish(n, n as u8, 1_000 + n);
        }
        assert!(
            file.read(0, &mut pixels).is_none(),
            "slot 0 now holds a later frame"
        );
        assert_eq!(
            file.read(u64::from(SLOTS), &mut pixels).unwrap().present_ns,
            1_000 + u64::from(SLOTS)
        );
        unsafe { (*writer.slot(1)).seq = WRITING };
        assert!(
            file.read(1, &mut pixels).is_none(),
            "a slot being written is skipped"
        );
        drop((file, writer));
        std::fs::remove_file(&path).unwrap();
    }
}
