//! The shared-memory frame file, written by the agent and read by the MCP server.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use protocol::frames::{self, Header, PRESENT_RING, SLOTS, Slot, WRITING};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    },
    System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE, UnmapViewOfFile,
    },
};

/// The frame size and format a file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

pub struct Shm {
    file: HANDLE,
    mapping: HANDLE,
    base: *mut u8,
    pub shape: Shape,
    written: u64,
    synced_presents: u64,
}

// SAFETY: the mapping is owned by this value and used from one thread at a time.
unsafe impl Send for Shm {}

/// # Safety
/// `field` must point into the live mapping and be 8-byte aligned.
unsafe fn atomic64<'a>(field: *mut u64) -> &'a AtomicU64 {
    unsafe { AtomicU64::from_ptr(field) }
}

impl Shm {
    /// Create (or replace) the file at the Windows path `path`, sized for `shape`.
    pub fn create(path: &str, shape: Shape) -> Result<Self, String> {
        let size = frames::file_size(shape.width, shape.height, SLOTS);
        let wide = endpoint::wide(path);
        let file = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if file == INVALID_HANDLE_VALUE {
            return Err(format!("cannot create {path}"));
        }
        let mapping = unsafe {
            CreateFileMappingW(
                file,
                std::ptr::null(),
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            unsafe { CloseHandle(file) };
            return Err(format!("cannot map {path} ({size} bytes)"));
        }
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size as usize) };
        if view.Value.is_null() {
            unsafe {
                CloseHandle(mapping);
                CloseHandle(file);
            }
            return Err(format!("cannot map a view of {path}"));
        }
        let shm = Self {
            file,
            mapping,
            base: view.Value.cast(),
            shape,
            written: 0,
            synced_presents: 0,
        };
        let header = shm.header();
        // SAFETY: the view is at least a header long and freshly zeroed by the new file.
        unsafe {
            (*header).version = frames::VERSION;
            (*header).width = shape.width;
            (*header).height = shape.height;
            (*header).format = shape.format;
            (*header).slots = SLOTS;
            (*header).slot_bytes = frames::slot_bytes(shape.width, shape.height);
            (*header).pixels_offset = frames::pixels_offset(SLOTS);
            AtomicU32::from_ptr(&raw mut (*header).state)
                .store(frames::state::RUNNING, Ordering::Relaxed);
            // Readers wait for the magic; everything above is visible once it is.
            AtomicU32::from_ptr(&raw mut (*header).magic).store(frames::MAGIC, Ordering::Release);
        }
        Ok(shm)
    }

    fn header(&self) -> *mut Header {
        self.base.cast()
    }

    /// Copy one frame whose rows start at `source`, `row_pitch` bytes apart, into the next slot.
    ///
    /// # Safety
    /// `source` must hold `shape.height` rows of `shape.width * 4` bytes, `row_pitch` apart.
    pub unsafe fn publish(
        &mut self,
        source: *const u8,
        row_pitch: usize,
        present: u64,
        present_ns: u64,
    ) {
        let n = self.written;
        let index = (n % u64::from(SLOTS)) as usize;
        let row = self.shape.width as usize * 4;
        // SAFETY: the slot headers follow the header, and each pixel slot fits `height` rows.
        unsafe {
            let slot = self.base.add(size_of::<Header>()).cast::<Slot>().add(index);
            let seq = atomic64(&raw mut (*slot).seq);
            seq.store(WRITING, Ordering::Relaxed);
            fence(Ordering::Release);
            let pixels = self.base.add(
                (frames::pixels_offset(SLOTS)
                    + index as u64 * frames::slot_bytes(self.shape.width, self.shape.height))
                    as usize,
            );
            for y in 0..self.shape.height as usize {
                std::ptr::copy_nonoverlapping(source.add(y * row_pitch), pixels.add(y * row), row);
            }
            (*slot).present_ns = present_ns;
            (*slot).present = present;
            seq.store(n + 1, Ordering::Release);
            atomic64(&raw mut (*self.header()).written).store(n + 1, Ordering::Release);
        }
        self.written = n + 1;
    }

    /// Copy present times `synced..presents` from the agent's ring into the header.
    pub fn sync_presents(&mut self, ring: &[AtomicU64; PRESENT_RING], presents: u64, dropped: u64) {
        let header = self.header();
        let first = self
            .synced_presents
            .max(presents.saturating_sub(PRESENT_RING as u64));
        // SAFETY: the header is mapped; ring entries are 8-byte aligned.
        unsafe {
            for n in first..presents {
                let at = (n % PRESENT_RING as u64) as usize;
                atomic64(&raw mut (*header).present_ns[at])
                    .store(ring[at].load(Ordering::Acquire), Ordering::Relaxed);
            }
            atomic64(&raw mut (*header).dropped).store(dropped, Ordering::Relaxed);
            atomic64(&raw mut (*header).presents).store(presents, Ordering::Release);
        }
        self.synced_presents = presents;
    }

    pub fn set_state(&self, state: u32) {
        // SAFETY: the header is mapped.
        unsafe {
            AtomicU32::from_ptr(&raw mut (*self.header()).state).store(state, Ordering::Release)
        };
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base.cast(),
            });
            CloseHandle(self.mapping);
            CloseHandle(self.file);
        }
    }
}
