//! GitHub Actions glob patterns: workflow filter patterns (branches, tags, paths) and the
//! patterns `hashFiles()` takes.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// One pattern element. `Lit`, `Class` and `AnyOne` match a single character and can carry a quantifier.
#[derive(Debug)]
enum Atom {
    Lit(char),
    Class(Vec<(char, char)>),
    /// `?` in hashFiles patterns: any one character except `/`.
    AnyOne,
    /// `*`: any run of characters without `/`.
    Star,
    /// `**`: any run of characters, `/` included.
    DoubleStar,
    /// `**/`: nothing, or any run of characters ending in `/` (zero or more directories).
    Dirs,
}

#[derive(Debug)]
struct Tok {
    atom: Atom,
    min: usize,
    max: usize,
}

impl Tok {
    fn new(atom: Atom) -> Self {
        Tok {
            atom,
            min: 1,
            max: 1,
        }
    }

    fn single(&self, c: char) -> bool {
        match &self.atom {
            Atom::Lit(l) => *l == c,
            Atom::Class(ranges) => ranges.iter().any(|&(a, b)| a <= c && c <= b),
            Atom::AnyOne => c != '/',
            _ => false,
        }
    }

    /// Whether a following `?`/`+` quantifies this token.
    fn quantifiable(&self) -> bool {
        matches!(self.atom, Atom::Lit(_) | Atom::Class(_) | Atom::AnyOne)
            && self.min == 1
            && self.max == 1
    }
}

/// `filter_mode`: GitHub filter syntax where `?`/`+` quantify the preceding character.
/// Otherwise (hashFiles) `?` matches any single character and `+` is literal.
fn parse(pattern: &str, filter_mode: bool) -> Vec<Tok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                toks.push(Tok::new(Atom::Lit(chars[i + 1])));
                i += 2;
                continue;
            }
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    if chars.get(i + 2) == Some(&'/') {
                        toks.push(Tok::new(Atom::Dirs));
                        i += 3;
                    } else {
                        toks.push(Tok::new(Atom::DoubleStar));
                        i += 2;
                    }
                } else {
                    toks.push(Tok::new(Atom::Star));
                    i += 1;
                }
                continue;
            }
            '?' if !filter_mode => toks.push(Tok::new(Atom::AnyOne)),
            '?' | '+' if filter_mode && toks.last().is_some_and(Tok::quantifiable) => {
                let last = toks.last_mut().unwrap();
                if c == '?' {
                    last.min = 0;
                } else {
                    last.max = usize::MAX;
                }
            }
            '[' => match parse_class(&chars, i) {
                Some((ranges, next)) => {
                    toks.push(Tok::new(Atom::Class(ranges)));
                    i = next;
                    continue;
                }
                None => toks.push(Tok::new(Atom::Lit('['))),
            },
            _ => toks.push(Tok::new(Atom::Lit(c))),
        }
        i += 1;
    }
    toks
}

/// Parse `[abc]` / `[a-z0-9]` starting at `start` (the `[`). Returns the ranges and the index after `]`.
fn parse_class(chars: &[char], start: usize) -> Option<(Vec<(char, char)>, usize)> {
    let mut ranges = Vec::new();
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && i > start + 1 {
            return Some((ranges, i + 1));
        }
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&d| d != ']') {
            ranges.push((c, chars[i + 2]));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    None
}

fn match_from(toks: &[Tok], s: &[char], ti: usize, si: usize) -> bool {
    let Some(tok) = toks.get(ti) else {
        return si == s.len();
    };
    match tok.atom {
        Atom::Star => {
            let mut k = si;
            loop {
                if match_from(toks, s, ti + 1, k) {
                    return true;
                }
                if k >= s.len() || s[k] == '/' {
                    return false;
                }
                k += 1;
            }
        }
        Atom::DoubleStar => (si..=s.len()).any(|k| match_from(toks, s, ti + 1, k)),
        Atom::Dirs => {
            match_from(toks, s, ti + 1, si)
                || (si + 1..=s.len()).any(|k| s[k - 1] == '/' && match_from(toks, s, ti + 1, k))
        }
        _ => {
            let mut n = 0;
            while n < tok.max && si + n < s.len() && tok.single(s[si + n]) {
                n += 1;
            }
            if n < tok.min {
                return false;
            }
            (tok.min..=n)
                .rev()
                .any(|c| match_from(toks, s, ti + 1, si + c))
        }
    }
}

fn matches_with(pattern: &str, value: &str, filter_mode: bool) -> bool {
    let toks = parse(pattern, filter_mode);
    let s: Vec<char> = value.chars().collect();
    match_from(&toks, &s, 0, 0)
}

/// GitHub workflow filter pattern match (branches/tags/paths): `*` matches any chars except `/`, `**` matches any chars incl `/`,
/// `?` zero-or-one of the preceding char, `+` one-or-more of the preceding char, `[abc]`/`[a-z]` char classes, `\` escapes.
/// (See docs "Filter pattern cheat sheet". Note `?` and `+` are quantifiers on the preceding char, not single-char wildcards.)
pub fn matches(pattern: &str, value: &str) -> bool {
    matches_with(pattern, value, true)
}

/// Evaluate an include list where entries starting with `!` negate; later entries override earlier ones (last match wins).
/// Returns true if `value` is included.
pub fn included(patterns: &[String], value: &str) -> bool {
    let mut result = false;
    for p in patterns {
        match p.strip_prefix('!') {
            Some(neg) if matches(neg, value) => result = false,
            Some(_) => {}
            None if matches(p, value) => result = true,
            None => {}
        }
    }
    result
}

/// hashFiles: patterns are relative to `root`, use `*`, `**`, `?` (here `?` = single char, as in @actions/glob) and `!` negation.
/// Walk `root` recursively (skip `.git`), match each file's `/`-separated relative path, then return hex of
/// SHA256 over the concatenation of each matched file's SHA256 digest in sorted path order. Return "" if nothing matched.
pub fn hash_files(root: &Path, patterns: &[String]) -> Result<String> {
    let mut files = Vec::new();
    walk(root, "", &mut files)?;
    let selected = select_files(&files, patterns);
    let digests = selected
        .iter()
        .map(|rel| {
            let bytes = std::fs::read(root.join(rel)).with_context(|| format!("hashFiles: reading {rel}"))?;
            Ok(Sha256::digest(&bytes).to_vec())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(combine_digests(&digests))
}

/// The `/`-separated relative paths (from `files`, sorted) that hashFiles' patterns select.
pub fn select_files(files: &[String], patterns: &[String]) -> Vec<String> {
    // (negated, pattern) with leading `./` removed.
    let pats: Vec<(bool, String)> = patterns
        .iter()
        .map(|p| {
            let p = p.trim();
            let (neg, p) = match p.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, p),
            };
            let p = p.strip_prefix("./").unwrap_or(p).trim_end_matches('/');
            (neg, p.to_string())
        })
        .filter(|(_, p)| !p.is_empty())
        .collect();
    // A pattern that matches a directory includes every file below it.
    let hit = |pattern: &str, path: &str| {
        matches_with(pattern, path, false)
            || path
                .match_indices('/')
                .any(|(i, _)| matches_with(pattern, &path[..i], false))
    };
    let mut files: Vec<String> = files.to_vec();
    files.sort();
    files
        .into_iter()
        .filter(|rel| {
            let mut included = false;
            for (neg, p) in &pats {
                if hit(p, rel) {
                    included = !neg;
                }
            }
            included
        })
        .collect()
}

/// hashFiles' result: SHA256 over the concatenated per-file SHA256 digests; "" for no files.
pub fn combine_digests(digests: &[Vec<u8>]) -> String {
    if digests.is_empty() {
        return String::new();
    }
    let mut outer = Sha256::new();
    for d in digests {
        outer.update(d);
    }
    hex::encode(outer.finalize())
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if prefix.is_empty() => {
            return Err(e).with_context(|| format!("hashFiles: reading {}", dir.display()));
        }
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if name != ".git" {
                walk(&entry.path(), &rel, out)?;
            }
        } else if ft.is_file() || (ft.is_symlink() && entry.path().is_file()) {
            out.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_cheat_sheet() {
        assert!(matches("feature/*", "feature/my-branch"));
        assert!(!matches("feature/*", "feature/your/branch"));
        assert!(matches("feature/**", "feature/your/branch"));
        assert!(matches("main", "main"));
        assert!(!matches("main", "main2"));
        assert!(!matches("*", "a/b"));
        assert!(matches("**", "a/b"));
        assert!(matches("*-alpha", "v2-alpha"));
        assert!(matches("**/*.js", "app.js"));
        assert!(matches("**/*.js", "src/js/app.js"));
        assert!(matches("**.js", "src/js/app.js"));
        assert!(!matches("*.js", "src/app.js"));
        assert!(matches("docs/**", "docs/a/b.md"));
        assert!(matches("**/migrate-*.sql", "migrate-10909.sql"));
        assert!(matches("**/migrate-*.sql", "db/sept/migrate-v1.sql"));
        assert!(matches("**/docs/**", "docs/hello.md"));
        assert!(matches("**/docs/**", "dir/docs/my-file.txt"));
    }

    #[test]
    fn quantifiers_and_classes() {
        let p = "v[12].[0-9]+.[0-9]+";
        assert!(matches(p, "v1.10.1"));
        assert!(matches(p, "v2.0.0"));
        assert!(!matches(p, "v3.0.0"));
        assert!(!matches(p, "v1.x.1"));
        assert!(!matches(p, "v1..1"));
        assert!(matches("*.jsx?", "page.js"));
        assert!(matches("*.jsx?", "page.jsx"));
        assert!(!matches("*.jsx?", "page.jsxx"));
        assert!(matches("[a-c]x", "bx"));
        assert!(!matches("[a-c]x", "dx"));
        // `[` without a closing bracket is literal.
        assert!(matches("a[b", "a[b"));
    }

    #[test]
    fn escapes() {
        assert!(matches(r"a\*b", "a*b"));
        assert!(!matches(r"a\*b", "axb"));
        assert!(matches(r"v\+1", "v+1"));
    }

    #[test]
    fn negation_last_match_wins() {
        let pats: Vec<String> = ["releases/**", "!releases/**-alpha"]
            .map(String::from)
            .to_vec();
        assert!(included(&pats, "releases/10"));
        assert!(included(&pats, "releases/beta/mona"));
        assert!(!included(&pats, "releases/10-alpha"));
        assert!(!included(&pats, "main"));
        let re: Vec<String> = [
            "sub-package/**",
            "!sub-package/docs/**",
            "sub-package/docs/keep.md",
        ]
        .map(String::from)
        .to_vec();
        assert!(included(&re, "sub-package/docs/keep.md"));
        assert!(!included(&re, "sub-package/docs/other.md"));
    }

    fn expected(files: &[&[u8]]) -> String {
        let mut h = Sha256::new();
        for f in files {
            h.update(Sha256::digest(f));
        }
        hex::encode(h.finalize())
    }

    #[test]
    fn hash_files_walks_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("sub/b.txt"), b"b").unwrap();
        std::fs::write(root.join("sub/deep/c.lock"), b"c").unwrap();
        std::fs::write(root.join(".git/x.txt"), b"git").unwrap();
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            hash_files(root, &p(&["**/*.txt"])).unwrap(),
            expected(&[b"a", b"b"])
        );
        assert_eq!(
            hash_files(root, &p(&["./*.txt"])).unwrap(),
            expected(&[b"a"])
        );
        assert_eq!(
            hash_files(root, &p(&["**/*.txt", "!sub/**"])).unwrap(),
            expected(&[b"a"])
        );
        assert_eq!(
            hash_files(root, &p(&["sub"])).unwrap(),
            expected(&[b"b", b"c"])
        );
        assert_eq!(
            hash_files(root, &p(&["sub/deep/c.loc?"])).unwrap(),
            expected(&[b"c"])
        );
        assert_eq!(hash_files(root, &p(&["nope"])).unwrap(), "");
        assert_eq!(hash_files(root, &p(&[".git/**"])).unwrap(), "");
    }
}
