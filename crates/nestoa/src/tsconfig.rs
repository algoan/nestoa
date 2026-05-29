//! Resolution of TypeScript `compilerOptions.paths` aliases.
//!
//! NestJS monorepos (Nx, pnpm workspaces) map package-style import specifiers
//! such as `@algoan/aggregator-raw-data` onto in-repo source folders via
//! `tsconfig.base.json` `paths`. nestoa does not run `tsc`, so it resolves these
//! aliases itself: the `extends` chain is followed to the config that declares
//! `paths`, and each pattern is matched (including a single `*` wildcard) to a
//! file under the resolved `baseUrl`.

use std::path::{Path, PathBuf};

/// A resolved set of path aliases plus the base directory their targets are
/// relative to.
#[derive(Debug, Default)]
pub struct TsPaths {
    /// Absolute directory that `paths` targets are resolved against.
    base_url: PathBuf,
    /// `(pattern, targets)` pairs straight from `compilerOptions.paths`.
    entries: Vec<(String, Vec<String>)>,
}

impl TsPaths {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Resolve a bare import specifier through the alias table to a file path.
    pub fn resolve(&self, specifier: &str) -> Option<PathBuf> {
        for (pattern, targets) in &self.entries {
            if let Some(captured) = match_pattern(pattern, specifier) {
                for target in targets {
                    let rel = target.replace('*', &captured);
                    let candidate = self.base_url.join(rel.trim_start_matches("./"));
                    if let Some(found) = resolve_file_with_exts(&candidate) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }
}

/// Match a tsconfig `paths` pattern against a specifier. A pattern has at most
/// one `*`; on a match the captured substring (empty for exact patterns) is
/// returned so it can be substituted into the target.
fn match_pattern(pattern: &str, specifier: &str) -> Option<String> {
    if let Some(star) = pattern.find('*') {
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        if specifier.len() >= prefix.len() + suffix.len()
            && specifier.starts_with(prefix)
            && specifier.ends_with(suffix)
        {
            return Some(specifier[prefix.len()..specifier.len() - suffix.len()].to_string());
        }
        None
    } else if pattern == specifier {
        Some(String::new())
    } else {
        None
    }
}

/// Try a candidate path as-is and with the usual TypeScript extensions.
fn resolve_file_with_exts(candidate: &Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return Some(candidate.canonicalize().unwrap_or_else(|_| candidate.to_path_buf()));
    }
    for ext in [
        ".ts", ".tsx", ".d.ts", "/index.ts", "/index.tsx", "/index.d.ts",
    ] {
        let mut p = candidate.as_os_str().to_os_string();
        p.push(ext);
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p.canonicalize().unwrap_or(p));
        }
    }
    None
}

/// Load path aliases for a project: read its `tsconfig.json` (or an explicit
/// path), following `extends` to whichever config declares `paths`.
pub fn load_tsconfig_paths(project_root: &Path, explicit: Option<&Path>) -> Option<TsPaths> {
    let start = match explicit {
        Some(p) => {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                project_root.join(p)
            }
        }
        None => project_root.join("tsconfig.json"),
    };
    let mut visited = std::collections::HashSet::new();
    let (base_url, entries) = collect(&start, &mut visited)?;
    Some(TsPaths { base_url, entries })
}

/// Walk the `extends` chain returning the `(base_url, paths)` from the most
/// derived config that declares `paths`.
fn collect(
    path: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Option<(PathBuf, Vec<(String, Vec<String>)>)> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(path.clone()) {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).ok()?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let co = json.get("compilerOptions");

    if let Some(paths) = co.and_then(|c| c.get("paths")).and_then(|p| p.as_object()) {
        if !paths.is_empty() {
            let base_rel = co
                .and_then(|c| c.get("baseUrl"))
                .and_then(|b| b.as_str())
                .unwrap_or(".");
            let base_url = dir.join(base_rel);
            let base_url = base_url.canonicalize().unwrap_or(base_url);
            let entries = paths
                .iter()
                .map(|(k, v)| {
                    let targets = v
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    (k.clone(), targets)
                })
                .collect();
            return Some((base_url, entries));
        }
    }

    // No `paths` here — follow `extends`.
    if let Some(ext) = json.get("extends").and_then(|e| e.as_str()) {
        let mut ext_path = dir.join(ext);
        if !ext_path.is_file() {
            ext_path = dir.join(format!("{}.json", ext));
        }
        return collect(&ext_path, visited);
    }
    None
}

/// Strip `//` and `/* */` comments and trailing commas so a JSONC tsconfig can
/// be parsed by serde_json. String literals are respected.
fn strip_jsonc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_str = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push('"');
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        chars.next();
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut prev = ' ';
                    for n in chars.by_ref() {
                        if prev == '*' && n == '/' {
                            break;
                        }
                        prev = n;
                    }
                }
                _ => out.push('/'),
            },
            _ => out.push(c),
        }
    }
    remove_trailing_commas(&out)
}

fn remove_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escape = false;
    for i in 0..chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                continue; // drop trailing comma
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_and_wildcard_patterns() {
        assert_eq!(match_pattern("@algoan/core", "@algoan/core"), Some(String::new()));
        assert_eq!(match_pattern("@algoan/core", "@algoan/other"), None);
        assert_eq!(
            match_pattern("@algoan/raw/*", "@algoan/raw/budget-insight"),
            Some("budget-insight".to_string())
        );
        assert_eq!(match_pattern("@algoan/raw/*", "@algoan/other/x"), None);
    }

    #[test]
    fn strips_comments_and_trailing_commas() {
        let src = r#"{
            // a line comment
            "compilerOptions": {
                "baseUrl": ".", /* block */
                "paths": { "a": ["b"], },
            },
        }"#;
        let parsed: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(parsed["compilerOptions"]["baseUrl"], ".");
    }

    #[test]
    fn resolves_alias_chain_via_extends() {
        let dir = std::env::temp_dir().join(format!("nestoa-tsc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("svc")).unwrap();
        std::fs::create_dir_all(dir.join("libs/core/src")).unwrap();
        std::fs::write(dir.join("libs/core/src/index.ts"), "export interface Core { a: string; }").unwrap();
        std::fs::write(
            dir.join("tsconfig.base.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@algoan/core": ["libs/core/src/index.ts"] } } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("svc/tsconfig.json"),
            r#"{ "extends": "../tsconfig.base.json", "compilerOptions": { "strict": true } }"#,
        )
        .unwrap();

        let paths = load_tsconfig_paths(&dir.join("svc"), None).expect("load");
        assert_eq!(paths.len(), 1);
        let resolved = paths.resolve("@algoan/core").expect("resolve alias");
        assert!(resolved.ends_with("libs/core/src/index.ts"), "got {:?}", resolved);
        assert!(paths.resolve("@algoan/missing").is_none());
    }
}
