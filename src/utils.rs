/// Glob-like match: `*` matches any run of characters (including `/`); a pattern
/// without `*` is a substring match.
pub fn matches_pattern(path: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return path.contains(pattern);
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);

    if !path.starts_with(first) { return false; }
    let mut rest = &path[first.len()..];

    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() { continue; }
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }

    rest.ends_with(last)
}

/// Quotes and escapes `s` as a JSON string literal.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::matches_pattern;

    #[test]
    fn single_star() {
        assert!(matches_pattern("a/b/c.ydr", "*.ydr"));
        assert!(matches_pattern("a/b/c.ydr", "a/*"));
        assert!(matches_pattern("a/b/c.ydr", "a/*.ydr"));
        assert!(!matches_pattern("a/b/c.ytd", "*.ydr"));
        assert!(!matches_pattern("x/b/c.ydr", "a/*"));
    }

    #[test]
    fn multiple_stars_match_in_order() {
        assert!(matches_pattern("x64b.rpf/levels/icons.rpf/prop.ydr", "*icons.rpf/*.ydr"));
        assert!(!matches_pattern("x64b.rpf/levels/icons.rpf/prop.ytd", "*icons.rpf/*.ydr"));
        assert!(!matches_pattern("x64b.rpf/levels/icons.rpf", "*icons.rpf/*"));
        assert!(matches_pattern("a/b/c/d", "a*c*d"));
        assert!(!matches_pattern("a/d/c", "a*c*d"));
        assert!(matches_pattern("anything", "*"));
        assert!(matches_pattern("abc", "a**c"));
    }

    #[test]
    fn no_star_is_substring() {
        assert!(matches_pattern("levels/inner.rpf/x", "inner.rpf"));
        assert!(!matches_pattern("levels/other.rpf", "inner.rpf"));
    }
}

/// Every file under `dir`, recursively. A link to a file is followed like
/// any other file; a link to a *directory* is not walked into — a junction
/// pointing at one of its own parents would otherwise recurse without end —
/// and a subdirectory that cannot be read costs one stderr line rather than
/// the whole walk. Only `dir` itself being unreadable is an error: the caller
/// named that one.
pub fn walkdir(dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    use anyhow::Context as _;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("skipping an entry under {}: {e}", dir.display());
                continue;
            }
        };
        // `file_type` reads the directory entry itself, unlike `Path::is_dir`,
        // which follows the link and asks about the target.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                eprintln!("skipping {}: {e}", entry.path().display());
                continue;
            }
        };
        let path = entry.path();
        if file_type.is_symlink() {
            // A link to a file is a file: a resource folder that links its
            // shared props in should still have them drawn. Only a link to a
            // directory is left alone, since that is the one that can point
            // at its own parent. `is_dir` here resolves the link on purpose.
            if !path.is_dir() {
                out.push(path);
            }
            continue;
        }
        if file_type.is_dir() {
            match walkdir(&path) {
                Ok(files) => out.extend(files),
                Err(e) => eprintln!("skipping {}: {e:#}", path.display()),
            }
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

/// `"X,Y"` as a pair of numbers — a world position, a cell index, a band.
pub fn parse_pair(s: &str) -> anyhow::Result<(f32, f32)> {
    let mut it = s.split(',').map(|p| p.trim().parse::<f32>());
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(a)), Some(Ok(b)), None) => Ok((a, b)),
        _ => anyhow::bail!("expected two comma-separated numbers, got '{s}'"),
    }
}

/// `"X0,Y0,X1,Y1"` as a world-space box.
pub fn parse_quad(s: &str) -> anyhow::Result<[f32; 4]> {
    let vals: Result<Vec<f32>, _> = s.split(',').map(|p| p.trim().parse::<f32>()).collect();
    match vals {
        Ok(v) if v.len() == 4 => Ok([v[0], v[1], v[2], v[3]]),
        _ => anyhow::bail!("expected four comma-separated numbers, got '{s}'"),
    }
}

/// `"X,Y,LABEL"` as a map marker; the label may be empty and may contain commas.
pub fn parse_marker(s: &str) -> anyhow::Result<(f32, f32, String)> {
    let mut it = s.splitn(3, ',');
    let (Some(x), Some(y)) = (
        it.next().and_then(|v| v.trim().parse::<f32>().ok()),
        it.next().and_then(|v| v.trim().parse::<f32>().ok()),
    ) else {
        anyhow::bail!("expected x,y,label; got '{s}'");
    };
    Ok((x, y, it.next().unwrap_or("").to_string()))
}

#[cfg(test)]
mod walk_tests {
    use super::walkdir;
    use std::path::Path;

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn a_normal_tree_yields_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("top.ydr"));
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        touch(&root.join("sub/mid.ymap"));
        touch(&root.join("sub/deeper/leaf.ytyp"));

        let mut names: Vec<String> =
            walkdir(root).unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["leaf.ytyp", "mid.ymap", "top.ydr"]);
    }

    /// A directory that links back to its own parent used to recurse until
    /// the stack blew; the walk must not follow it.
    #[test]
    fn a_directory_link_pointing_at_its_own_parent_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("real.ydr"));
        let link = root.join("loop");

        if !make_dir_link(root, &link) {
            eprintln!("skipping: this OS/account cannot create directory links");
            return;
        }

        let files = walkdir(root).expect("a link loop should not fail the walk");
        let names: Vec<String> =
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["real.ydr"], "the linked directory should be skipped, not walked");
    }

    /// A symlinked *file* is a file like any other: a resource folder that
    /// links its shared props in should still have them drawn. Only the
    /// directory case risks a loop.
    #[test]
    fn a_linked_file_is_walked_like_any_other() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let real = root.join("real.ydr");
        touch(&real);
        let link = root.join("linked.ydr");

        if !make_file_link(&real, &link) {
            eprintln!("skipping: this OS/account cannot link files");
            return;
        }

        let mut names: Vec<String> =
            walkdir(root).unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["linked.ydr", "real.ydr"], "a linked file should be yielded, not dropped");
    }

    /// True when `link` now stands for the file `target`. A symlink is what
    /// this is really about; a hard link is the fallback where Windows
    /// refuses one, and leaves the symlink branch untested on that machine.
    fn make_file_link(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        let symlinked = std::os::windows::fs::symlink_file(target, link).is_ok();
        #[cfg(not(windows))]
        let symlinked = std::os::unix::fs::symlink(target, link).is_ok();

        if symlinked {
            return true;
        }
        eprintln!("note: falling back to a hard link; the symlink branch is untested here");
        std::fs::hard_link(target, link).is_ok()
    }

    /// True when `link` now points at `target`. Windows needs either developer
    /// mode (for a symlink) or a junction, and may refuse both.
    fn make_dir_link(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_dir(target, link).is_ok() {
                return true;
            }
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }
}
