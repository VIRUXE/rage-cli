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

/// Every file under `dir`, recursively.
pub fn walkdir(dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    use anyhow::Context as _;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() { out.extend(walkdir(&path)?); } else { out.push(path); }
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
