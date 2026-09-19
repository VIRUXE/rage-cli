//! GitHub-based version checking and self-update.
//!
//! Two independent things live here: a passive daily check that main() spawns
//! on a background thread and reports after the real command has run, and the
//! `plan_update`/`install` pair that `rpf update install` drives directly.
//! Everything here is silent on failure except through `log::debug!` — an
//! update check must never break or even slow down an unrelated command.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// A release as reported by the GitHub API: its tag and every asset's
/// (name, browser_download_url).
pub struct Release {
    pub tag: String,
    pub assets: Vec<(String, String)>,
}

/// The `owner/repo` this crate is published under, taken from `Cargo.toml`.
pub fn repo_slug() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/").trim_end_matches('/')
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Parses a release tag (`v0.13.0`, `0.13.0`, tolerating a `-rc1`/`+build`
/// suffix) into `(major, minor, patch)`. `None` for anything else, including
/// a tag with more or fewer than three numeric components.
pub fn parse_version(tag: &str) -> Option<(u32, u32, u32)> {
    let s = tag.strip_prefix('v').unwrap_or(tag);
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?, patch.parse().ok()?))
}

/// `true` only when both sides parse and `latest` is strictly greater —
/// an unparseable tag on either side never triggers a nag.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// The release asset name for this platform, or `None` when there is no
/// prebuilt binary for it.
pub fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("rage-windows-x86_64.exe"),
        ("linux", "x86_64") => Some("rage-linux-x86_64"),
        _ => None,
    }
}

fn agent(total: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!(
            "rage-cli/", env!("CARGO_PKG_VERSION"), " (+https://github.com/VIRUXE/rage-cli)"
        ))
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(total))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Queries `GET /repos/{repo}/releases/latest`, sending `etag` as
/// `If-None-Match` when given. `Ok((None, None))` means "not modified" (a
/// 304): the caller should keep whatever it already had cached.
pub fn fetch_latest(etag: Option<&str>) -> Result<(Option<Release>, Option<String>)> {
    let agent = agent(Duration::from_secs(8));
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo_slug());

    let mut req = agent.get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(etag) = etag {
        req = req.header("If-None-Match", etag);
    }

    let mut res = req.call().context("requesting the latest release from GitHub")?;

    match res.status().as_u16() {
        304 => Ok((None, None)),
        200 => {
            let new_etag = res.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
            let body = res.body_mut().read_to_string().context("reading the GitHub API response")?;
            let parsed = json::parse(&body).context("parsing the GitHub API response")?;

            let tag = parsed["tag_name"].as_str()
                .context("GitHub release has no tag_name")?
                .to_string();
            let assets = parsed["assets"].members().filter_map(|a| {
                let name = a["name"].as_str()?.to_string();
                let url = a["browser_download_url"].as_str()?.to_string();
                Some((name, url))
            }).collect();

            Ok((Some(Release { tag, assets }), new_etag))
        }
        403 | 429 => bail!("rate limited by the GitHub API (HTTP {})", res.status()),
        status => bail!("GitHub API returned HTTP {status}"),
    }
}

/// Streams `url` into `dest`, hashing as it goes. Returns the byte count and
/// the SHA-256 digest, so the caller never has to re-read the file to verify
/// it. Does not use ureq's `read_to_vec()`, which caps at 10 MB by default.
pub fn download_to(url: &str, dest: &mut File) -> Result<(u64, [u8; 32])> {
    let agent = agent(Duration::from_secs(600));
    let mut res = agent.get(url).call().with_context(|| format!("downloading {url}"))?;
    if !res.status().is_success() {
        bail!("downloading {url} failed: HTTP {}", res.status());
    }

    let mut reader = res.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;

    loop {
        let n = reader.read(&mut buf).context("reading the download")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dest.write_all(&buf[..n]).context("writing the downloaded file")?;
        total += n as u64;
    }
    dest.flush()?;
    dest.sync_all()?;

    Ok((total, hasher.finalize().into()))
}

/// Checks `digest` against the `<hex>  <name>` line for `asset` inside a
/// `SHA256SUMS` file. Fails closed: a file with no line for `asset` is an
/// error, not a pass.
pub fn verify_sha256(digest: &[u8; 32], sums: &str, asset: &str) -> Result<()> {
    let hex_digest = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();

    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else { continue };
        if name.trim_start_matches('*') != asset {
            continue;
        }
        if hash.eq_ignore_ascii_case(&hex_digest) {
            return Ok(());
        }
        bail!("checksum mismatch for {asset}: SHA256SUMS says {hash}, downloaded file hashes to {hex_digest}");
    }
    bail!("SHA256SUMS has no entry for {asset}");
}

/// Everything `install` needs to replace the running binary.
pub struct Plan {
    pub current: String,
    pub latest: String,
    pub asset: String,
    pub url: String,
    pub sums_url: String,
    pub target: PathBuf,
}

/// `None` when the running binary is already at least as new as the latest
/// release and `force` was not given.
pub fn plan_update(force: bool) -> Result<Option<Plan>> {
    let (release, _etag) = fetch_latest(None)?;
    let release = release.context("GitHub returned no releases for this repository")?;

    let latest_tuple = parse_version(&release.tag)
        .with_context(|| format!("could not parse the release tag '{}'", release.tag))?;
    let current_str = current_version();
    let current_tuple = parse_version(current_str).expect("CARGO_PKG_VERSION is always well-formed");

    if !force && latest_tuple <= current_tuple {
        return Ok(None);
    }

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let asset_name = asset_for(os, arch).with_context(|| format!(
        "self-update is not available for {os}/{arch} — install from source: cargo install --git https://github.com/{}",
        repo_slug(),
    ))?;

    let url = release.assets.iter().find(|(name, _)| name == asset_name).map(|(_, url)| url.clone())
        .with_context(|| format!(
            "release {} has no asset named {asset_name} (it may still be building) — try again shortly",
            release.tag,
        ))?;
    let sums_url = release.assets.iter().find(|(name, _)| name == "SHA256SUMS").map(|(_, url)| url.clone())
        .context("this release has no SHA256SUMS asset; rpf update only installs releases published with checksums")?;

    let target = std::env::current_exe()
        .context("locating the running binary")?
        .canonicalize()
        .context("resolving the running binary's real path")?;

    Ok(Some(Plan {
        current: current_str.to_string(),
        latest: format!("{}.{}.{}", latest_tuple.0, latest_tuple.1, latest_tuple.2),
        asset: asset_name.to_string(),
        url,
        sums_url,
        target,
    }))
}

/// Downloads, verifies, and swaps in `plan`'s release. The running binary is
/// never touched until the download is fully verified.
pub fn install(plan: &Plan) -> Result<()> {
    let dir = plan.target.parent().context("the running binary has no parent directory")?;

    // A leftover from a previous Windows update that this process's earlier
    // run could not remove because it was still running as that binary.
    #[cfg(windows)]
    { let _ = std::fs::remove_file(plan.target.with_extension("old")); }

    let tmp_path = dir.join(format!(".rage-update-{}.tmp", std::process::id()));
    let mut tmp_file = File::create(&tmp_path).with_context(|| format!(
        "cannot write to {}: is the directory writable? re-run with elevated privileges, \
         or reinstall with `cargo install --git https://github.com/{}`",
        dir.display(), repo_slug(),
    ))?;

    let verified = download_and_verify(plan, &mut tmp_file);
    drop(tmp_file);

    if let Err(e) = verified {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    replace_binary(&tmp_path, &plan.target)
}

fn download_and_verify(plan: &Plan, tmp_file: &mut File) -> Result<()> {
    let (len, digest) = download_to(&plan.url, tmp_file)?;
    if len < 1024 * 1024 {
        bail!("downloaded asset is only {len} bytes — looks truncated");
    }

    let sums_agent = agent(Duration::from_secs(30));
    let mut sums_res = sums_agent.get(&plan.sums_url).call().context("downloading SHA256SUMS")?;
    if !sums_res.status().is_success() {
        bail!("downloading SHA256SUMS failed: HTTP {}", sums_res.status());
    }
    let sums = sums_res.body_mut().read_to_string().context("reading SHA256SUMS")?;

    verify_sha256(&digest, &sums, &plan.asset)
}

/// Atomically swaps `tmp` in for `target`. On Windows this is a two-step
/// rename dance since a running `.exe` cannot be deleted, only renamed; on
/// Unix a single rename suffices since replacing a running executable's
/// directory entry is safe (the process keeps its open inode).
fn replace_binary(tmp: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target).map(|m| m.permissions().mode()).unwrap_or(0o755) | 0o111;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))
            .context("setting the new binary's permissions")?;
        std::fs::rename(tmp, target).context("replacing the running binary")?;
    }
    #[cfg(windows)]
    {
        let old = target.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old).context("renaming the current binary aside")?;
        if let Err(e) = std::fs::rename(tmp, target) {
            let _ = std::fs::rename(&old, target);
            return Err(e).context("installing the new binary");
        }
        let _ = std::fs::remove_file(&old);
    }
    Ok(())
}

// ─── daily background check ────────────────────────────────────────────────

struct Stamp {
    last_check: u64,
    latest_version: Option<String>,
    etag: Option<String>,
}

fn stamp_path() -> Option<PathBuf> {
    if let Some(p) = crate::paths::env_var("RAGE_UPDATE_CACHE") {
        return Some(PathBuf::from(p));
    }
    Some(crate::paths::config_root()?.join("update-check.json"))
}

fn load_stamp(p: &Path) -> Option<Stamp> {
    let text = std::fs::read_to_string(p).ok()?;
    let parsed = json::parse(&text).ok()?;
    Some(Stamp {
        last_check: parsed["last_check"].as_u64()?,
        latest_version: parsed["latest_version"].as_str().map(str::to_string),
        etag: parsed["etag"].as_str().map(str::to_string),
    })
}

fn store_stamp(p: &Path, s: &Stamp) {
    let Some(dir) = p.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        log::debug!("update-check cache {} is not writable", p.display());
        return;
    }
    let body = format!(
        "{{\"last_check\":{},\"latest_version\":{},\"etag\":{}}}\n",
        s.last_check,
        s.latest_version.as_deref().map_or("null".to_string(), crate::utils::json_string),
        s.etag.as_deref().map_or("null".to_string(), crate::utils::json_string),
    );
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, p)).is_err() {
        log::debug!("failed to write the update-check cache at {}", p.display());
    }
}

/// Records the result of an explicit `rpf update check`, resetting the daily
/// clock the same way a background check would.
pub fn record_check(latest: &str, etag: Option<&str>) {
    let Some(path) = stamp_path() else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    store_stamp(&path, &Stamp { last_check: now, latest_version: Some(latest.to_string()), etag: etag.map(str::to_string) });
}

fn note(latest: &str) -> String {
    format!("note: rpf {latest} is available (you have {}) — run `rpf update install` to upgrade", current_version())
}

/// Kicks off the daily check on a background thread when it is due and not
/// suppressed, returning a channel the caller can poll once the real command
/// has finished. Returns `None` whenever there is nothing to wait on —
/// disabled, non-interactive, or simply not due yet.
pub fn spawn_background_check(disabled: bool) -> Option<Receiver<String>> {
    if disabled
        || crate::paths::env_var("RAGE_NO_UPDATE_CHECK").is_some()
        || std::env::var_os("CI").is_some()
    {
        return None;
    }
    // Also keeps every integration test (which pipes stderr) off the network.
    if !std::io::stderr().is_terminal() {
        return None;
    }

    let path = stamp_path()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let stamp = load_stamp(&path);
    let prev_latest = stamp.as_ref().and_then(|s| s.latest_version.clone());
    let prev_etag = stamp.as_ref().and_then(|s| s.etag.clone());
    let due = stamp.as_ref().is_none_or(|s| now.saturating_sub(s.last_check) >= CHECK_INTERVAL_SECS || s.last_check > now);

    if !due {
        if let Some(latest) = &prev_latest
            && is_newer(latest, current_version())
        {
            let (tx, rx) = mpsc::channel();
            let _ = tx.send(note(latest));
            return Some(rx);
        }
        return None;
    }

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        match fetch_latest(prev_etag.as_deref()) {
            Ok((Some(release), new_etag)) => {
                let latest = release.tag.trim_start_matches('v').to_string();
                store_stamp(&path, &Stamp {
                    last_check: now,
                    latest_version: Some(latest.clone()),
                    etag: new_etag.or(prev_etag),
                });
                if is_newer(&latest, current_version()) {
                    let _ = tx.send(note(&latest));
                }
            }
            Ok((None, _)) => {
                // 304 Not Modified — nothing changed since prev_etag.
                store_stamp(&path, &Stamp { last_check: now, latest_version: prev_latest.clone(), etag: prev_etag });
                if let Some(latest) = &prev_latest
                    && is_newer(latest, current_version())
                {
                    let _ = tx.send(note(latest));
                }
            }
            Err(e) => {
                log::debug!("update check failed: {e:#}");
                // Stamp anyway so a rate limit or offline network doesn't
                // get retried for another 24h.
                store_stamp(&path, &Stamp { last_check: now, latest_version: prev_latest, etag: prev_etag });
            }
        }
    });
    Some(rx)
}

/// Waits briefly for the background check and prints its note, if any. Must
/// be called only after the real command has already succeeded.
pub fn report_background_check(rx: Option<Receiver<String>>) {
    let Some(rx) = rx else { return };
    if let Ok(msg) = rx.recv_timeout(Duration::from_millis(300)) {
        eprintln!("{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("v0.13.0"), Some((0, 13, 0)));
        assert_eq!(parse_version("0.13.0"), Some((0, 13, 0)));
        assert_eq!(parse_version("v0.13"), Some((0, 13, 0)));
        assert_eq!(parse_version("0.13.0-rc1"), Some((0, 13, 0)));
        assert_eq!(parse_version("0.13.0+build5"), Some((0, 13, 0)));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("garbage"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn compares_versions_numerically_not_lexically() {
        assert!(is_newer("0.13.0", "0.12.2"));
        assert!(is_newer("1.0.0", "0.13.0"));
        assert!(is_newer("0.12.0", "0.9.0"), "0.12.0 must sort after 0.9.0, not before it");
        assert!(!is_newer("0.12.2", "0.12.2"));
        assert!(!is_newer("0.12.0", "0.12.2"));
        assert!(!is_newer("garbage", "0.12.2"));
    }

    #[test]
    fn maps_known_platforms_only() {
        assert_eq!(asset_for("windows", "x86_64"), Some("rage-windows-x86_64.exe"));
        assert_eq!(asset_for("linux", "x86_64"), Some("rage-linux-x86_64"));
        assert_eq!(asset_for("macos", "aarch64"), None);
        assert_eq!(asset_for("linux", "aarch64"), None);
    }

    #[test]
    fn parses_the_release_fixture() {
        let body = include_str!("../tests/fixtures/latest-release.json");
        let parsed = json::parse(body).unwrap();
        assert_eq!(parsed["tag_name"].as_str(), Some("v0.12.2"));

        let assets: Vec<(String, String)> = parsed["assets"].members().filter_map(|a| {
            Some((a["name"].as_str()?.to_string(), a["browser_download_url"].as_str()?.to_string()))
        }).collect();
        assert!(assets.iter().any(|(name, _)| name == "rage-windows-x86_64.exe"));
        assert!(assets.iter().any(|(name, _)| name == "rage-linux-x86_64"));
    }

    #[test]
    fn verifies_matching_and_rejects_mismatched_checksums() {
        let digest: [u8; 32] = Sha256::digest(b"hello").into();
        let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();

        let sums = format!("{hex}  rage-linux-x86_64\n");
        assert!(verify_sha256(&digest, &sums, "rage-linux-x86_64").is_ok());
        assert!(verify_sha256(&digest, &sums, "rage-windows-x86_64.exe").is_err());

        let wrong = format!("{}  rage-linux-x86_64\n", "0".repeat(64));
        assert!(verify_sha256(&digest, &wrong, "rage-linux-x86_64").is_err());
    }

    #[test]
    fn stamp_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");

        assert!(load_stamp(&path).is_none(), "missing file reads as no stamp");

        store_stamp(&path, &Stamp { last_check: 1_700_000_000, latest_version: Some("0.13.0".into()), etag: Some("W/\"abc\"".into()) });
        let loaded = load_stamp(&path).expect("stamp should round-trip");
        assert_eq!(loaded.last_check, 1_700_000_000);
        assert_eq!(loaded.latest_version.as_deref(), Some("0.13.0"));
        assert_eq!(loaded.etag.as_deref(), Some("W/\"abc\""));

        std::fs::write(&path, b"not json").unwrap();
        assert!(load_stamp(&path).is_none(), "corrupt file reads as no stamp");
    }
}
