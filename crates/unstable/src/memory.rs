//! Checked reads of game memory: every range is found committed and readable before it is
//! read, so a pointer from a layout that moved yields None instead of a fault. Callers read
//! only objects the game does not change during the read (on the thread that owns them, or
//! arguments of the call being hooked).

use std::ffi::c_void;

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};

/// Committed, readable address ranges seen during one read, so most checks skip
/// `VirtualQuery`.
#[derive(Default)]
pub struct Memory {
    readable: Vec<(usize, usize)>,
}

impl Memory {
    pub fn readable(&mut self, address: usize, length: usize) -> bool {
        let Some(end) = address.checked_add(length) else {
            return false;
        };
        if address < 0x10000 {
            return false;
        }
        let mut at = address;
        while at < end {
            if let Some(&(_, region_end)) = self
                .readable
                .iter()
                .find(|(start, region_end)| *start <= at && at < *region_end)
            {
                at = region_end;
                continue;
            }
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: VirtualQuery only describes the address; it never touches it.
            let written = unsafe {
                VirtualQuery(
                    at as *const c_void,
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if written == 0 || info.State != MEM_COMMIT {
                return false;
            }
            let readable = PAGE_READONLY
                | PAGE_READWRITE
                | PAGE_WRITECOPY
                | PAGE_EXECUTE_READ
                | PAGE_EXECUTE_READWRITE
                | PAGE_EXECUTE_WRITECOPY;
            if info.Protect & readable == 0 || info.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
                return false;
            }
            let start = info.BaseAddress as usize;
            let region_end = start + info.RegionSize;
            self.readable.push((start, region_end));
            at = region_end;
        }
        true
    }

    pub fn read<T: Copy>(&mut self, address: usize) -> Option<T> {
        // SAFETY: the range was just found committed and readable, and the caller reads
        // only objects the game does not change during the read.
        self.readable(address, size_of::<T>())
            .then(|| unsafe { std::ptr::read_unaligned(address as *const T) })
    }

    /// An 8-byte-aligned pointer to at least `length` readable bytes.
    pub fn object(&mut self, address: usize, length: usize) -> bool {
        address.is_multiple_of(8) && self.readable(address, length)
    }

    /// An MSVC `std::string`, cut to `max` bytes.
    pub fn string(&mut self, address: usize, max: usize) -> Option<String> {
        let (buffer, size, capacity) = self.read::<([u8; 16], usize, usize)>(address)?;
        // An MSVC string always has room for at least its 15-byte inline buffer.
        if capacity < size || !(15..=1 << 30).contains(&capacity) {
            return None;
        }
        let data = if capacity >= 16 {
            usize::from_le_bytes(buffer[..8].try_into().ok()?)
        } else {
            address
        };
        let length = size.min(max);
        if !self.readable(data, length) {
            return None;
        }
        // SAFETY: checked readable above.
        let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
        let text = String::from_utf8_lossy(bytes);
        Some(
            text.chars()
                .filter(|character| !character.is_control())
                .collect(),
        )
    }

    /// An MSVC `std::vector` of `element`-byte items: (first, count).
    pub fn vector(&mut self, address: usize, element: usize, max: usize) -> Option<(usize, usize)> {
        let [first, last, end] = self.read::<[usize; 3]>(address)?;
        if first == 0 && last == 0 {
            return Some((0, 0));
        }
        if first > last || last > end || (last - first) % element != 0 {
            return None;
        }
        let count = (last - first) / element;
        (count <= max && self.readable(first, count * element)).then_some((first, count))
    }
}
