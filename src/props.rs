//! What to draw for a placed entity of an exterior map: its model, found in
//! the files given to `plot` or in the game's archives through the index;
//! failing that its archetype's bounding box; failing that nothing but the
//! mark. One lookup per archetype, however many times it is placed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, Result};
use rage_formats::{parse_ydd, parse_ydr, parse_yft, rage_joaat, Archetype, Drawable, Vec3};

use crate::index::{EntryLoc, GameIndex, Parts};
use crate::plot_inputs::PlotSources;
use crate::rpf::GtaKeys;

/// The geometry standing for an archetype.
#[derive(Clone)]
pub enum PropShape {
    /// A drawable read from the inputs: `entry` indexes `PlotSources::
    /// drawables`, `member` a drawable inside that entry (a `.ydd` member).
    Folder { entry: usize, member: Option<usize> },
    /// A drawable read from the game's archives.
    Game { drawables: Rc<Vec<Drawable>>, member: Option<usize> },
    /// Only the archetype's box is known.
    Box(Vec3, Vec3),
    /// Nothing but the mark.
    None,
}

/// How the props of a plot were resolved, for the caption.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PropStats {
    pub archetypes: usize,
    pub from_folder: usize,
    pub from_game: usize,
    pub boxes: usize,
    pub unresolved: usize,
    /// Archetypes past `--props`, drawn as boxes or marks instead.
    pub over_budget: usize,
}

impl PropStats {
    /// `props: 3 from folder, 12 from game, 2 as boxes, 1 unresolved`.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if self.from_folder > 0 {
            parts.push(format!("{} from folder", self.from_folder));
        }
        if self.from_game > 0 {
            parts.push(format!("{} from game", self.from_game));
        }
        if self.boxes > 0 {
            parts.push(format!("{} as boxes", self.boxes));
        }
        if self.unresolved > 0 {
            parts.push(format!("{} unresolved", self.unresolved));
        }
        if self.over_budget > 0 {
            parts.push(format!("{} over --props budget", self.over_budget));
        }
        format!("props: {}", parts.join(", "))
    }
}

pub struct PropResolver<'a> {
    sources: &'a PlotSources,
    keys: Option<&'a GtaKeys>,
    /// The game index, once looked for: `None` until then, `Some(None)`
    /// when there is none to use.
    index: Option<Option<GameIndex>>,
    exe: Option<&'a Path>,
    /// Whether to look past the inputs at all.
    use_game: bool,
    /// Whether to draw geometry at all (`--no-props` keeps the marks only).
    draw: bool,
    /// How many distinct archetypes may have their model drawn.
    budget: usize,
    shapes: HashMap<u32, PropShape>,
    /// Model file hash -> the drawables read from the game, so a dictionary
    /// shared by many archetypes is read once.
    game_files: HashMap<u32, Option<Rc<Vec<Drawable>>>>,
    /// Entries of `sources.drawables` that stand for an archetype and so are
    /// drawn per placement rather than once in world space.
    pub matched_folder: HashSet<usize>,
    pub stats: PropStats,
    warned_no_index: bool,
}

impl<'a> PropResolver<'a> {
    pub fn new(sources: &'a PlotSources, keys: Option<&'a GtaKeys>, exe: Option<&'a Path>, draw: bool, budget: usize) -> Self {
        Self {
            sources,
            keys,
            index: None,
            exe,
            use_game: exe.is_some(),
            draw,
            budget,
            shapes: HashMap::new(),
            game_files: HashMap::new(),
            matched_folder: HashSet::new(),
            stats: PropStats::default(),
            warned_no_index: false,
        }
    }

    /// The shape for `archetype`, resolved on first sight.
    pub fn resolve(&mut self, archetype: u32) -> PropShape {
        if let Some(shape) = self.shapes.get(&archetype) {
            return shape.clone();
        }
        let shape = self.lookup(archetype);
        self.stats.archetypes += 1;
        match &shape {
            PropShape::Folder { .. } => self.stats.from_folder += 1,
            PropShape::Game { .. } => self.stats.from_game += 1,
            PropShape::Box(..) => self.stats.boxes += 1,
            PropShape::None => self.stats.unresolved += 1,
        }
        self.shapes.insert(archetype, shape.clone());
        shape
    }

    fn lookup(&mut self, archetype: u32) -> PropShape {
        if !self.draw {
            return PropShape::None;
        }
        let meshes_so_far = self.stats.from_folder + self.stats.from_game;
        if meshes_so_far >= self.budget {
            self.stats.over_budget += 1;
            return self.box_for(archetype).unwrap_or(PropShape::None);
        }
        if let Some(shape) = self.from_folder(archetype) {
            return shape;
        }
        if let Some(shape) = self.from_game(archetype) {
            return shape;
        }
        self.box_for(archetype).unwrap_or(PropShape::None)
    }

    /// A drawable among the inputs named after the archetype, or a
    /// dictionary member that is.
    fn from_folder(&mut self, archetype: u32) -> Option<PropShape> {
        for (i, entry) in self.sources.drawables.iter().enumerate() {
            if stem_hash(&entry.name) == archetype {
                self.matched_folder.insert(i);
                return Some(PropShape::Folder { entry: i, member: None });
            }
            if let Some(m) = entry.data.iter().position(|d| d.name_hash == archetype || rage_joaat(&d.name.to_lowercase()) == archetype) {
                self.matched_folder.insert(i);
                return Some(PropShape::Folder { entry: i, member: Some(m) });
            }
        }
        None
    }

    /// The model the game's own archives hold for the archetype: its
    /// `.ydr`/`.yft`, or the `.ydd` member its type file points at.
    fn from_game(&mut self, archetype: u32) -> Option<PropShape> {
        if !self.use_game {
            return None;
        }
        let (kind, model, loc) = {
            let index = self.index()?;
            let (kind, model) = index.archetype_asset.get(&archetype).copied().unwrap_or((Archetype::ASSET_TYPE_DRAWABLE, archetype));
            (kind, model, index.drawable_by_name.get(&model)?.clone())
        };
        let drawables = self.load_game_file(model, &loc)?;
        let member = if kind == Archetype::ASSET_TYPE_DRAWABLEDICTIONARY || drawables.len() > 1 {
            Some(drawables.iter().position(|d| d.name_hash == archetype || rage_joaat(&d.name.to_lowercase()) == archetype).unwrap_or(0))
        } else {
            None
        };
        Some(PropShape::Game { drawables, member })
    }

    fn load_game_file(&mut self, model: u32, loc: &EntryLoc) -> Option<Rc<Vec<Drawable>>> {
        if let Some(cached) = self.game_files.get(&model) {
            return cached.clone();
        }
        let loaded = match self.read_drawables(loc) {
            Ok(d) => Some(Rc::new(d)),
            Err(err) => {
                eprintln!("skipping {}: {err:#}", loc.inner_path);
                None
            }
        };
        self.game_files.insert(model, loaded.clone());
        loaded
    }

    fn read_drawables(&self, loc: &EntryLoc) -> Result<Vec<Drawable>> {
        let index = self.index.as_ref().and_then(|i| i.as_ref()).context("no game index")?;
        let data = index.load_bytes(loc, self.keys)?;
        let ext = crate::resources::extension_of(&loc.inner_path).to_lowercase();
        Ok(match ext.as_str() {
            "ydr" => vec![parse_ydr(&data)?],
            "ydd" => parse_ydd(&data)?.into_iter().map(|e| e.drawable).collect(),
            "yft" => parse_yft(&data)?.drawable.into_iter().collect(),
            other => anyhow::bail!("{other} is not a drawable"),
        })
    }

    /// The archetype's box from a type file among the inputs, else from the
    /// game index.
    fn box_for(&mut self, archetype: u32) -> Option<PropShape> {
        for (_, ytyp) in &self.sources.ytyps {
            if let Some(a) = ytyp.archetypes.iter().find(|a| a.name_hash == archetype) {
                return Some(PropShape::Box(a.bb_min, a.bb_max));
            }
        }
        if !self.use_game {
            return None;
        }
        let (lo, hi) = self.index()?.archetype_box.get(&archetype).copied()?;
        Some(PropShape::Box(lo, hi))
    }

    /// The models part of the game index, loaded from the cache or built
    /// on first use.
    fn index(&mut self) -> Option<&GameIndex> {
        if self.index.is_none() {
            let loaded = GameIndex::load(self.exe, self.keys, Parts::MODELS);
            if loaded.is_none() && !self.warned_no_index {
                self.warned_no_index = true;
                eprintln!("props are not resolved from the game: no game index (needs --exe or GTAV_PATH)");
            }
            self.index = Some(loaded);
        }
        self.index.as_ref().and_then(|i| i.as_ref())
    }
}

/// `joaat(lowercase stem)` of a file name or inner archive path, with any
/// `hi@`/`ma@`/`lo@` detail prefix removed.
pub fn stem_hash(name: &str) -> u32 {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s).to_lowercase();
    let stem = ["hi@", "ma@", "lo@"].iter().find_map(|p| stem.strip_prefix(p)).unwrap_or(stem.as_str());
    rage_joaat(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_hash_like_archetypes() {
        assert_eq!(stem_hash("prop_gas_pump_1a.ydr"), rage_joaat("prop_gas_pump_1a"));
        assert_eq!(stem_hash("levels/gta5/props.rpf/HI@Prop_Bench_01a.ydr"), rage_joaat("prop_bench_01a"));
    }

    #[test]
    fn the_caption_line_lists_only_what_happened() {
        let stats = PropStats { archetypes: 5, from_folder: 2, from_game: 0, boxes: 3, unresolved: 0, over_budget: 0 };
        assert_eq!(stats.line(), "props: 2 from folder, 3 as boxes");
    }
}
