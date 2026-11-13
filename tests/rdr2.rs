//! Generated RDR2 fixtures; no game files or keys are distributed.
use std::{
    io::{Seek, SeekFrom, Write},
    path::Path,
    process::{Command, Output},
};

fn rage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", "nowhere/names.txt")
        .env_remove("GTAV_PATH")
        .env_remove("RAGE_RDR2_KEYS")
        .output()
        .unwrap()
}
fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}
fn p(p: &Path) -> &str {
    p.to_str().unwrap()
}
fn put32(b: &mut [u8], o: usize, n: u32) {
    b[o..o + 4].copy_from_slice(&n.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, n: u64) {
    b[o..o + 8].copy_from_slice(&n.to_le_bytes());
}

fn metadata() -> Vec<u8> {
    let mut sys = vec![0; 0x200];
    put32(&mut sys, 0x10, 0x50524430); // PRD0, little endian
    put32(&mut sys, 0x14, b'y' as u32);
    put64(&mut sys, 0x20, 1); // root block
    put64(&mut sys, 0x28, 0x50000080); // classes
    put64(&mut sys, 0x38, 0x500000E0); // blocks
    sys[0x48] = 1;
    sys[0x4C] = 1;
    put32(&mut sys, 0x80, rage_formats::rage_joaat("FixtureMeta"));
    put64(&mut sys, 0x90, 0x500000A0); // members
    put32(&mut sys, 0x98, 12);
    sys[0x9E] = 3;
    for (i, (name, ty)) in [("count", 0x15u8), ("enabled", 1), ("distance", 0x21)]
        .iter()
        .enumerate()
    {
        put32(&mut sys, 0xA0 + i * 16, rage_formats::rage_joaat(name));
        put32(&mut sys, 0xA4 + i * 16, (i * 4) as u32);
        sys[0xA8 + i * 16] = *ty;
    }
    put32(&mut sys, 0xE0, rage_formats::rage_joaat("FixtureMeta"));
    put32(&mut sys, 0xE4, 12);
    put64(&mut sys, 0xE8, 0x50000100);
    put32(&mut sys, 0x100, 42);
    sys[0x104] = 1;
    put32(&mut sys, 0x108, 1.5f32.to_bits());
    let mut out = Vec::new();
    for n in [rage_formats::RSC8_MAGIC, 0x1F02, 0x202, 0] {
        out.extend(n.to_le_bytes());
    }
    out.extend(sys);
    out
}

fn archive(payload: &[u8], compressor: u8, offset: u64) -> (Vec<u8>, Vec<u8>) {
    let size = (16 + payload.len() + 15) & !15;
    let mut head = vec![0; 0x110 + 24];
    put32(&mut head, 0, rpf_archive::RPF8_MAGIC);
    put32(&mut head, 4, 1);
    head[12] = 0xFF;
    head[14] = b'y';
    let a = 0x12345678 | (0xFFu64 << 40) | (11u64 << 48) | (1u64 << 56);
    let b = (size as u64 >> 4) | ((offset >> 4) << 28) | ((compressor as u64) << 59);
    put64(&mut head, 0x110, a);
    put64(&mut head, 0x118, b);
    put64(&mut head, 0x120, 0x202);
    let mut body = metadata()[..16].to_vec();
    body.extend(payload);
    body.resize(size, 0);
    (head, body)
}

fn write_archive(path: &Path, compressor: u8, offset: u64) {
    let sys = metadata()[16..].to_vec();
    let compressed = match compressor {
        1 => {
            let mut deflate =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            deflate.write_all(&sys).unwrap();
            deflate.finish().unwrap()
        }
        2 => {
            let mut oodle = vec![0x4C, 0x06];
            oodle.extend(sys);
            oodle
        }
        _ => sys,
    };
    let (head, body) = archive(&compressed, compressor, offset);
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(&head).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&body).unwrap();
}

#[test]
fn batch_exports_rpf8_metadata_stored_deflate_and_oodle() {
    let tmp = tempfile::tempdir().unwrap();
    for compressor in 0..=2 {
        let input = tmp.path().join(format!("update_{compressor}.rpf"));
        write_archive(&input, compressor, 512);
        let out = tmp.path().join(format!("out{compressor}"));
        let run = rage(&["resource", "convert", p(&input), "-o", p(&out)]);
        assert!(run.status.success(), "{}", text(&run));
        let xml = std::fs::read_to_string(
            out.join(input.file_name().unwrap())
                .join("12345678.ymt.xml"),
        )
        .unwrap();
        assert!(
            xml.contains("42") && xml.contains("true") && xml.contains("1.5"),
            "{xml}"
        );
        let tree = rage(&["tree", p(&input)]);
        assert!(
            tree.status.success() && text(&tree).contains("12345678.ymt"),
            "{}",
            text(&tree)
        );
        let dump = rage(&["resource", "dump", "12345678.ymt", "--archive", p(&input)]);
        assert!(
            dump.status.success() && text(&dump).contains("42"),
            "{}",
            text(&dump)
        );
    }
}

#[test]
fn archive_offsets_beyond_four_gib_are_preserved() {
    if usize::BITS < 64 {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("large.rpf");
    write_archive(&input, 0, 0x100000200);
    let run = rage(&[
        "resource",
        "export",
        p(&input),
        "-o",
        p(&tmp.path().join("out")),
        "--dry-run",
    ]);
    assert!(run.status.success(), "{}", text(&run));
    assert!(!tmp.path().join("out").exists());
}

#[test]
fn loose_metadata_dump_and_batch_export_and_failure_accounting() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = tmp.path().join("pack/stream");
    std::fs::create_dir_all(&pack).unwrap();
    let input = pack.join("test.ymt");
    std::fs::write(&input, metadata()).unwrap();
    let run = rage(&["resource", "dump", p(&input), "--json"]);
    assert!(
        run.status.success() && text(&run).contains("42"),
        "{}",
        text(&run)
    );
    std::fs::write(pack.join("broken.ymt"), b"RSC8").unwrap();
    let out = tmp.path().join("out");
    let run = rage(&[
        "resource",
        "export",
        p(&tmp.path().join("pack")),
        "-o",
        p(&out),
        "--json",
    ]);
    assert!(
        !run.status.success() && text(&run).contains("Exported 1, failed 1"),
        "{}",
        text(&run)
    );
    assert!(out.join("stream/test.ymt.json").is_file());
    assert!(!out.join("stream/broken.ymt.json").exists());
}

#[test]
fn encrypted_toc_requires_rdr2_keys_and_never_lists_ciphertext() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("update_4.rpf");
    write_archive(&input, 0, 512);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&input)
        .unwrap();
    file.seek(SeekFrom::Start(12)).unwrap();
    file.write_all(&12u16.to_le_bytes()).unwrap();
    for args in [
        vec!["tree", p(&input)],
        vec!["resource", "convert", p(&input), "-o", p(tmp.path())],
    ] {
        let run = rage(&args);
        assert!(!run.status.success(), "{}", text(&run));
        assert!(text(&run).contains("--rdr2-keys"), "{}", text(&run));
        assert!(!text(&run).contains("12345678.ymt"), "{}", text(&run));
    }
}

#[test]
fn binary_metadata_entries_can_be_dumped_and_batch_exported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("binary.rpf");
    let payload = b"<FixtureMeta><count value=\"42\"/></FixtureMeta>";
    let stored = (payload.len() + 15) & !15;
    let mut data = vec![0; 512 + stored];
    put32(&mut data, 0, rpf_archive::RPF8_MAGIC);
    put32(&mut data, 4, 1);
    data[12] = 0xFF;
    data[14] = b'y';
    put64(
        &mut data,
        0x110,
        0x12345678 | (0xFFu64 << 40) | (11u64 << 48),
    );
    put64(
        &mut data,
        0x118,
        (stored as u64 >> 4) | ((512u64 >> 4) << 28),
    );
    put64(&mut data, 0x120, payload.len() as u64);
    data[512..512 + payload.len()].copy_from_slice(payload);
    std::fs::write(&path, data).unwrap();
    let run = rage(&["resource", "dump", "12345678.ymt", "--archive", p(&path)]);
    assert!(
        run.status.success() && text(&run).contains("42"),
        "{}",
        text(&run)
    );
    let run = rage(&[
        "resource",
        "export",
        p(&path),
        "-o",
        p(&tmp.path().join("out")),
    ]);
    assert!(run.status.success(), "{}", text(&run));
}
