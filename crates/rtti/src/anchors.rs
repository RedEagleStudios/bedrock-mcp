//! Finds vtables without RTTI, through what their functions use.
//!
//! A build without RTTI keeps no class names, but its code still loads the
//! same string literals, and the `std::function` lambdas it builds keep type
//! names that spell out the function declaring them. A [`Source`] is one of
//! those, and an [`Anchor`] ties a source to a vtable: one slot's function
//! uses it (or calls a function that does), or a function that uses it
//! stores the vtable into an object.
//!
//! Functions get their extent from the exception directory: every function
//! that can unwind has a `RUNTIME_FUNCTION`, and its cold fragments chain back
//! to it. A vtable is a run of code pointers in `.rdata` that code loads with
//! a RIP-relative `lea` and stores into an object at the subobject's offset.
//! It ends where code loads the next word as another vtable, or where the
//! code pointers end; no code loads a word inside it. A rule asks for an
//! exact count, or for a minimum where the class may gain virtuals between
//! game versions. A rule that matches no run, or more than one, leaves its
//! class unavailable. Nothing in a rule depends on one build's addresses.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use crate::{Image, Section, Vtable};

mod rules;

pub use rules::RULES;

/// Something code uses that names its purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// Loads the address of this NUL-terminated string literal.
    Literal(&'static str),
    /// Builds a `std::function` from a lambda declared in this function,
    /// written `Class::method` (with namespaces, if any).
    Lambda(&'static str),
    /// Calls this function, imported by its decorated name, through its
    /// import address table slot (`call [rip+slot]`). A build without RTTI
    /// still imports the DLLs it links by name.
    Import(&'static str),
}

/// One fact a vtable must satisfy.
#[derive(Clone, Copy, Debug)]
pub enum Anchor {
    /// The function in `slot` uses `source`.
    Slot { slot: usize, source: Source },
    /// The function in `slot` uses `source`, or reaches a function that does
    /// in at most `depth` direct calls: a game version can move the use into a
    /// helper of its own.
    Calls {
        slot: usize,
        source: Source,
        depth: u8,
    },
    /// Some slot holds a function that uses `source`: for a class that may gain
    /// virtuals between game versions, where the slot moves. First, it starts
    /// a candidate at every word before such a slot, within [`MAX_SLOTS`],
    /// that code loads; the bounds keep the vtable that holds the slot.
    Holds { source: Source },
    /// A function that uses `source`, or one it reaches in at most `depth`
    /// direct calls, stores the vtable at the rule's offset.
    Stored { source: Source, depth: u8 },
    /// Next to every place a function stores the vtable of the rule for
    /// `class`/`offset` (an earlier rule), it stores this vtable through the
    /// same register at this rule's offset: both are subobjects of one
    /// object. Requiring every store skips the base-class vtables a
    /// constructor stores first and then overwrites.
    Sibling { class: &'static str, offset: u32 },
    /// The vtable starts right after the vtable of the rule for
    /// `class`/`offset` (an earlier rule), at most one word later: the
    /// compiler emits the vtables of one source file together, and a build
    /// with RTTI puts a locator pointer between them.
    After { class: &'static str, offset: u32 },
    /// Next to every place code stores the vtable at the rule's offset, it
    /// stores a run of `slots` code pointers through the same register at
    /// `offset`: the object has a base with that many slots there.
    Beside { offset: u32, slots: usize },
}

/// How many slots a vtable must have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slots {
    /// Exactly this many.
    Exact(usize),
    /// At least this many: the class may gain virtuals between game versions, so
    /// the vtable ends where the next one starts or the code pointers end.
    AtLeast(usize),
}

impl Slots {
    /// Whether a vtable of `count` slots satisfies this.
    pub fn admits(self, count: usize) -> bool {
        match self {
            Slots::Exact(slots) => count == slots,
            Slots::AtLeast(slots) => count >= slots,
        }
    }

    fn min(self) -> usize {
        match self {
            Slots::Exact(slots) | Slots::AtLeast(slots) => slots,
        }
    }

    /// Code pointers to read from a candidate's start.
    fn span(self) -> usize {
        match self {
            Slots::Exact(slots) => slots,
            Slots::AtLeast(_) => MAX_SLOTS,
        }
    }
}

impl fmt::Display for Slots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Slots::Exact(slots) => write!(f, "{slots}"),
            Slots::AtLeast(slots) => write!(f, "at least {slots}"),
        }
    }
}

/// How to find one vtable of `class` without RTTI.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    /// The class name as in source, e.g. `SceneStack`.
    pub class: &'static str,
    /// Offset of the subobject this vtable belongs to; 0 is the primary one.
    pub offset: u32,
    pub slots: Slots,
    /// Wrapper templates can reuse the class's functions in vtables of their
    /// own. With `Some(slot)`, runs that all hold one function in `slot`
    /// resolve together as a [`Outcome::Family`] instead of being ambiguous.
    pub shared: Option<usize>,
    /// Every anchor must hold. The first one picks the candidates.
    pub anchors: &'static [Anchor],
}

const UNW_FLAG_CHAININFO: u8 = 0x4;
const RUNTIME_FUNCTION_SIZE: usize = 12;
/// Chained fragments deeper than this are malformed.
const MAX_CHAIN: usize = 32;
/// Bytes read from a leaf function, which has no listed extent.
const LEAF_SPAN: u32 = 256;
/// `std::_Func_base::_Target_type`'s slot in a `std::function` target's vtable.
const TARGET_TYPE_SLOT: u32 = 3;
/// A sibling's store must be this close to the referent's.
const SIBLING_SPAN: u32 = 128;
/// A lambda's type name ends at most this far after the start of its prefix.
const MAX_LAMBDA_PREFIX: usize = 64;
/// A vtable longer than this is a run of several.
const MAX_SLOTS: usize = 4096;
/// An import name table longer than this is malformed.
const MAX_IMPORTS: u32 = 1 << 16;

/// The functions the exception directory describes.
pub struct Functions<'i, 'a> {
    image: &'i Image<'a>,
    table: &'a [u8],
}

impl<'i, 'a> Functions<'i, 'a> {
    pub fn new(image: &'i Image<'a>) -> Self {
        let range = &image.exceptions;
        let table = image
            .at(range.start, range.end.saturating_sub(range.start) as usize)
            .unwrap_or(&[]);
        Functions { image, table }
    }

    pub fn len(&self) -> usize {
        self.table.len() / RUNTIME_FUNCTION_SIZE
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn entry(&self, index: usize) -> (u32, u32, u32) {
        let at = index * RUNTIME_FUNCTION_SIZE;
        let word = |offset: usize| {
            u32::from_le_bytes(self.table[at + offset..at + offset + 4].try_into().unwrap())
        };
        (word(0), word(4), word(8))
    }

    /// Index of the first entry that ends after `rva`.
    fn search(&self, rva: u32) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.entry(middle).1 <= rva {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// The fragment `[start, end)` and unwind info RVA covering `rva`.
    fn fragment(&self, rva: u32) -> Option<(u32, u32, u32)> {
        let index = self.search(rva);
        (index < self.len())
            .then(|| self.entry(index))
            .filter(|&(start, _, _)| start <= rva)
    }

    /// The primary fragment a fragment with this unwind info chains to.
    fn root(&self, mut start: u32, mut unwind: u32) -> Option<u32> {
        for _ in 0..MAX_CHAIN {
            let header = self.image.at(unwind, 4)?;
            if header[0] >> 3 & UNW_FLAG_CHAININFO == 0 {
                return Some(start);
            }
            // The chained RUNTIME_FUNCTION follows the unwind codes, padded
            // to an even count.
            let codes = u32::from(header[2]).next_multiple_of(2);
            let chained = self.image.at(unwind + 4 + codes * 2, 12)?;
            let word =
                |offset: usize| u32::from_le_bytes(chained[offset..offset + 4].try_into().unwrap());
            start = word(0);
            unwind = word(8);
        }
        None
    }

    /// Start of the function `rva` belongs to, following chained fragments
    /// back to the primary one.
    pub fn start_of(&self, rva: u32) -> Option<u32> {
        match self.fragment(rva) {
            Some((start, _, unwind)) => self.root(start, unwind),
            None => self.leaf_start(rva),
        }
    }

    /// A leaf function needs no unwind data, so the exception directory
    /// leaves it out. An instruction outside every listed function starts a
    /// leaf function when it sits where the compiler starts functions: after
    /// `int3` padding or on a 16-byte boundary. Inside a leaf, this guess can
    /// only name a start no vtable slot or call points at.
    fn leaf_start(&self, rva: u32) -> Option<u32> {
        let padded = rva
            .checked_sub(1)
            .and_then(|before| self.image.at(before, 1))
            .is_some_and(|before| before[0] == 0xCC);
        (self.image.is_code(rva) && (padded || rva.is_multiple_of(16))).then_some(rva)
    }

    /// The byte ranges of each function in `roots`: its fragments, or for a
    /// leaf, up to [`LEAF_SPAN`] bytes until the next listed function.
    pub fn extents(&self, roots: &HashSet<u32>) -> HashMap<u32, Vec<(u32, u32)>> {
        let mut extents: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        if roots.is_empty() {
            return extents;
        }
        for index in 0..self.len() {
            let (start, end, unwind) = self.entry(index);
            if let Some(root) = self.root(start, unwind).filter(|root| roots.contains(root)) {
                extents.entry(root).or_default().push((start, end));
            }
        }
        for &root in roots {
            extents.entry(root).or_insert_with(|| {
                let index = self.search(root);
                let next = if index < self.len() {
                    self.entry(index).0
                } else {
                    u32::MAX
                };
                vec![(root, root.saturating_add(LEAF_SPAN).min(next))]
            });
        }
        extents
    }
}

fn rdata<'i, 'a>(image: &'i Image<'a>) -> impl Iterator<Item = &'i Section<'a>> {
    image
        .sections
        .iter()
        .filter(|section| section.name.starts_with(b".rdata"))
}

/// Every place in `.rdata` holding each string followed by its NUL.
pub fn find_strings<'s>(image: &Image<'_>, strings: &[&'s str]) -> HashMap<&'s str, Vec<u32>> {
    let mut found = HashMap::new();
    for &string in strings {
        let mut needle = Vec::with_capacity(string.len() + 1);
        needle.extend_from_slice(string.as_bytes());
        needle.push(0);
        let finder = memchr::memmem::Finder::new(&needle);
        let mut places = Vec::new();
        for section in rdata(image) {
            places.extend(
                finder
                    .find_iter(section.bytes)
                    .map(|at| section.rva + at as u32),
            );
        }
        found.insert(string, places);
    }
    found
}

/// The decorated-name fragment of a function as it appears inside the type
/// names of lambdas it declares: `SceneStack::update` → `??update@SceneStack@@`.
fn lambda_scope(function: &str) -> String {
    let mut parts: Vec<&str> = function.split("::").collect();
    parts.reverse();
    format!("??{}@@", parts.join("@"))
}

/// For each function, the type descriptors of the lambdas it declares (MSVC
/// keeps them for `std::function::target_type`): each one's name is
/// `.?AV<lambda_N>@?0??method@Class@@...`, 16 bytes into the descriptor.
pub fn find_lambdas<'s>(image: &Image<'_>, functions: &[&'s str]) -> HashMap<&'s str, Vec<u32>> {
    const PREFIX: &[u8] = b".?AV<lambda_";
    let mut found = HashMap::new();
    for &function in functions {
        let scope = lambda_scope(function);
        let finder = memchr::memmem::Finder::new(scope.as_bytes());
        let mut descriptors = Vec::new();
        for section in image.sections.iter().filter(|section| !section.executable) {
            let bytes = section.bytes;
            for at in finder.find_iter(bytes) {
                let window = &bytes[at.saturating_sub(MAX_LAMBDA_PREFIX)..at];
                let Some(start) = memchr::memmem::rfind(window, PREFIX) else {
                    continue;
                };
                let name = at - window.len() + start;
                if !bytes[name..at].contains(&0)
                    && bytes[name..at].ends_with(b">@?0")
                    && let Some(descriptor) = (section.rva + name as u32).checked_sub(16)
                {
                    descriptors.push(descriptor);
                }
            }
        }
        found.insert(function, descriptors);
    }
    found
}

/// A RIP-relative `lea r64, [rip+disp32]` at `at`: its register and target.
fn lea_at(bytes: &[u8], at: usize, rva: u32) -> Option<(u8, u32)> {
    let code = bytes.get(at..at + 7)?;
    if code[0] & 0xFB != 0x48 || code[1] != 0x8D || code[2] & 0xC7 != 0x05 {
        return None;
    }
    let register = (code[0] >> 2 & 1) << 3 | (code[2] >> 3 & 7);
    let displacement = i32::from_le_bytes(code[3..7].try_into().unwrap());
    Some((register, (rva + 7).wrapping_add_signed(displacement)))
}

/// For each target in `targets`, the RVAs of the RIP-relative `lea`
/// instructions in executable sections that load it.
///
/// It scans bytes, not decoded instructions: a match inside another
/// instruction must still hit one of the targets exactly to count, which the
/// callers' further checks make harmless.
pub fn lea_references(image: &Image<'_>, targets: &HashSet<u32>) -> HashMap<u32, Vec<u32>> {
    let mut found: HashMap<u32, Vec<u32>> = HashMap::new();
    if targets.is_empty() {
        return found;
    }
    for section in image.sections.iter().filter(|section| section.executable) {
        let bytes = section.bytes;
        for opcode in memchr::memchr_iter(0x8D, bytes) {
            let Some(at) = opcode.checked_sub(1) else {
                continue;
            };
            let rva = section.rva + at as u32;
            if let Some((_, target)) = lea_at(bytes, at, rva)
                && targets.contains(&target)
            {
                found.entry(target).or_default().push(rva);
            }
        }
    }
    found
}

/// For each function RVA in `functions`, the 8-byte-aligned `.rdata` words
/// that point at it.
pub fn pointers_to(image: &Image<'_>, functions: &HashSet<u32>) -> HashMap<u32, Vec<u32>> {
    let mut found: HashMap<u32, Vec<u32>> = HashMap::new();
    let (Some(&low), Some(&high)) = (functions.iter().min(), functions.iter().max()) else {
        return found;
    };
    let (low, high) = (image.base + u64::from(low), image.base + u64::from(high));
    for section in rdata(image) {
        let skip = (section.rva.next_multiple_of(8) - section.rva) as usize;
        let Some(words) = section.bytes.get(skip..) else {
            continue;
        };
        for (index, word) in words.as_chunks::<8>().0.iter().enumerate() {
            let pointer = u64::from_le_bytes(*word);
            if pointer < low || pointer > high {
                continue;
            }
            let target = (pointer - image.base) as u32;
            if functions.contains(&target) {
                found
                    .entry(target)
                    .or_default()
                    .push(section.rva + (skip + index * 8) as u32);
            }
        }
    }
    found
}

/// The function a code pointer in `.rdata` holds, if it points into code.
fn code_pointer(image: &Image<'_>, rva: u32) -> Option<u32> {
    let target = image.rva_of(image.u64(rva)?)?;
    image.is_code(target).then_some(target)
}

/// The `slots` code pointers starting at `start`, if all are.
fn run(image: &Image<'_>, start: u32, slots: usize) -> Option<Vec<u32>> {
    (0..slots as u32)
        .map(|index| code_pointer(image, start + index * 8))
        .collect()
}

/// The code pointers starting at `start`, at most `slots`, up to the first
/// word that is not one.
fn prefix(image: &Image<'_>, start: u32, slots: usize) -> Vec<u32> {
    (0..slots as u32)
        .map_while(|index| code_pointer(image, start + index * 8))
        .collect()
}

/// One `mov` with a REX.W prefix.
struct Mov {
    /// `0x89` stores the register, `0x8B` loads it.
    opcode: u8,
    register: u8,
    /// Base register and displacement; `None` for RIP-relative or register
    /// operands.
    memory: Option<(u8, i32)>,
    length: usize,
}

/// The `mov` at `at`, if it is one. Indexed addressing is not decoded.
fn mov_at(code: &[u8], at: usize) -> Option<Mov> {
    let bytes = code.get(at..)?;
    let (rex, opcode, modrm) = (*bytes.first()?, *bytes.get(1)?, *bytes.get(2)?);
    if rex & 0xF8 != 0x48 || !matches!(opcode, 0x89 | 0x8B) {
        return None;
    }
    let mode = modrm >> 6;
    let register = (rex >> 2 & 1) << 3 | (modrm >> 3 & 7);
    let mov = |memory, length| {
        Some(Mov {
            opcode,
            register,
            memory,
            length,
        })
    };
    if mode == 3 {
        return mov(None, 3);
    }
    let mut length = 3;
    let mut base = modrm & 7;
    if base == 4 {
        let sib = *bytes.get(3)?;
        length += 1;
        if (rex >> 1 & 1) << 3 | (sib >> 3 & 7) != 4 || (sib & 7 == 5 && mode == 0) {
            return None;
        }
        base = sib & 7;
    } else if base == 5 && mode == 0 {
        return mov(None, 7);
    }
    let base = (rex & 1) << 3 | base;
    let displacement = match mode {
        0 => 0,
        1 => {
            length += 1;
            i32::from(*bytes.get(length - 1)? as i8)
        }
        _ => {
            length += 4;
            i32::from_le_bytes(bytes.get(length - 4..length)?.try_into().unwrap())
        }
    };
    mov(Some((base, displacement)), length)
}

/// After the `lea` at `at` that loads a vtable, the store that puts it into
/// an object: `mov [base+disp], reg`, possibly after one `mov` that loads the
/// base register.
fn store_after(code: &[u8], at: usize) -> Option<(u8, i32)> {
    let (register, _) = lea_at(code, at, 0)?;
    let mut next = at + 7;
    for _ in 0..2 {
        let mov = mov_at(code, next)?;
        match (mov.opcode, mov.memory) {
            (0x89, Some(store)) if mov.register == register => return Some(store),
            (0x8B, _) if mov.register != register => next += mov.length,
            _ => return None,
        }
    }
    None
}

/// Code bytes at `rva` and the index of `rva` in them.
fn code_around<'a>(image: &Image<'a>, rva: u32) -> Option<(&'a [u8], usize)> {
    let section = image.sections.iter().find(|section| {
        section.executable
            && rva >= section.rva
            && ((rva - section.rva) as usize) < section.bytes.len()
    })?;
    Some((section.bytes, (rva - section.rva) as usize))
}

/// The store after the `lea` at `rva`.
fn store_at(image: &Image<'_>, rva: u32) -> Option<(u8, i32)> {
    let (code, at) = code_around(image, rva)?;
    store_after(code, at)
}

/// Every `lea` + object store inside `extent`: (lea RVA, target, base, disp).
fn stores_in(image: &Image<'_>, (start, end): (u32, u32)) -> Vec<(u32, u32, u8, i32)> {
    let Some((code, first)) = code_around(image, start) else {
        return Vec::new();
    };
    let last = (first + (end - start) as usize).min(code.len());
    let mut found = Vec::new();
    for opcode in memchr::memchr_iter(0x8D, &code[first..last]) {
        let Some(at) = (first + opcode).checked_sub(1) else {
            continue;
        };
        let rva = start - first as u32 + at as u32;
        if let Some((_, target)) = lea_at(code, at, rva)
            && let Some((base, displacement)) = store_after(code, at)
        {
            found.push((rva, target, base, displacement));
        }
    }
    found
}

/// Direct call targets (`call rel32`) inside `extent` that start functions.
fn callees(image: &Image<'_>, functions: &Functions<'_, '_>, (start, end): (u32, u32)) -> Vec<u32> {
    let Some((code, first)) = code_around(image, start) else {
        return Vec::new();
    };
    let last = (first + (end - start) as usize).min(code.len());
    memchr::memchr_iter(0xE8, &code[first..last])
        .filter_map(|offset| {
            let at = first + offset;
            let displacement = i32::from_le_bytes(code.get(at + 1..at + 5)?.try_into().unwrap());
            let target = (start + offset as u32 + 5).wrapping_add_signed(displacement);
            (functions.start_of(target) == Some(target)).then_some(target)
        })
        .collect()
}

/// Functions with a direct call (`call rel32`) to a function in `targets`.
fn callers(
    image: &Image<'_>,
    functions: &Functions<'_, '_>,
    targets: &HashSet<u32>,
) -> HashSet<u32> {
    let mut found = HashSet::new();
    if targets.is_empty() {
        return found;
    }
    for section in image.sections.iter().filter(|section| section.executable) {
        let code = section.bytes;
        for at in memchr::memchr_iter(0xE8, code) {
            let Some(bytes) = code.get(at + 1..at + 5) else {
                continue;
            };
            let rva = section.rva + at as u32;
            let target =
                (rva + 5).wrapping_add_signed(i32::from_le_bytes(bytes.try_into().unwrap()));
            if targets.contains(&target)
                && let Some(start) = functions.start_of(rva)
            {
                found.insert(start);
            }
        }
    }
    found
}

/// For each decorated name, the import address table slots it is imported
/// through. The import name table keeps each name's hint/name entry (a
/// 2-byte hint, then the name) after the loader fills the address table, and
/// the import descriptor that lists a name table gives the address table that
/// runs beside it. A file's unbound address table holds the same entries.
pub fn find_imports<'s>(image: &Image<'_>, names: &[&'s str]) -> HashMap<&'s str, Vec<u32>> {
    let u32_at = |rva: u32| Some(u32::from_le_bytes(image.at(rva, 4)?.try_into().ok()?));
    // Whether the import descriptor at `descriptor` names a DLL.
    let descriptor = |descriptor: u32| {
        u32_at(descriptor + 12)
            .and_then(|name| image.c_string(name))
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(".dll"))
    };
    // Aligned places in data sections holding `value`, `size` bytes wide.
    let holding = |value: u64, size: usize| -> Vec<u32> {
        let needle = &value.to_le_bytes()[..size];
        let finder = memchr::memmem::Finder::new(needle);
        image
            .sections
            .iter()
            .filter(|section| !section.executable)
            .flat_map(|section| {
                finder
                    .find_iter(section.bytes)
                    .map(move |at| section.rva + at as u32)
            })
            .filter(|rva| rva % size as u32 == 0)
            .collect()
    };
    let mut found = HashMap::new();
    for (name, places) in find_strings(image, names) {
        let mut slots = Vec::new();
        for entry in places.iter().filter_map(|place| place.checked_sub(2)) {
            for thunk in holding(u64::from(entry), 8) {
                // The thunk array runs back to the word after the previous
                // array's terminator.
                let mut first = thunk;
                while thunk - first < MAX_IMPORTS * 8
                    && let Some(previous) = first.checked_sub(8)
                    && image.u64(previous).is_some_and(|word| word != 0)
                {
                    first = previous;
                }
                for field in holding(u64::from(first), 4) {
                    // `OriginalFirstThunk` at +0 of a descriptor: the
                    // address table is its `FirstThunk` at +16. Or the
                    // `FirstThunk` itself: the thunk is the slot.
                    if descriptor(field)
                        && let Some(addresses) = u32_at(field + 16)
                    {
                        slots.push(addresses + (thunk - first));
                    }
                    if field.checked_sub(16).is_some_and(descriptor) {
                        slots.push(thunk);
                    }
                }
            }
        }
        slots.retain(|&slot| image.at(slot, 8).is_some());
        slots.sort_unstable();
        slots.dedup();
        found.insert(name, slots);
    }
    found
}

/// For each slot in `slots`, the RVAs of the `call [rip+disp32]`
/// instructions in executable sections that call through it.
pub fn call_references(image: &Image<'_>, slots: &HashSet<u32>) -> HashMap<u32, Vec<u32>> {
    let mut found: HashMap<u32, Vec<u32>> = HashMap::new();
    if slots.is_empty() {
        return found;
    }
    for section in image.sections.iter().filter(|section| section.executable) {
        let code = section.bytes;
        for at in memchr::memchr_iter(0xFF, code) {
            let Some(bytes) = code.get(at + 1..at + 6) else {
                continue;
            };
            if bytes[0] != 0x15 {
                continue;
            }
            let rva = section.rva + at as u32;
            let displacement = i32::from_le_bytes(bytes[1..].try_into().unwrap());
            let slot = (rva + 6).wrapping_add_signed(displacement);
            if slots.contains(&slot) {
                found.entry(slot).or_default().push(rva);
            }
        }
    }
    found
}

/// What anchors found for one rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Found(Vtable),
    /// Several runs matched, all holding the same function in the rule's
    /// shared slot: the class and wrapper templates around it, by RVA.
    Family(Vec<Vtable>),
    /// No run matched; says which check failed.
    Missing(String),
    /// More than one run matched, at these RVAs.
    Ambiguous(Vec<u32>),
}

impl Outcome {
    /// The vtable found, or the first of a family.
    pub fn vtable(&self) -> Option<&Vtable> {
        self.vtables().first()
    }

    /// Every vtable found.
    pub fn vtables(&self) -> &[Vtable] {
        match self {
            Outcome::Found(table) => std::slice::from_ref(table),
            Outcome::Family(tables) => tables,
            _ => &[],
        }
    }
}

/// A run of code pointers that satisfied a rule's anchors; its bounds are
/// checked once every candidate's `lea` references are known.
struct Candidate {
    start: u32,
    slots: Vec<u32>,
}

/// Whether the run of `slots` at `start` is one whole vtable stored at
/// `offset`, given the `lea` references to every word in and after it.
fn bounded(
    image: &Image<'_>,
    loaded: &HashMap<u32, Vec<u32>>,
    start: u32,
    slots: usize,
    offset: u32,
) -> bool {
    let end = start + slots as u32 * 8;
    let stored = loaded.get(&start).is_some_and(|loads| {
        loads
            .iter()
            .any(|&load| store_at(image, load).is_some_and(|(_, disp)| disp == offset as i32))
    });
    stored
        && (start + 8..end)
            .step_by(8)
            .all(|word| !loaded.contains_key(&word))
        && (code_pointer(image, end).is_none() || loaded.contains_key(&end))
}

/// The slot count of the whole vtable stored at `offset` that `candidate`
/// starts, if it is one and `slots` admits it.
fn whole(
    image: &Image<'_>,
    loaded: &HashMap<u32, Vec<u32>>,
    candidate: &Candidate,
    slots: Slots,
    offset: u32,
) -> Option<usize> {
    let length = match slots {
        Slots::Exact(slots) => slots,
        Slots::AtLeast(_) => (1..candidate.slots.len())
            .find(|&index| loaded.contains_key(&(candidate.start + index as u32 * 8)))
            .unwrap_or(candidate.slots.len()),
    };
    (slots.admits(length) && bounded(image, loaded, candidate.start, length, offset))
        .then_some(length)
}

fn words(start: u32, slots: usize) -> impl Iterator<Item = u32> {
    (0..=slots as u32).map(move |index| start + index * 8)
}

/// Why no candidate of `rule` was a whole vtable: most often the build's
/// vtable has a different slot count than the rule expects.
fn miss(
    image: &Image<'_>,
    loaded: &HashMap<u32, Vec<u32>>,
    rule: &Rule,
    rejected: &[Candidate],
) -> String {
    let stored = |start: u32| {
        loaded.get(&start).is_some_and(|loads| {
            loads.iter().any(|&load| {
                store_at(image, load).is_some_and(|(_, disp)| disp == rule.offset as i32)
            })
        })
    };
    let mut lengths: Vec<(String, bool)> = rejected
        .iter()
        .filter(|candidate| stored(candidate.start))
        .map(|candidate| {
            let run = candidate.slots.len();
            let end = candidate.start + run as u32 * 8;
            match (1..run).find(|&index| loaded.contains_key(&(candidate.start + index as u32 * 8)))
            {
                Some(length) => (format!("{length}"), rule.slots.admits(length)),
                None if code_pointer(image, end).is_some() && !loaded.contains_key(&end) => {
                    (format!("more than {run}"), false)
                }
                None => (format!("{run}"), rule.slots.admits(run)),
            }
        })
        .collect();
    lengths.sort();
    lengths.dedup();
    match &lengths[..] {
        [(length, false)] => format!(
            "the anchored vtable has {length} slots, expected {}",
            rule.slots
        ),
        _ => format!(
            "no run of {} slots with the anchored functions is a whole vtable stored at {:#x}",
            rule.slots, rule.offset
        ),
    }
}

/// Where a vtable right after one of `referents` can start.
fn after(referents: &[Vtable]) -> impl Iterator<Item = u32> + '_ {
    referents.iter().flat_map(|referent| {
        let end = referent.rva + referent.slots.len() as u32 * 8;
        [end, end + 8]
    })
}

/// Whether every store of the vtable at `start` to `offset` has a store of
/// `slots` code pointers to `beside` through the same register next to it.
fn beside(
    image: &Image<'_>,
    loaded: &HashMap<u32, Vec<u32>>,
    start: u32,
    offset: u32,
    beside: u32,
    slots: usize,
) -> bool {
    let mut stores = loaded
        .get(&start)
        .into_iter()
        .flatten()
        .filter_map(|&load| Some((load, store_at(image, load)?)))
        .filter(|&(_, (_, disp))| disp == offset as i32)
        .peekable();
    stores.peek().is_some()
        && stores.all(|(load, (base, _))| {
            let window = (load.saturating_sub(SIBLING_SPAN), load + SIBLING_SPAN);
            stores_in(image, window)
                .into_iter()
                .any(|(_, target, other, disp)| {
                    other == base && disp == beside as i32 && run(image, target, slots).is_some()
                })
        })
}

struct Resolver<'i, 'a> {
    image: &'i Image<'a>,
    functions: Functions<'i, 'a>,
    /// The functions using each source.
    users: HashMap<Source, HashSet<u32>>,
    /// For each `Calls` anchor's source and depth, the users and the
    /// functions that reach one in at most that many direct calls.
    callers: HashMap<(Source, u8), HashSet<u32>>,
}

impl Resolver<'_, '_> {
    fn new<'i, 'a>(image: &'i Image<'a>, rules: &[Rule]) -> Resolver<'i, 'a> {
        let functions = Functions::new(image);
        let mut sources: Vec<Source> = rules
            .iter()
            .flat_map(|rule| rule.anchors)
            .filter_map(|anchor| match *anchor {
                Anchor::Slot { source, .. }
                | Anchor::Calls { source, .. }
                | Anchor::Stored { source, .. }
                | Anchor::Holds { source } => Some(source),
                _ => None,
            })
            .collect();
        sources.sort_unstable_by_key(|source| format!("{source:?}"));
        sources.dedup();
        let literals: Vec<&str> = sources
            .iter()
            .filter_map(|source| match source {
                Source::Literal(text) => Some(*text),
                _ => None,
            })
            .collect();
        let lambdas: Vec<&str> = sources
            .iter()
            .filter_map(|source| match source {
                Source::Lambda(function) => Some(*function),
                _ => None,
            })
            .collect();
        let places = find_strings(image, &literals);
        let descriptors = find_lambdas(image, &lambdas);
        let targets: HashSet<u32> = places
            .values()
            .chain(descriptors.values())
            .flatten()
            .copied()
            .collect();
        let loads = lea_references(image, &targets);
        let users_of = |targets: &[u32], loads: &HashMap<u32, Vec<u32>>| -> HashSet<u32> {
            targets
                .iter()
                .flat_map(|target| loads.get(target).into_iter().flatten())
                .filter_map(|&load| functions.start_of(load))
                .collect()
        };
        let mut users: HashMap<Source, HashSet<u32>> = places
            .iter()
            .map(|(&text, places)| (Source::Literal(text), users_of(places, &loads)))
            .collect();
        // Lambda type descriptor → `_Target_type` → the `std::function`
        // target's vtable → the functions that store it.
        let target_types: HashMap<&str, HashSet<u32>> = descriptors
            .iter()
            .map(|(&function, descriptors)| (function, users_of(descriptors, &loads)))
            .collect();
        let all: HashSet<u32> = target_types.values().flatten().copied().collect();
        let slots = pointers_to(image, &all);
        let tables: HashMap<&str, Vec<u32>> = target_types
            .iter()
            .map(|(&function, target_types)| {
                let tables = target_types
                    .iter()
                    .flat_map(|function| slots.get(function).into_iter().flatten())
                    .filter_map(|slot| slot.checked_sub(TARGET_TYPE_SLOT * 8))
                    .collect();
                (function, tables)
            })
            .collect();
        let all: HashSet<u32> = tables.values().flatten().copied().collect();
        let loads = lea_references(image, &all);
        users.extend(
            tables
                .iter()
                .map(|(&function, tables)| (Source::Lambda(function), users_of(tables, &loads))),
        );
        let imports: Vec<&str> = sources
            .iter()
            .filter_map(|source| match source {
                Source::Import(name) => Some(*name),
                _ => None,
            })
            .collect();
        let slots = find_imports(image, &imports);
        let all: HashSet<u32> = slots.values().flatten().copied().collect();
        let calls = call_references(image, &all);
        users.extend(
            slots
                .iter()
                .map(|(&name, slots)| (Source::Import(name), users_of(slots, &calls))),
        );
        // One scan of the code per call level.
        let mut reaching = HashMap::new();
        for anchor in rules.iter().flat_map(|rule| rule.anchors) {
            if let Anchor::Calls { source, depth, .. } = *anchor {
                reaching.entry((source, depth)).or_insert_with(|| {
                    let mut reached = users[&source].clone();
                    let mut frontier = reached.clone();
                    for _ in 0..depth {
                        frontier = callers(image, &functions, &frontier);
                        frontier.retain(|caller| !reached.contains(caller));
                        if frontier.is_empty() {
                            break;
                        }
                        reached.extend(&frontier);
                    }
                    reached
                });
            }
        }
        Resolver {
            image,
            functions,
            users,
            callers: reaching,
        }
    }

    /// The users of `source` and the functions they reach in at most `depth`
    /// direct calls.
    fn reach(&self, source: Source, depth: u8) -> HashSet<u32> {
        let mut reached = self.users[&source].clone();
        let mut frontier = reached.clone();
        for _ in 0..depth {
            let extents = self.functions.extents(&frontier);
            frontier = extents
                .values()
                .flatten()
                .flat_map(|&extent| callees(self.image, &self.functions, extent))
                .filter(|callee| !reached.contains(callee))
                .collect();
            reached.extend(&frontier);
        }
        reached
    }

    /// Starts of runs whose slot `slot` holds a function in `functions`.
    fn by_slot(&self, functions: &HashSet<u32>, slot: usize) -> HashSet<u32> {
        let places = pointers_to(self.image, functions);
        places
            .values()
            .flatten()
            .filter_map(|place| place.checked_sub(slot as u32 * 8))
            .collect()
    }

    /// Starts of runs that a function in `functions` stores at `offset`.
    fn by_store(&self, functions: &HashSet<u32>, offset: u32) -> HashSet<u32> {
        let extents = self.functions.extents(functions);
        extents
            .values()
            .flatten()
            .flat_map(|&extent| stores_in(self.image, extent))
            .filter(|&(_, _, _, disp)| disp == offset as i32)
            .map(|(_, target, _, _)| target)
            .collect()
    }

    /// Starts of runs that hold a function in `functions` in some slot: the
    /// words that code loads among the code pointers up to [`MAX_SLOTS`]
    /// before each place that points at one.
    fn by_holding(&self, functions: &HashSet<u32>) -> HashSet<u32> {
        let mut words = HashSet::new();
        for &place in pointers_to(self.image, functions).values().flatten() {
            words.extend(
                (0..MAX_SLOTS as u32)
                    .map_while(|index| place.checked_sub(index * 8))
                    .take_while(|&word| code_pointer(self.image, word).is_some()),
            );
        }
        lea_references(self.image, &words).into_keys().collect()
    }

    /// Whether one of `slots` holds a function that uses `source`.
    fn holds(&self, source: Source, slots: &[u32]) -> bool {
        slots.iter().any(|&function| {
            self.functions
                .start_of(function)
                .is_some_and(|start| self.users[&source].contains(&start))
        })
    }

    /// Starts of runs stored at `offset` next to every store of a
    /// `referent` at its offset, through the same register.
    fn by_sibling(
        &self,
        referents: &[Vtable],
        loaded: &HashMap<u32, Vec<u32>>,
        offset: u32,
    ) -> HashSet<u32> {
        let mut starts = HashSet::new();
        for referent in referents {
            let mut common: Option<HashSet<u32>> = None;
            for &load in loaded.get(&referent.rva).into_iter().flatten() {
                let Some((base, disp)) = store_at(self.image, load) else {
                    continue;
                };
                if disp != referent.offset as i32 {
                    continue;
                }
                let window = (load.saturating_sub(SIBLING_SPAN), load + SIBLING_SPAN);
                let near: HashSet<u32> = stores_in(self.image, window)
                    .into_iter()
                    .filter(|&(_, _, other, disp)| other == base && disp == offset as i32)
                    .map(|(_, target, _, _)| target)
                    .collect();
                common = Some(match common {
                    None => near,
                    Some(common) => &common & &near,
                });
            }
            starts.extend(common.into_iter().flatten());
        }
        starts
    }

    /// Whether `candidate` satisfies every `Slot` and `Calls` anchor of `rule`.
    fn slots_hold(&self, rule: &Rule, candidate: &Candidate) -> bool {
        rule.anchors.iter().all(|anchor| {
            let (slot, functions) = match *anchor {
                Anchor::Slot { slot, source } => (slot, &self.users[&source]),
                Anchor::Calls {
                    slot,
                    source,
                    depth,
                } => (slot, &self.callers[&(source, depth)]),
                _ => return true,
            };
            candidate.slots.get(slot).is_some_and(|&function| {
                self.functions
                    .start_of(function)
                    .is_some_and(|start| functions.contains(&start))
            })
        })
    }
}

/// Resolve every rule against `image`. `Sibling` anchors must name an
/// earlier rule. Costs a handful of scans of the code and `.rdata`.
pub fn resolve(image: &Image<'_>, rules: &[Rule]) -> Vec<Outcome> {
    let mut outcomes: Vec<Option<Outcome>> = vec![None; rules.len()];
    if image.exceptions.is_empty() {
        return rules
            .iter()
            .map(|_| Outcome::Missing("the image has no exception directory".into()))
            .collect();
    }
    let resolver = Resolver::new(image, rules);
    let leans = |rule: &Rule| {
        rule.anchors
            .iter()
            .any(|anchor| matches!(anchor, Anchor::Sibling { .. } | Anchor::After { .. }))
    };
    // Rules that stand alone first, then the rules that lean on them.
    let mut loaded = HashMap::new();
    for pass in [false, true] {
        let mut candidates: Vec<(usize, Result<Vec<Candidate>, String>)> = Vec::new();
        let mut bounds = HashSet::new();
        for (index, rule) in rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| leans(rule) == pass)
        {
            let found = generate(&resolver, rule, rules, &outcomes, &loaded);
            if let Ok(found) = &found {
                for candidate in found {
                    bounds.extend(words(candidate.start, candidate.slots.len()));
                }
            }
            candidates.push((index, found));
        }
        loaded = lea_references(image, &bounds);
        for (index, found) in candidates {
            let rule = &rules[index];
            outcomes[index] = Some(match found {
                Err(reason) => Outcome::Missing(reason),
                Ok(found) => {
                    let mut matched = Vec::new();
                    let mut rejected = Vec::new();
                    for mut candidate in found {
                        let length = whole(image, &loaded, &candidate, rule.slots, rule.offset)
                            .filter(|&length| {
                                rule.anchors.iter().all(|anchor| match *anchor {
                                    Anchor::Beside { offset, slots } => beside(
                                        image,
                                        &loaded,
                                        candidate.start,
                                        rule.offset,
                                        offset,
                                        slots,
                                    ),
                                    Anchor::Holds { source } => {
                                        resolver.holds(source, &candidate.slots[..length])
                                    }
                                    _ => true,
                                })
                            });
                        match length {
                            Some(length) => {
                                candidate.slots.truncate(length);
                                matched.push(candidate);
                            }
                            None => rejected.push(candidate),
                        }
                    }
                    let table = |candidate: Candidate| Vtable {
                        name: crate::decorated(rule.class, false),
                        offset: rule.offset,
                        rva: candidate.start,
                        slots: candidate.slots,
                    };
                    let family = rule.shared.is_some_and(|slot| {
                        matched
                            .windows(2)
                            .all(|pair| pair[0].slots[slot] == pair[1].slots[slot])
                    });
                    match matched.len() {
                        0 => Outcome::Missing(miss(image, &loaded, rule, &rejected)),
                        1 => Outcome::Found(table(matched.remove(0))),
                        _ if family => Outcome::Family(matched.into_iter().map(table).collect()),
                        _ => Outcome::Ambiguous(
                            matched.iter().map(|candidate| candidate.start).collect(),
                        ),
                    }
                }
            });
        }
    }
    outcomes
        .into_iter()
        .map(|outcome| outcome.unwrap_or_else(|| Outcome::Missing("not resolved".into())))
        .collect()
}

fn describe(source: Source) -> String {
    match source {
        Source::Literal(text) => format!("\"{}\"", text.escape_debug()),
        Source::Lambda(function) => format!("a lambda in {function}"),
        Source::Import(name) => format!("a call to the import {name}"),
    }
}

/// The candidates for `rule` from its first anchor, filtered by the others
/// that can be checked before bounds.
fn generate(
    resolver: &Resolver<'_, '_>,
    rule: &Rule,
    rules: &[Rule],
    outcomes: &[Option<Outcome>],
    loaded: &HashMap<u32, Vec<u32>>,
) -> Result<Vec<Candidate>, String> {
    let referent = |class: &str, offset: u32| -> Result<&[Vtable], String> {
        rules
            .iter()
            .position(|other| other.class == class && other.offset == offset)
            .and_then(|index| outcomes[index].as_ref())
            .map(Outcome::vtables)
            .filter(|tables| !tables.is_empty())
            .ok_or_else(|| {
                format!("needs the {class} vtable at {offset:#x}, which anchors did not find")
            })
    };
    let starts = match *rule.anchors.first().ok_or("no anchors")? {
        Anchor::Slot { slot, source } => {
            let users = &resolver.users[&source];
            if users.is_empty() {
                return Err(format!("no function uses {}", describe(source)));
            }
            resolver.by_slot(users, slot)
        }
        Anchor::Calls {
            slot,
            source,
            depth,
        } => {
            if resolver.users[&source].is_empty() {
                return Err(format!("no function uses {}", describe(source)));
            }
            resolver.by_slot(&resolver.callers[&(source, depth)], slot)
        }
        Anchor::Stored { source, depth } => {
            if resolver.users[&source].is_empty() {
                return Err(format!("no function uses {}", describe(source)));
            }
            resolver.by_store(&resolver.reach(source, depth), rule.offset)
        }
        Anchor::Holds { source } => {
            let users = &resolver.users[&source];
            if users.is_empty() {
                return Err(format!("no function uses {}", describe(source)));
            }
            resolver.by_holding(users)
        }
        Anchor::Sibling { class, offset } => {
            resolver.by_sibling(referent(class, offset)?, loaded, rule.offset)
        }
        Anchor::After { class, offset } => after(referent(class, offset)?).collect(),
        Anchor::Beside { .. } => return Err("a Beside anchor cannot come first".into()),
    };
    let mut starts: Vec<u32> = starts.into_iter().collect();
    starts.sort_unstable();
    let mut candidates: Vec<Candidate> = starts
        .into_iter()
        .map(|start| Candidate {
            start,
            slots: prefix(resolver.image, start, rule.slots.span()),
        })
        .collect();
    candidates.retain(|candidate| resolver.slots_hold(rule, candidate));
    for anchor in &rule.anchors[1..] {
        let starts: HashSet<u32> = match *anchor {
            Anchor::Slot { .. }
            | Anchor::Calls { .. }
            | Anchor::Beside { .. }
            | Anchor::Holds { .. } => continue,
            Anchor::After { class, offset } => after(referent(class, offset)?).collect(),
            Anchor::Stored { source, depth } => {
                resolver.by_store(&resolver.reach(source, depth), rule.offset)
            }
            Anchor::Sibling { class, offset } => {
                resolver.by_sibling(referent(class, offset)?, loaded, rule.offset)
            }
        };
        candidates.retain(|candidate| starts.contains(&candidate.start));
    }
    let (whole, short): (Vec<Candidate>, Vec<Candidate>) = candidates
        .into_iter()
        .partition(|candidate| candidate.slots.len() >= rule.slots.min());
    if whole.is_empty() {
        let mut lengths: Vec<usize> = short
            .iter()
            .map(|candidate| candidate.slots.len())
            .collect();
        lengths.dedup();
        return Err(match lengths[..] {
            [length] => format!(
                "the anchored vtable has at most {length} slots, expected {}",
                rule.slots
            ),
            _ => format!(
                "no run of {} code pointers satisfies the anchors",
                rule.slots
            ),
        });
    }
    Ok(whole)
}

/// Every vtable of `slots` slots, stored at offset 0, whose `slot` holds
/// `function`, checked like a rule's run. Wrapper templates reuse a class's
/// functions in vtables of their own.
pub fn sharing(image: &Image<'_>, function: u32, slot: usize, slots: usize) -> Vec<u32> {
    let places = pointers_to(image, &HashSet::from([function]));
    let starts: Vec<u32> = places
        .get(&function)
        .into_iter()
        .flatten()
        .filter_map(|&place| place.checked_sub(slot as u32 * 8))
        .filter(|&start| run(image, start, slots).is_some())
        .collect();
    let bounds: HashSet<u32> = starts
        .iter()
        .flat_map(|&start| words(start, slots))
        .collect();
    let loaded = lea_references(image, &bounds);
    starts
        .into_iter()
        .filter(|&start| bounded(image, &loaded, start, slots, 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lambda_scopes_reverse_the_path() {
        assert_eq!(lambda_scope("SceneStack::update"), "??update@SceneStack@@");
        assert_eq!(lambda_scope("OreUI::View::render"), "??render@View@OreUI@@");
    }

    /// A `lea r64, [rip+target]` at `at` in a section at `rva`.
    fn lea(code: &mut [u8], at: usize, rva: u32, register: u8, target: u32) {
        let displacement = target as i64 - (rva as i64 + at as i64 + 7);
        code[at..at + 3].copy_from_slice(&[0x48, 0x8D, 0x05 | register << 3]);
        code[at + 3..at + 7].copy_from_slice(&(displacement as i32).to_le_bytes());
    }

    /// `.text` at 0x1000: a leaf at 0x1000 loads the string, a constructor at
    /// 0x1020 stores the vtable, two more functions fill slots. `.rdata` at
    /// 0x3000: the string, then a three-slot vtable at 0x3100. `.pdata` at
    /// 0x5000 lists the constructor.
    fn synthetic() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let base = 0x1_4000_0000u64;
        let mut text = vec![0xCCu8; 0x100];
        lea(&mut text, 0, 0x1000, 2, 0x3000);
        text[7] = 0xC3;
        lea(&mut text, 0x20, 0x1000, 0, 0x3100);
        // mov [rcx], rax; ret
        text[0x27..0x2B].copy_from_slice(&[0x48, 0x89, 0x01, 0xC3]);
        text[0x40] = 0xC3;
        text[0x50] = 0xC3;
        let mut rdata = vec![0u8; 0x200];
        rdata[..12].copy_from_slice(b"anchor text\0");
        for (index, target) in [0x1040u64, 0x1000, 0x1050].iter().enumerate() {
            rdata[0x100 + index * 8..0x108 + index * 8]
                .copy_from_slice(&(base + target).to_le_bytes());
        }
        let mut pdata = vec![0u8; 0x200];
        for (index, word) in [0x1020u32, 0x102B, 0x5100].iter().enumerate() {
            pdata[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        // Unwind info at 0x5100: version 1, no flags, no codes.
        pdata[0x100] = 1;
        (text, rdata, pdata)
    }

    fn image<'a>(text: &'a [u8], rdata: &'a [u8], pdata: &'a [u8]) -> Image<'a> {
        let section = |name: &[u8; 8], rva, bytes, executable| Section {
            name: *name,
            rva,
            bytes,
            executable,
        };
        Image {
            base: 0x1_4000_0000,
            sections: vec![
                section(b".text\0\0\0", 0x1000, text, true),
                section(b".rdata\0\0", 0x3000, rdata, false),
                section(b".pdata\0\0", 0x5000, pdata, false),
            ],
            exceptions: 0x5000..0x500C,
        }
    }

    fn rule(slots: Slots) -> Rule {
        Rule {
            class: "Thing",
            offset: 0,
            slots,
            shared: None,
            anchors: &[Anchor::Slot {
                slot: 1,
                source: Source::Literal("anchor text"),
            }],
        }
    }

    #[test]
    fn a_slot_anchor_finds_the_stored_vtable() {
        let (text, rdata, pdata) = synthetic();
        let image = image(&text, &rdata, &pdata);
        let outcomes = resolve(&image, &[rule(Slots::Exact(3)), rule(Slots::Exact(2))]);
        let found = outcomes[0].vtable().expect("the three-slot rule matches");
        assert_eq!(found.rva, 0x3100);
        assert_eq!(found.slots, vec![0x1040, 0x1000, 0x1050]);
        // Two slots end inside the run: not a whole vtable.
        assert!(matches!(outcomes[1], Outcome::Missing(_)));
    }

    #[test]
    fn a_calls_anchor_follows_direct_calls_to_the_user() {
        let (mut text, rdata, pdata) = synthetic();
        // Slot 0's function calls the leaf that loads the string.
        let displacement = 0x1000i32 - 0x1045;
        text[0x40] = 0xE8;
        text[0x41..0x45].copy_from_slice(&displacement.to_le_bytes());
        text[0x45] = 0xC3;
        let image = image(&text, &rdata, &pdata);
        let calls = |slot, depth| Rule {
            class: "Thing",
            offset: 0,
            slots: Slots::Exact(3),
            shared: None,
            anchors: match (slot, depth) {
                (0, 1) => &[Anchor::Calls {
                    slot: 0,
                    source: Source::Literal("anchor text"),
                    depth: 1,
                }],
                (0, 0) => &[Anchor::Calls {
                    slot: 0,
                    source: Source::Literal("anchor text"),
                    depth: 0,
                }],
                _ => &[Anchor::Calls {
                    slot: 1,
                    source: Source::Literal("anchor text"),
                    depth: 0,
                }],
            },
        };
        let outcomes = resolve(&image, &[calls(0, 1), calls(0, 0), calls(1, 0)]);
        assert_eq!(outcomes[0].vtable().map(|table| table.rva), Some(0x3100));
        assert!(matches!(outcomes[1], Outcome::Missing(_)));
        assert_eq!(outcomes[2].vtable().map(|table| table.rva), Some(0x3100));
    }

    #[test]
    fn a_minimum_count_ends_the_vtable_where_the_next_one_starts() {
        let (mut text, mut rdata, pdata) = synthetic();
        // A second vtable right after the first, which code loads.
        rdata[0x118..0x120].copy_from_slice(&(0x1_4000_1050u64).to_le_bytes());
        lea(&mut text, 0x60, 0x1000, 0, 0x3118);
        let image = image(&text, &rdata, &pdata);
        let outcomes = resolve(
            &image,
            &[
                rule(Slots::AtLeast(2)),
                rule(Slots::AtLeast(3)),
                rule(Slots::AtLeast(4)),
                rule(Slots::Exact(4)),
            ],
        );
        for outcome in &outcomes[..2] {
            let found = outcome.vtable().expect("the minimum admits three slots");
            assert_eq!(found.slots, vec![0x1040, 0x1000, 0x1050]);
        }
        assert!(matches!(&outcomes[2], Outcome::Missing(reason) if reason.contains("has 3 slots")));
        assert!(matches!(outcomes[3], Outcome::Missing(_)));
    }

    #[test]
    fn a_holds_anchor_finds_the_vtable_whose_slot_moved() {
        let (mut text, mut rdata, pdata) = synthetic();
        // Another stored vtable right before the anchored one.
        for (index, target) in [0x1040u64, 0x1050, 0x1040].iter().enumerate() {
            rdata[0xE8 + index * 8..0xF0 + index * 8]
                .copy_from_slice(&(0x1_4000_0000 + target).to_le_bytes());
        }
        lea(&mut text, 0x60, 0x1000, 0, 0x30E8);
        text[0x67..0x6B].copy_from_slice(&[0x48, 0x89, 0x01, 0xC3]);
        let image = image(&text, &rdata, &pdata);
        let holds = |slots| Rule {
            class: "Thing",
            offset: 0,
            slots,
            shared: None,
            anchors: &[Anchor::Holds {
                source: Source::Literal("anchor text"),
            }],
        };
        let outcomes = resolve(
            &image,
            &[holds(Slots::AtLeast(2)), holds(Slots::AtLeast(4))],
        );
        let found = outcomes[0].vtable().expect("one vtable holds the user");
        assert_eq!(found.rva, 0x3100);
        assert_eq!(found.slots, vec![0x1040, 0x1000, 0x1050]);
        assert!(matches!(outcomes[1], Outcome::Missing(_)));
    }

    #[test]
    fn an_import_source_finds_the_callers_of_its_address_table_slot() {
        let (mut text, mut rdata, mut pdata) = synthetic();
        // The constructor calls the import, then stores the vtable:
        // call [rip+0x3060]; lea rax, [rip+0x3100]; mov [rcx], rax; ret
        text[0x20..0x22].copy_from_slice(&[0xFF, 0x15]);
        text[0x22..0x26].copy_from_slice(&(0x3060i32 - 0x1026).to_le_bytes());
        lea(&mut text, 0x26, 0x1000, 0, 0x3100);
        text[0x2D..0x31].copy_from_slice(&[0x48, 0x89, 0x01, 0xC3]);
        pdata[4..8].copy_from_slice(&0x1031u32.to_le_bytes());
        // Hint/name entry at 0x3020, name table at 0x3040, a loaded address
        // table at 0x3060, the descriptor at 0x3080, the DLL name at 0x30A0.
        rdata[0x22..0x33].copy_from_slice(b"??0Base@@QEAA@XZ\0");
        rdata[0x40..0x48].copy_from_slice(&0x3020u64.to_le_bytes());
        rdata[0x60..0x68].copy_from_slice(&0x7FF0_1234_0000u64.to_le_bytes());
        for (index, word) in [0x3040u32, 0, 0, 0x30A0, 0x3060].iter().enumerate() {
            rdata[0x80 + index * 4..0x84 + index * 4].copy_from_slice(&word.to_le_bytes());
        }
        rdata[0xA0..0xA9].copy_from_slice(b"base.dll\0");
        let image = image(&text, &rdata, &pdata);
        assert_eq!(
            find_imports(&image, &["??0Base@@QEAA@XZ"])["??0Base@@QEAA@XZ"],
            vec![0x3060]
        );
        let stored = |name| Rule {
            class: "Thing",
            offset: 0,
            slots: Slots::Exact(3),
            shared: None,
            anchors: match name {
                "base" => &[Anchor::Stored {
                    source: Source::Import("??0Base@@QEAA@XZ"),
                    depth: 0,
                }],
                _ => &[Anchor::Stored {
                    source: Source::Import("??0Other@@QEAA@XZ"),
                    depth: 0,
                }],
            },
        };
        let outcomes = resolve(&image, &[stored("base"), stored("other")]);
        assert_eq!(outcomes[0].vtable().map(|table| table.rva), Some(0x3100));
        assert!(matches!(outcomes[1], Outcome::Missing(_)));
    }

    #[test]
    fn stores_after_a_vtable_load_decode() {
        // lea rax, [rip+0]; mov [rbx+0x18], rax
        let code = [0x48, 0x8D, 0x05, 0, 0, 0, 0, 0x48, 0x89, 0x43, 0x18];
        assert_eq!(store_after(&code, 0), Some((3, 0x18)));
        // lea rcx, [rip+0]; mov [rax], rcx
        let code = [0x48, 0x8D, 0x0D, 0, 0, 0, 0, 0x48, 0x89, 0x08];
        assert_eq!(store_after(&code, 0), Some((0, 0)));
        // lea rax, [rip+0]; mov r8, [rbp+0x38]; mov [r8], rax
        let code = [
            0x48, 0x8D, 0x05, 0, 0, 0, 0, 0x4C, 0x8B, 0x45, 0x38, 0x49, 0x89, 0x00,
        ];
        assert_eq!(store_after(&code, 0), Some((8, 0)));
        // lea rax, [rip+0]; mov rdi, [rbp+0x90]: no store follows.
        let code = [
            0x48, 0x8D, 0x05, 0, 0, 0, 0, 0x48, 0x8B, 0xBD, 0x90, 0, 0, 0, 0x48,
        ];
        assert_eq!(store_after(&code, 0), None);
    }
}
