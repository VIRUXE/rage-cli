// Thin adapter over rpf_archive for rpf-cli commands.
// Re-exports rpf_archive types that commands use directly.
pub use rpf_archive::{
    DirNode, FileRef, GtaKeys, RpfArchive, RpfEncryption,
    build_directory_tree, list_all_files,
};

use anyhow::Result;
use std::path::{Path, PathBuf};
pub use rpf_archive::RpfVersion;

static IMG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_img_dir(path: PathBuf) { let _ = IMG_DIR.set(path); }

/// The bytes an [`Archive`] reads from: a shared buffer plus the window of
/// it that holds this archive. A top-level archive is memory-mapped, so only
/// the pages actually touched (the TOC, the entries extracted) are read from
/// disk; a nested archive stored uncompressed and unencrypted is a narrower
/// window onto its parent's buffer instead of a copy of it.
#[derive(Clone)]
struct Backing {
    buffer: std::sync::Arc<Buffer>,
    range : std::ops::Range<usize>,
}

enum Buffer {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl Backing {
    fn owned(data: Vec<u8>) -> Self {
        let range = 0..data.len();
        Self { buffer: std::sync::Arc::new(Buffer::Owned(data)), range }
    }

    fn bytes(&self) -> &[u8] {
        let all: &[u8] = match &*self.buffer {
            Buffer::Owned(v) => v,
            Buffer::Mapped(m) => m,
        };
        &all[self.range.clone()]
    }
}

/// Full archive with parsed metadata, directory tree, and its bytes.
pub struct Archive {
    #[allow(dead_code)]
    pub path        : std::path::PathBuf,
    pub encryption  : RpfEncryption,
    pub entry_count : usize,
    pub dir_count   : usize,
    pub root        : DirNode,
    archive         : RpfArchive,
    rdr2            : Option<crate::rdr2::Archive>,
    data            : Backing,
    img1_dir        : Option<Vec<u8>>,
}

impl Archive {
    /// Memory-maps the file rather than reading it: GTA V's top-level
    /// archives run to gigabytes and most callers touch a small part.
    /// Falls back to a plain read where mapping fails (e.g. an empty file).
    pub fn open(path: &Path, keys: Option<&GtaKeys>) -> Result<Self> {
        Self::open_with_dir(path, IMG_DIR.get().map(PathBuf::as_path), keys)
    }

    pub fn open_with_dir(path: &Path, dir: Option<&Path>, keys: Option<&GtaKeys>) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the map is read-only; a game archive modified by another
        // process while mapped would yield garbage entries, not UB in safe
        // code that only ever reads bytes out of it.
        let backing = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(map) => {
                let range = 0..map.len();
                Backing { buffer: std::sync::Arc::new(Buffer::Mapped(map)), range }
            }
            Err(_) => Backing::owned(std::fs::read(path)?),
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let img = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("img"));
        let known_magic = backing.bytes().get(..4).is_some_and(|m|
            m == b"VER2" || m == [0x52, 0x2a, 0x4e, 0xa9]);
        if img && !known_magic {
            let paired = path.with_extension("dir");
            let dir = dir.unwrap_or(&paired);
            let toc = std::fs::read(dir).map_err(|e| anyhow::anyhow!(
                "IMG1 requires its paired DIR file '{}': {e} (or pass --img-dir)", dir.display()))?;
            validate_sector_toc(&toc, backing.bytes().len(), false)?;
            let archive = RpfArchive::parse_img1(&toc, &name)?;
            let mut result = Self::with_archive(backing, archive, None, &name)?;
            result.img1_dir = Some(toc);
            return Ok(result);
        }
        Self::from_backing(backing, &name, keys)
    }

    /// Parse an archive from in-memory bytes (used to descend into nested RPFs).
    pub fn from_bytes(data: Vec<u8>, name: &str, keys: Option<&GtaKeys>) -> Result<Self> {
        Self::from_backing(Backing::owned(data), name, keys)
    }

    fn from_backing(data: Backing, name: &str, keys: Option<&GtaKeys>) -> Result<Self> {
        if data.bytes().starts_with(b"VER2") {
            let bytes = data.bytes();
            anyhow::ensure!(bytes.len() >= 8, "IMG2 header truncated");
            let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
            let end = count.checked_mul(32).and_then(|n| n.checked_add(8)).ok_or_else(|| anyhow::anyhow!("IMG2 TOC overflow"))?;
            let toc = bytes.get(8..end).ok_or_else(|| anyhow::anyhow!("IMG2 TOC truncated"))?;
            validate_sector_toc(toc, bytes.len(), true)?;
        }
        if data.bytes().starts_with(&[0x52, 0x2a, 0x4e, 0xa9]) {
            validate_img3(data.bytes())?;
        }
        let header = data.bytes();
        let (rdr2, archive) = if header.len() >= 4
            && u32::from_le_bytes(header[..4].try_into().unwrap()) == rpf_archive::RPF8_MAGIC
        {
            let (rdr2, archive) = crate::rdr2::Archive::parse(header, name)?;
            (Some(rdr2), archive)
        } else {
            (None, RpfArchive::parse(header, name, keys)?)
        };

        Self::with_archive(data, archive, rdr2, name)
    }

    fn with_archive(data: Backing, archive: RpfArchive, rdr2: Option<crate::rdr2::Archive>, name: &str) -> Result<Self> {

        let encryption = archive.encryption;
        let entry_count = archive.entries.len();
        let dir_count = archive.entries.iter().filter(|e| e.is_directory()).count();
        let mut root = build_directory_tree(&archive.entries);
        if let Some(rdr2) = &rdr2 {
            for file in &mut root.files { file.mem_size = rdr2.entry_size(file.entry_index); }
        }

        Ok(Self { path: PathBuf::from(name), encryption, entry_count, dir_count, root, archive, rdr2, data, img1_dir: None })
    }

    pub fn version(&self) -> RpfVersion { self.archive.version }

    /// The complete stored payload, retaining IMG3 resource headers/compression
    /// for archive copying. Semantic resource consumers use `extract` instead.
    pub fn rebuild_entry_bytes(&self, file: &FileRef, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
        if self.version() != RpfVersion::Img3 { return self.extract(file, keys); }
        let row = self.img3_rows().into_iter().find(|(name, _)| name.eq_ignore_ascii_case(&file.name))
            .map(|(_, row)| row).ok_or_else(|| anyhow::anyhow!("IMG3 entry not found"))?;
        let start = u32::from_le_bytes(row[8..12].try_into().unwrap()) as usize * 2048;
        let size = u16::from_le_bytes(row[12..14].try_into().unwrap()) as usize * 2048
            - (u16::from_le_bytes(row[14..16].try_into().unwrap()) & 0x7ff) as usize;
        Ok(self.data.bytes()[start..start + size].to_vec())
    }

    fn img3_rows(&self) -> Vec<(String, &[u8])> {
        let bytes = self.data.bytes();
        let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let stride = u16::from_le_bytes(bytes[16..18].try_into().unwrap()) as usize;
        let stride = if stride == 0 { 16 } else { stride };
        let mut names = &bytes[20 + count * stride..];
        bytes[20..20 + count * stride].chunks_exact(stride).map(|row| {
            let end = names.iter().position(|b| *b == 0).unwrap();
            let name = String::from_utf8_lossy(&names[..end]).into_owned();
            names = &names[end + 1..]; (name, row)
        }).collect()
    }

    /// Carry uninterpreted directory bytes through IMG1/2 rebuilding. Sector
    /// offsets/counts are regenerated by the writer; names and reserved bytes
    /// belong to the source entry and are not normalized away.
    pub fn preserve_directory_metadata(&self, bytes: &mut [u8], dir: Option<&mut [u8]>) {
        if self.version() == RpfVersion::Img3 {
            bytes[4..8].copy_from_slice(&self.data.bytes()[4..8]);
            bytes[18..20].copy_from_slice(&self.data.bytes()[18..20]);
            let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let mut names = bytes[20 + count * 16..].split(|b| *b == 0);
            let keys: Vec<_> = (0..count).map(|_| String::from_utf8_lossy(names.next().unwrap()).to_lowercase()).collect();
            let original: std::collections::HashMap<_, _> = self.img3_rows().into_iter().map(|(n, r)| (n.to_lowercase(), r)).collect();
            for (key, row) in keys.iter().zip(bytes[20..20 + count * 16].chunks_exact_mut(16)) {
                if let Some(source) = original.get(key) {
                    row[..8].copy_from_slice(&source[..8]);
                    let flags = u16::from_le_bytes(source[14..16].try_into().unwrap()) & !0x7ff;
                    let padding = u16::from_le_bytes(row[14..16].try_into().unwrap()) & 0x7ff;
                    row[14..16].copy_from_slice(&(flags | padding).to_le_bytes());
                }
            }
            return;
        }
        let (old, new, img2) = match self.version() {
            RpfVersion::Img1 => (self.img1_dir.as_deref().unwrap_or(&[]), dir.unwrap_or(&mut []), false),
            RpfVersion::Img2 => {
                let count = self.archive.entries.len();
                let new_count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
                (&self.data.bytes()[8..8 + count * 32], &mut bytes[8..8 + new_count * 32], true)
            }
            _ => return,
        };
        let name = |row: &[u8]| row[8..].split(|b| *b == 0).next().unwrap().to_ascii_lowercase();
        let originals: std::collections::HashMap<_, _> = old.chunks_exact(32).map(|row| (name(row), row)).collect();
        for row in new.chunks_exact_mut(32) {
            if let Some(source) = originals.get(&name(row)) {
                row[8..].copy_from_slice(&source[8..]);
                if img2 { row[6..8].copy_from_slice(&source[6..8]); }
            }
        }
    }

    pub fn verify_ranges(&self) -> Result<()> {
        for file in self.list_files() {
            match self.entry_kind(file) {
                rpf_archive::RpfEntryKind::BinaryFile { file_offset, file_size, uncompressed_size, .. } => {
                    let scale = if self.archive.version == RpfVersion::V7 { 512 } else { 1 };
                    let start = (*file_offset as usize).checked_mul(scale).and_then(|n| n.checked_add(self.archive.start_offset));
                    let len = if *file_size == 0 { *uncompressed_size } else { *file_size } as usize;
                    anyhow::ensure!(start.and_then(|n| n.checked_add(len)).is_some_and(|end| end <= self.data.bytes().len()), "entry '{}' extends beyond archive", file.path);
                }
                rpf_archive::RpfEntryKind::ResourceFile { file_offset, file_size, .. } => {
                    let scale = if self.archive.version == RpfVersion::V7 { 512 } else { 1 };
                    let start = (*file_offset as usize).checked_mul(scale).and_then(|n| n.checked_add(self.archive.start_offset));
                    anyhow::ensure!(start.and_then(|n| n.checked_add(*file_size as usize)).is_some_and(|end| end <= self.data.bytes().len()), "resource '{}' extends beyond archive", file.path);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Opens a nested `.rpf` entry as an archive. Nested archives are stored
    /// uncompressed and unencrypted in practice, so this is a window onto
    /// this archive's bytes, not a copy; anything else is extracted.
    pub fn open_nested(&self, file: &FileRef, keys: Option<&GtaKeys>) -> Result<Self> {
        if let Some(range) = self.stored_range(file) {
            let mut data = self.data.clone();
            data.range = data.range.start + range.start..data.range.start + range.end;
            return Self::from_backing(data, &file.name, keys);
        }
        Self::from_bytes(self.extract(file, keys)?, &file.name, keys)
    }

    /// The byte range (within this archive) of a plain stored binary entry:
    /// unencrypted and uncompressed, so its bytes are usable as they lie.
    fn stored_range(&self, file: &FileRef) -> Option<std::ops::Range<usize>> {
        if self.rdr2.is_some() { return None; }
        let rpf_archive::RpfEntryKind::BinaryFile { file_offset, file_size, uncompressed_size, is_encrypted } =
            self.archive.entries[file.entry_index].kind
        else {
            return None;
        };
        if is_encrypted || (file_size != 0 && file_size != uncompressed_size) || uncompressed_size == 0 {
            return None;
        }
        let offset = match self.archive.version {
            rpf_archive::RpfVersion::V7 => file_offset as usize * 512,
            _ => file_offset as usize,
        };
        let start = self.archive.start_offset + offset;
        let end = start.checked_add(uncompressed_size as usize)?;
        (end <= self.data.range.len()).then_some(start..end)
    }

    /// Entry names and contents decode to noise without keys, so say so
    /// instead of printing garbage.
    pub fn require_keys(&self, keys: Option<&GtaKeys>) -> Result<()> {
        if keys.is_none() && matches!(self.encryption, RpfEncryption::Ng | RpfEncryption::Aes) {
            anyhow::bail!(
                "archive is {:?}-encrypted — pass --exe <GTA5.exe> or set GTAV_PATH",
                self.encryption
            );
        }
        Ok(())
    }

    pub fn list_files(&self) -> Vec<&FileRef> {
        list_all_files(&self.root)
    }

    pub fn find_file(&self, path: &str) -> Option<&FileRef> {
        let path = path.replace('\\', "/").to_lowercase();
        find_in_dir(&self.root, &path)
    }

    pub fn extract(&self, file: &FileRef, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
        if let Some(rdr2) = &self.rdr2 {
            return rdr2.extract(self.data.bytes(), file.entry_index);
        }
        let entry = &self.archive.entries[file.entry_index];
        self.archive.extract_entry(self.data.bytes(), entry, keys)
    }

    /// The inflated system section of an RSC7 resource entry (structs, names
    /// and pointers, no vertex or pixel data). For an unencrypted RPF7 entry
    /// it is inflated straight out of the archive's mapping and stops at the
    /// end of the system section, so the graphics pages are never read;
    /// anything else is extracted whole first.
    pub fn resource_system(&self, file: &FileRef, keys: Option<&GtaKeys>) -> Result<Vec<u8>> {
        let entry = &self.archive.entries[file.entry_index];
        if let rpf_archive::RpfEntryKind::ResourceFile { file_offset, file_size, system_flags, is_encrypted: false, .. } = entry.kind
            && matches!(self.archive.version, rpf_archive::RpfVersion::V7)
            && file_size as usize > 16
        {
            let start = self.archive.start_offset + file_offset as usize * 512;
            if let Some(body) = self.data.bytes().get(start + 16..start + file_size as usize) {
                return rage_formats::inflate_rsc7_system(body, system_flags);
            }
        }
        rage_formats::prepare_rsc7_system(&self.extract(file, keys)?)
    }

    pub fn entry_kind(&self, file: &FileRef) -> &rpf_archive::RpfEntryKind {
        &self.archive.entries[file.entry_index].kind
    }
}

fn find_in_dir<'a>(dir: &'a DirNode, path: &str) -> Option<&'a FileRef> {
    for f in &dir.files {
        if f.path == path || f.name.to_lowercase() == path { return Some(f); }
    }
    for sub in &dir.subdirs {
        if let Some(f) = find_in_dir(sub, path) { return Some(f); }
    }
    None
}

fn validate_sector_toc(toc: &[u8], length: usize, img2: bool) -> Result<()> {
    anyhow::ensure!(toc.len() % 32 == 0, "IMG DIR table is not aligned to 32 bytes");
    let mut ranges = Vec::new();
    let mut names = std::collections::HashSet::new();
    let minimum = if img2 { (8 + toc.len()).div_ceil(2048) * 2048 } else { 0 };
    for row in toc.chunks_exact(32) {
        let start = u32::from_le_bytes(row[..4].try_into().unwrap()).checked_mul(2048)
            .ok_or_else(|| anyhow::anyhow!("IMG sector offset overflow"))? as usize;
        let sectors = if img2 { u16::from_le_bytes(row[4..6].try_into().unwrap()) as u32 }
            else { u32::from_le_bytes(row[4..8].try_into().unwrap()) };
        let size = sectors.checked_mul(2048).ok_or_else(|| anyhow::anyhow!("IMG sector size overflow"))? as usize;
        let name = row[8..].split(|b| *b == 0).next().unwrap();
        anyhow::ensure!(!name.is_empty(), "IMG entry has an empty name");
        anyhow::ensure!(names.insert(name.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>()), "duplicate IMG entry name");
        anyhow::ensure!(size == 0 || start >= minimum, "IMG entry overlaps its directory");
        let end = start.checked_add(size).ok_or_else(|| anyhow::anyhow!("IMG entry size overflow"))?;
        anyhow::ensure!(end <= length, "IMG entry extends beyond archive");
        if size != 0 { ranges.push((start, end)); }
    }
    ranges.sort_unstable();
    anyhow::ensure!(ranges.windows(2).all(|r| r[0].1 <= r[1].0), "IMG entry sectors overlap");
    Ok(())
}

fn validate_img3(bytes: &[u8]) -> Result<()> {
    anyhow::ensure!(bytes.len() >= 20, "IMG3 header truncated");
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let header = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let stride = u16::from_le_bytes(bytes[16..18].try_into().unwrap()) as usize;
    let stride = if stride == 0 { 16 } else { stride };
    anyhow::ensure!(stride >= 16, "IMG3 entry size is smaller than 16 bytes");
    anyhow::ensure!(count.checked_mul(stride).is_some_and(|n| n <= header)
        && header.checked_add(20).is_some_and(|end| end <= bytes.len()), "IMG3 directory truncated or overflowing");
    let names_start = 20 + count * stride;
    let mut names = &bytes[names_start..20 + header];
    for _ in 0..count {
        let end = names.iter().position(|b| *b == 0).ok_or_else(|| anyhow::anyhow!("IMG3 name table is truncated"))?;
        anyhow::ensure!(end > 0, "IMG3 entry has an empty name");
        names = &names[end + 1..];
    }
    let mut ranges = Vec::new();
    for row in bytes[20..names_start].chunks_exact(stride) {
        let offset = u32::from_le_bytes(row[8..12].try_into().unwrap()).checked_mul(2048)
            .ok_or_else(|| anyhow::anyhow!("IMG3 sector offset overflows"))? as usize;
        let sectors = u16::from_le_bytes(row[12..14].try_into().unwrap()) as usize;
        let padding = (u16::from_le_bytes(row[14..16].try_into().unwrap()) & 0x7ff) as usize;
        let length = (sectors * 2048).checked_sub(padding).ok_or_else(|| anyhow::anyhow!("IMG3 padding exceeds entry size"))?;
        anyhow::ensure!(length == 0 || offset >= 20 + header, "IMG3 entry overlaps its directory");
        anyhow::ensure!(offset.checked_add(length).is_some_and(|end| end <= bytes.len()), "IMG3 entry extends beyond archive");
        if length > 0 { ranges.push((offset, offset + length)); }
    }
    ranges.sort_unstable();
    anyhow::ensure!(ranges.windows(2).all(|r| r[0].1 <= r[1].0), "IMG3 entry ranges overlap");
    Ok(())
}
