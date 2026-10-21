//! PE/COFF reading for `Lossless.dll`.
//!
//! Two jobs, both low-level and both worth having in Rust: find the header and
//! section table, and walk the resource directory to the RCDATA entries that
//! hold the shader blobs. The shader ids the engine asks for are name ids under
//! resource type 10, so the walk is fixed-depth: type → name → language → leaf.
//!
//! Replacement for the `pe_*` / `resource_*` half of GameNative's `lsfg_dll.c`.
//! The behaviour it must preserve is the error split: a file that is not a
//! PE is `NotPortableExecutable`, a PE without usable RCDATA is
//! `MissingShaders`.

use crate::LsfgStatus;

/// Resource type whose leaves are the shader blobs.
const RESOURCE_TYPE_RCDATA: u32 = 10;
/// Highest resource name id the table tracks; ids at or above it are ignored.
pub const MAX_RESOURCE_ID: usize = 512;

const DOS_MAGIC: u16 = 0x5A4D;
const PE_SIGNATURE: u32 = 0x0000_4550;
const PE32_MAGIC: u16 = 0x010B;
const PE32_PLUS_MAGIC: u16 = 0x020B;

const DOS_LFANEW_OFFSET: usize = 0x3C;
const COFF_HEADER_SIZE: usize = 20;
const OPTIONAL_HEADER_SIZE_OFFSET: usize = 16;
const SECTION_HEADER_SIZE: usize = 40;
const DATA_DIRECTORY_ENTRY_SIZE: usize = 8;
const DATA_DIRECTORY_OFFSET_PE32: usize = 96;
const DATA_DIRECTORY_OFFSET_PE32P: usize = 112;
const RESOURCE_DATA_DIRECTORY_INDEX: usize = 2;

const RESOURCE_DIRECTORY_SIZE: usize = 16;
const RESOURCE_NAMED_COUNT_OFFSET: usize = 12;
const RESOURCE_ID_COUNT_OFFSET: usize = 14;
const RESOURCE_ENTRY_SIZE: usize = 8;
const RESOURCE_SUBDIRECTORY_FLAG: u32 = 0x8000_0000;

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[derive(Clone, Copy)]
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_address: u32,
    raw_size: u32,
}

/// One resource directory entry, resolved to the two fields the walk needs.
#[derive(Clone, Copy)]
struct Entry {
    id: u32,
    offset: usize,
    is_directory: bool,
    is_named: bool,
}

/// A parsed DLL: the bytes, the section table, and the RCDATA leaves by name id.
pub struct PeImage<'a> {
    data: &'a [u8],
    sections: Vec<Section>,
    resources: Vec<Option<(usize, usize)>>,
}

impl<'a> PeImage<'a> {
    /// Parse the header, sections and RCDATA table.
    ///
    /// The returned image borrows `data` for its whole life; no copy of the file
    /// is made for the resource entries.
    pub fn parse(data: &'a [u8]) -> Result<Self, LsfgStatus> {
        let pe_offset = find_header(data).ok_or(LsfgStatus::NotPortableExecutable)?;
        let sections = read_sections(data, pe_offset).ok_or(LsfgStatus::NotPortableExecutable)?;
        let directory = data_directory_offset(data, pe_offset + 4 + COFF_HEADER_SIZE)
            .ok_or(LsfgStatus::NotPortableExecutable)?;
        let resource_rva = read_u32(
            data,
            directory + RESOURCE_DATA_DIRECTORY_INDEX * DATA_DIRECTORY_ENTRY_SIZE,
        )
        .filter(|rva| *rva != 0)
        .ok_or(LsfgStatus::MissingShaders)?;
        let base = rva_to_offset(&sections, resource_rva).ok_or(LsfgStatus::MissingShaders)?;

        let mut image = Self {
            data,
            sections,
            resources: vec![None; MAX_RESOURCE_ID],
        };
        image.collect_rcdata(base)?;
        Ok(image)
    }

    /// The leaf bytes for a resource name id, if the DLL carries one.
    #[must_use]
    pub fn resource(&self, id: u32) -> Option<&'a [u8]> {
        let (offset, size) = self.resources.get(id as usize).copied().flatten()?;
        self.data.get(offset..offset.checked_add(size)?)
    }

    /// Whole-file bytes; the cache header hashes these.
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    fn collect_rcdata(&mut self, base: usize) -> Result<(), LsfgStatus> {
        for t in 0..entry_count(self.data, base).ok_or(LsfgStatus::MissingShaders)? {
            let Some(entry) = entry_at(self.data, base, t) else {
                return Err(LsfgStatus::MissingShaders);
            };
            if entry.is_named || entry.id != RESOURCE_TYPE_RCDATA || !entry.is_directory {
                continue;
            }
            let name_base = base + entry.offset;
            for n in 0..entry_count(self.data, name_base).ok_or(LsfgStatus::MissingShaders)? {
                let Some(name) = entry_at(self.data, name_base, n) else {
                    return Err(LsfgStatus::MissingShaders);
                };
                if name.is_named || !name.is_directory || name.id as usize >= MAX_RESOURCE_ID {
                    continue;
                }
                let language_base = base + name.offset;
                for l in
                    0..entry_count(self.data, language_base).ok_or(LsfgStatus::MissingShaders)?
                {
                    let Some(language) = entry_at(self.data, language_base, l) else {
                        return Err(LsfgStatus::MissingShaders);
                    };
                    if language.is_directory {
                        continue;
                    }
                    // One leaf per name id: the first language that resolves wins,
                    // which is what the C walk did.
                    if let Some(leaf) = read_leaf(self.data, &self.sections, base + language.offset)
                    {
                        self.resources[name.id as usize] = Some(leaf);
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

fn find_header(data: &[u8]) -> Option<usize> {
    if read_u16(data, 0)? != DOS_MAGIC {
        return None;
    }
    let pe_offset = read_u32(data, DOS_LFANEW_OFFSET)? as usize;
    if read_u32(data, pe_offset)? != PE_SIGNATURE {
        return None;
    }
    Some(pe_offset)
}

fn data_directory_offset(data: &[u8], optional_header: usize) -> Option<usize> {
    Some(
        optional_header
            + match read_u16(data, optional_header)? {
                PE32_MAGIC => DATA_DIRECTORY_OFFSET_PE32,
                PE32_PLUS_MAGIC => DATA_DIRECTORY_OFFSET_PE32P,
                _ => return None,
            },
    )
}

fn read_sections(data: &[u8], pe_offset: usize) -> Option<Vec<Section>> {
    let count = read_u16(data, pe_offset + 4 + 2)? as usize;
    let optional_size = read_u16(data, pe_offset + 4 + OPTIONAL_HEADER_SIZE_OFFSET)? as usize;
    if count == 0 {
        return None;
    }
    let table = pe_offset + 4 + COFF_HEADER_SIZE + optional_size;
    let mut sections = Vec::with_capacity(count);
    for i in 0..count {
        let at = table + i * SECTION_HEADER_SIZE;
        sections.push(Section {
            virtual_size: read_u32(data, at + 8)?,
            virtual_address: read_u32(data, at + 12)?,
            raw_size: read_u32(data, at + 16)?,
            raw_address: read_u32(data, at + 20)?,
        });
    }
    Some(sections)
}

fn rva_to_offset(sections: &[Section], rva: u32) -> Option<usize> {
    for section in sections {
        let span = section.virtual_size.max(section.raw_size);
        if span == 0 || rva < section.virtual_address {
            continue;
        }
        let relative = rva - section.virtual_address;
        if relative < span {
            return Some(section.raw_address as usize + relative as usize);
        }
    }
    None
}

fn entry_count(data: &[u8], directory: usize) -> Option<usize> {
    let named = read_u16(data, directory + RESOURCE_NAMED_COUNT_OFFSET)? as usize;
    let ids = read_u16(data, directory + RESOURCE_ID_COUNT_OFFSET)? as usize;
    // Bounded by the table the caller is about to walk; a count that does not fit
    // the file is rejected by `entry_at`'s reads.
    Some(named + ids)
}

fn entry_at(data: &[u8], directory: usize, index: usize) -> Option<Entry> {
    let at = directory + RESOURCE_DIRECTORY_SIZE + index.checked_mul(RESOURCE_ENTRY_SIZE)?;
    let name = read_u32(data, at)?;
    let value = read_u32(data, at + 4)?;
    Some(Entry {
        id: name & !RESOURCE_SUBDIRECTORY_FLAG,
        offset: (value & !RESOURCE_SUBDIRECTORY_FLAG) as usize,
        is_directory: value & RESOURCE_SUBDIRECTORY_FLAG != 0,
        is_named: name & RESOURCE_SUBDIRECTORY_FLAG != 0,
    })
}

fn read_leaf(data: &[u8], sections: &[Section], at: usize) -> Option<(usize, usize)> {
    let rva = read_u32(data, at)?;
    let size = read_u32(data, at + 4)? as usize;
    if size == 0 {
        return None;
    }
    let offset = rva_to_offset(sections, rva)?;
    // The leaf must lie inside the file as shipped; a truncated DLL would
    // otherwise hand out a slice past the mapping.
    data.get(offset..offset.checked_add(size)?)?;
    Some((offset, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit PE with one section covering RVA 0x1000 and a two-level RCDATA
    /// directory: type 10 → name 256 → language → leaf.
    fn synthetic_dll(payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 0x1200];
        data[0..2].copy_from_slice(&DOS_MAGIC.to_le_bytes());
        data[DOS_LFANEW_OFFSET..DOS_LFANEW_OFFSET + 4].copy_from_slice(&0x80u32.to_le_bytes());

        let pe = 0x80usize;
        data[pe..pe + 4].copy_from_slice(&PE_SIGNATURE.to_le_bytes());
        // section count at pe+6, optional header size at pe+20
        data[pe + 6..pe + 8].copy_from_slice(&1u16.to_le_bytes());
        data[pe + 20..pe + 22].copy_from_slice(&240u16.to_le_bytes());
        // optional header: PE32+
        let opt = pe + 24;
        data[opt..opt + 2].copy_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());
        let dirs = opt + DATA_DIRECTORY_OFFSET_PE32P;
        // resource directory RVA
        data[dirs + RESOURCE_DATA_DIRECTORY_INDEX * DATA_DIRECTORY_ENTRY_SIZE
            ..dirs + RESOURCE_DATA_DIRECTORY_INDEX * DATA_DIRECTORY_ENTRY_SIZE + 4]
            .copy_from_slice(&0x1000u32.to_le_bytes());

        // section header: vsize, rva, raw size, raw ptr
        let section = pe + 24 + 240;
        data[section + 8..section + 12].copy_from_slice(&0x200u32.to_le_bytes());
        data[section + 12..section + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        data[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        data[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());

        // Directory tree at file 0x200 (RVA 0x1000).
        let base = 0x200usize;
        // type entry: id 10, subdirectory -> +0x20
        data[base + 12..base + 14].copy_from_slice(&0u16.to_le_bytes());
        data[base + 14..base + 16].copy_from_slice(&1u16.to_le_bytes());
        data[base + 16..base + 20].copy_from_slice(&10u32.to_le_bytes());
        data[base + 20..base + 24]
            .copy_from_slice(&(0x20u32 | RESOURCE_SUBDIRECTORY_FLAG).to_le_bytes());
        // name entry: id 256, subdirectory -> +0x40
        let names = base + 0x20;
        data[names + 14..names + 16].copy_from_slice(&1u16.to_le_bytes());
        data[names + 16..names + 20].copy_from_slice(&256u32.to_le_bytes());
        data[names + 20..names + 24]
            .copy_from_slice(&(0x40u32 | RESOURCE_SUBDIRECTORY_FLAG).to_le_bytes());
        // language entry: id 1033, leaf -> +0x80
        let languages = base + 0x40;
        data[languages + 14..languages + 16].copy_from_slice(&1u16.to_le_bytes());
        data[languages + 16..languages + 20].copy_from_slice(&1033u32.to_le_bytes());
        data[languages + 20..languages + 24].copy_from_slice(&0x80u32.to_le_bytes());
        // leaf: data RVA 0x1100, size
        let leaf = base + 0x80;
        data[leaf..leaf + 4].copy_from_slice(&0x1100u32.to_le_bytes());
        data[leaf + 4..leaf + 8].copy_from_slice(&(payload.len() as u32).to_le_bytes());

        data[0x300..0x300 + payload.len()].copy_from_slice(payload);
        data
    }

    #[test]
    fn reads_an_rcdata_leaf_by_name_id() {
        let dll = synthetic_dll(b"shader-bytes");
        let image = PeImage::parse(&dll).expect("parse");
        assert_eq!(image.resource(256), Some(&b"shader-bytes"[..]));
        assert_eq!(image.resource(255), None);
    }

    #[test]
    fn rejects_a_non_pe_file() {
        let junk = vec![0u8; 0x100];
        assert_eq!(
            PeImage::parse(&junk).err(),
            Some(LsfgStatus::NotPortableExecutable)
        );
    }
}
