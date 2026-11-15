use std::{fs, process::Command};
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rage")).args(args)
        .env_remove("GTAV_PATH").env("RAGE_NO_UPDATE_CHECK", "1").output().unwrap()
}
#[test]
fn renderware_info_dump_build_preserves_unknown_chunks_and_padding() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("test.txd");
    let dump = t.path().join("test.json");
    let output = t.path().join("out.txd");
    let mut bytes = Vec::new();
    for n in [0x16u32, 31, 0x1803ffff, 1, 4, 0x1803ffff, 0, 0x12345678, 3, 0x1803ffff] {
        bytes.extend_from_slice(&n.to_le_bytes());
    }
    bytes.extend_from_slice(&[1,2,3]); bytes.resize(2048,0);
    fs::write(&source,&bytes).unwrap();
    let result = run(&["resource","info",source.to_str().unwrap(),"--json"]);
    assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
    let value = json::parse(&String::from_utf8_lossy(&result.stdout)).unwrap();
    assert_eq!(value["format"],"txd");
    let result = run(&["resource","dump",source.to_str().unwrap(),"-o",dump.to_str().unwrap()]);
    assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
    let result = run(&["resource","build",dump.to_str().unwrap(),"-o",output.to_str().unwrap()]);
    assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
    assert!(fs::read(output).unwrap() == bytes);
    let wrong = t.path().join("wrong.dff");
    assert!(!run(&["resource","build",dump.to_str().unwrap(),"-o",wrong.to_str().unwrap()]).status.success());
    assert!(!wrong.exists());
}
