//! `rage names fetch` and `names info` against a local HTTP server and a
//! temporary list; needs no game install and never touches the network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

fn rage(list: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rage"))
        .args(args)
        .env("RAGE_NO_UPDATE_CHECK", "1")
        .env("RAGE_NAMES", list)
        .output()
        .expect("failed to run the rage binary")
}

fn ok(list: &Path, args: &[&str]) -> String {
    let output = rage(list, args);
    assert!(
        output.status.success(),
        "`rage {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Serves `body` to the next `hits` GET requests on a free local port and
/// returns the URL. Each request is answered on its own thread so a slow
/// client never blocks the test.
fn serve(body: &'static str, hits: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/ObjectList.ini", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for _ in 0..hits {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    url
}

#[test]
fn fetch_writes_a_list_that_info_and_lookup_read() {
    let tmp = tempfile::tempdir().unwrap();
    let list = tmp.path().join("names.txt");
    let url = serve("# community dump\n[Objects]\nprop_beach_fire\nprop_barier_conc_05b\n", 1);

    let out = ok(&list, &["names", "fetch", "--url", &url, "--build", "3717"]);
    assert!(out.contains("Wrote 2 names"), "{out}");
    assert!(out.contains("covers game build 3717"), "{out}");

    let text = std::fs::read_to_string(&list).unwrap();
    assert!(text.contains(&format!("# source: fetch {url} on ")), "{text}");
    assert!(text.contains("# build: 3717\n"), "{text}");
    assert!(text.ends_with("prop_barier_conc_05b\nprop_beach_fire\n"), "{text}");

    let out = ok(&list, &["names", "info"]);
    assert!(out.contains("Names:          2"), "{out}");
    assert!(out.contains("Coverage:       covers game build 3717"), "{out}");
    assert!(out.contains("Sources:\n  fetch "), "{out}");

    let out = ok(&list, &["names", "lookup", "0xC079B265"]);
    assert_eq!(out.trim(), "0xC079B265  prop_beach_fire");
}

#[test]
fn a_second_fetch_merges_and_the_build_only_moves_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let list = tmp.path().join("names.txt");
    let newer = serve("prop_a\nprop_b\n", 1);
    let older = serve("prop_b\nprop_c\n", 2);

    ok(&list, &["names", "fetch", "--url", &newer, "--build", "3889"]);
    let out = ok(&list, &["names", "fetch", "--url", &older, "--build", "3717"]);
    assert!(out.contains("Wrote 3 names"), "{out}");
    assert!(out.contains("(1 new; the list covers game build 3889)"), "{out}");

    let out = ok(&list, &["names", "info"]);
    assert_eq!(out.matches("\n  fetch ").count(), 2, "{out}");

    // --replace starts over, and a list with no stated build says so.
    let out = ok(&list, &["names", "fetch", "--url", &older, "--replace"]);
    assert!(out.contains("Wrote 2 names"), "{out}");
    let out = ok(&list, &["names", "info"]);
    assert!(out.contains("of unstated game build"), "{out}");
    assert_eq!(out.matches("\n  fetch ").count(), 1, "{out}");
}

#[test]
fn an_empty_or_missing_list_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let list = tmp.path().join("names.txt");
    let url = serve("# nothing but comments\n", 1);
    let output = rage(&list, &["names", "fetch", "--url", &url]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("holds no names"));
    assert!(!list.exists());

    let out = ok(&list, &["names", "info"]);
    assert!(out.contains("None yet"), "{out}");
    assert!(out.contains("rage names fetch"), "{out}");
}

/// A list written before the header existed still reads as a harvest.
#[test]
fn a_pre_header_harvest_is_recognised() {
    let tmp = tempfile::tempdir().unwrap();
    let list = tmp.path().join("names.txt");
    std::fs::write(&list, "# Names harvested from the game's own files by `rage names harvest`.\nprop_a\n").unwrap();
    let out = ok(&list, &["names", "info"]);
    assert!(out.contains("Names:          1"), "{out}");
    assert!(out.contains("Coverage:       of unstated game build"), "{out}");
    assert!(out.contains("harvest by an earlier rage"), "{out}");
}
