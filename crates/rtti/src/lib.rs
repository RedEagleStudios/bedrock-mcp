//! Finds C++ classes' virtual-function tables in a 64-bit MSVC PE image.
//!
//! MSVC emits RTTI for polymorphic classes: each vtable is preceded by a
//! pointer to a `CompleteObjectLocator`, which points at the class's
//! `TypeDescriptor` holding its decorated name (`.?AVClientInstance@@`).
//! Walking the locators once indexes every class by name, so callers find a
//! vtable by class name instead of by address or byte pattern. Names survive
//! game updates; slot numbers usually do too, but callers must check them.
//! A build without RTTI keeps no names; [`anchors`] finds vtables there.
//!
//! An [`Image`] reads either a file (for offline analysis) or the mapped
//! module inside the process (pointers then hold the relocated base).

use std::collections::HashMap;

pub mod anchors;

/// x64 `RTTICompleteObjectLocator` signature.
const LOCATOR_SIGNATURE: u32 = 1;
const LOCATOR_SIZE: usize = 24;
/// Offset of the decorated name inside a `TypeDescriptor`.
const TYPE_NAME_OFFSET: u32 = 16;
const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const MAX_NAME: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotPe(&'static str),
    Truncated,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotPe(what) => write!(f, "not a 64-bit PE image: {what}"),
            Error::Truncated => f.write_str("the PE image is truncated"),
        }
    }
}

impl std::error::Error for Error {}

/// One section's bytes as mapped at `rva`.
#[derive(Clone, Copy)]
pub struct Section<'a> {
    pub name: [u8; 8],
    pub rva: u32,
    pub bytes: &'a [u8],
    pub executable: bool,
}

/// A PE image: where it is based and its sections' bytes by RVA.
pub struct Image<'a> {
    /// Base the image's absolute pointers are relative to: the preferred
    /// `ImageBase` for a file, the load address in memory.
    pub base: u64,
    pub sections: Vec<Section<'a>>,
    /// RVA range of the exception directory: one `RUNTIME_FUNCTION` per
    /// function or function fragment, sorted by start.
    pub exceptions: std::ops::Range<u32>,
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(
        bytes
            .get(at..at + 2)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(
        bytes
            .get(at..at + 8)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap(),
    ))
}

/// Header fields both sources need.
struct Headers {
    image_base: u64,
    sections: Vec<(
        /* name */ [u8; 8],
        /* rva */ u32,
        /* virtual size */ u32,
        /* raw offset */ u32,
        /* raw size */ u32,
        /* flags */ u32,
    )>,
    /// RVA range of the exception directory (`.pdata` entries).
    exceptions: std::ops::Range<u32>,
}

fn headers(bytes: &[u8]) -> Result<Headers, Error> {
    if bytes.get(..2) != Some(b"MZ") {
        return Err(Error::NotPe("no MZ header"));
    }
    let pe = u32_at(bytes, 0x3c)? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err(Error::NotPe("no PE signature"));
    }
    let count = u16_at(bytes, pe + 6)? as usize;
    let optional_size = u16_at(bytes, pe + 20)? as usize;
    let optional = pe + 24;
    if u16_at(bytes, optional)? != 0x20b {
        return Err(Error::NotPe("not PE32+"));
    }
    let image_base = u64_at(bytes, optional + 24)?;
    // Data directory 3 is the exception directory.
    let exceptions = if u32_at(bytes, optional + 108)? > 3 {
        let rva = u32_at(bytes, optional + 112 + 3 * 8)?;
        rva..rva + u32_at(bytes, optional + 112 + 3 * 8 + 4)?
    } else {
        0..0
    };
    let table = optional + optional_size;
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        let at = table + index * 40;
        let name: [u8; 8] = bytes
            .get(at..at + 8)
            .ok_or(Error::Truncated)?
            .try_into()
            .unwrap();
        sections.push((
            name,
            u32_at(bytes, at + 12)?,
            u32_at(bytes, at + 8)?,
            u32_at(bytes, at + 20)?,
            u32_at(bytes, at + 16)?,
            u32_at(bytes, at + 36)?,
        ));
    }
    Ok(Headers {
        image_base,
        sections,
        exceptions,
    })
}

impl<'a> Image<'a> {
    /// An image read from a file's bytes (sections at their raw offsets).
    pub fn from_file(bytes: &'a [u8]) -> Result<Self, Error> {
        let headers = headers(bytes)?;
        let sections = headers
            .sections
            .iter()
            .map(|&(name, rva, virtual_size, raw, raw_size, flags)| {
                let length = raw_size.min(virtual_size.max(raw_size)) as usize;
                let start = (raw as usize).min(bytes.len());
                let end = start.saturating_add(length).min(bytes.len());
                Section {
                    name,
                    rva,
                    bytes: &bytes[start..end],
                    executable: flags & IMAGE_SCN_MEM_EXECUTE != 0,
                }
            })
            .collect();
        Ok(Image {
            base: headers.image_base,
            sections,
            exceptions: headers.exceptions,
        })
    }

    /// The image mapped at `base` in this process.
    ///
    /// # Safety
    /// `base` must be the start of a loaded module that stays mapped for `'a`.
    pub unsafe fn from_memory(base: *const u8) -> Result<Self, Error> {
        // SAFETY: the caller guarantees a mapped module; its headers fit in one page.
        let header = unsafe { std::slice::from_raw_parts(base, 4096) };
        let headers = headers(header)?;
        let sections = headers
            .sections
            .iter()
            .map(|&(name, rva, virtual_size, _, _, flags)| Section {
                name,
                rva,
                // SAFETY: every section of a loaded module is mapped for its virtual size.
                bytes: unsafe {
                    std::slice::from_raw_parts(base.add(rva as usize), virtual_size as usize)
                },
                executable: flags & IMAGE_SCN_MEM_EXECUTE != 0,
            })
            .collect();
        Ok(Image {
            base: base as u64,
            sections,
            exceptions: headers.exceptions,
        })
    }

    fn section_of(&self, rva: u32) -> Option<&Section<'a>> {
        self.sections.iter().find(|section| {
            rva >= section.rva && ((rva - section.rva) as usize) < section.bytes.len()
        })
    }

    /// Bytes at `rva`, if mapped.
    pub fn at(&self, rva: u32, length: usize) -> Option<&'a [u8]> {
        let section = self.section_of(rva)?;
        let start = (rva - section.rva) as usize;
        section.bytes.get(start..start.checked_add(length)?)
    }

    fn u32(&self, rva: u32) -> Option<u32> {
        Some(u32::from_le_bytes(self.at(rva, 4)?.try_into().ok()?))
    }

    fn u64(&self, rva: u32) -> Option<u64> {
        Some(u64::from_le_bytes(self.at(rva, 8)?.try_into().ok()?))
    }

    /// The RVA an absolute pointer refers to, if it falls inside the image.
    pub fn rva_of(&self, pointer: u64) -> Option<u32> {
        let rva = u32::try_from(pointer.checked_sub(self.base)?).ok()?;
        self.section_of(rva).map(|_| rva)
    }

    /// Whether `rva` lies in an executable section.
    pub fn is_code(&self, rva: u32) -> bool {
        self.section_of(rva)
            .is_some_and(|section| section.executable)
    }

    fn c_string(&self, rva: u32) -> Option<&'a str> {
        let section = self.section_of(rva)?;
        let start = (rva - section.rva) as usize;
        let tail = &section.bytes[start..section.bytes.len().min(start + MAX_NAME)];
        let end = tail.iter().position(|&byte| byte == 0)?;
        std::str::from_utf8(&tail[..end]).ok()
    }
}

/// One vtable of a class: classes with several bases have one per base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vtable {
    /// Decorated RTTI name, e.g. `.?AVClientInstance@@`.
    pub name: String,
    /// Offset of the subobject this vtable belongs to; 0 is the primary one.
    pub offset: u32,
    /// RVA of slot 0.
    pub rva: u32,
    /// RVAs of the functions in each slot, until the first non-code entry.
    pub slots: Vec<u32>,
}

/// The decorated name MSVC uses for a class or struct: `ClientInstance` →
/// `.?AVClientInstance@@`, `Automation::AutomationClient` →
/// `.?AVAutomationClient@Automation@@`.
pub fn decorated(name: &str, is_struct: bool) -> String {
    let mut parts: Vec<&str> = name.split("::").collect();
    parts.reverse();
    format!(
        ".?A{}{}@@",
        if is_struct { 'U' } else { 'V' },
        parts.join("@")
    )
}

/// Every class with RTTI in the image, by decorated name.
pub fn index(image: &Image<'_>) -> HashMap<String, Vec<Vtable>> {
    // Locators are 4-byte aligned in read-only data: signature, offset,
    // cdOffset, TypeDescriptor RVA, ClassHierarchyDescriptor RVA, self RVA.
    let mut locators: HashMap<u64, (String, u32)> = HashMap::new();
    for section in image.sections.iter().filter(|section| !section.executable) {
        let bytes = section.bytes;
        let mut at = 0;
        while at + LOCATOR_SIZE <= bytes.len() {
            let word = |offset: usize| {
                u32::from_le_bytes(bytes[at + offset..at + offset + 4].try_into().unwrap())
            };
            if word(0) == LOCATOR_SIGNATURE
                && word(20) == section.rva + at as u32
                && let Some(name) = image
                    .c_string(word(12) + TYPE_NAME_OFFSET)
                    .filter(|name| name.starts_with(".?A"))
            {
                let locator = image.base + u64::from(section.rva) + at as u64;
                locators.insert(locator, (name.to_owned(), word(4)));
            }
            at += 4;
        }
    }
    // A vtable is preceded by a pointer to its locator.
    let mut classes: HashMap<String, Vec<Vtable>> = HashMap::new();
    for section in image.sections.iter().filter(|section| !section.executable) {
        let bytes = section.bytes;
        let mut at = 0;
        while at + 16 <= bytes.len() {
            let pointer = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            if let Some((name, offset)) = locators.get(&pointer) {
                let rva = section.rva + at as u32 + 8;
                let slots = slots(image, rva);
                if !slots.is_empty() {
                    classes.entry(name.clone()).or_default().push(Vtable {
                        name: name.clone(),
                        offset: *offset,
                        rva,
                        slots,
                    });
                }
            }
            at += 8;
        }
    }
    for tables in classes.values_mut() {
        tables.sort_by_key(|table| (table.offset, table.rva));
    }
    classes
}

fn slots(image: &Image<'_>, vtable: u32) -> Vec<u32> {
    let mut slots = Vec::new();
    let mut rva = vtable;
    while let Some(target) = image.u64(rva).and_then(|pointer| image.rva_of(pointer)) {
        if !image.is_code(target) {
            break;
        }
        slots.push(target);
        rva += 8;
    }
    slots
}

/// Read a 32-bit value at `rva`; exposed for capability self-checks.
pub fn read_u32(image: &Image<'_>, rva: u32) -> Option<u32> {
    image.u32(rva)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decorated_names_follow_msvc_order() {
        assert_eq!(decorated("ClientInstance", false), ".?AVClientInstance@@");
        assert_eq!(
            decorated("Automation::AutomationClient", false),
            ".?AVAutomationClient@Automation@@"
        );
        assert_eq!(
            decorated("ISceneStack::SceneElement", true),
            ".?AUSceneElement@ISceneStack@@"
        );
    }

    /// A minimal image: .text at 0x1000, .rdata at 0x2000 with a type
    /// descriptor, a locator and a three-slot vtable.
    fn synthetic() -> (Vec<u8>, Vec<u8>) {
        let base = 0x1_4000_0000u64;
        let text = vec![0xC3u8; 0x100];
        let mut rdata = vec![0u8; 0x200];
        let put32 = |data: &mut Vec<u8>, at: usize, value: u32| {
            data[at..at + 4].copy_from_slice(&value.to_le_bytes())
        };
        let put64 = |data: &mut Vec<u8>, at: usize, value: u64| {
            data[at..at + 8].copy_from_slice(&value.to_le_bytes())
        };
        // TypeDescriptor at 0x2000: name at +16.
        rdata[16..16 + 19].copy_from_slice(b".?AVClientInstance@");
        rdata[35..37].copy_from_slice(b"@\0");
        // Locator at 0x2080.
        put32(&mut rdata, 0x80, LOCATOR_SIGNATURE);
        put32(&mut rdata, 0x84, 0);
        put32(&mut rdata, 0x8c, 0x2000);
        put32(&mut rdata, 0x94, 0x2080);
        // Vtable meta pointer at 0x2100, slots from 0x2108, then a non-code entry.
        put64(&mut rdata, 0x100, base + 0x2080);
        for (index, target) in [0x1000u64, 0x1010, 0x1020].iter().enumerate() {
            put64(&mut rdata, 0x108 + index * 8, base + target);
        }
        put64(&mut rdata, 0x120, base + 0x2000);
        (text, rdata)
    }

    #[test]
    fn index_finds_a_vtable_through_its_locator() {
        let (text, rdata) = synthetic();
        let image = Image {
            base: 0x1_4000_0000,
            sections: vec![
                Section {
                    name: *b".text\0\0\0",
                    rva: 0x1000,
                    bytes: &text,
                    executable: true,
                },
                Section {
                    name: *b".rdata\0\0",
                    rva: 0x2000,
                    bytes: &rdata,
                    executable: false,
                },
            ],
            exceptions: 0..0,
        };
        let classes = index(&image);
        let tables = &classes[".?AVClientInstance@@"];
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rva, 0x2108);
        assert_eq!(tables[0].slots, vec![0x1000, 0x1010, 0x1020]);
    }
}
