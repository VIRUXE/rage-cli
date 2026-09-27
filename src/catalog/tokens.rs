//! Turning RAGE asset names into search tokens, and user queries into safe
//! FTS5 expressions.
//!
//! Asset names are `snake_case` with camelCase and digits mixed in
//! (`Prop_ChairPlastic01a_LOD`). The FTS5 table keeps `_` inside tokens, so
//! the whole name is one token (good for `prop_chair*` prefix searches), and
//! this module adds each part on its own (`chair`, `plastic`, `01`, `a`) so a
//! plain word finds it too.

/// The search text for one name: the lowercased name, then each
/// underscore-separated part, then every camelCase / letter-digit piece of
/// those parts. Duplicates are dropped; order is stable.
pub fn tokenize_name(name: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: String| {
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    push(trimmed.to_lowercase().replace(|c: char| !(c.is_alphanumeric() || c == '_'), "_"));
    for part in trimmed.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        push(part.to_lowercase());
        for piece in split_case_and_digits(part) {
            push(piece.to_lowercase());
        }
    }
    out.join(" ")
}

/// `ChairPlastic01a` -> `Chair`, `Plastic`, `01`, `a`.
fn split_case_and_digits(part: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = part.char_indices().collect();
    let mut pieces = Vec::new();
    let mut start = 0;
    for i in 1..chars.len() {
        let (_, prev) = chars[i - 1];
        let (at, cur) = chars[i];
        let boundary = (prev.is_ascii_digit() != cur.is_ascii_digit())
            || (prev.is_lowercase() && cur.is_uppercase())
            // `HTMLParser` -> `HTML` `Parser`: an upper followed by upper+lower.
            || (prev.is_uppercase() && cur.is_uppercase() && chars.get(i + 1).is_some_and(|(_, n)| n.is_lowercase()));
        if boundary {
            pieces.push(&part[start..at]);
            start = at;
        }
    }
    pieces.push(&part[start..]);
    pieces
}

/// The tokens of an archive path: each segment's name parts, without the
/// `.rpf`/extension noise.
pub fn tokenize_path(segments: &[&str]) -> String {
    let mut out = Vec::new();
    for seg in segments {
        for part in seg.split(['/', '\\']) {
            let stem = part.rsplit_once('.').map(|(s, _)| s).unwrap_or(part);
            let t = tokenize_name(stem);
            if !t.is_empty() {
                out.push(t);
            }
        }
    }
    out.join(" ")
}

/// A user query as an FTS5 expression. Each whitespace-separated term is
/// reduced to letters, digits and `_`, quoted, and (from two characters up)
/// made a prefix match; terms are ANDed. `None` when nothing searchable is
/// left. With `raw`, the query is passed through untouched for callers who
/// want FTS5's own syntax (`OR`, `NEAR`, column filters).
pub fn fts_query(user: &str, raw: bool) -> Option<String> {
    if raw {
        let q = user.trim();
        return (!q.is_empty()).then(|| q.to_string());
    }
    let terms: Vec<String> = user
        .split_whitespace()
        .map(|t| t.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect::<String>().to_lowercase())
        .filter(|t| !t.is_empty())
        .map(|t| if t.chars().count() >= 2 { format!("\"{t}\"*") } else { format!("\"{t}\"") })
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_underscores_case_and_digits() {
        assert_eq!(
            tokenize_name("Prop_ChairPlastic01a_LOD"),
            "prop_chairplastic01a_lod prop chairplastic01a chair plastic 01 a lod"
        );
    }

    #[test]
    fn keeps_lod_prefixes_as_tokens() {
        let t = tokenize_name("hi@prop_bin_05a");
        assert!(t.starts_with("hi_prop_bin_05a "), "{t}");
        assert!(t.split(' ').any(|x| x == "hi"));
        assert!(t.split(' ').any(|x| x == "bin"));
    }

    #[test]
    fn splits_acronyms() {
        assert_eq!(split_case_and_digits("HTMLParser"), vec!["HTML", "Parser"]);
        assert_eq!(split_case_and_digits("v_ilev_fib"), vec!["v_ilev_fib"]);
    }

    #[test]
    fn empty_names_give_nothing() {
        assert_eq!(tokenize_name("  "), "");
    }

    #[test]
    fn path_tokens_drop_extensions() {
        assert_eq!(tokenize_path(&["x64c.rpf", "levels/gta5/props.rpf", "v_res_chair.ydr"]),
                   "x64c x 64 c levels gta5 gta 5 props v_res_chair v res chair");
    }

    #[test]
    fn queries_are_quoted_and_prefixed() {
        assert_eq!(fts_query("red  Chair", false).unwrap(), "\"red\"* AND \"chair\"*");
        assert_eq!(fts_query("a", false).unwrap(), "\"a\"");
        assert_eq!(fts_query("prop_bin\"; DROP", false).unwrap(), "\"prop_bin\"* AND \"drop\"*");
        assert_eq!(fts_query("OR NEAR(", false).unwrap(), "\"or\"* AND \"near\"*");
        assert!(fts_query(" \"* ", false).is_none());
        assert_eq!(fts_query("chair OR stool", true).unwrap(), "chair OR stool");
    }
}
