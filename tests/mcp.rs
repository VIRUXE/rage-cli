//! `rage mcp` over stdio against the hand-built install from `catalog.rs`;
//! needs no game.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use rage_formats::{build_rsc7, serialize_ytd, stride_for, TextureFormat, YtdTexture};
use rpf_archive::{RpfBuilder, RpfEncryption};

fn rage() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_rage"));
    c.env("RAGE_NO_UPDATE_CHECK", "1").env("RAGE_NAMES", "/nonexistent/names.txt").env_remove("GTAV_PATH").env_remove("RAGE_CATALOG");
    c
}

fn ydr() -> Vec<u8> {
    let (system, graphics) = rage_formats::ydd::tests::minimal_ydr_sections(false);
    build_rsc7(165, &system, &graphics)
}

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

/// A base archive with a drawable, its texture dictionary and a nested
/// archive holding a second drawable.
fn game(dir: &Path) -> (PathBuf, PathBuf) {
    let mut nested = RpfBuilder::new(RpfEncryption::None);
    nested.add_file("prop_nested_b.ydr", ydr());
    let nested = nested.build(None).unwrap();
    let mut rpf = RpfBuilder::new(RpfEncryption::None);
    rpf.add_file("models/prop_test_a.ydr", ydr());
    rpf.add_file("textures/prop_test_a.ytd", ytd("prop_test_a_diff"));
    rpf.add_file("props/nested.rpf", nested);
    let game = dir.join("game");
    std::fs::create_dir_all(&game).unwrap();
    std::fs::write(game.join("x64a.rpf"), rpf.build(None).unwrap()).unwrap();
    let db = dir.join("cat").join("catalog.sqlite");
    let out = rage().args(["catalog", "--db", db.to_str().unwrap(), "build", "--game", game.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (game, db)
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn start(db: &Path, output: &Path) -> Self {
        let mut child = rage()
            .args(["mcp", "--db", db.to_str().unwrap(), "--output", output.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start rage mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next_id: 1 }
    }

    fn send_raw(&mut self, text: &str) {
        self.stdin.write_all(text.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    fn read(&mut self) -> json::JsonValue {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).unwrap();
        assert!(n > 0, "the server closed stdout");
        json::parse(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    fn request(&mut self, method: &str, params: json::JsonValue) -> json::JsonValue {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json::object! { jsonrpc: "2.0", id: id, method: method, params: params };
        self.send_raw(&msg.dump());
        let reply = self.read();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id, "{}", reply.pretty(2));
        reply
    }

    fn call(&mut self, tool: &str, args: json::JsonValue) -> json::JsonValue {
        let reply = self.request("tools/call", json::object! { name: tool, arguments: args });
        assert!(reply["error"].is_null(), "{} failed at the protocol level: {}", tool, reply.pretty(2));
        reply["result"].clone()
    }

    /// A call that must succeed; returns `structuredContent`.
    fn ok(&mut self, tool: &str, args: json::JsonValue) -> json::JsonValue {
        let result = self.call(tool, args);
        assert_eq!(result["isError"], false, "{tool}: {}", result.pretty(2));
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(json::parse(result["content"][0]["text"].as_str().unwrap()).unwrap(), result["structuredContent"], "text and structured content differ");
        result["structuredContent"].clone()
    }

    fn close(mut self) {
        drop(self.stdin);
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the server exited with {status}");
    }
}

#[test]
fn the_review_loop_runs_over_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let (_game, db) = game(dir.path());
    let out = dir.path().join("out");
    let mut c = Client::start(&db, &out);

    // initialize, then the notification the client sends, which gets no reply.
    let init = c.request("initialize", json::object! { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "test", version: "0" } });
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "rage");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["instructions"].as_str().unwrap().contains("catalog_search"));
    c.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert!(c.request("ping", json::object! {})["result"].is_object());

    let tools = c.request("tools/list", json::object! {});
    let names: Vec<&str> = tools["result"]["tools"].members().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 8, "{names:?}");
    assert!(names.contains(&"catalog_search") && names.contains(&"screenshot"));
    for t in tools["result"]["tools"].members() {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t.pretty(2));
        assert!(!t["description"].as_str().unwrap().is_empty());
    }

    let info = c.ok("catalog_info", json::object! {});
    assert_eq!(info["items"]["drawable"]["rows"], 2, "{}", info.pretty(2));

    let found = c.ok("catalog_search", json::object! { query: "prop test", kind: ["ydr"] });
    assert_eq!(found["schema"], "rage-catalog-search/1");
    assert_eq!(found["results"].len(), 1, "{}", found.pretty(2));
    let hit = &found["results"][0];
    assert_eq!(hit["name"], "prop_test_a");
    let key = hit["key"].as_str().unwrap().to_string();
    assert!(hit["commands"]["screenshot"].as_str().unwrap().starts_with("rage screenshot"));

    // A wrong size spec is a tool error, not a protocol error.
    let bad = c.call("catalog_search", json::object! { query: "prop", min_size: "huge" });
    assert_eq!(bad["isError"], true);
    assert!(bad["content"][0]["text"].as_str().unwrap().contains("min_size"));

    // A wrong type and an unknown tool are protocol errors.
    let reply = c.request("tools/call", json::object! { name: "catalog_search", arguments: { limit: "ten" } });
    assert_eq!(reply["error"]["code"], -32602, "{}", reply.pretty(2));
    let reply = c.request("tools/call", json::object! { name: "nope", arguments: {} });
    assert_eq!(reply["error"]["code"], -32602);
    let reply = c.request("no/such", json::object! {});
    assert_eq!(reply["error"]["code"], -32601);

    // The nested entry comes out through catalog_get (which prints paths in
    // the CLI: those lines must not reach the protocol stream).
    let nested = c.ok("catalog_search", json::object! { query: "nested" });
    let nested_key = nested["results"][0]["key"].as_str().unwrap().to_string();
    let got = c.ok("catalog_get", json::object! { key: nested_key.clone(), rpf: true });
    let rpf = PathBuf::from(got["files"][0].as_str().unwrap());
    assert!(rpf.is_file() && rpf.ends_with("nested.rpf"), "{}", got.pretty(2));
    assert!(rpf.starts_with(&out), "default output dir is --output: {}", rpf.display());
    let loose = c.ok("catalog_get", json::object! { key: nested_key.clone(), output_dir: dir.path().join("loose").to_str().unwrap() });
    let loose_file = PathBuf::from(loose["files"][0].as_str().unwrap());
    assert!(loose_file.ends_with("prop_nested_b.ydr"));

    let rinfo = c.ok("resource_info", json::object! { file: loose_file.to_str().unwrap() });
    assert!(rinfo["container"].is_object() || rinfo["file"].is_string(), "{}", rinfo.pretty(2));

    // Contact sheets, with the images inline.
    let sheet = c.call("catalog_sheet", json::object! { query: "prop test", kind: ["ydr"], views: ["iso", "top"], cell: 64, inline_images: true });
    assert_eq!(sheet["isError"], false, "{}", sheet.pretty(2));
    let packet = &sheet["structuredContent"];
    assert_eq!(packet["tiles"], 1, "{}", packet.pretty(2));
    let packet_path = PathBuf::from(packet["packet"].as_str().unwrap());
    assert!(packet_path.is_file());
    let image = sheet["content"].members().find(|m| m["type"] == "image").expect("an image content item");
    assert_eq!(image["mimeType"], "image/png");
    assert!(image["data"].as_str().unwrap().starts_with("iVBOR"), "base64 of a PNG starts with iVBOR");

    // Describe the tile and import, giving the responses inline.
    let packet_json = json::parse(&std::fs::read_to_string(&packet_path).unwrap()).unwrap();
    let tile = &packet_json["sheets"][0]["tiles"][0];
    let responses = json::object! {
        schema: "rage-catalog-responses/1",
        packet_id: packet_json["packet_id"].clone(),
        reviewer: "agent",
        reviewer_name: "mcp-test",
        tiles: [ { tile: tile["tile"].clone(), sha256: tile["sha256"].clone(), description: "a small red plastic chair", tags: ["chair", "red"], confidence: 0.8 } ],
    };
    let imported = c.ok("catalog_annotate", json::object! { packet: packet_path.to_str().unwrap(), responses_json: responses });
    assert_eq!(imported["imported"], 1, "{}", imported.pretty(2));
    assert!(PathBuf::from(imported["responses"].as_str().unwrap()).is_file());

    let found = c.ok("catalog_search", json::object! { query: "red chair", annotated: true });
    assert_eq!(found["results"].len(), 1);
    assert_eq!(found["results"][0]["annotations"][0]["reviewer_name"], "mcp-test");

    let revealed = c.ok("catalog_reveal", json::object! { packet_id: packet_json["packet_id"].clone() });
    assert_eq!(revealed["tiles"][0]["name"], "prop_test_a");

    // A render by key; the texture dictionary is found in the same archive.
    let shot = c.ok("screenshot", json::object! { key: key.clone(), views: ["iso"], size: "64x64" });
    assert_eq!(shot["schema"], "rage-screenshot/1");
    assert_eq!(shot["entries"].len(), 1, "{}", shot.pretty(2));
    let path = PathBuf::from(shot["entries"][0]["images"][0]["path"].as_str().unwrap());
    assert!(path.is_file(), "{}", path.display());
    assert!(shot["notes"].members().any(|n| n.as_str().unwrap().contains("prop_test_a.ytd")), "{}", shot.pretty(2));

    // ... and by key inside a nested archive, which is unpacked first.
    let shot = c.ok("screenshot", json::object! { key: nested_key, views: ["iso"], size: "64x64", inline_images: true });
    assert_eq!(shot["entries"][0]["images"].len(), 1, "{}", shot.pretty(2));

    // A texture is not a model.
    let tex = c.ok("catalog_search", json::object! { query: "diff", kind: ["texture"] });
    let bad = c.call("screenshot", json::object! { key: tex["results"][0]["key"].clone() });
    assert_eq!(bad["isError"], true);
    assert!(bad["content"][0]["text"].as_str().unwrap().contains("not a model"));

    // A batch is answered as a batch.
    c.send_raw(r#"[{"jsonrpc":"2.0","id":900,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/cancelled"},{"jsonrpc":"2.0","id":901,"method":"ping"}]"#);
    let batch = c.read();
    assert!(batch.is_array() && batch.len() == 2, "{}", batch.pretty(2));

    // Garbage gets a parse error and the server stays up.
    c.send_raw("{nope");
    assert_eq!(c.read()["error"]["code"], -32700);
    assert!(c.request("ping", json::object! {})["result"].is_object());

    c.close();
}

#[test]
fn without_a_catalogue_the_server_still_answers() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::start(&dir.path().join("none.sqlite"), &dir.path().join("out"));
    let r = c.call("catalog_info", json::object! {});
    assert_eq!(r["isError"], true);
    assert!(r["content"][0]["text"].as_str().unwrap().contains("catalog"), "{}", r.pretty(2));
    c.close();
}
