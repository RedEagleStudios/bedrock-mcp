//! Little-endian NBT, the format of Bedrock's `level.dat`: a `u32` storage version, the
//! `u32` length of the rest, then a root compound.

use crate::error::{Error, Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<i8>),
    String(String),
    /// The element type (kept for empty lists) and the elements.
    List(u8, Vec<Tag>),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

/// Named tags in file order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compound(pub Vec<(String, Tag)>);

impl Compound {
    pub fn get(&self, name: &str) -> Option<&Tag> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, tag)| tag)
    }

    /// Replace the tag named `name`, or append it.
    pub fn set(&mut self, name: &str, tag: Tag) {
        match self.0.iter_mut().find(|(key, _)| key == name) {
            Some((_, slot)) => *slot = tag,
            None => self.0.push((name.to_owned(), tag)),
        }
    }

    /// The compound named `name`, made (or replacing a tag of another type) when missing.
    pub fn compound_mut(&mut self, name: &str) -> &mut Compound {
        if !matches!(self.get(name), Some(Tag::Compound(_))) {
            self.set(name, Tag::Compound(Compound::default()));
        }
        match self.0.iter_mut().find(|(key, _)| key == name) {
            Some((_, Tag::Compound(compound))) => compound,
            _ => unreachable!("set above"),
        }
    }
}

impl Tag {
    fn id(&self) -> u8 {
        match self {
            Self::Byte(_) => 1,
            Self::Short(_) => 2,
            Self::Int(_) => 3,
            Self::Long(_) => 4,
            Self::Float(_) => 5,
            Self::Double(_) => 6,
            Self::ByteArray(_) => 7,
            Self::String(_) => 8,
            Self::List(..) => 9,
            Self::Compound(_) => 10,
            Self::IntArray(_) => 11,
            Self::LongArray(_) => 12,
        }
    }
}

/// A parsed `level.dat`.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelDat {
    pub storage_version: u32,
    pub root_name: String,
    pub root: Compound,
}

/// Nesting deeper than this is treated as a broken file.
const MAX_DEPTH: usize = 512;

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self
            .data
            .get(self.at..self.at + N)
            .ok_or_else(|| Error::msg("The NBT data ends early."))?;
        self.at += N;
        Ok(bytes.try_into().expect("N bytes"))
    }

    fn length(&mut self) -> Result<usize> {
        let length = i32::from_le_bytes(self.take()?);
        let length = usize::try_from(length).map_err(|_| Error::msg("A negative NBT length."))?;
        if length > self.data.len() - self.at {
            bail!("An NBT length runs past the end of the data.");
        }
        Ok(length)
    }

    fn string(&mut self) -> Result<String> {
        let length = usize::from(u16::from_le_bytes(self.take()?));
        let bytes = self
            .data
            .get(self.at..self.at + length)
            .ok_or_else(|| Error::msg("The NBT data ends early."))?;
        self.at += length;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::msg("An NBT string is not UTF-8."))
    }

    fn compound(&mut self, depth: usize) -> Result<Compound> {
        let mut entries = Vec::new();
        loop {
            let [id] = self.take()?;
            if id == 0 {
                return Ok(Compound(entries));
            }
            let name = self.string()?;
            entries.push((name, self.payload(id, depth + 1)?));
        }
    }

    fn payload(&mut self, id: u8, depth: usize) -> Result<Tag> {
        if depth > MAX_DEPTH {
            bail!("The NBT data nests too deep.");
        }
        Ok(match id {
            1 => Tag::Byte(i8::from_le_bytes(self.take()?)),
            2 => Tag::Short(i16::from_le_bytes(self.take()?)),
            3 => Tag::Int(i32::from_le_bytes(self.take()?)),
            4 => Tag::Long(i64::from_le_bytes(self.take()?)),
            5 => Tag::Float(f32::from_le_bytes(self.take()?)),
            6 => Tag::Double(f64::from_le_bytes(self.take()?)),
            7 => {
                let length = self.length()?;
                Tag::ByteArray(
                    (0..length)
                        .map(|_| self.take().map(i8::from_le_bytes))
                        .collect::<Result<_>>()?,
                )
            }
            8 => Tag::String(self.string()?),
            9 => {
                let [element] = self.take()?;
                let length = self.length()?;
                let items = (0..length)
                    .map(|_| self.payload(element, depth + 1))
                    .collect::<Result<_>>()?;
                Tag::List(element, items)
            }
            10 => Tag::Compound(self.compound(depth)?),
            11 => {
                let length = self.length()?;
                Tag::IntArray(
                    (0..length)
                        .map(|_| self.take().map(i32::from_le_bytes))
                        .collect::<Result<_>>()?,
                )
            }
            12 => {
                let length = self.length()?;
                Tag::LongArray(
                    (0..length)
                        .map(|_| self.take().map(i64::from_le_bytes))
                        .collect::<Result<_>>()?,
                )
            }
            0 if depth > 0 => bail!("An NBT list holds end tags."),
            other => bail!("Unknown NBT tag type {other}."),
        })
    }
}

fn put_string(out: &mut Vec<u8>, value: &str) -> Result<()> {
    let length =
        u16::try_from(value.len()).map_err(|_| Error::msg("An NBT string is too long."))?;
    out.extend(length.to_le_bytes());
    out.extend(value.as_bytes());
    Ok(())
}

fn put_length(out: &mut Vec<u8>, length: usize) -> Result<()> {
    let length = i32::try_from(length).map_err(|_| Error::msg("An NBT array is too long."))?;
    out.extend(length.to_le_bytes());
    Ok(())
}

fn put_compound(out: &mut Vec<u8>, compound: &Compound) -> Result<()> {
    for (name, tag) in &compound.0 {
        out.push(tag.id());
        put_string(out, name)?;
        put_payload(out, tag)?;
    }
    out.push(0);
    Ok(())
}

fn put_payload(out: &mut Vec<u8>, tag: &Tag) -> Result<()> {
    match tag {
        Tag::Byte(value) => out.extend(value.to_le_bytes()),
        Tag::Short(value) => out.extend(value.to_le_bytes()),
        Tag::Int(value) => out.extend(value.to_le_bytes()),
        Tag::Long(value) => out.extend(value.to_le_bytes()),
        Tag::Float(value) => out.extend(value.to_le_bytes()),
        Tag::Double(value) => out.extend(value.to_le_bytes()),
        Tag::ByteArray(values) => {
            put_length(out, values.len())?;
            out.extend(values.iter().flat_map(|value| value.to_le_bytes()));
        }
        Tag::String(value) => put_string(out, value)?,
        Tag::List(element, items) => {
            if items.iter().any(|item| item.id() != *element) {
                bail!("An NBT list mixes tag types.");
            }
            out.push(*element);
            put_length(out, items.len())?;
            for item in items {
                put_payload(out, item)?;
            }
        }
        Tag::Compound(compound) => put_compound(out, compound)?,
        Tag::IntArray(values) => {
            put_length(out, values.len())?;
            out.extend(values.iter().flat_map(|value| value.to_le_bytes()));
        }
        Tag::LongArray(values) => {
            put_length(out, values.len())?;
            out.extend(values.iter().flat_map(|value| value.to_le_bytes()));
        }
    }
    Ok(())
}

impl LevelDat {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut reader = Reader { data, at: 0 };
        let storage_version = u32::from_le_bytes(reader.take()?);
        let length = u32::from_le_bytes(reader.take()?) as usize;
        if length != data.len() - 8 {
            bail!(
                "level.dat says it holds {length} bytes but holds {}.",
                data.len() - 8
            );
        }
        let [id] = reader.take()?;
        if id != 10 {
            bail!("level.dat does not start with a compound.");
        }
        let root_name = reader.string()?;
        let root = reader.compound(0)?;
        if reader.at != data.len() {
            bail!("level.dat has bytes after its root compound.");
        }
        Ok(Self {
            storage_version,
            root_name,
            root,
        })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut body = vec![10];
        put_string(&mut body, &self.root_name)?;
        put_compound(&mut body, &self.root)?;
        let length = u32::try_from(body.len()).map_err(|_| Error::msg("level.dat is too big."))?;
        let mut out = Vec::with_capacity(body.len() + 8);
        out.extend(self.storage_version.to_le_bytes());
        out.extend(length.to_le_bytes());
        out.extend(body);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LevelDat {
        let mut nested = Compound::default();
        nested.set("gametest", Tag::Byte(0));
        LevelDat {
            storage_version: 10,
            root_name: String::new(),
            root: Compound(vec![
                ("LevelName".into(), Tag::String("Test world".into())),
                ("Generator".into(), Tag::Int(1)),
                ("RandomSeed".into(), Tag::Long(-5)),
                ("rainLevel".into(), Tag::Float(0.5)),
                ("experiments".into(), Tag::Compound(nested)),
                (
                    "lastOpenedWithVersion".into(),
                    Tag::List(3, vec![Tag::Int(1), Tag::Int(0)]),
                ),
                ("empty".into(), Tag::List(10, Vec::new())),
                ("bytes".into(), Tag::ByteArray(vec![1, -1])),
                ("longs".into(), Tag::LongArray(vec![7])),
            ]),
        }
    }

    #[test]
    fn level_dat_round_trips_byte_for_byte() {
        let bytes = sample().to_bytes().unwrap();
        assert_eq!(&bytes[..4], &10u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        let parsed = LevelDat::parse(&bytes).unwrap();
        assert_eq!(parsed, sample());
        assert_eq!(parsed.to_bytes().unwrap(), bytes);
    }

    #[test]
    fn broken_level_dat_is_an_error_not_a_panic() {
        let bytes = sample().to_bytes().unwrap();
        for end in 0..bytes.len() {
            assert!(LevelDat::parse(&bytes[..end]).is_err());
        }
        let mut huge = bytes.clone();
        huge[8 + 3..8 + 7].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(LevelDat::parse(&huge).is_err());
    }

    #[test]
    fn compound_mut_replaces_other_types_and_keeps_order() {
        let mut level = sample();
        level.root.set("experiments", Tag::Int(3));
        level
            .root
            .compound_mut("experiments")
            .set("gametest", Tag::Byte(1));
        assert_eq!(level.root.0[4].0, "experiments");
        let Some(Tag::Compound(experiments)) = level.root.get("experiments") else {
            panic!("compound");
        };
        assert_eq!(experiments.get("gametest"), Some(&Tag::Byte(1)));
    }
}
