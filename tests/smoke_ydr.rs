//! Retail drawables and bounds through `resource dump` → `resource build` → `resource dump`,
//! checked against each other, against `resource info`, and against CodeWalker.Core itself when
//! `codewalker-cli` is built next to this repo (`../codewalker-cli/bin/Release/codewalker-cli.exe`).
//!
//! Skipped unless `GTAV_PATH` points at the game, since it needs the retail archives and the keys.
//!
//! The oracle checks, per file:
//! - CodeWalker's export of the original equals our dump of it (the reader and the dump);
//! - CodeWalker's export of the rebuilt file equals our dump of it (CodeWalker reads our file as we do);
//! - with a CodeWalker.Core built from source (`CODEWALKER_CORE_DIR`, or `../codewalker-cli/core/`),
//!   CodeWalker's own import of our first dump, saved and exported, equals our dump of the rebuilt
//!   file (our build is CodeWalker's build). The installed release DLL predates the source the port
//!   follows (it quantises bound vertices differently), so that check needs the source build.
//!
//! Both sides are normalised first: whitespace runs collapse, every float goes through the same
//! shortest round-trip format (CodeWalker falls back to `G9`), and a named `Name`/`FileName` becomes
//! its `hash_XXXXXXXX` (CodeWalker's name index is empty in the oracle process).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rage_formats::blocks::xml::float;
use rage_formats::rage_joaat;

fn run(cmd: &mut Command) -> Output {
    let shown = format!("{cmd:?}");
    let output = cmd.output().unwrap_or_else(|err| panic!("failed to run {shown}: {err}"));
    assert!(
        output.status.success(),
        "{shown} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn rage(args: &[&str]) -> String {
    let output = run(Command::new(env!("CARGO_BIN_EXE_rage")).args(args).env("RAGE_NO_UPDATE_CHECK", "1").env("RAGE_NAMES", "nowhere/names.txt"));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// Depth-first search for a file called `name` somewhere under `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file.eq_ignore_ascii_case(name)) {
            return Some(path);
        }
    }
    None
}

fn sibling_tool() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("codewalker-cli")
}

/// `codewalker-cli.exe`, when it has been built.
fn oracle() -> Option<PathBuf> {
    let exe = sibling_tool().join("bin").join("Release").join("codewalker-cli.exe");
    exe.is_file().then_some(exe)
}

/// A folder holding a CodeWalker.Core.dll built from source, if one is configured.
fn source_core() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEWALKER_CORE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let dir = sibling_tool().join("core");
    dir.join("CodeWalker.Core.dll").is_file().then_some(dir)
}

/// `codewalker-cli verify INPUT -o OUT`; returns the XML it wrote.
fn verify(exe: &Path, core: Option<&Path>, input: &Path, out: &Path) -> String {
    let mut cmd = Command::new(exe);
    cmd.args(["verify", s(input), "-o", s(out)]);
    match core {
        Some(dir) => cmd.env("CODEWALKER_CORE_DIR", dir),
        None => cmd.env_remove("CODEWALKER_CORE_DIR"),
    };
    run(&mut cmd);
    read(out)
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

/// Every float token (one with a `.` or an exponent) printed the way our dump prints floats.
fn floats(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let starts = (chars[i].is_ascii_digit() || (chars[i] == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)))
            && (i == 0 || !is_word(chars[i - 1]));
        if !starts {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let digits = |j: &mut usize| while *j < chars.len() && chars[*j].is_ascii_digit() { *j += 1 };
        digits(&mut j);
        if j + 1 < chars.len() && chars[j] == '.' && chars[j + 1].is_ascii_digit() {
            j += 1;
            digits(&mut j);
        }
        if j < chars.len() && (chars[j] == 'e' || chars[j] == 'E') {
            let mut k = j + 1;
            if k < chars.len() && (chars[k] == '+' || chars[k] == '-') {
                k += 1;
            }
            if k < chars.len() && chars[k].is_ascii_digit() {
                j = k;
                digits(&mut j);
            }
        }
        let token: String = chars[i..j].iter().collect();
        let whole = j >= chars.len() || !is_word(chars[j]);
        match token.parse::<f32>() {
            Ok(v) if whole && token.contains(['.', 'e', 'E']) => out.push_str(&float(v)),
            _ => out.push_str(&token),
        }
        i = j;
    }
    out
}

/// A `<Name>`, `<FileName>` or `<ProjectedTextureHash>` holding a name, as the `hash_` CodeWalker
/// prints when the name is not in its index.
fn hashed_names(line: &str) -> String {
    for tag in ["Name", "FileName", "ProjectedTextureHash"] {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        if let Some(inner) = line.strip_prefix(&open).and_then(|rest| rest.strip_suffix(&close)) {
            let literal = inner.strip_prefix("hash_").is_some_and(|h| h.len() == 8 && h.chars().all(|c| c.is_ascii_hexdigit()));
            if !inner.is_empty() && !literal {
                return format!("{open}hash_{:08X}{close}", rage_joaat(&inner.to_lowercase()));
            }
        }
    }
    line.to_owned()
}

fn normalize(xml: &str) -> Vec<String> {
    xml.trim_start_matches('\u{feff}')
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .map(|line| floats(&hashed_names(&line)))
        .collect()
}

/// `Err` naming the first few lines where two normalised documents differ.
fn same_normalized(ours: &str, theirs: &str) -> Result<(), String> {
    let (a, b) = (normalize(ours), normalize(theirs));
    if a == b {
        return Ok(());
    }
    let mut msg = format!("{} lines (ours) vs {} (CodeWalker)", a.len(), b.len());
    let mut shown = 0;
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).map_or("<none>", String::as_str), b.get(i).map_or("<none>", String::as_str));
        if x != y {
            msg.push_str(&format!("\n  line {}: ours `{x}`\n  {:>w$}  CodeWalker `{y}`", i + 1, "", w = format!("line {}:", i + 1).len()));
            shown += 1;
            if shown == 5 {
                break;
            }
        }
    }
    Err(msg)
}

fn vec3_attrs(line: &str) -> Option<[f32; 3]> {
    let attr = |name: &str| -> Option<f32> {
        let start = line.find(&format!(" {name}=\""))? + name.len() + 3;
        line[start..].split('"').next()?.parse().ok()
    };
    Some([attr("x")?, attr("y")?, attr("z")?])
}

fn row3(line: &str) -> Option<[f32; 3]> {
    let v: Vec<f32> = line.split(',').map(|t| t.trim().parse().ok()).collect::<Option<_>>()?;
    (v.len() == 3).then(|| [v[0], v[1], v[2]])
}

fn tag_of(line: &str) -> String {
    let t = line.trim();
    match t.strip_prefix('<') {
        Some(rest) => rest.split([' ', '>', '/']).next().unwrap_or("").to_owned(),
        None => "#row".to_owned(),
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The kind of bound geometry a line opens (`<Item type="Geometry">`, `<Bounds type="GeometryBVH">`...).
fn opens_geometry(line: &str) -> Option<&str> {
    let t = line.trim();
    let kind = ["<Item type=\"", "<Bounds type=\""].iter().find_map(|p| t.strip_prefix(p))?.split('"').next()?;
    matches!(kind, "Geometry" | "GeometryBVH").then_some(kind)
}

/// The lines of the element opening at `lines[start]`, through its closing line (same indent).
fn element<'a, 'l>(lines: &'l [&'a str], start: usize) -> &'l [&'a str] {
    let indent = indent_of(lines[start]);
    let close = format!("</{}>", tag_of(lines[start]));
    let end = (start + 1..lines.len()).find(|&j| indent_of(lines[j]) == indent && lines[j].trim() == close).unwrap_or(lines.len() - 1);
    &lines[start..=end]
}

/// The lines inside a geometry's `<name>` list (without its own tags), and the block with the list cut out.
fn cut<'a>(block: &[&'a str], name: &str) -> (Vec<&'a str>, Vec<&'a str>) {
    let open = format!("<{name}>");
    match block.iter().position(|l| l.trim() == open) {
        Some(at) => {
            let list = element(block, at);
            let inner = list[1..list.len() - 1].to_vec();
            (inner, block[..at].iter().chain(&block[at + list.len()..]).copied().collect())
        }
        None => (Vec::new(), block.to_vec()),
    }
}

/// The `<Item>` blocks of a list, each as one string.
fn items_of(inner: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < inner.len() {
        let item = element(inner, i);
        out.push(item.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "));
        i += item.len();
    }
    out
}

/// `Geometry`: line for line, except that a vertex row may move by up to one quantum per axis
/// (`CalculateQuantum` from the box, then the truncating `BoundVertex_s`).
fn same_geometry(a: &[&str], b: &[&str]) -> Result<(), String> {
    if a.len() != b.len() {
        return Err(format!("{} lines before the rebuild, {} after", a.len(), b.len()));
    }
    let find = |tag: &str| b.iter().find(|l| l.trim_start().starts_with(tag)).and_then(|l| vec3_attrs(l));
    let (min, max) = (find("<BoxMin ").unwrap_or([0.0; 3]), find("<BoxMax ").unwrap_or([0.0; 3]));
    let quantum = [0, 1, 2].map(|k| (max[k] - min[k]) * 0.5 / 32767.0);
    let mut in_vertices = false;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let differs = |why: &str| Err(format!("its line {}: {why}
  before `{}`
  after  `{}`", i + 1, x.trim(), y.trim()));
        match x.trim() {
            "<Vertices>" => in_vertices = true,
            "</Vertices>" => in_vertices = false,
            _ => {}
        }
        if x == y {
            continue;
        }
        if !in_vertices {
            return differs("differs outside the vertices");
        }
        let (Some(p), Some(q)) = (row3(x), row3(y)) else { return differs("a vertex row that does not parse") };
        for k in 0..3 {
            let tolerance = quantum[k] * 1.01 + p[k].abs() * f32::EPSILON * 4.0;
            if (p[k] - q[k]).abs() > tolerance {
                return differs(&format!("a vertex moved by {} on axis {k}, more than one quantum ({})", (p[k] - q[k]).abs(), quantum[k]));
            }
        }
    }
    Ok(())
}

/// `GeometryBVH`: `BuildBVH` reorders the polygons, `BuildMaterials` then lists the materials in the
/// new order and drops unused ones, the box and sphere become the BVH's and the vertices are
/// requantised against that box. What must hold: the same vertex count, the same polygons by kind,
/// no material that was not there before, and every other field unchanged.
fn same_bvh_geometry(a: &[&str], b: &[&str]) -> Result<(), String> {
    let (va, rest_a) = cut(a, "Vertices");
    let (vb, rest_b) = cut(b, "Vertices");
    if va.len() != vb.len() {
        return Err(format!("{} vertices before the rebuild, {} after", va.len(), vb.len()));
    }
    let (pa, rest_a) = cut(&rest_a, "Polygons");
    let (pb, rest_b) = cut(&rest_b, "Polygons");
    let kinds = |p: &[&str]| { let mut k: Vec<String> = p.iter().map(|l| tag_of(l)).collect(); k.sort(); k };
    if kinds(&pa) != kinds(&pb) {
        return Err(format!("{} polygons before the rebuild, {} after, or of other kinds", pa.len(), pb.len()));
    }
    let (ma, rest_a) = cut(&rest_a, "Materials");
    let (mb, rest_b) = cut(&rest_b, "Materials");
    let before = items_of(&ma);
    if let Some(new) = items_of(&mb).into_iter().find(|m| !before.contains(m)) {
        return Err(format!("a material that was not there before: {new}"));
    }
    let derived = ["<BoxMin ", "<BoxMax ", "<BoxCenter ", "<SphereCenter ", "<SphereRadius "];
    let fixed = |r: &[&str]| r.iter().filter(|l| !derived.iter().any(|d| l.trim_start().starts_with(d))).map(|l| l.to_string()).collect::<Vec<_>>();
    let (fa, fb) = (fixed(&rest_a), fixed(&rest_b));
    if let Some((x, y)) = fa.iter().zip(&fb).find(|(x, y)| x != y) {
        return Err(format!("a field changed
  before `{}`
  after  `{}`", x.trim(), y.trim()));
    }
    if fa.len() != fb.len() {
        return Err("the fields around the lists changed".to_owned());
    }
    Ok(())
}

/// Our dump of the original against our dump of the rebuilt file. Outside bound geometries they
/// must be identical; inside one, CodeWalker's build rewrites what the XML cannot pin (see
/// [`same_geometry`] and [`same_bvh_geometry`]). What the build writes there is pinned exactly by
/// the CodeWalker import check instead.
fn same_up_to_codewalker(before: &str, after: &str) -> Result<(), String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] != b[j] {
            return Err(format!("line {} differs outside a bound geometry
  before `{}`
  after  `{}`", i + 1, a[i].trim(), b[j].trim()));
        }
        if let Some(kind) = opens_geometry(a[i]) {
            let (ga, gb) = (element(&a, i), element(&b, j));
            let same = if kind == "GeometryBVH" { same_bvh_geometry(ga, gb) } else { same_geometry(ga, gb) };
            same.map_err(|e| format!("the {kind} at line {}: {e}", i + 1))?;
            i += ga.len();
            j += gb.len();
        } else {
            i += 1;
            j += 1;
        }
    }
    if i != a.len() || j != b.len() {
        return Err(format!("{} lines before the rebuild, {} after", a.len(), b.len()));
    }
    Ok(())
}

/// `resource info` without the lines that describe the container (path, page flags, deflated size).
fn info_body(path: &Path) -> String {
    rage(&["resource", "info", s(path)])
        .lines()
        .filter(|l| !["File:", "System:", "Graphics:", "Body:"].iter().any(|p| l.starts_with(p)))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Case {
    /// What the file is here to cover, checked against its dump.
    feature: &'static str,
    has_feature: fn(&str) -> bool,
    /// Top-level archive, the nested archive inside it, and the file inside that.
    archive: &'static str,
    nested: &'static str,
    file: &'static str,
}

fn child_kinds(xml: &str) -> std::collections::BTreeSet<&str> {
    xml.lines().filter_map(|l| l.trim().strip_prefix("<Item type=\"")).filter_map(|r| r.split('"').next()).collect()
}

fn bones(xml: &str) -> usize {
    xml.split("<Bones>").nth(1).and_then(|r| r.split("</Bones>").next()).map_or(0, |b| b.matches("<Item>").count())
}

const CASES: &[Case] = &[
    Case {
        feature: "a plain prop",
        has_feature: |x| x.contains("<Drawable>") && !x.contains("<TextureDictionary>") && !x.contains("<Lights>") && bones(x) == 1,
        archive: "x64c.rpf", nested: "lev_des.rpf", file: "prop_paper_bag_small.ydr",
    },
    Case {
        feature: "embedded textures",
        has_feature: |x| x.contains("<TextureDictionary>") && x.contains("<FileName>prop_cs_bag_01.dds</FileName>"),
        archive: "x64c.rpf", nested: "lev_des.rpf", file: "prop_cs_heist_bag_02.ydr",
    },
    Case {
        feature: "lights",
        has_feature: |x| x.contains("<Lights>") && x.split("<Lights>").nth(1).is_some_and(|l| l.contains("<Item>")),
        archive: "x64c.rpf", nested: "int_lev_des.rpf", file: "v_ilev_fh_lampa_on.ydr",
    },
    Case {
        feature: "a skeleton",
        has_feature: |x| bones(x) > 1,
        archive: "x64c.rpf", nested: "lev_des.rpf", file: "prop_v_parachute.ydr",
    },
    Case {
        feature: "a composite bound of mixed kinds",
        has_feature: |x| x.contains("<Bounds type=\"Composite\">") && child_kinds(x).len() >= 3,
        archive: "x64c.rpf", nested: "lev_des.rpf", file: "prop_ld_greenscreen_01.ydr",
    },
    Case {
        feature: "a standalone .ybn with GeometryBVH children",
        has_feature: |x| x.contains("<BoundsFile>") && x.contains("<Item type=\"GeometryBVH\">"),
        archive: "x64i.rpf", nested: "dt1_01.rpf", file: "dt1_01_0.ybn",
    },
];

/// Runs one file through the pipeline; `Err` lists every check it failed.
fn check(case: &Case, original: &Path, work: &Path, oracle: Option<&Path>, core: Option<&Path>) -> Result<String, String> {
    let ext = original.extension().unwrap().to_str().unwrap();
    let mut failures = Vec::new();

    let first = work.join("first").join("dump.xml");
    rage(&["resource", "dump", s(original), "-o", s(&first)]);
    let dump0 = read(&first);
    if !(case.has_feature)(&dump0) {
        failures.push(format!("the file does not have {}", case.feature));
    }

    let rebuilt = work.join(format!("rebuilt.{ext}"));
    rage(&["resource", "build", s(&first), "-o", s(&rebuilt)]);
    let second = work.join("second").join("dump.xml");
    rage(&["resource", "dump", s(&rebuilt), "-o", s(&second), "--no-dds"]);
    let dump1 = read(&second);

    if let Err(e) = same_up_to_codewalker(&dump0, &dump1) {
        failures.push(format!("the rebuilt file dumps differently: {e}"));
    }
    let (info0, info1) = (info_body(original), info_body(&rebuilt));
    if info0 != info1 {
        failures.push(format!("`resource info` differs:\n--- original\n{info0}\n--- rebuilt\n{info1}"));
    }

    let mut oracle_note = "no oracle";
    if let Some(exe) = oracle {
        let theirs = verify(exe, core, original, &work.join("cw_original.xml"));
        if let Err(e) = same_normalized(&dump0, &theirs) {
            failures.push(format!("CodeWalker exports the original differently: {e}"));
        }
        let theirs = verify(exe, core, &rebuilt, &work.join("cw_rebuilt.xml"));
        if let Err(e) = same_normalized(&dump1, &theirs) {
            failures.push(format!("CodeWalker exports the rebuilt file differently: {e}"));
        }
        oracle_note = "CodeWalker reads";
        if core.is_some() {
            let theirs = verify(exe, core, &first, &work.join("cw_import.xml"));
            if let Err(e) = same_normalized(&dump1, &theirs) {
                failures.push(format!("CodeWalker builds our dump differently: {e}"));
            }
            oracle_note = "CodeWalker reads and builds";
        }
    }

    let moved = dump0 != dump1;
    if failures.is_empty() {
        Ok(format!("{oracle_note} agree{}", if moved { "; bound geometry re-derived by the rebuild, as CodeWalker does" } else { "; dumps identical" }))
    } else {
        Err(failures.join("\n"))
    }
}

#[test]
fn retail_drawables_and_bounds_round_trip() {
    let Ok(gtav) = std::env::var("GTAV_PATH") else {
        println!("GTAV_PATH not set; skipping smoke test");
        return;
    };
    let oracle = oracle();
    let core = oracle.as_ref().and(source_core());
    match (&oracle, &core) {
        (None, _) => println!("codewalker-cli not built; the CodeWalker checks are skipped"),
        (Some(_), None) => println!("no CodeWalker.Core built from source; the CodeWalker build check is skipped"),
        (Some(_), Some(dir)) => println!("CodeWalker.Core from {}", dir.display()),
    }

    let tmp = tempfile::tempdir().expect("failed to make a temp dir");
    let mut failures = Vec::new();
    for (n, case) in CASES.iter().enumerate() {
        let nested_dir = tmp.path().join("nested");
        let nested = match find_file(&nested_dir, case.nested) {
            Some(found) => found,
            None => {
                rage(&["extract", &format!("{gtav}/{}", case.archive), &format!("*/{}", case.nested), "-o", s(&nested_dir)]);
                find_file(&nested_dir, case.nested).unwrap_or_else(|| panic!("{} was not extracted", case.nested))
            }
        };
        let files_dir = tmp.path().join("files");
        rage(&["extract", s(&nested), case.file, "-o", s(&files_dir)]);
        let original = find_file(&files_dir, case.file).unwrap_or_else(|| panic!("{} was not extracted", case.file));

        let work = tmp.path().join(format!("case{n}"));
        match check(case, &original, &work, oracle.as_deref(), core.as_deref()) {
            Ok(note) => println!("ok   {} ({}): {note}", case.file, case.feature),
            Err(e) => {
                println!("FAIL {} ({})", case.file, case.feature);
                failures.push(format!("{} ({}):\n{e}", case.file, case.feature));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn normalising_floats_and_names() {
    assert_eq!(floats(r#"<BoxCenter x="5.722046E-05" y="0.0062789917" z="-0.0284814835" />"#), r#"<BoxCenter x="0.00005722046" y="0.0062789917" z="-0.028481483" />"#);
    assert_eq!(floats("-0.004263085, 0.131095141, 1E+10"), "-0.004263085, 0.13109514, 10000000000");
    assert_eq!(floats("<Name>prop_cs_heist_bag_01</Name> hash_38DD00DF 12 v1=\"3910\""), "<Name>prop_cs_heist_bag_01</Name> hash_38DD00DF 12 v1=\"3910\"");
    assert_eq!(hashed_names("<Name>default</Name>"), format!("<Name>hash_{:08X}</Name>", rage_joaat("default")));
    assert_eq!(hashed_names("<Name>hash_38DD00DF</Name>"), "<Name>hash_38DD00DF</Name>");
    assert_eq!(hashed_names("<FileName />"), "<FileName />");
}

#[test]
fn a_rebuilt_geometry_may_move_its_vertices_by_one_quantum() {
    let before = "<Bounds type=\"Geometry\">\n  <BoxMin x=\"-1\" y=\"-1\" z=\"-1\" />\n  <BoxMax x=\"1\" y=\"1\" z=\"1\" />\n  <Vertices>\n    0.5, 0.25, 0\n  </Vertices>\n</Bounds>";
    let q = 1.0 / 32767.0;
    let after = before.replace("0.5, 0.25, 0", &format!("{}, 0.25, 0", 0.5 - q));
    assert!(same_up_to_codewalker(before, &after).is_ok());
    let far = before.replace("0.5, 0.25, 0", &format!("{}, 0.25, 0", 0.5 - 3.0 * q));
    assert!(same_up_to_codewalker(before, &far).is_err());
    let outside = before.replace("<BoxMax x=\"1\"", "<BoxMax x=\"2\"");
    assert!(same_up_to_codewalker(before, &outside).is_err(), "only vertex rows may move");
}
