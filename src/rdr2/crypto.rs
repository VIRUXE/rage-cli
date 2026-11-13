//! TFIT2 CBC for RDR2 PC. Format reference: librdr3 (MIT, see LICENSE.librdr3).
//! Secret identifiers are fingerprints; key material is supplied by the user.
use super::ids::{CONTEXT_IDS, KEY_IDS};
use anyhow::{Context, Result, bail};
use std::{collections::HashMap, io::Write, path::Path};

const ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
const FACTOR: u32 = 0x9E3779B1;

fn fingerprint(bytes: &[u8]) -> [u8; 16] {
    let hash = bytes
        .iter()
        .fold(0u32, |h, b| h.wrapping_add(*b as u32).wrapping_mul(FACTOR));
    let mut crc = !0u64;
    for b in bytes {
        crc ^= *b as u64;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xC96C5795D7870F42u64 & 0u64.wrapping_sub(crc & 1));
        }
    }
    let mut id = [0u8; 16];
    id[..4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    id[4..8].copy_from_slice(&hash.to_le_bytes());
    id[8..].copy_from_slice(&(!crc).to_le_bytes());
    id
}

fn decode_id(id: &str) -> [u8; 16] {
    let mut result = [0; 16];
    for (src, dst) in id
        .as_bytes()
        .chunks_exact(5)
        .zip(result.chunks_exact_mut(4))
    {
        let n = src.iter().fold(0u32, |n, c| {
            n.wrapping_mul(85)
                .wrapping_add(ALPHABET.iter().position(|x| x == c).unwrap() as u32)
        });
        dst.copy_from_slice(&n.to_be_bytes());
    }
    result
}

/// Recover the known PC secrets from an executable or a contiguous process
/// memory dump. Some executable builds are packed: in that case scan a dump
/// of the running game, or import Swage's secrets.bin directly.
pub fn extract(input: &Path, output: &Path) -> Result<()> {
    let file = std::fs::File::open(input)?;
    let data =
        unsafe { memmap2::Mmap::map(&file) }.context("mapping RDR2 executable/memory dump")?;
    let ids: Vec<_> = KEY_IDS
        .iter()
        .chain(CONTEXT_IDS.iter())
        .copied()
        .chain(std::iter::once("5C8xGcF{L^AxqTQd!ofA"))
        .map(decode_id)
        .collect();
    let found = find_secrets(&data, &ids);
    let mut store = Vec::new();
    for bytes in &found {
        store.write_all(&(bytes.len() as u32).to_le_bytes())?;
        store.write_all(bytes)?;
    }
    // Do not save a misleading partial key file.
    let keys = Keys::from_store(&store).with_context(|| format!(
        "found {} of {} RDR2 secrets; scan an unpacked executable/process memory dump or supply Swage secrets.bin", found.len(), ids.len()))?;
    if keys.keys.is_empty() {
        bail!("no RDR2 PC cipher keys found");
    }
    std::fs::write(output, store).with_context(|| format!("writing {}", output.display()))?;
    eprintln!(
        "Wrote {} RDR2 PC keys and cipher tables to {}",
        keys.keys.len(),
        output.display()
    );
    Ok(())
}

fn find_secrets<'a>(data: &'a [u8], ids: &[[u8; 16]]) -> Vec<&'a [u8]> {
    use rayon::prelude::*;
    let mut by_size: HashMap<usize, HashMap<u32, Vec<[u8; 16]>>> = HashMap::new();
    for id in ids {
        let size = u32::from_le_bytes(id[..4].try_into().unwrap()) as usize;
        let hash = u32::from_le_bytes(id[4..8].try_into().unwrap());
        by_size
            .entry(size)
            .or_default()
            .entry(hash)
            .or_default()
            .push(*id);
    }
    by_size
        .into_par_iter()
        .flat_map_iter(|(size, mut wanted)| {
            let mut found = Vec::new();
            if size == 0 || size > data.len() {
                return found;
            }
            let factor = FACTOR.wrapping_pow(size as u32);
            let first = fingerprint(&data[..size]);
            let mut hash = u32::from_le_bytes(first[4..8].try_into().unwrap());
            for start in 0..=data.len() - size {
                if let Some(candidates) = wanted.get(&hash) {
                    let bytes = &data[start..start + size];
                    if candidates.contains(&fingerprint(bytes)) {
                        found.push(bytes);
                        // Real identifiers are unique; finding another copy of
                        // the same secret does not require scanning it again.
                        wanted.remove(&hash);
                        if wanted.is_empty() {
                            break;
                        }
                    }
                }
                if start + size < data.len() {
                    hash = hash
                        .wrapping_sub((data[start] as u32).wrapping_mul(factor))
                        .wrapping_add(data[start + size] as u32)
                        .wrapping_mul(FACTOR);
                }
            }
            found
        })
        .collect()
}

struct Block {
    masks: Vec<u64>,
    xor: usize,
}
struct Round {
    lookup: Vec<u64>,
    blocks: Vec<Block>,
}
pub struct Keys {
    keys: HashMap<u16, Vec<u64>>,
    init: Vec<u64>,
    rounds: Vec<Round>,
    masks: Vec<u64>,
    end: Vec<u8>,
    xor: Vec<u8>,
    iv: [u8; 16],
}
fn words(data: &[u8]) -> Vec<u64> {
    data.chunks_exact(8)
        .map(|v| u64::from_le_bytes(v.try_into().unwrap()))
        .collect()
}

impl Keys {
    /// Swage/librdr3 secrets.bin: repeated little-endian length + secret bytes.
    /// Index by both hashes to reject corrupt or unrelated material.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("reading RDR2 secrets {}", path.display()))?;
        Self::from_store(&data)
    }

    fn from_store(mut data: &[u8]) -> Result<Self> {
        let mut store = HashMap::new();
        while !data.is_empty() {
            if data.len() < 4 {
                bail!("truncated RDR2 secret length");
            }
            let size = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
            data = &data[4..];
            let bytes = data.get(..size).context("truncated RDR2 secret")?;
            if size == 0 || size > 32768 {
                bail!("invalid RDR2 secret size {size}");
            }
            store.insert(fingerprint(bytes), bytes);
            data = &data[size..];
        }
        let get = |id: &str| -> Result<&[u8]> {
            store.get(&decode_id(id)).copied().with_context(|| {
                format!(
                    "RDR2 secret {id} missing; supply a complete PC secrets.bin from Swage/librdr3"
                )
            })
        };
        let mut keys = HashMap::new();
        for (i, id) in KEY_IDS.iter().enumerate() {
            if let Some(data) = store.get(&decode_id(id)) {
                let tag = match i {
                    163 => 0xC0,
                    164 => 0xC5,
                    165 => 0xC6,
                    _ => i as u16,
                };
                keys.insert(tag, words(data));
            }
        }
        let mut ids = CONTEXT_IDS.iter();
        let mut next = || get(ids.next().unwrap());
        let init = words(next()?);
        let mut rounds = Vec::new();
        for _ in 0..17 {
            let lookup = words(next()?);
            let mut blocks = Vec::new();
            for _ in 0..16 {
                let masks = words(next()?);
                let xor =
                    u32::from_le_bytes(next()?.try_into().context("TFIT2 xor size")?) as usize;
                if xor >= 4096 {
                    bail!("invalid TFIT2 round xor");
                }
                blocks.push(Block { masks, xor });
            }
            rounds.push(Round { lookup, blocks });
        }
        let masks = words(next()?);
        let end = next()?.to_vec();
        let xor = next()?.to_vec();
        let iv = get("5C8xGcF{L^AxqTQd!ofA")?
            .try_into()
            .context("RDR2 IV size")?;
        Ok(Self {
            keys,
            init,
            rounds,
            masks,
            end,
            xor,
            iv,
        })
    }

    pub fn decrypt(&self, tag: u16, data: &mut [u8]) -> Result<()> {
        self.decrypt_ranges(tag, data, &[(0, data.len())])
    }

    /// CBC chaining continues between the encrypted ranges; clear bytes do
    /// not update the IV. A partial final block is left as stored.
    pub fn decrypt_ranges(
        &self,
        tag: u16,
        data: &mut [u8],
        ranges: &[(usize, usize)],
    ) -> Result<()> {
        if tag == 0xFF {
            return Ok(());
        }
        let key = self
            .keys
            .get(&tag)
            .with_context(|| format!("RDR2 TFIT2 key 0x{tag:02x} missing"))?;
        let mut iv = self.iv;
        for &(start, end) in ranges {
            let range = data
                .get_mut(start..end)
                .context("RDR2 encryption range out of bounds")?;
            for block in range.chunks_exact_mut(16) {
                let ciphertext: [u8; 16] = block.try_into().unwrap();
                let plain = self.block(key, ciphertext);
                for i in 0..16 {
                    block[i] = plain[i] ^ iv[i];
                }
                iv = ciphertext;
            }
        }
        Ok(())
    }

    fn block(&self, key: &[u64], data: [u8; 16]) -> [u8; 16] {
        let mut state = [0u64; 2];
        for i in 0..16 {
            state[i / 8] ^= self.init[i * 256 + data[i] as usize];
        }
        for (ri, round) in self.rounds.iter().enumerate() {
            let bytes: Vec<u8> = state.iter().flat_map(|x| x.to_le_bytes()).collect();
            let halves: Vec<[u64; 8]> = bytes.chunks_exact(8).map(splat).collect();
            state = [key[(ri + 1) * 2], key[(ri + 1) * 2 + 1]];
            for i in 0..16 {
                let half = if ri < 2 || ri == 16 {
                    i / 8
                } else {
                    const MIX: [usize; 16] = [0, 1, 1, 0, 0, 0, 1, 1, 1, 0, 0, 1, 1, 1, 0, 0];
                    MIX[i]
                };
                let block = &round.blocks[i];
                let mut lo = 0;
                let mut hi = 0;
                for j in 0..8 {
                    lo ^= block.masks[8 + j] & halves[half][j];
                    hi ^= block.masks[j] & halves[half][j];
                }
                lo ^= lo >> 1;
                lo ^= lo >> 2;
                lo ^= lo >> 4;
                lo &= 0x0101010101010101;
                lo |= lo >> 7;
                lo |= lo >> 14;
                lo |= lo >> 28;
                hi ^= hi << 1;
                hi ^= hi >> 2;
                hi ^= hi >> 4;
                hi &= 0x0202020202020202;
                hi |= hi << 7;
                hi |= hi >> 14;
                state[i / 8] ^= round.lookup[((lo & 0xFF) ^ (hi & 0xF00)) as usize ^ block.xor];
            }
        }
        let bytes: Vec<u8> = state.iter().flat_map(|x| x.to_le_bytes()).collect();
        let halves: Vec<[u64; 8]> = bytes.chunks_exact(8).map(splat).collect();
        let mut out = [0; 16];
        for i in 0..16 {
            let mut v = 0;
            for j in 0..8 {
                v ^= self.masks[i * 8 + j] & halves[i / 8][j];
            }
            v ^= v >> 1;
            v ^= v >> 2;
            v ^= v >> 4;
            v &= 0x0101010101010101;
            v |= v >> 7;
            v |= v >> 14;
            v |= v >> 28;
            out[i] = self.end[i * 256 + ((v as u8) ^ self.xor[i]) as usize];
        }
        out
    }
}

fn splat(bytes: &[u8]) -> [u64; 8] {
    std::array::from_fn(|i| 0x0101010101010101u64 * bytes[7 - i] as u64)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn fixture() -> (Keys, Vec<u8>) {
        let mut seed = 0x0123456789abcdefu64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let init = (0..4096).map(|_| next()).collect();
        let rounds = (0..17)
            .map(|_| {
                let lookup = (0..4096).map(|_| next()).collect();
                let blocks = (0..16)
                    .map(|_| Block {
                        masks: (0..16).map(|_| next()).collect(),
                        xor: (next() & 4095) as usize,
                    })
                    .collect();
                Round { lookup, blocks }
            })
            .collect();
        let masks = (0..128).map(|_| next()).collect();
        let end = (0..4096).map(|_| next() as u8).collect();
        let xor = (0..16).map(|_| next() as u8).collect();
        let key = (0..36).map(|_| next()).collect();
        let iv = std::array::from_fn(|_| next() as u8);
        let input = (0..48).map(|_| next() as u8).collect();
        (
            Keys {
                keys: HashMap::from([(12, key)]),
                init,
                rounds,
                masks,
                end,
                xor,
                iv,
            },
            input,
        )
    }

    /// An identity block transform made from synthetic TFIT2 tables. The
    /// 14 cross-half rounds cancel in pairs, so CBC encryption is XOR with
    /// the previous ciphertext. This lets archive tests encrypt known TOCs
    /// and payloads without distributing retail key material.
    pub(crate) fn identity() -> Keys {
        let init = (0..4096)
            .map(|i| ((i % 256) as u64) << ((i / 256 % 8) * 8))
            .collect();
        let rounds = (0..17)
            .map(|_| {
                let lookup = (0..4096)
                    .map(|i| ((i % 256) as u64) << ((i / 256 % 8) * 8))
                    .collect();
                let blocks = (0..16)
                    .map(|i| {
                        let mut masks = vec![0; 16];
                        masks[8 + 7 - i % 8] = 0x8040201008040201;
                        Block {
                            masks,
                            xor: i * 256,
                        }
                    })
                    .collect();
                Round { lookup, blocks }
            })
            .collect();
        let mut masks = vec![0; 128];
        for i in 0..16 {
            masks[i * 8 + 7 - i % 8] = 0x8040201008040201;
        }
        Keys {
            keys: HashMap::from([(12, vec![0; 36])]),
            init,
            rounds,
            masks,
            end: (0..4096).map(|i| (i % 256) as u8).collect(),
            xor: vec![0; 16],
            iv: [0; 16],
        }
    }

    pub(crate) fn identity_encrypt(data: &mut [u8]) {
        let mut previous = [0u8; 16];
        for block in data.chunks_exact_mut(16) {
            for i in 0..16 {
                block[i] ^= previous[i];
            }
            previous.copy_from_slice(block);
        }
    }

    #[test]
    fn matches_independent_swage_cpp_reference_vector() {
        // Generated by the unmodified Swage TFIT2 cipher with xorshift64
        // synthetic tables, key, IV and ciphertext (seed 0x0123456789abcdef).
        // No retail game data appears in this vector.
        let (keys, mut input) = fixture();
        keys.decrypt(12, &mut input).unwrap();
        let hex: String = input.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "7ca4cb0e02231b1104cd124042926efa70698c6194cd1425502cd9214f30f2bb0ee415e825e3ccfa7e9522cb00c26c3d"
        );
    }

    #[test]
    fn cbc_continues_between_ranges_and_preserves_clear_bytes_and_partial_blocks() {
        let (keys, input) = fixture();
        let mut contiguous = input.clone();
        keys.decrypt(12, &mut contiguous).unwrap();
        let mut strided = input[..16].to_vec();
        strided.extend([0xA5; 32]);
        strided.extend(&input[16..]);
        strided.extend([0x55; 3]);
        keys.decrypt_ranges(12, &mut strided, &[(0, 16), (48, 83)])
            .unwrap();
        assert_eq!(&strided[..16], &contiguous[..16]);
        assert_eq!(&strided[16..48], &[0xA5; 32]);
        assert_eq!(&strided[48..80], &contiguous[16..]);
        assert_eq!(&strided[80..], &[0x55; 3]);
    }

    #[test]
    fn fingerprints_match_public_identifiers_and_store_rejects_truncation() {
        let id = decode_id(KEY_IDS[0]);
        assert_eq!(u32::from_le_bytes(id[..4].try_into().unwrap()), 288);
        let id = fingerprint(b"123456789");
        assert_eq!(
            u64::from_le_bytes(id[8..].try_into().unwrap()),
            0x995DC9BBDF1939FA
        );
        assert!(Keys::from_store(&[1, 0, 0, 0]).is_err());
        assert!(Keys::from_store(&[0]).is_err());
        assert!(Keys::from_store(&[0, 0, 0, 0]).is_err());
    }

    #[test]
    fn scanner_finds_unaligned_secrets_with_distinct_lengths_and_checks_both_hashes() {
        let data = b"padding\0secret-one\0another-secret\0secret-one\0";
        let one = fingerprint(b"secret-one");
        let two = fingerprint(b"another-secret");
        let mut invalid = fingerprint(b"padding");
        invalid[8] ^= 1;
        let found = find_secrets(data, &[one, two, invalid]);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&&b"secret-one"[..]));
        assert!(found.contains(&&b"another-secret"[..]));
    }
}
