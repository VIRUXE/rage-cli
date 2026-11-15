use rage_formats::{
    TextureFormat, YtdTexture,
    image::{Rgba, RgbaImage},
    renderware::{Game, txd::Txd},
};
use std::{fs, path::Path, process::Command};
fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env_remove("GTAV_PATH")
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap()
}
fn ok(args: &[&str]) -> String {
    let out = run(args);
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn sample(name: &str) -> YtdTexture {
    YtdTexture {
        name: name.into(),
        name_hash: 0,
        width: 2,
        height: 2,
        depth: 1,
        format: TextureFormat::A8B8G8R8,
        levels: 1,
        stride: 8,
        pixel_data: [255, 0, 0, 128].repeat(4),
    }
}
#[test]
fn export_txd_and_rebuild_from_images() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source.txd");
    fs::write(
        &source,
        Txd::from_textures(vec![sample("red"), sample("keep")], Game::SanAndreas)
            .unwrap()
            .build()
            .unwrap(),
    )
    .unwrap();
    let dir = t.path().join("images");
    ok(&["textures", s(&source), "-o", s(&dir), "--sheet"]);
    assert_eq!(
        rage_formats::image::open(dir.join("red.png"))
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0)
            .0,
        [255, 0, 0, 128]
    );
    let png = t.path().join("red.png");
    RgbaImage::from_pixel(2, 2, Rgba([0, 255, 0, 255]))
        .save(&png)
        .unwrap();
    let edited = t.path().join("edited.txd");
    ok(&[
        "textures",
        "build",
        s(&png),
        "--from",
        s(&source),
        "-o",
        s(&edited),
        "--format",
        "rgba8",
    ]);
    let built = Txd::parse(&fs::read(&edited).unwrap()).unwrap();
    let index = built
        .native_textures
        .iter()
        .position(|t| t.name == "red")
        .unwrap();
    assert_eq!(
        built.decode_mip(index, 0).unwrap(),
        [0, 255, 0, 255].repeat(4)
    );
    let index = built
        .native_textures
        .iter()
        .position(|t| t.name == "keep")
        .unwrap();
    assert_eq!(
        built.decode_mip(index, 0).unwrap(),
        [255, 0, 0, 128].repeat(4)
    );
    let removed = t.path().join("removed.txd");
    ok(&[
        "textures",
        "build",
        "--from",
        s(&edited),
        "--remove",
        "keep",
        "-o",
        s(&removed),
    ]);
    assert_eq!(
        Txd::parse(&fs::read(removed).unwrap())
            .unwrap()
            .native_textures
            .len(),
        1
    );
    let fresh = t.path().join("fresh.txd");
    assert!(
        !run(&["textures", "build", s(&png), "-o", s(&fresh)])
            .status
            .success()
    );
    ok(&[
        "textures",
        "build",
        s(&png),
        "--game",
        "sa",
        "-o",
        s(&fresh),
        "--format",
        "rgba8",
    ]);
}

#[test]
fn txd_resource_edits_and_conversion_to_ytd() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source.txd");
    fs::write(
        &source,
        Txd::from_textures(vec![sample("red")], Game::SanAndreas)
            .unwrap()
            .build()
            .unwrap(),
    )
    .unwrap();
    let info = json::parse(&ok(&["resource", "info", s(&source), "--json"])).unwrap();
    assert_eq!(info["textures"][0]["name"], "red");
    let dump = t.path().join("source.json");
    ok(&["resource", "dump", s(&source), "-o", s(&dump)]);
    let mut doc = json::parse(&fs::read_to_string(&dump).unwrap()).unwrap();
    doc["textures"][0]["name"] = "renamed".into();
    fs::write(&dump, doc.pretty(2)).unwrap();
    let edited = t.path().join("edited.txd");
    ok(&["resource", "build", s(&dump), "-o", s(&edited)]);
    assert_eq!(
        Txd::parse(&fs::read(edited).unwrap())
            .unwrap()
            .native_textures[0]
            .name,
        "renamed"
    );
    let target = t.path().join("converted");
    ok(&[
        "resource",
        "convert",
        s(&source),
        "--to",
        "gta5",
        "-o",
        s(&target),
    ]);
    let converted = rage_formats::parse_ytd(&fs::read(target.join("source.ytd")).unwrap()).unwrap();
    assert_eq!(converted[0].name, "red");
    assert_eq!(
        rage_formats::decompress_texture(&converted[0]).unwrap(),
        [255, 0, 0, 128].repeat(4)
    );
}

#[test]
fn dds_export_reports_the_actual_mip_chain() {
    let t=tempfile::tempdir().unwrap();let source=t.path().join("source.txd");
    let mut txd=Txd::from_textures(vec![sample("red")],Game::SanAndreas).unwrap();
    txd.native_textures[0].mips.push(vec![0,0,255,128]);
    fs::write(&source,txd.build().unwrap()).unwrap();
    let output=t.path().join("dds");
    let text=ok(&["textures",s(&source),"--dds","-o",s(&output)]);
    let bytes=fs::read(output.join("red.dds")).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()),2);
    assert!(text.contains("2 mip(s)"),"{text}");
}
