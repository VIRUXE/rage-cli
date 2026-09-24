// Thin adapter over rpf_archive for rpf-cli commands.
// Re-exports rpf_archive types that commands use directly.
pub use rpf_archive::{
    DirNode, FileRef, GtaKeys, RpfArchive, RpfEncryption,
    build_directory_tree, list_all_files,
};

use anyhow::Result;
use std::path::{Path, PathBuf};

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
    data            : Backing,
}

impl Archive {
    /// Memory-maps the file rather than reading it: GTA V's top-level
    /// archives run to gigabytes and most callers touch a small part.
    /// Falls back to a plain read where mapping fails (e.g. an empty file).
    pub fn open(path: &Path, keys: Option<&GtaKeys>) -> Result<Self> {
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
        Self::from_backing(backing, &name, keys)
    }

    /// Parse an archive from in-memory bytes (used to descend into nested RPFs).
    pub fn from_bytes(data: Vec<u8>, name: &str, keys: Option<&GtaKeys>) -> Result<Self> {
        Self::from_backing(Backing::owned(data), name, keys)
    }

    fn from_backing(data: Backing, name: &str, keys: Option<&GtaKeys>) -> Result<Self> {
        let archive = RpfArchive::parse(data.bytes(), name, keys)?;

        let encryption = archive.encryption;
        let entry_count = archive.entries.len();
        let dir_count = archive.entries.iter().filter(|e| e.is_directory()).count();
        let root = build_directory_tree(&archive.entries);

        Ok(Self { path: PathBuf::from(name), encryption, entry_count, dir_count, root, archive, data })
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
        let entry = &self.archive.entries[file.entry_index];
        self.archive.extract_entry(self.data.bytes(), entry, keys)
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
