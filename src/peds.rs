//! Composing a ped from its variation info, as CodeWalker's ped viewer does
//! (`World/Ped.cs`, `PedsForm.cs`): every one of the 12 component slots gets a
//! drawable out of the ped's `.ydd` (or the streamed per-component `.ydd` of
//! that name) and a diffuse texture out of its `.ytd` (or the streamed `.ytd`
//! of that name), named by the rules of `MCPVDrawblData.GetDrawableName` /
//! `GetTextureName`, and the texture replaces the drawable's `DiffuseSampler`
//! when it is drawn (`Renderer.RenderPedComponent` → `RenderDrawable`'s
//! `diffOverride`).

use anyhow::{bail, Context, Result};

use rage_formats::{parse_ydd, parse_ymt, parse_ytd, rage_joaat, Drawable, PedVariationInfo, YtdTexture};
use rage_render::RenderPart;

use crate::index::GameIndex;
use crate::rpf::GtaKeys;

/// What `--component SLOT=SPEC` asks for one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotChoice {
    /// Leave the slot out.
    None,
    /// Drawable, texture and alternative indices into the slot's variations
    /// (`PedsForm`'s combo box picks the same three).
    Pick { drawable: usize, texture: usize, alternative: usize },
}

impl SlotChoice {
    /// What `Ped.LoadDefaultComponents` picks for every slot.
    pub const DEFAULT: SlotChoice = SlotChoice::Pick { drawable: 0, texture: 0, alternative: 0 };
}

/// Parses `SLOT=D[:T[:A]]` or `SLOT=none` into the slot's index and choice.
pub fn parse_component(spec: &str) -> Result<(usize, SlotChoice), String> {
    let (slot_name, choice) = spec.split_once('=').ok_or_else(|| format!("expected SLOT=D[:T[:A]] or SLOT=none, got '{spec}'"))?;
    let slot = PedVariationInfo::slot_index(slot_name)
        .ok_or_else(|| format!("'{}' is not a component slot (one of {})", slot_name.trim(), rage_formats::PED_COMPONENT_NAMES.join(", ")))?;
    let choice = choice.trim();
    if choice.eq_ignore_ascii_case("none") {
        return Ok((slot, SlotChoice::None));
    }
    let mut numbers = choice.split(':').map(|n| n.trim().parse::<usize>().map_err(|_| format!("'{n}' is not an index in '{spec}'")));
    let drawable = numbers.next().ok_or_else(|| format!("missing drawable index in '{spec}'"))??;
    let texture = numbers.next().transpose()?.unwrap_or(0);
    let alternative = numbers.next().transpose()?.unwrap_or(0);
    if numbers.next().is_some() {
        return Err(format!("too many indices in '{spec}' (drawable:texture:alternative)"));
    }
    Ok((slot, SlotChoice::Pick { drawable, texture, alternative }))
}

/// What one slot ended up showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotReport {
    pub slot: &'static str,
    /// The drawable's name, when the slot shows one.
    pub drawable: Option<String>,
    /// The texture's name, when the drawable takes one.
    pub texture: Option<String>,
}

impl SlotReport {
    /// `uppr: uppr_000_u / uppr_diff_000_a_whi`, or `uppr: none`.
    pub fn line(&self) -> String {
        match (&self.drawable, &self.texture) {
            (Some(d), Some(t)) => format!("{}: {d} / {t}", self.slot),
            (Some(d), None) => format!("{}: {d}", self.slot),
            (None, _) => format!("{}: none", self.slot),
        }
    }
}

/// A ped composed for rendering: its drawables, the texture each takes, and
/// every texture dictionary that took part.
pub struct Composed {
    /// The ped's name as `peds.ymt` spells it.
    pub name: String,
    pub slots: Vec<SlotReport>,
    /// Dictionaries to draw from: the ped's own `.ytd` first, then each
    /// streamed `.ytd` that was needed.
    pub textures: Vec<YtdTexture>,
    pub warnings: Vec<String>,
    drawables: Vec<(Drawable, Option<String>)>,
}

impl Composed {
    /// One part per shown slot, each with its texture as the diffuse override,
    /// all turned to face the `front` view.
    pub fn render_parts(&self) -> Vec<RenderPart<'_>> {
        self.drawables
            .iter()
            .map(|(drawable, texture)| RenderPart { diffuse_override: texture.as_deref(), transform: facing_front(), ..RenderPart::new(drawable) })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.drawables.is_empty()
    }
}

/// Ped meshes are modelled facing -Y, the opposite of vehicles, so the
/// `front` view (which looks at a vehicle's grille) would show a ped's
/// back: a half turn about Z puts the face in it.
fn facing_front() -> rage_formats::Mat4 {
    let mut m = rage_formats::Mat4::identity();
    m.0[0] = -1.0;
    m.0[5] = -1.0;
    m
}

/// Reads and parses one of the ped's files.
fn load<T>(index: &GameIndex, loc: &crate::index::EntryLoc, keys: Option<&GtaKeys>, parse: impl FnOnce(&[u8]) -> anyhow::Result<T>) -> Result<T> {
    let data = index.load_bytes(loc, keys)?;
    parse(&data).with_context(|| format!("failed to parse '{}'", loc.inner_path))
}

/// Composes `name` with the given slot choices (every other slot takes
/// `SlotChoice::DEFAULT`). The index must hold `Parts::PEDS`.
pub fn compose(index: &GameIndex, keys: Option<&GtaKeys>, name: &str, choices: &[(usize, SlotChoice)]) -> Result<Composed> {
    let hash = rage_joaat(&name.trim().to_lowercase());
    let Some(init) = index.ped_init.get(&hash) else {
        bail!("'{name}' is not a ped: no peds.ymt/peds.meta in the game lists it");
    };
    let Some(files) = index.ped_files.get(&hash) else {
        bail!("'{}' has no files: no {}.ymt with its dictionaries was found in the game's archives", init.name, name.trim().to_lowercase());
    };
    let Some(ymt) = &files.ymt else { bail!("'{}' has no .ymt", init.name) };

    let (info, _, _) = load(index, ymt, keys, |data| parse_ymt(data))?;
    let own_drawables = match &files.ydd {
        Some(loc) => load(index, loc, keys, |data| parse_ydd(data))?,
        None => Vec::new(),
    };
    let mut textures = match &files.ytd {
        Some(loc) => load(index, loc, keys, |data| parse_ytd(data))?,
        None => Vec::new(),
    };

    let mut composed = Composed { name: init.name.clone(), slots: Vec::new(), textures: Vec::new(), warnings: Vec::new(), drawables: Vec::new() };
    let mut streamed_textures: Vec<YtdTexture> = Vec::new();

    for slot in 0..12 {
        let slot_name = PedVariationInfo::slot_name(slot);
        let choice = choices.iter().rev().find(|(s, _)| *s == slot).map_or(SlotChoice::DEFAULT, |(_, c)| *c);
        let mut report = SlotReport { slot: slot_name, drawable: None, texture: None };
        let SlotChoice::Pick { drawable, texture, alternative } = choice else {
            composed.slots.push(report);
            continue;
        };
        let Some(component) = info.component(slot) else {
            composed.slots.push(report);
            continue;
        };
        let Some(data) = component.drawables.get(drawable) else {
            if !component.drawables.is_empty() {
                composed.warnings.push(format!("{slot_name}: drawable {drawable} is out of range (the slot has {})", component.drawables.len()));
            }
            composed.slots.push(report);
            continue;
        };
        if alternative > data.num_alternatives as usize {
            composed.warnings.push(format!("{slot_name}: alternative {alternative} is out of range (the drawable has {})", data.num_alternatives));
        }
        let drawable_name = data.drawable_name(slot, drawable, alternative);
        let texture_name = data.texture_name(slot, drawable, texture);
        if texture_name.is_none() && !data.textures.is_empty() {
            composed.warnings.push(format!("{slot_name}: texture {texture} is out of range (the drawable has {})", data.textures.len()));
        }

        // The drawable: the ped's own dictionary, then a streamed file of that name.
        let drawable_hash = rage_joaat(&drawable_name.to_lowercase());
        let found = match own_drawables.iter().find(|entry| entry.hash == drawable_hash) {
            Some(entry) => Some(entry.drawable.clone()),
            None => match files.streamed_file(drawable_hash) {
                Some(loc) => load(index, loc, keys, |data| parse_ydd(data))?.into_iter().next().map(|entry| entry.drawable),
                None => None,
            },
        };
        let Some(found) = found else {
            composed.warnings.push(format!("{slot_name}: drawable {drawable_name} is in neither the ped's dictionary nor a streamed file"));
            composed.slots.push(report);
            continue;
        };

        // The texture: the ped's own dictionary, then a streamed file of that name.
        if let Some(tex) = &texture_name {
            let texture_hash = rage_joaat(&tex.to_lowercase());
            let held = textures.iter().chain(&streamed_textures).any(|t| t.name.eq_ignore_ascii_case(tex));
            if !held {
                match files.streamed_file(texture_hash) {
                    Some(loc) => streamed_textures.extend(load(index, loc, keys, |data| parse_ytd(data))?),
                    None => composed.warnings.push(format!("{slot_name}: texture {tex} is in neither the ped's dictionary nor a streamed file")),
                }
            }
        }

        report.drawable = Some(drawable_name);
        report.texture = texture_name.clone();
        composed.drawables.push((found, texture_name));
        composed.slots.push(report);
    }

    textures.extend(streamed_textures);
    composed.textures = textures;
    Ok(composed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_specs_name_a_slot_and_up_to_three_indices() {
        assert_eq!(parse_component("uppr=2"), Ok((3, SlotChoice::Pick { drawable: 2, texture: 0, alternative: 0 })));
        assert_eq!(parse_component("HAIR = 1:2"), Ok((2, SlotChoice::Pick { drawable: 1, texture: 2, alternative: 0 })));
        assert_eq!(parse_component("jbib=0:1:3"), Ok((11, SlotChoice::Pick { drawable: 0, texture: 1, alternative: 3 })));
        assert_eq!(parse_component("berd=none"), Ok((1, SlotChoice::None)));
        assert_eq!(parse_component("berd=NONE"), Ok((1, SlotChoice::None)));
        assert!(parse_component("uppr").is_err());
        assert!(parse_component("hat=1").unwrap_err().contains("not a component slot"));
        assert!(parse_component("uppr=a").is_err());
        assert!(parse_component("uppr=1:2:3:4").unwrap_err().contains("too many"));
        assert!(parse_component("uppr=").is_err());
    }

    #[test]
    fn slot_lines_read_like_the_viewer() {
        let full = SlotReport { slot: "uppr", drawable: Some("uppr_000_u".into()), texture: Some("uppr_diff_000_a_whi".into()) };
        assert_eq!(full.line(), "uppr: uppr_000_u / uppr_diff_000_a_whi");
        let bare = SlotReport { slot: "accs", drawable: Some("accs_001_u".into()), texture: None };
        assert_eq!(bare.line(), "accs: accs_001_u");
        let none = SlotReport { slot: "berd", drawable: None, texture: None };
        assert_eq!(none.line(), "berd: none");
    }

    #[test]
    fn composing_an_unknown_ped_says_so() {
        let index = GameIndex::default();
        let err = match compose(&index, None, "nobody", &[]) {
            Ok(_) => panic!("an unknown ped composed"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not a ped"), "{err}");
    }
}
