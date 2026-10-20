//! The shader cache: variant selection, SPIR-V adoption, and the on-disk format.
//!
//! `Lossless.dll` holds its compute shaders as RCDATA blobs. Depending on the
//! build they are either SPIR-V already (two variants, `fp16` and `fp32`, at
//! fixed id offsets) or DXBC that has to be translated. This module picks a
//! variant, normalises the descriptor bindings, and writes the result to a cache
//! file that `vkr_lsfg` loads once per session.
//!
//! Replacement for `lsfg_dll.c`. The cache format is unchanged (magic
//! `0x4746534C`, version 1, FNV-1a over the source DLL), so a cache built by
//! GameNative's app-side code loads here and vice versa.

use std::collections::BTreeMap;
use std::path::Path;

use crate::pe::{PeImage, MAX_RESOURCE_ID};
use crate::LsfgStatus;

/// Shader ids the engine expects, in the order they are written to the cache.
pub const SHADER_IDS: [u32; 25] = [
    255, 256, 280, 281, 282, 283, 284, 285, 286, 287, 288, 289, 290, 291, 292, 293, 294, 295, 296,
    297, 298, 299, 300, 301, 302,
];

/// Id offsets of the pre-built SPIR-V variants inside the resource table.
const VARIANT_FP16_OFFSET: u32 = 49;
const VARIANT_FP32_OFFSET: u32 = 98;

const CACHE_MAGIC: u32 = 0x4746_534C;
const CACHE_VERSION: u32 = 1;
const HEADER_BYTES: usize = 32;
const MAX_SPIRV_WORDS: usize = 16 * 1024 * 1024;

const SPIRV_MAGIC: u32 = 0x0723_0203;
const SPIRV_HEADER_WORDS: usize = 5;
const SPIRV_WORD_COUNT_SHIFT: u32 = 16;
const SPIRV_OPCODE_MASK: u32 = 0xFFFF;
const SPIRV_OP_FUNCTION: u32 = 54;
const SPIRV_OP_DECORATE: u32 = 71;
const SPIRV_DECORATION_BINDING: u32 = 33;
const SPIRV_DECORATION_DESCRIPTOR_SET: u32 = 34;
/// Offset of the literal a decoration writes, within the instruction.
const DECORATION_LITERAL_WORD: usize = 3;

/// Which set of shaders the cache holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum Variant {
    None = 0,
    Fp16 = 1,
    Fp32 = 2,
    Dxbc = 3,
}

impl Variant {
    fn offset(self) -> u32 {
        match self {
            Self::Fp16 => VARIANT_FP16_OFFSET,
            _ => VARIANT_FP32_OFFSET,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Fp16 => "spirv-fp16",
            Self::Fp32 => "spirv-fp32",
            Self::Dxbc => "dxbc-translated",
            Self::None => "none",
        }
    }
}

/// One shader module: its engine id and its SPIR-V words.
pub struct Module {
    pub id: u32,
    pub words: Vec<u32>,
}

/// What the cache file starts with.
struct Header {
    source_size: u64,
    source_hash: u64,
    variant: Variant,
}

extern "C" {
    /// dxvk's DXBC→SPIR-V translator, in `lsfg/dxbc`. Returns a `malloc`ed
    /// buffer the caller owns.
    fn lsfg_translate_dxbc(
        bytecode: *const u8,
        size: u32,
        out_words: *mut *mut u32,
        out_word_count: *mut u32,
    ) -> bool;
}

/// FNV-1a over the whole DLL, with the offset basis GameNative's cache uses.
#[must_use]
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 1_469_598_103_934_665_603;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

fn is_spirv(blob: Option<&[u8]>) -> bool {
    let Some(blob) = blob else { return false };
    if blob.len() < SPIRV_HEADER_WORDS * 4 || blob.len() % 4 != 0 {
        return false;
    }
    matches!(read_word(blob, 0), Some(SPIRV_MAGIC))
}

fn read_word(words: &[u8], index: usize) -> Option<u32> {
    let at = index.checked_mul(4)?;
    let bytes = words.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Descriptor set the variable decorated at `target` belongs to; 0 when absent.
fn descriptor_set(words: &[u32], target: u32) -> u32 {
    let mut offset = SPIRV_HEADER_WORDS;
    while offset < words.len() {
        let length = (words[offset] >> SPIRV_WORD_COUNT_SHIFT) as usize;
        let opcode = words[offset] & SPIRV_OPCODE_MASK;
        if length == 0 || offset + length > words.len() || opcode == SPIRV_OP_FUNCTION {
            break;
        }
        if opcode == SPIRV_OP_DECORATE
            && length >= 4
            && words[offset + 2] == SPIRV_DECORATION_DESCRIPTOR_SET
            && words[offset + 1] == target
        {
            return words[offset + 3];
        }
        offset += length;
    }
    0
}

/// Renumber `Binding` decorations to a dense 0..n range ordered by
/// `(descriptor set, binding)`.
///
/// The shaders were authored against the Windows app's descriptor layout; the
/// engine builds its sets in this order, so the literal in each decoration has
/// to match the position the engine will bind at.
fn renumber_bindings(words: &mut [u32]) -> bool {
    let mut slots: Vec<(u32, u32, usize)> = Vec::new();
    let mut offset = SPIRV_HEADER_WORDS;
    while offset < words.len() {
        let length = (words[offset] >> SPIRV_WORD_COUNT_SHIFT) as usize;
        let opcode = words[offset] & SPIRV_OPCODE_MASK;
        if length == 0 || offset + length > words.len() {
            return false;
        }
        if opcode == SPIRV_OP_FUNCTION {
            break;
        }
        if opcode == SPIRV_OP_DECORATE
            && length >= 4
            && words[offset + 2] == SPIRV_DECORATION_BINDING
        {
            let set = descriptor_set(words, words[offset + 1]);
            slots.push((set, words[offset + 3], offset + DECORATION_LITERAL_WORD));
        }
        offset += length;
    }
    if slots.is_empty() {
        return true;
    }

    slots.sort_by_key(|(set, binding, _)| (*set, *binding));
    for (index, (_, _, literal)) in slots.iter().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            return false;
        };
        words[*literal] = index;
    }
    true
}

/// Take a pre-built SPIR-V blob as-is, after binding renumbering.
fn adopt_spirv(blob: &[u8]) -> Option<Vec<u32>> {
    if !is_spirv(Some(blob)) {
        return None;
    }
    let mut words: Vec<u32> = blob
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    if words.len() > MAX_SPIRV_WORDS || !renumber_bindings(&mut words) {
        return None;
    }
    Some(words)
}

/// Read the translator's `malloc`ed output into a `Vec` and release it.
fn take_translated(words: *mut u32, count: u32) -> Option<Vec<u32>> {
    if words.is_null() || count == 0 || count as usize > MAX_SPIRV_WORDS {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(words, count as usize) };
    let out = slice.to_vec();
    unsafe { libc::free(words.cast()) };
    Some(out)
}

fn translate(blob: &[u8]) -> Option<Vec<u32>> {
    let size = u32::try_from(blob.len()).ok()?;
    let mut words: *mut u32 = std::ptr::null_mut();
    let mut count = 0u32;
    let ok = unsafe { lsfg_translate_dxbc(blob.as_ptr(), size, &raw mut words, &raw mut count) };
    if !ok {
        return None;
    }
    take_translated(words, count)
}

fn has_native_variant(image: &PeImage<'_>, offset: u32) -> bool {
    SHADER_IDS.iter().all(|id| {
        image
            .resource(id + offset)
            .is_some_and(|b| is_spirv(Some(b)))
    })
}

fn has_base_chain(image: &PeImage<'_>) -> bool {
    SHADER_IDS.iter().all(|id| image.resource(*id).is_some())
}

/// Prefer `fp16` when the device supports it, then `fp32`, then translation.
fn select_variant(image: &PeImage<'_>, prefer_fp16: bool) -> Variant {
    if prefer_fp16 && has_native_variant(image, VARIANT_FP16_OFFSET) {
        return Variant::Fp16;
    }
    if has_native_variant(image, VARIANT_FP32_OFFSET) {
        return Variant::Fp32;
    }
    if has_native_variant(image, VARIANT_FP16_OFFSET) {
        return Variant::Fp16;
    }
    Variant::None
}

fn encode(header: &Header, modules: &[Module]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(HEADER_BYTES + modules.len() * 16);
    out.extend_from_slice(&CACHE_MAGIC.to_le_bytes());
    out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    out.extend_from_slice(&header.source_size.to_le_bytes());
    out.extend_from_slice(&header.source_hash.to_le_bytes());
    let count = u32::try_from(modules.len()).ok()?;
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(header.variant as u32).to_le_bytes());
    for module in modules {
        out.extend_from_slice(&module.id.to_le_bytes());
        let words = u32::try_from(module.words.len()).ok()?;
        out.extend_from_slice(&words.to_le_bytes());
        for word in &module.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    Some(out)
}

fn decode(bytes: &[u8]) -> Option<(Variant, Vec<Module>)> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let slice = bytes.get(at..at.checked_add(n)?)?;
        at += n;
        Some(slice)
    };
    let word =
        |slice: &[u8]| -> u32 { u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]) };

    if word(take(4)?) != CACHE_MAGIC || word(take(4)?) != CACHE_VERSION {
        return None;
    }
    let _size = take(8)?;
    let _hash = take(8)?;
    let count = word(take(4)?) as usize;
    let variant = match word(take(4)?) {
        0 => Variant::None,
        1 => Variant::Fp16,
        2 => Variant::Fp32,
        _ => Variant::Dxbc,
    };
    if count != SHADER_IDS.len() {
        return None;
    }

    let mut modules = Vec::with_capacity(count);
    for _ in 0..count {
        let id = word(take(4)?);
        let words = word(take(4)?) as usize;
        if words == 0 || words > MAX_SPIRV_WORDS {
            return None;
        }
        let body = take(words * 4)?;
        modules.push(Module {
            id,
            words: {
                let (chunks, _) = body.as_chunks::<4>();
                chunks.iter().map(|c| u32::from_le_bytes(*c)).collect()
            },
        });
    }
    // The engine looks modules up by id; a cache missing one is unusable even
    // though its framing is intact.
    let present: BTreeMap<u32, ()> = modules.iter().map(|m| (m.id, ())).collect();
    if SHADER_IDS.iter().any(|id| !present.contains_key(id)) {
        return None;
    }
    Some((variant, modules))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    use std::io::Write;

    let mut temp = path.as_os_str().to_owned();
    let nonce = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NONCE: AtomicU64 = AtomicU64::new(0);
        NONCE.fetch_add(1, Ordering::Relaxed)
    };
    temp.push(format!(".{}.{nonce}.tmp", std::process::id()));
    let temp = Path::new(&temp);

    let written = std::fs::File::create(temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if written.is_err() {
        let _ = std::fs::remove_file(temp);
        return false;
    }
    if std::fs::rename(temp, path).is_err() {
        let _ = std::fs::remove_file(temp);
        return false;
    }
    true
}

/// Load `Lossless.dll`, build every module, and write the cache.
pub fn build(dll_path: &Path, cache_path: &Path, prefer_fp16: bool) -> LsfgStatus {
    let Some(bytes) = std::fs::read(dll_path).ok() else {
        return LsfgStatus::NotInstalled;
    };
    let image = match PeImage::parse(&bytes) {
        Ok(image) => image,
        Err(status) => return status,
    };

    let variant = select_variant(&image, prefer_fp16);
    let translating = variant == Variant::None;
    if translating && !has_base_chain(&image) {
        return LsfgStatus::MissingShaders;
    }
    let offset = if translating { 0 } else { variant.offset() };

    let mut modules = Vec::with_capacity(SHADER_IDS.len());
    for id in SHADER_IDS {
        let blob = image.resource(id + offset);
        let words = match (translating, blob) {
            (true, Some(blob)) => translate(blob),
            (false, Some(blob)) => adopt_spirv(blob),
            _ => None,
        };
        let Some(words) = words else {
            log!(::gndrm_obf::obf!(
                "lsfg: shader {} failed ({})",
                id,
                if translating { "dxbc" } else { "spirv" }
            ));
            return LsfgStatus::TranslationFailed;
        };
        modules.push(Module { id, words });
    }

    let header = Header {
        source_size: bytes.len() as u64,
        source_hash: fnv1a64(&bytes),
        variant: if translating { Variant::Dxbc } else { variant },
    };
    let Some(encoded) = encode(&header, &modules) else {
        return LsfgStatus::CacheUnusable;
    };
    if !write_atomic(cache_path, &encoded) {
        return LsfgStatus::CacheUnusable;
    }
    log!(::gndrm_obf::obf!(
        "lsfg: cached {} modules (source={})",
        modules.len(),
        header.variant.name()
    ));
    LsfgStatus::Ok
}

/// Is the cache at `cache_path` still derived from the DLL at `dll_path`?
pub fn matches_source(cache_path: &Path, dll_path: &Path) -> Result<bool, LsfgStatus> {
    let Ok(bytes) = std::fs::read(cache_path) else {
        return Err(LsfgStatus::NotInstalled);
    };
    if bytes.len() < HEADER_BYTES {
        return Err(LsfgStatus::CacheUnusable);
    }
    let word =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    if word(0) != CACHE_MAGIC || word(4) != CACHE_VERSION {
        return Err(LsfgStatus::CacheUnusable);
    }
    let source_size = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let source_hash = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));

    let Ok(dll) = std::fs::read(dll_path) else {
        return Err(LsfgStatus::NotInstalled);
    };
    Ok(source_size == dll.len() as u64 && source_hash == fnv1a64(&dll))
}

/// Read a built cache back into modules.
pub fn load(cache_path: &Path) -> Result<(Variant, Vec<Module>), LsfgStatus> {
    let Ok(bytes) = std::fs::read(cache_path) else {
        return Err(LsfgStatus::NotInstalled);
    };
    decode(&bytes).ok_or(LsfgStatus::CacheUnusable)
}

/// Highest resource id the table can address, re-exported for the module
/// boundary.
pub const fn max_resource_id() -> usize {
    MAX_RESOURCE_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(target, set, binding)` per variable, as a shader declares them.
    fn decorated(variables: &[(u32, u32, u32)]) -> Vec<u32> {
        let mut words = vec![SPIRV_MAGIC, 0x0001_0000, 0, 100, 0];
        for (target, set, binding) in variables {
            words.extend_from_slice(&[
                (4 << SPIRV_WORD_COUNT_SHIFT) | SPIRV_OP_DECORATE,
                *target,
                SPIRV_DECORATION_DESCRIPTOR_SET,
                *set,
            ]);
            words.extend_from_slice(&[
                (4 << SPIRV_WORD_COUNT_SHIFT) | SPIRV_OP_DECORATE,
                *target,
                SPIRV_DECORATION_BINDING,
                *binding,
            ]);
        }
        // OpFunction carries its own operands, so its declared length must fit.
        words.extend_from_slice(&[(4 << SPIRV_WORD_COUNT_SHIFT) | SPIRV_OP_FUNCTION, 1, 2, 0]);
        words
    }

    #[test]
    fn fnv_matches_the_offset_basis_on_empty_input() {
        assert_eq!(fnv1a64(&[]), 1_469_598_103_934_665_603);
        assert_eq!(fnv1a64(b"a"), 0x44bd_8ad4_73cd_9906);
    }

    #[test]
    fn renumbering_orders_by_set_then_binding() {
        // Same declarations in source order: set 0 binding 3, set 0 binding 1,
        // set 1 binding 0. Dense renumbering sorts by (set, binding).
        let mut words = decorated(&[(10, 0, 3), (11, 0, 1), (12, 1, 0)]);
        assert!(renumber_bindings(&mut words));
        // Walk instructions by their declared length: the 5-word header makes
        // fixed-width chunking meaningless.
        let mut at = SPIRV_HEADER_WORDS;
        let mut bindings = Vec::new();
        while at < words.len() {
            let length = (words[at] >> SPIRV_WORD_COUNT_SHIFT) as usize;
            if length == 0 {
                break;
            }
            if words[at] & SPIRV_OPCODE_MASK == SPIRV_OP_DECORATE
                && words[at + 2] == SPIRV_DECORATION_BINDING
            {
                bindings.push(words[at + 3]);
            }
            at += length;
        }
        assert_eq!(bindings, vec![1, 0, 2]);
    }

    #[test]
    fn renumbering_rejects_a_truncated_instruction() {
        let mut words = vec![SPIRV_MAGIC, 0, 0, 0, 0, (9 << SPIRV_WORD_COUNT_SHIFT) | 1];
        assert!(!renumber_bindings(&mut words));
    }

    #[test]
    fn cache_round_trips_and_rejects_a_missing_shader() {
        let modules: Vec<Module> = SHADER_IDS
            .iter()
            .map(|id| Module {
                id: *id,
                words: vec![SPIRV_MAGIC, 0, 0, 0, 0],
            })
            .collect();
        let header = Header {
            source_size: 42,
            source_hash: 7,
            variant: Variant::Fp16,
        };
        let encoded = encode(&header, &modules).expect("encode");
        let (variant, decoded) = decode(&encoded).expect("decode");
        assert_eq!(variant, Variant::Fp16);
        assert_eq!(decoded.len(), SHADER_IDS.len());
        assert_eq!(decoded[0].id, 255);

        // Dropping one module makes the set unusable even though framing holds.
        let short: Vec<Module> = SHADER_IDS
            .iter()
            .skip(1)
            .map(|id| Module {
                id: *id,
                words: vec![SPIRV_MAGIC],
            })
            .collect();
        assert!(decode(&encode(&header, &short).expect("encode")).is_none());
    }

    #[test]
    fn a_non_spirv_blob_is_not_adopted() {
        assert!(adopt_spirv(b"not spirv at all").is_none());
        assert!(adopt_spirv(&[]).is_none());
    }
}
