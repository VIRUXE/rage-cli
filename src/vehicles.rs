//! Vehicle variants for `screenshot`: the high-detail model
//! (`Vehicle.Init(hidef)` loads `<name>_hi.yft`), liveries and paint from the
//! game's carcols/carvariations data. CodeWalker's vehicle viewer stops at
//! the `_hi` swap; the game itself picks a livery by pointing the model's
//! `*_sign_1` texture references at `*_sign_<N+1>`, and paints a spawn from a
//! carvariations colour combination's carcols indices, which is what the
//! livery and paint helpers here do.

use anyhow::{bail, Context, Result};

use rage_formats::{rage_joaat, Drawable, ShaderParameterValue};
use rage_render::TextureSet;

use crate::index::{EntryLoc, GameIndex};
use crate::rpf::{Archive, GtaKeys};

/// The vehicle name a model file stands for: its stem without a `_hi`.
pub fn base_name(stem: &str) -> String {
    let lower = stem.to_lowercase();
    lower.strip_suffix("_hi").map_or(lower.clone(), str::to_owned)
}

/// A vehicle model picked through the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleModel {
    /// The vehicle's name (lowercase), what its textures and paint are looked up by.
    pub name: String,
    /// The model file's stem (`police` or `police_hi`), what the images are named by.
    pub stem: String,
    pub loc: EntryLoc,
    pub warnings: Vec<String>,
}

/// Finds `name`'s `.yft` (the `_hi` one when `hi` is set and the game has
/// it) through an index holding `Parts::MODELS` and `Parts::VEHICLES`.
pub fn resolve_vehicle(index: &GameIndex, name: &str, hi: bool) -> Result<VehicleModel> {
    let name = name.trim().to_lowercase();
    let hash = rage_joaat(&name);
    let mut warnings = Vec::new();
    if !index.vehicle_init.contains_key(&hash) {
        if index.vehicle_init.is_empty() {
            warnings.push("the index holds no vehicles.meta entries; the name is taken as a model file".to_string());
        } else {
            bail!("'{name}' is not a vehicle: no vehicles.meta in the game lists it");
        }
    }
    let yft = |stem: &str| index.drawable_by_name.get(&rage_joaat(stem)).filter(|loc| loc.inner_path.to_lowercase().ends_with(".yft")).cloned();
    let hi_stem = format!("{name}_hi");
    let (stem, loc) = match (hi, yft(&hi_stem)) {
        (true, Some(loc)) => (hi_stem, loc),
        (true, None) => {
            warnings.push(format!("{hi_stem}.yft is not in the game; rendering {name}.yft"));
            (name.clone(), yft(&name).with_context(|| format!("no {name}.yft in the game's archives"))?)
        }
        (false, _) => (name.clone(), yft(&name).with_context(|| format!("no {name}.yft in the game's archives"))?),
    };
    Ok(VehicleModel { name, stem, loc, warnings })
}

/// The `_hi` model beside a positional `.yft`: `<stem>_hi.yft` in the same
/// archive, else in the game (an index holding `Parts::MODELS`).
pub fn hi_model_bytes(archive: &Archive, stem: &str, index: Option<&GameIndex>, keys: Option<&GtaKeys>) -> Result<Option<(String, Vec<u8>)>> {
    let hi_stem = format!("{}_hi", stem.to_lowercase());
    if let Some(file) = archive.find_file(&format!("{hi_stem}.yft")) {
        let data = archive.extract(file, keys).with_context(|| format!("failed to extract '{hi_stem}.yft'"))?;
        return Ok(Some((hi_stem, data)));
    }
    if let Some(loc) = index.and_then(|i| i.drawable_by_name.get(&rage_joaat(&hi_stem))).filter(|loc| loc.inner_path.to_lowercase().ends_with(".yft")) {
        let data = index.unwrap().load_bytes(loc, keys)?;
        return Ok(Some((hi_stem, data)));
    }
    Ok(None)
}

/// Every texture name the drawables' shaders reference, once each.
fn referenced_textures<'a>(drawables: impl IntoIterator<Item = &'a Drawable>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for drawable in drawables {
        let Some(group) = &drawable.shader_group else { continue };
        for shader in &group.shaders {
            for param in &shader.parameters {
                if let ShaderParameterValue::Texture { name, .. } = &param.value
                    && !name.is_empty()
                    && !names.iter().any(|n| n.eq_ignore_ascii_case(name))
                {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// What a livery swap did: each `_sign_1` reference and the texture it now
/// reads, or the name that was missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiverySwap {
    pub from: String,
    pub to: String,
    pub found: bool,
}

/// Points every `*_sign_1` texture the drawables reference at
/// `*_sign_<livery + 1>` in `textures`. Returns one entry per reference;
/// none at all means the model has no livery texture.
pub fn apply_livery<'a>(textures: &mut TextureSet, drawables: impl IntoIterator<Item = &'a Drawable>, livery: usize) -> Vec<LiverySwap> {
    let mut swaps = Vec::new();
    for name in referenced_textures(drawables) {
        let lower = name.to_lowercase();
        let Some(prefix) = lower.strip_suffix("_sign_1") else { continue };
        let to = format!("{prefix}_sign_{}", livery + 1);
        let found = textures.alias(&name, &to);
        swaps.push(LiverySwap { from: name, to, found });
    }
    swaps
}

/// Warnings the index has about livery `livery` of `name`: the model lists
/// no liveries, fewer than that, or none of its colour combinations allows
/// it. Empty when the index does not know the model.
pub fn livery_warnings(index: &GameIndex, name: &str, livery: usize) -> Vec<String> {
    let Some(variation) = index.car_variations.get(&rage_joaat(&name.to_lowercase())) else { return Vec::new() };
    let count = variation.livery_count();
    if count == 0 {
        vec![format!("carvariations lists no liveries for {name}")]
    } else if livery >= count {
        vec![format!("carvariations lists {count} liveries for {name}; livery {livery} is past the end")]
    } else if !variation.allows_livery(livery) {
        vec![format!("no colour combination of {name} in carvariations allows livery {livery}")]
    } else {
        Vec::new()
    }
}

/// A paint picked from the game's data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paint {
    pub rgb: [u8; 3],
    /// The carcols colour's name, for the report.
    pub name: String,
    /// Its index in carcols.
    pub index: u8,
}

/// The primary colour of `name`'s colour combination `combination` in
/// carvariations, looked up in carcols (an index holding `Parts::VEHICLES`).
pub fn paint_from_carcols(index: &GameIndex, name: &str, combination: usize) -> Result<Paint> {
    let name = name.to_lowercase();
    let Some(variation) = index.car_variations.get(&rage_joaat(&name)) else {
        bail!("no carvariations entry for {name}; pass --paint instead");
    };
    let Some((indices, _)) = variation.colors.get(combination) else {
        bail!("{name} has {} colour combination(s) in carvariations; {combination} is past the end", variation.colors.len());
    };
    let Some(&primary) = indices.first() else { bail!("colour combination {combination} of {name} lists no colours") };
    let Some(colour) = index.car_colors.get(primary as usize) else {
        bail!("carcols lists {} colours; {name}'s combination {combination} asks for colour {primary}", index.car_colors.len());
    };
    Ok(Paint { rgb: colour.rgb(), name: colour.name.clone(), index: primary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{CarColorEntry, VariationEntry};
    use rage_formats::{DrawableBounds, LodLevel, ShaderFx, ShaderGroup, ShaderParameter, Vec3, YtdTexture};

    fn drawable_referencing(names: &[&str]) -> Drawable {
        let parameters = names
            .iter()
            .map(|name| ShaderParameter {
                name_hash: rage_formats::ydd::DIFFUSE_SAMPLER,
                data_type: 0,
                data_pointer: 0,
                value: ShaderParameterValue::Texture { name: name.to_string(), name_hash: rage_joaat(&name.to_lowercase()) },
            })
            .collect();
        Drawable {
            name: "police".into(),
            name_hash: rage_joaat("police"),
            bounds: DrawableBounds { center: Vec3::ZERO, sphere_radius: 0.0, box_min: Vec3::ZERO, box_max: Vec3::ZERO },
            lod_distances: [0.0; 4],
            render_masks: [0; 4],
            shader_group: Some(ShaderGroup {
                textures: Vec::new(),
                shaders: vec![ShaderFx { name_hash: 0, file_name_hash: 0, render_bucket: 0, render_bucket_mask: 0, parameter_count: 0, texture_parameter_count: 0, parameters }],
            }),
            lods: vec![rage_formats::DrawableLod { level: LodLevel::High, models: Vec::new() }],
        }
    }

    fn texture(name: &str) -> YtdTexture {
        YtdTexture {
            name: name.to_string(),
            name_hash: rage_joaat(&name.to_lowercase()),
            width: 1,
            height: 1,
            depth: 1,
            format: rage_formats::ytd::TextureFormat::A8B8G8R8,
            levels: 1,
            stride: 4,
            pixel_data: vec![1, 2, 3, 255],
        }
    }

    #[test]
    fn base_name_drops_the_hi_suffix() {
        assert_eq!(base_name("police_hi"), "police");
        assert_eq!(base_name("Police"), "police");
        assert_eq!(base_name("adder"), "adder");
    }

    #[test]
    fn liveries_point_sign_1_references_at_the_wanted_sign() {
        let mut set = TextureSet::new();
        set.push_layer(&[texture("policenew_sign_1"), texture("policenew_sign_3"), texture("police_badges")]);
        let drawable = drawable_referencing(&["policenew_sign_1", "police_badges", "POLICENEW_SIGN_1"]);
        let swaps = apply_livery(&mut set, [&drawable], 2);
        assert_eq!(swaps, vec![LiverySwap { from: "policenew_sign_1".into(), to: "policenew_sign_3".into(), found: true }]);
        assert!(apply_livery(&mut set, [&drawable], 5)[0].found == false, "no _sign_6 here");
        assert!(apply_livery(&mut set, [&drawable_referencing(&["police_badges"])], 0).is_empty());
    }

    #[test]
    fn livery_warnings_follow_carvariations() {
        let mut index = GameIndex::default();
        assert!(livery_warnings(&index, "police", 0).is_empty(), "an unknown model gets no warning");
        index.car_variations.insert(rage_joaat("police"), VariationEntry { colors: vec![(vec![0], vec![false, true])], kits: vec![] });
        assert!(livery_warnings(&index, "police", 1).is_empty());
        assert!(livery_warnings(&index, "police", 0)[0].contains("allows livery 0"));
        assert!(livery_warnings(&index, "police", 2)[0].contains("past the end"));
        index.car_variations.insert(rage_joaat("adder"), VariationEntry { colors: vec![(vec![0], vec![])], kits: vec![] });
        assert!(livery_warnings(&index, "adder", 0)[0].contains("no liveries"));
    }

    #[test]
    fn paint_comes_from_the_combination_and_carcols() {
        let mut index = GameIndex::default();
        assert!(paint_from_carcols(&index, "police", 0).unwrap_err().to_string().contains("no carvariations entry"));
        index.car_colors = vec![
            CarColorEntry { color: 0xFF0D_0D0D, name: "0 Metallic Black".into(), metallic_id: 1 },
            CarColorEntry { color: 0xFF8B_1A13, name: "Dark Red".into(), metallic_id: 0 },
        ];
        index.car_variations.insert(rage_joaat("police"), VariationEntry { colors: vec![(vec![1, 0, 0, 156], vec![]), (vec![0, 0, 0, 156], vec![]), (vec![7], vec![])], kits: vec![] });
        assert_eq!(paint_from_carcols(&index, "Police", 0).unwrap(), Paint { rgb: [0x8B, 0x1A, 0x13], name: "Dark Red".into(), index: 1 });
        assert_eq!(paint_from_carcols(&index, "police", 1).unwrap().name, "0 Metallic Black");
        assert!(paint_from_carcols(&index, "police", 2).unwrap_err().to_string().contains("carcols lists 2 colours"));
        assert!(paint_from_carcols(&index, "police", 3).unwrap_err().to_string().contains("past the end"));
    }

    #[test]
    fn resolving_prefers_the_hi_model_and_falls_back_with_a_warning() {
        let loc = |path: &str| EntryLoc { top_archive: "x64e.rpf".into(), nested_rpfs: vec![], inner_path: path.to_string() };
        let mut index = GameIndex::default();
        index.vehicle_init.insert(rage_joaat("police"), crate::index::VehicleIndexEntry { model_name: "police".into(), ..Default::default() });
        index.vehicle_init.insert(rage_joaat("adder"), crate::index::VehicleIndexEntry { model_name: "adder".into(), ..Default::default() });
        index.drawable_by_name.insert(rage_joaat("police"), loc("levels/gta5/vehicles.rpf/police.yft"));
        index.drawable_by_name.insert(rage_joaat("police_hi"), loc("levels/gta5/vehicles.rpf/police_hi.yft"));
        index.drawable_by_name.insert(rage_joaat("adder"), loc("levels/gta5/vehicles.rpf/adder.yft"));

        let hi = resolve_vehicle(&index, "Police", true).unwrap();
        assert_eq!((hi.name.as_str(), hi.stem.as_str(), hi.loc.inner_path.as_str()), ("police", "police_hi", "levels/gta5/vehicles.rpf/police_hi.yft"));
        assert!(hi.warnings.is_empty());
        let base = resolve_vehicle(&index, "police", false).unwrap();
        assert_eq!(base.stem, "police");
        let fallback = resolve_vehicle(&index, "adder", true).unwrap();
        assert_eq!(fallback.stem, "adder");
        assert!(fallback.warnings[0].contains("adder_hi.yft is not in the game"));
        assert!(resolve_vehicle(&index, "zentorno", false).unwrap_err().to_string().contains("not a vehicle"));
        index.drawable_by_name.insert(rage_joaat("dinghy"), loc("dinghy.ydr"));
        index.vehicle_init.insert(rage_joaat("dinghy"), Default::default());
        assert!(resolve_vehicle(&index, "dinghy", false).unwrap_err().to_string().contains("no dinghy.yft"));
    }
}
