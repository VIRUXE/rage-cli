//! RDR2 PC RPF8 reader. Layout reference: librdr3 (MIT).
//! Preserve the full 35-bit byte offsets and entry encryption/compression
//! fields, which rpf-archive 0.10's generic entries cannot represent.
mod crypto;
mod ids;
pub mod metadata;

use anyhow::{Context, Result, bail};
use rpf_archive::{RpfArchive, RpfEncryption, RpfEntry, RpfEntryKind, RpfVersion};
use std::{
    io::Read,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

static KEYS_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static KEYS: OnceLock<std::result::Result<Arc<crypto::Keys>, String>> = OnceLock::new();
pub fn set_keys_path(path: Option<PathBuf>) {
    let _ = KEYS_PATH.set(path);
}
pub use crypto::extract as extract_keys;
fn keys() -> Result<Arc<crypto::Keys>> {
    fn load() -> Result<Arc<crypto::Keys>> {
        let path = KEYS_PATH.get().and_then(|p| p.as_ref())
            .context("RDR2 TFIT2 decryption requires --rdr2-keys <secrets.bin> (or RAGE_RDR2_KEYS); GTA V --exe/GTAV_PATH keys do not apply")?;
        crypto::Keys::load(path).map(Arc::new)
    }
    KEYS.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

pub fn u32_at(data: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(at..at + 4)
            .context("RDR2 truncated u32")?
            .try_into()
            .unwrap(),
    ))
}
pub fn u64_at(data: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(at..at + 8)
            .context("RDR2 truncated u64")?
            .try_into()
            .unwrap(),
    ))
}

struct Entry {
    a: u64,
    b: u64,
    c: u64,
}
impl Entry {
    fn resource(&self) -> bool {
        self.a & (1 << 56) != 0
    }
    fn disk_size(&self) -> usize {
        ((self.b & 0xFFFFFFF) << 4) as usize
    }
    fn offset(&self) -> usize {
        (((self.b >> 28) & 0x7FFFFFFF) << 4) as usize
    }
    fn compressor(&self) -> u8 {
        (self.b >> 59) as u8
    }
    fn size(&self) -> Result<usize> {
        let size = if self.resource() {
            (self.c & 0xFFFFFFF0) + ((self.c >> 32) & 0xFFFFFFF0)
        } else {
            self.c
        };
        usize::try_from(size).context("RDR2 entry too large for this platform")
    }
}
pub struct Archive {
    entries: Vec<Entry>,
    cipher: Option<Arc<crypto::Keys>>,
}

impl Archive {
    pub fn entry_size(&self, index: usize) -> u32 {
        self.entries[index].size().unwrap() as u32
    }
    pub fn parse(data: &[u8], name: &str) -> Result<(Self, RpfArchive)> {
        Self::parse_with_keys(data, name, None)
    }

    fn parse_with_keys(
        data: &[u8],
        name: &str,
        mut cipher: Option<Arc<crypto::Keys>>,
    ) -> Result<(Self, RpfArchive)> {
        if data.len() < 0x110 {
            bail!("RPF8 header/signature truncated");
        }
        let platform = u16::from_le_bytes(data[14..16].try_into().unwrap());
        if platform != b'y' as u16 {
            bail!("RPF8 platform 0x{platform:04x} is not supported; expected RDR2 PC ('y')");
        }
        let tag = u16::from_le_bytes(data[12..14].try_into().unwrap());
        let count = u32_at(data, 4)? as usize;
        let length = count.checked_mul(24).context("RPF8 entry count overflow")?;
        let end = 0x110usize
            .checked_add(length)
            .context("RPF8 TOC size overflow")?;
        let mut toc = data
            .get(0x110..end)
            .context("RPF8 entries truncated")?
            .to_vec();
        if tag != 0xFF {
            let material = match &cipher {
                Some(k) => k.clone(),
                None => keys()?,
            };
            material.decrypt(tag, &mut toc)?;
            cipher = Some(material);
        }
        let mut entries = Vec::new();
        let mut generic = Vec::new();
        for rec in toc.chunks_exact(24) {
            let e = Entry {
                a: u64_at(rec, 0)?,
                b: u64_at(rec, 8)?,
                c: u64_at(rec, 16)?,
            };
            let ext_id = (e.a >> 48) as u8;
            if ext_id == 0xFE {
                bail!("RPF8 directory entries are not supported");
            }
            let name = format!("{:08X}.{}", e.a as u32, extension(ext_id));
            let size = u32::try_from(e.size()?)
                .context("RPF8 entry exceeds the CLI's 4 GiB file-size limit")?;
            let kind = if e.resource() {
                RpfEntryKind::ResourceFile {
                    file_offset: 0,
                    file_size: e.disk_size() as u32,
                    system_flags: e.c as u32,
                    graphics_flags: (e.c >> 32) as u32,
                    is_encrypted: (e.a >> 40) as u8 != 0xFF,
                }
            } else {
                RpfEntryKind::BinaryFile {
                    file_offset: 0,
                    file_size: e.disk_size() as u32,
                    uncompressed_size: size,
                    is_encrypted: (e.a >> 40) as u8 != 0xFF,
                }
            };
            let stored_end = e
                .offset()
                .checked_add(e.disk_size())
                .context("RPF8 entry offset overflow")?;
            if stored_end > data.len() {
                bail!(
                    "RPF8 entry {name} lies outside the archive; incorrect decryption keys or corrupt TOC"
                );
            }
            generic.push(RpfEntry {
                name_lower: name.to_lowercase(),
                name,
                kind,
            });
            entries.push(e);
        }
        let header = RpfArchive {
            name: name.into(),
            start_offset: 0,
            entries: generic,
            encryption: if tag == 0xFF {
                RpfEncryption::None
            } else {
                RpfEncryption::Tfit
            },
            version: RpfVersion::V8,
        };
        Ok((Self { entries, cipher }, header))
    }

    pub fn extract(&self, data: &[u8], index: usize) -> Result<Vec<u8>> {
        let e = self
            .entries
            .get(index)
            .context("RPF8 entry index out of range")?;
        let mut start = e.offset();
        let mut raw = e.disk_size();
        if e.a & (1 << 57) != 0 {
            raw = raw.checked_sub(0x100).context("RPF8 signature truncated")?;
        }
        if e.resource() {
            start += 16;
            raw = raw
                .checked_sub(16)
                .context("RPF8 resource header truncated")?;
        }
        let size = e.size()?;
        let length = if e.compressor() == 0 { size } else { raw };
        let end = start
            .checked_add(length)
            .context("RPF8 entry size overflow")?;
        let mut input = data
            .get(start..end)
            .context("RPF8 entry payload truncated")?
            .to_vec();
        let tag = (e.a >> 40) as u8 as u16;
        if tag != 0xFF {
            let chunk = if e.resource() {
                if e.compressor() != 0 { 0x80000 } else { size }
            } else if e.compressor() != 0 {
                0x2000
            } else {
                0x1000
            };
            let material = match &self.cipher {
                Some(k) => k.clone(),
                None => keys()?,
            };
            material.decrypt_ranges(
                tag,
                &mut input,
                &cipher_ranges((e.a >> 32) as u8, raw, chunk),
            )?;
        }
        let payload = decompress(e.compressor(), &input, size)?;
        if !e.resource() {
            return Ok(payload);
        }
        let version = ((e.c as u32 & 0xF) << 4) | ((e.c >> 32) as u32 & 0xF);
        let mut out = Vec::with_capacity(16 + payload.len());
        for v in [
            rage_formats::RSC8_MAGIC,
            version | 0x1F00,
            e.c as u32,
            (e.c >> 32) as u32,
        ] {
            out.extend(v.to_le_bytes());
        }
        out.extend(payload);
        Ok(out)
    }
}

pub fn decompress(compressor: u8, input: &[u8], size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .context("RDR2 uncompressed size too large")?;
    match compressor {
        0 => out.extend_from_slice(input.get(..size).context("RDR2 stored payload truncated")?),
        1 => {
            flate2::read::DeflateDecoder::new(input)
                .take(size as u64 + 1)
                .read_to_end(&mut out)
                .context("RDR2 deflate decompression failed")?;
        }
        2 => {
            out.resize(size, 0);
            let n = oozextract::Extractor::new()
                .read_from_slice(input, &mut out)
                .context("RDR2 Oodle decompression failed")?;
            if n != size {
                bail!("RDR2 Oodle size mismatch: expected {size}, got {n}");
            }
        }
        c => bail!("RDR2 compressor {c} is not supported"),
    }
    if out.len() != size {
        bail!(
            "RDR2 decompressed size mismatch: expected {size}, got {}",
            out.len()
        );
    }
    Ok(out)
}

fn extension(id: u8) -> &'static str {
    const BASE: [&str; 14] = [
        "rpf", "ymf", "ydr", "yft", "ydd", "ytd", "ybn", "ybd", "ypd", "ybs", "ysd", "ymt", "ysc",
        "ycs",
    ];
    const EXTRA: [&str; 24] = [
        "mrf", "cut", "gfx", "ycd", "yld", "ypmd", "ypm", "yed", "ypt", "ymap", "ytyp", "ych",
        "yldb", "yjd", "yad", "ynv", "yhn", "ypl", "ynd", "yvr", "ywr", "ynh", "yfd", "yas",
    ];
    BASE.get(id as usize)
        .or_else(|| id.checked_sub(64).and_then(|i| EXTRA.get(i as usize)))
        .copied()
        .unwrap_or("bin")
}

fn cipher_ranges(config: u8, size: usize, chunk: usize) -> Vec<(usize, usize)> {
    let head = if config & 3 == 0 {
        0
    } else {
        0x400 << ((config & 3) * 2)
    };
    let len_bits = (config >> 2) & 7;
    let length = if len_bits == 0 { 0 } else { 0x400 << len_bits };
    let stride = if len_bits == 0 {
        0
    } else {
        (((config >> 5) & 7) as usize + 1) << 16
    };
    let tail = size.saturating_sub(0x400);
    let mut result = Vec::new();
    let mut offset = head.min(size);
    if offset > 0 {
        result.push((0, offset));
    }
    if head < tail {
        if stride != 0 {
            if stride == chunk && tail < stride && stride < size {
                offset = stride;
            } else {
                let mut block = if offset != 0 { stride } else { 0 };
                while block + length <= tail {
                    let start = block.max(offset);
                    let end = (block + length).min(size);
                    if start < end {
                        result.push((start, end));
                        offset = end;
                    }
                    block += stride;
                }
            }
        }
        let start = tail.max(offset);
        if start < size {
            result.push((start, size));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_an_encrypted_toc_and_encrypted_resource_payload() {
        let cipher = Arc::new(crypto::tests::identity());
        let mut data = vec![0; 512 + 16 + 512];
        data[..4].copy_from_slice(&rpf_archive::RPF8_MAGIC.to_le_bytes());
        data[4..8].copy_from_slice(&2u32.to_le_bytes());
        data[12..14].copy_from_slice(&12u16.to_le_bytes());
        data[14] = b'y';
        for (i, hash) in [0x12345678u32, 0xABCDEF01].iter().enumerate() {
            let a = *hash as u64 | (1u64 << 32) | (12u64 << 40) | (11u64 << 48) | (1u64 << 56);
            let b = (528u64 >> 4) | ((512u64 >> 4) << 28);
            let c = 512u64 | (2u64 << 32);
            for (j, word) in [a, b, c].iter().enumerate() {
                let start = 0x110 + i * 24 + j * 8;
                data[start..start + 8].copy_from_slice(&word.to_le_bytes());
            }
        }
        crypto::tests::identity_encrypt(&mut data[0x110..0x140]);
        let payload: Vec<_> = (0..512).map(|i| i as u8).collect();
        data[528..].copy_from_slice(&payload);
        crypto::tests::identity_encrypt(&mut data[528..]);
        let (archive, header) = Archive::parse_with_keys(&data, "test.rpf", Some(cipher)).unwrap();
        assert_eq!(header.entries[0].name, "12345678.ymt");
        assert_eq!(header.entries[1].name, "ABCDEF01.ymt");
        let resource = archive.extract(&data, 1).unwrap();
        assert_eq!(&resource[..4], b"RSC8");
        assert_eq!(&resource[16..], payload);
    }

    #[test]
    fn strided_ranges_match_the_game_layout_including_small_files_and_chunk_boundary_quirk() {
        assert!(cipher_ranges(0, 512, 0x2000).is_empty());
        assert!(cipher_ranges(0, 1024, 0x2000).is_empty());
        assert_eq!(cipher_ranges(0, 2048, 0x2000), [(1024, 2048)]);
        assert_eq!(cipher_ranges(1, 512, 0x2000), [(0, 512)]);
        assert_eq!(
            cipher_ranges(4, 0x30000, 0x2000),
            [
                (0, 0x800),
                (0x10000, 0x10800),
                (0x20000, 0x20800),
                (0x2FC00, 0x30000)
            ]
        );
        assert_eq!(cipher_ranges(4, 0x10010, 0x10000), [(0x10000, 0x10010)]);
    }

    #[test]
    fn malformed_headers_and_entries_are_errors() {
        assert!(Archive::parse(b"8FPR", "bad").is_err());
        let mut header = vec![0u8; 0x110];
        header[12] = 0xFF;
        header[14] = b'y';
        header[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Archive::parse(&header, "bad").is_err());
        header[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(Archive::parse(&header, "empty").is_ok());
        header[14] = b'o';
        assert!(Archive::parse(&header, "ps4").is_err());
    }
}
