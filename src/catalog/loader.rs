//! Reading catalogued entries back out of the game for rendering: archives
//! (and nested archives) stay open across a batch, texture dictionaries are
//! cached raw under a byte budget, and a model's textures are resolved from
//! the catalogue's own tables, in the order `screenshot` uses: embedded,
//! then the archetype's dictionary and its parents, then the same-stem one.

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use rage_formats::{parse_ytd, rage_joaat, YtdTexture};
use rage_render::TextureSet;

use super::search::{txd_chain, ItemView};
use super::Catalog;
use crate::index::RESIDENT_DICTS;
use crate::rpf::{Archive, GtaKeys};

/// Keeps archives open between reads.
pub struct EntryLoader<'k> {
    keys: Option<&'k GtaKeys>,
    archives: HashMap<String, Rc<Archive>>,
}

impl<'k> EntryLoader<'k> {
    pub fn new(keys: Option<&'k GtaKeys>) -> Self {
        EntryLoader { keys, archives: HashMap::new() }
    }

    fn archive(&mut self, top: &str, nested: &[String]) -> Result<Rc<Archive>> {
        let mut key = top.to_string();
        let mut current = match self.archives.get(&key) {
            Some(a) => a.clone(),
            None => {
                let a = Archive::open(&PathBuf::from(top), self.keys)?;
                a.require_keys(self.keys)?;
                let a = Rc::new(a);
                self.archives.insert(key.clone(), a.clone());
                a
            }
        };
        for n in nested {
            key.push_str("//");
            key.push_str(n);
            current = match self.archives.get(&key) {
                Some(a) => a.clone(),
                None => {
                    let file = current.find_file(n).with_context(|| format!("'{n}' not found in '{top}'"))?;
                    let a = Rc::new(current.open_nested(file, self.keys).with_context(|| format!("failed to open nested archive '{n}'"))?);
                    self.archives.insert(key.clone(), a.clone());
                    a
                }
            };
        }
        Ok(current)
    }

    /// The raw bytes of `inner` inside `top` (through `nested`).
    pub fn bytes(&mut self, top: &str, nested: &[String], inner: &str) -> Result<Vec<u8>> {
        let archive = self.archive(top, nested)?;
        let file = archive.find_file(inner).with_context(|| format!("'{inner}' not found"))?;
        archive.extract(file, self.keys).with_context(|| format!("failed to extract '{inner}'"))
    }

    /// The raw bytes of a catalogue item's container file.
    pub fn item_bytes(&mut self, item: &ItemView) -> Result<Vec<u8>> {
        self.bytes(&item.archive_path, &item.nested, &item.inner_path)
    }
}

/// Parsed texture dictionaries by hash, with their pixel data, dropped in
/// bulk when they pass `budget` bytes.
pub struct TxdCache {
    raw: HashMap<u32, Option<Rc<Vec<YtdTexture>>>>,
    bytes: usize,
    budget: usize,
}

impl TxdCache {
    pub fn new(budget: usize) -> Self {
        TxdCache { raw: HashMap::new(), bytes: 0, budget }
    }

    /// The winning `.ytd` for `hash`, or `None` when the catalogue has none
    /// (or it would not parse).
    pub fn get(&mut self, cat: &Catalog, loader: &mut EntryLoader, hash: u32) -> Result<Option<Rc<Vec<YtdTexture>>>> {
        if let Some(hit) = self.raw.get(&hash) {
            return Ok(hit.clone());
        }
        let row: Option<(String, String, String)> = cat
            .conn
            .query_row(
                "SELECT a.path, i.nested, i.inner_path FROM items i JOIN archives a ON a.id = i.archive_id
                 WHERE i.kind = 'txd' AND i.hash = ?1 ORDER BY i.winner DESC, a.load_rank DESC LIMIT 1",
                [hash as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let loaded = match row {
            None => None,
            Some((top, nested, inner)) => {
                let nested: Vec<String> = if nested.is_empty() { Vec::new() } else { nested.split("//").map(str::to_string).collect() };
                match loader.bytes(&top, &nested, &inner).and_then(|d| parse_ytd(&d)) {
                    Ok(textures) => Some(Rc::new(textures)),
                    Err(e) => {
                        log::debug!("catalog: texture dictionary {inner} unreadable: {e}");
                        None
                    }
                }
            }
        };
        let size: usize = loaded.as_ref().map(|t| t.iter().map(|x| x.pixel_data.len()).sum()).unwrap_or(0);
        if self.bytes + size > self.budget {
            self.raw.clear();
            self.bytes = 0;
        }
        self.bytes += size;
        self.raw.insert(hash, loaded.clone());
        Ok(loaded)
    }
}

/// The texture dictionaries a model draws from, highest priority first:
/// its archetype's dictionary (and parents), then the same-stem one (and
/// parents). Mirrors `GameIndex::resolution_order`.
pub fn resolution_order(cat: &Catalog, item: &ItemView) -> Result<Vec<u32>> {
    let mut order = Vec::new();
    let archetype_txd: Option<i64> = cat
        .conn
        .query_row(
            "SELECT txd_hash FROM archetypes WHERE name_hash = ?1 AND winner = 1 AND txd_hash <> 0 LIMIT 1",
            [item.hash as i64],
            |r| r.get(0),
        )
        .optional()?;
    let starts = [archetype_txd.map(|h| h as u32), item.txd_hash];
    for start in starts.into_iter().flatten() {
        for (h, _) in txd_chain(cat, start)? {
            if !order.contains(&h) {
                order.push(h);
            }
        }
    }
    Ok(order)
}

/// Decoded external texture layers for one resolution order, reused
/// across consecutive tiles that share it.
pub struct DecodedChain {
    pub order: Vec<u32>,
    pub set: TextureSet,
}

/// Builds the texture set for one model.
///
/// With no embedded textures the decoded chain is reused as is (tiles are
/// sorted so neighbours share it). With embedded textures they must take
/// priority, so a fresh set is built on top of the raw cache.
pub fn texture_set_for<'c>(
    cat: &Catalog,
    loader: &mut EntryLoader,
    txds: &mut TxdCache,
    shared: &'c mut Option<DecodedChain>,
    order: &[u32],
    embedded: &[YtdTexture],
) -> Result<std::borrow::Cow<'c, TextureSet>> {
    if embedded.is_empty() {
        let fresh = !matches!(shared, Some(c) if c.order == order);
        if fresh {
            let mut set = TextureSet::new();
            for h in order {
                if let Some(textures) = txds.get(cat, loader, *h)? {
                    set.push_layer(&textures);
                }
            }
            *shared = Some(DecodedChain { order: order.to_vec(), set });
        }
        return Ok(std::borrow::Cow::Borrowed(&shared.as_ref().unwrap().set));
    }
    let mut set = TextureSet::new();
    set.push_layer(embedded);
    for h in order {
        if let Some(textures) = txds.get(cat, loader, *h)? {
            set.push_layer(&textures);
        }
    }
    Ok(std::borrow::Cow::Owned(set))
}

/// The resident dictionaries (`mapdetail`, `vehshare`) that hold any of
/// `missing`, as the last resort `screenshot` also uses.
pub fn resident_for(cat: &Catalog, missing: &[String]) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let resident: Vec<i64> = RESIDENT_DICTS.iter().map(|d| rage_joaat(d) as i64).collect();
    let mut stmt = cat.conn.prepare_cached(
        "SELECT p.hash FROM items t JOIN items p ON p.id = t.parent_id
         WHERE t.kind = 'texture' AND t.hash = ?1 AND p.kind = 'txd' AND p.hash IN (?2, ?3) AND p.winner = 1 LIMIT 1",
    )?;
    for name in missing {
        let h = rage_joaat(&name.to_lowercase()) as i64;
        if let Some(dict) = stmt.query_row(rusqlite::params![h, resident[0], resident[1]], |r| r.get::<_, i64>(0)).optional()? {
            let dict = dict as u32;
            if !out.contains(&dict) {
                out.push(dict);
            }
        }
    }
    Ok(out)
}
