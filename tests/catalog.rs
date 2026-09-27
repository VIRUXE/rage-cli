//! `rage catalog` against a hand-built install; needs no game.
//!
//! The fixture has a base archive (`x64a.rpf`) holding a drawable, a drawable
//! dictionary, a texture dictionary, an archetype file, a nested archive
//! and one broken drawable, and a DLC pack overriding the drawable.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rage_formats::{build_rsc7, serialize_ytd, stride_for, TextureFormat, YtdTexture};
use rpf_archive::{RpfBuilder, RpfEncryption};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", "/nonexistent/names.txt")
        .env_remove("GTAV_PATH")
        .env_remove("RAGE_CATALOG")
        .output()
        .expect("failed to run the rage binary")
}

fn ok(args: &[&str]) -> (String, String) {
    let output = rage(args);
    assert!(
        output.status.success(),
        "`rage {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

fn ydr() -> Vec<u8> {
    let (system, graphics) = rage_formats::ydd::tests::minimal_ydr_sections(false);
    build_rsc7(165, &system, &graphics)
}

fn ydd() -> Vec<u8> {
    let (system, graphics) = rage_formats::ydd::tests::minimal_ydr_sections(true);
    build_rsc7(165, &system, &graphics)
}

/// A 4x4 red texture.
fn ytd(name: &str) -> Vec<u8> {
    let format = TextureFormat::A8R8G8B8;
    let stride = stride_for(format, 4, 4);
    serialize_ytd(&[YtdTexture {
        name: name.to_string(),
        name_hash: 0,
        width: 4,
        height: 4,
        depth: 1,
        format,
        levels: 1,
        stride,
        pixel_data: [0u8, 0, 255, 255].repeat(16),
    }])
    .unwrap()
}

fn base_archive(extra: Option<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut nested = RpfBuilder::new(RpfEncryption::None);
    nested.add_file("prop_nested_b.ydr", ydr());
    let nested = nested.build(None).unwrap();

    let mut rpf = RpfBuilder::new(RpfEncryption::None);
    rpf.add_file("models/prop_test_a.ydr", ydr());
    rpf.add_file("models/dict_test.ydd", ydd());
    rpf.add_file("models/bad.ydr", b"not a resource".to_vec());
    rpf.add_file("textures/prop_test_a.ytd", ytd("prop_test_a_diff"));
    rpf.add_file("types/test.ytyp", rage_formats::ytyp::tests::minimal_mlo_ytyp());
    rpf.add_file("props/nested.rpf", nested);
    if let Some((path, data)) = extra {
        rpf.add_file(path, data);
    }
    rpf.build(None).unwrap()
}

fn dlc_archive(extra: bool) -> Vec<u8> {
    let mut rpf = RpfBuilder::new(RpfEncryption::None);
    rpf.add_file("models/prop_test_a.ydr", ydr());
    if extra {
        rpf.add_file("models/prop_test_c.ydr", ydr());
    }
    rpf.build(None).unwrap()
}

/// Writes the fixture install and returns (game dir, db path).
fn game(dir: &Path) -> (PathBuf, PathBuf) {
    let game = dir.join("game");
    std::fs::create_dir_all(game.join("update/x64/dlcpacks/mptest")).unwrap();
    std::fs::write(game.join("x64a.rpf"), base_archive(None)).unwrap();
    std::fs::write(game.join("update/x64/dlcpacks/mptest/dlc.rpf"), dlc_archive(false)).unwrap();
    (game, dir.join("cat").join("catalog.sqlite"))
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn build(game: &Path, db: &Path) -> (String, String) {
    ok(&["catalog", "--db", s(db), "build", "--game", s(game)])
}

fn search_json(db: &Path, args: &[&str]) -> json::JsonValue {
    let mut all = vec!["catalog", "--db", s(db), "search", "--json"];
    all.extend_from_slice(args);
    let (out, _) = ok(&all);
    json::parse(&out).unwrap_or_else(|e| panic!("search output is not JSON ({e}):\n{out}"))
}

#[test]
fn builds_reports_failures_and_rebuilds_incrementally() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());

    let (out, err) = build(&game, &db);
    assert!(out.contains("Catalogued 2 archive(s) (0 unchanged)"), "{out}");
    assert!(out.contains("1 entry failed to parse"), "{out}");
    assert!(err.contains("2 archive(s) in scope: 0 unchanged, 2 to scan"), "{err}");

    let report = json::parse(&std::fs::read_to_string(db.with_file_name("catalog-report.json")).unwrap()).unwrap();
    assert_eq!(report["schema"], "rage-catalog-report/1");
    assert_eq!(report["failures"].len(), 1);
    assert_eq!(report["failures"][0]["stage"], "parse_ydr");
    assert_eq!(report["failures"][0]["path"], "models/bad.ydr");
    assert_eq!(report["items"]["texture"], 1);
    assert_eq!(report["items"]["dd_entry"], 1);

    // Nothing changed: nothing rescanned.
    let (_, err) = build(&game, &db);
    assert!(err.contains("2 unchanged, 0 to scan"), "{err}");

    // A changed DLC archive is the only one rescanned, and its new entry appears.
    std::fs::write(game.join("update/x64/dlcpacks/mptest/dlc.rpf"), dlc_archive(true)).unwrap();
    let (_, err) = build(&game, &db);
    assert!(err.contains("1 unchanged, 1 to scan"), "{err}");
    let found = search_json(&db, &["prop_test_c"]);
    assert_eq!(found["results"].len(), 1);

    // A removed archive's rows go with it.
    std::fs::remove_file(game.join("update/x64/dlcpacks/mptest/dlc.rpf")).unwrap();
    let (_, err) = build(&game, &db);
    assert!(err.contains("1 removed"), "{err}");
    assert_eq!(search_json(&db, &["prop_test_c"])["results"].len(), 0);
}

#[test]
fn strict_build_fails_on_parse_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    let out = rage(&["catalog", "--db", s(&db), "build", "--game", s(&game), "--strict"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 entr(ies) failed"));
}

#[test]
fn the_dlc_copy_wins_and_overridden_copies_are_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);

    let hits = search_json(&db, &["prop_test_a", "--kind", "ydr"]);
    assert_eq!(hits["schema"], "rage-catalog-search/1");
    assert_eq!(hits["results"].len(), 1, "{}", hits.pretty(2));
    let hit = &hits["results"][0];
    assert_eq!(hit["winner"], true);
    assert_eq!(hit["archive"]["tier"], "dlc");
    assert_eq!(hit["archive"]["dlc_pack"], "mptest");
    assert_eq!(hit["name"], "prop_test_a");
    assert!(hit["bounds"]["size"].is_array(), "{}", hit.pretty(2));
    assert!(hit["commands"]["screenshot"].as_str().unwrap().starts_with("rage screenshot "));

    // An entry rage cannot parse is still the copy the game loads, and findable.
    let bad = search_json(&db, &["bad"]);
    assert_eq!(bad["results"].len(), 1);
    assert!(bad["results"][0]["parse_error"].as_str().unwrap().contains("RSC7"));

    let all = search_json(&db, &["prop_test_a", "--kind", "ydr", "--all-copies"]);
    assert_eq!(all["results"].len(), 2);
    let tiers: Vec<&str> = all["results"].members().map(|r| r["archive"]["tier"].as_str().unwrap()).collect();
    assert!(tiers.contains(&"base") && tiers.contains(&"dlc"), "{tiers:?}");

    let base_only = search_json(&db, &["prop_test_a", "--kind", "ydr", "--dlc", "base", "--all-copies"]);
    assert_eq!(base_only["results"].len(), 1);
    assert_eq!(base_only["results"][0]["winner"], false);
}

#[test]
fn textures_are_searchable_by_name_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);

    let tex = search_json(&db, &["diff", "--kind", "texture"]);
    assert_eq!(tex["results"].len(), 1, "{}", tex.pretty(2));
    let t = &tex["results"][0];
    assert_eq!(t["name"], "prop_test_a_diff");
    assert_eq!(t["texture"]["width"], 4);
    assert_eq!(t["texture"]["format"], "A8R8G8B8");
    assert_eq!(t["texture"]["embedded"], false);
    assert!(t["key"].as_str().unwrap().ends_with("textures/prop_test_a.ytd#prop_test_a_diff"), "{}", t["key"]);

    assert_eq!(search_json(&db, &["--kind", "texture", "--min-size", "100"])["results"].len(), 0);
    assert_eq!(search_json(&db, &["--kind", "texture", "--max-size", "4"])["results"].len(), 1);

    // Words inside names match, and the coverage line is honest.
    let words = search_json(&db, &["test", "a"]);
    assert!(words["results"].len() >= 3, "{}", words.pretty(2));
    assert_eq!(words["coverage"]["results_annotated"], 0);

    let (table, err) = ok(&["catalog", "--db", s(&db), "search", "prop_test"]);
    assert!(table.contains("prop_test_a"), "{table}");
    assert!(err.contains("0 of"), "{err}");
}

#[test]
fn nested_entries_are_reachable_through_get() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);

    let hits = search_json(&db, &["prop_nested_b"]);
    assert_eq!(hits["results"].len(), 1);
    let hit = &hits["results"][0];
    assert_eq!(hit["archive"]["nested"][0], "props/nested.rpf");
    assert!(hit["commands"]["screenshot"].is_null(), "screenshot cannot open nested entries");
    assert!(hit["commands"]["get"].as_str().unwrap().contains("--rpf"));
    let key = hit["key"].as_str().unwrap().to_string();

    let out_dir = dir.path().join("out");
    let (out, _) = ok(&["catalog", "--db", s(&db), "get", &key, "-o", s(&out_dir), "--rpf"]);
    assert!(out.trim().ends_with("nested.rpf"), "{out}");
    let (listing, _) = ok(&["list", s(&out_dir.join("nested.rpf"))]);
    assert!(listing.contains("prop_nested_b.ydr"), "{listing}");

    let (out, _) = ok(&["catalog", "--db", s(&db), "get", &key, "-o", s(&out_dir)]);
    let loose = PathBuf::from(out.trim());
    assert!(loose.ends_with("prop_nested_b.ydr"), "{out}");
    let (info, _) = ok(&["resource", "info", s(&loose)]);
    assert!(info.contains("RSC7"), "{info}");

    // A top-level entry has no nested archive to write.
    let top = search_json(&db, &["prop_test_a", "--kind", "ydr"]);
    let key = top["results"][0]["key"].as_str().unwrap();
    let out = rage(&["catalog", "--db", s(&db), "get", key, "-o", s(&out_dir), "--rpf"]);
    assert!(!out.status.success());
}

#[test]
fn info_reports_counts() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);
    let (out, _) = ok(&["catalog", "--db", s(&db), "info", "--json"]);
    let info = json::parse(&out).unwrap();
    // prop_test_a twice (base and DLC), prop_nested_b, and the unparseable bad.ydr.
    assert_eq!(info["items"]["drawable"]["rows"], 4, "{}", info.pretty(2));
    assert_eq!(info["items"]["drawable"]["winners"], 3, "{}", info.pretty(2));
    assert_eq!(info["archives"]["ok"], 2);
    assert_eq!(info["parse_failures"], 1);
    let (text, _) = ok(&["catalog", "--db", s(&db), "info"]);
    assert!(text.contains("Archives: 2 ok"), "{text}");
}

#[test]
fn a_missing_catalogue_says_how_to_make_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = rage(&["catalog", "--db", s(&dir.path().join("none.sqlite")), "search", "x"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("run `rage catalog build` first"));
}

/// Sheets `prop_test` and returns (packet dir, packet, responses template).
fn sheet(db: &Path, dir: &Path) -> (PathBuf, json::JsonValue, json::JsonValue) {
    let out = dir.join("packet");
    let (stdout, _) = ok(&["catalog", "--db", s(db), "sheet", "prop_test", "-o", s(&out), "--views", "iso,top", "--per-sheet", "4", "--cell", "64"]);
    assert!(stdout.starts_with("Packet pk_"), "{stdout}");
    let packet = json::parse(&std::fs::read_to_string(out.join("packet.json")).unwrap()).unwrap();
    let template = json::parse(&std::fs::read_to_string(out.join("responses.template.json")).unwrap()).unwrap();
    (out, packet, template)
}

#[test]
fn sheets_are_blind_and_reviews_become_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);
    let (out, packet, template) = sheet(&db, dir.path());

    assert_eq!(packet["schema"], "rage-catalog-packet/1");
    let text = std::fs::read_to_string(out.join("packet.json")).unwrap();
    assert!(!text.contains("prop_test") && !text.contains(".ydr") && !text.contains("x64a"), "the packet must not name assets:\n{text}");
    let first_sheet = packet["sheets"][0]["file"].as_str().unwrap();
    let img = rage_formats::image::open(out.join(first_sheet)).expect("sheet is an image");
    assert!(img.width() > 64);
    let tiles: usize = packet["sheets"].members().map(|s| s["tiles"].len()).sum();
    // prop_test_a (the DLC copy), the texture dictionary's one texture.
    assert!(tiles >= 2, "{}", packet.pretty(2));
    assert_eq!(template["tiles"].len(), tiles);

    // Describe every tile; the texture tile as a red swatch.
    let mut responses = template.clone();
    responses["reviewer_name"] = "test-model".into();
    for t in responses["tiles"].members_mut() {
        t["description"] = "a small red plastic chair".into();
        t["tags"] = json::array!["chair", "red", "plastic"];
        t["confidence"] = 0.7.into();
    }
    let rpath = out.join("responses.json");
    std::fs::write(&rpath, responses.dump()).unwrap();
    let (line, _) = ok(&["catalog", "--db", s(&db), "annotate", "--packet", s(&out.join("packet.json")), "--responses", s(&rpath), "--verify-files"]);
    assert_eq!(line.trim(), format!("Imported {tiles} annotation(s)"));

    // The same answers again are duplicates, not new rows.
    let (line, _) = ok(&["catalog", "--db", s(&db), "annotate", "--packet", s(&out.join("packet.json")), "--responses", s(&rpath)]);
    assert!(line.contains("Imported 0") && line.contains(&format!("{tiles} duplicate(s)")), "{line}");

    let found = search_json(&db, &["red", "plastic", "chair"]);
    assert_eq!(found["results"].len(), tiles, "{}", found.pretty(2));
    let a = &found["results"][0]["annotations"][0];
    assert_eq!(a["reviewer"], "agent");
    assert_eq!(a["reviewer_name"], "test-model");
    assert_eq!(a["method"], "visual");
    assert_eq!(found["coverage"]["results_annotated"], tiles);
    let annotated = search_json(&db, &["--annotated", "--kind", "ydr"]);
    assert_eq!(annotated["results"].len(), 1);

    // --reveal maps tiles back to assets, for people.
    let pid = packet["packet_id"].as_str().unwrap();
    let (revealed, _) = ok(&["catalog", "--db", s(&db), "sheet", "--reveal", pid, "--json"]);
    let revealed = json::parse(&revealed).unwrap();
    assert_eq!(revealed["tiles"].len(), tiles);
    assert!(revealed["tiles"].members().any(|t| t["name"] == "prop_test_a"));
}

#[test]
fn descriptions_of_other_images_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);
    let (out, _, template) = sheet(&db, dir.path());

    let mut responses = template.clone();
    responses["tiles"][0]["description"] = "something".into();
    responses["tiles"][0]["sha256"] = "0000".into();
    let rpath = out.join("responses.json");
    std::fs::write(&rpath, responses.dump()).unwrap();
    let packet_path = out.join("packet.json");
    let args = ["catalog", "--db", s(&db), "annotate", "--packet", s(&packet_path), "--responses", s(&rpath), "--strict"];
    let result = rage(&args);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("1 rejected: tile 1: sha256 does not match"), "{}", String::from_utf8_lossy(&result.stdout));

    // A changed sheet file invalidates its tiles under --verify-files.
    let mut responses = template.clone();
    responses["tiles"][0]["description"] = "something".into();
    std::fs::write(&rpath, responses.dump()).unwrap();
    std::fs::write(out.join("sheet-001.png"), b"tampered").unwrap();
    let (line, _) = ok(&["catalog", "--db", s(&db), "annotate", "--packet", s(&out.join("packet.json")), "--responses", s(&rpath), "--verify-files"]);
    assert!(line.contains("sheet-001.png has changed"), "{line}");

    // The reviewer must be named as an agent or a human.
    let mut responses = template.clone();
    responses["reviewer"] = "robot".into();
    std::fs::write(&rpath, responses.dump()).unwrap();
    let result = rage(&["catalog", "--db", s(&db), "annotate", "--packet", s(&out.join("packet.json")), "--responses", s(&rpath)]);
    assert!(!result.status.success());
}

#[test]
fn packs_carry_reviews_to_another_catalogue() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);
    let (out, _, template) = sheet(&db, dir.path());
    let mut responses = template.clone();
    for t in responses["tiles"].members_mut() {
        t["description"] = "a dented metal bin".into();
        t["tags"] = json::array!["bin"];
    }
    let rpath = out.join("responses.json");
    std::fs::write(&rpath, responses.dump()).unwrap();
    ok(&["catalog", "--db", s(&db), "annotate", "--packet", s(&out.join("packet.json")), "--responses", s(&rpath)]);

    let pack = dir.path().join("pack.json");
    let (line, _) = ok(&["catalog", "--db", s(&db), "pack", "export", "-o", s(&pack)]);
    assert!(line.starts_with("Wrote "), "{line}");
    let text = std::fs::read_to_string(&pack).unwrap();
    assert!(!text.contains("x64a") && !text.contains(".rpf") && !text.contains(s(dir.path())), "a pack must not carry paths:\n{text}");
    let parsed = json::parse(&text).unwrap();
    assert_eq!(parsed["schema"], "rage-catalog-pack/1");
    let n = parsed["count"].as_usize().unwrap();
    assert!(n >= 2);

    // A second catalogue of the same install matches every review by bytes.
    let other = dir.path().join("other").join("catalog.sqlite");
    build(&game, &other);
    let (out, _) = ok(&["catalog", "--db", s(&other), "pack", "import", s(&pack), "--json"]);
    let summary = json::parse(&out).unwrap();
    assert_eq!(summary["exact"], n, "{}", summary.pretty(2));
    let found = search_json(&other, &["dented", "bin"]);
    assert_eq!(found["results"].len(), n);
    let a = &found["results"][0]["annotations"][0];
    assert_eq!(a["method"], "shared-visual");
    // Importing again changes nothing.
    let (out, _) = ok(&["catalog", "--db", s(&other), "pack", "import", s(&pack), "--json"]);
    assert_eq!(json::parse(&out).unwrap()["duplicates"], n);

    // A shared review is not re-exported by default.
    let again = dir.path().join("again.json");
    ok(&["catalog", "--db", s(&other), "pack", "export", "-o", s(&again)]);
    assert_eq!(json::parse(&std::fs::read_to_string(&again).unwrap()).unwrap()["count"], 0);
}

#[test]
fn reviews_survive_a_rescan_of_their_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);
    let (out, _, template) = sheet(&db, dir.path());
    let mut responses = template.clone();
    for t in responses["tiles"].members_mut() {
        t["description"] = "a traffic cone".into();
    }
    let rpath = out.join("responses.json");
    std::fs::write(&rpath, responses.dump()).unwrap();
    ok(&["catalog", "--db", s(&db), "annotate", "--packet", s(&out.join("packet.json")), "--responses", s(&rpath)]);
    let before = search_json(&db, &["traffic", "cone"])["results"].len();
    assert!(before >= 1);

    // Rewrite the DLC archive (same content plus an extra entry): its rows are replaced.
    std::fs::write(game.join("update/x64/dlcpacks/mptest/dlc.rpf"), dlc_archive(true)).unwrap();
    build(&game, &db);
    assert_eq!(search_json(&db, &["traffic", "cone"])["results"].len(), before);
}

#[test]
fn parallel_imports_wait_for_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let (game, db) = game(dir.path());
    build(&game, &db);
    let (out, _, template) = sheet(&db, dir.path());
    let mut responses = template.clone();
    for t in responses["tiles"].members_mut() {
        t["description"] = "a plain test object".into();
    }
    let rpath = out.join("responses.json");
    std::fs::write(&rpath, responses.dump()).unwrap();
    let packet = out.join("packet.json");

    // Several rage processes writing one catalogue at once must queue on the
    // lock, not fail with "database is locked".
    let children: Vec<_> = (0..8)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_rage"))
                .args(["catalog", "--db", s(&db), "annotate", "--packet", s(&packet), "--responses", s(&rpath)])
                .env("RAGE_NO_UPDATE_CHECK", "1")
                .env("RAGE_NAMES", "/nonexistent/names.txt")
                .output_async()
        })
        .collect();
    let mut imported = 0;
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        if String::from_utf8_lossy(&output.stdout).starts_with("Imported") && !String::from_utf8_lossy(&output.stdout).starts_with("Imported 0") {
            imported += 1;
        }
    }
    assert_eq!(imported, 1, "exactly one import adds rows; the rest are duplicates");
}

trait OutputAsync {
    fn output_async(&mut self) -> std::process::Child;
}

impl OutputAsync for Command {
    fn output_async(&mut self) -> std::process::Child {
        self.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap()
    }
}
