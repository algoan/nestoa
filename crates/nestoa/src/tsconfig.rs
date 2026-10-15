//! Resolution of TypeScript `compilerOptions.paths` aliases.
//!
//! NestJS monorepos (Nx, pnpm workspaces) map package-style import specifiers
//! such as `@acme/shared-dto` onto in-repo source folders via
//! `tsconfig.base.json` `paths`. nestoa does not run `tsc`, so it resolves these
//! aliases itself: the `extends` chain is followed to the config that declares
//! `paths`, and each pattern is matched (including a single `*` wildcard) to a
//! file under the resolved `baseUrl`.

use std::path::{Path, PathBuf};

/// `(pattern, targets)` pairs straight from `compilerOptions.paths`.
type PathEntries = Vec<(String, Vec<String>)>;

/// A resolved set of path aliases plus the base directory their targets are
/// relative to.
#[derive(Debug, Default)]
pub struct TsPaths {
    /// Absolute directory that `paths` targets are resolved against.
    base_url: PathBuf,
    /// `(pattern, targets)` pairs straight from `compilerOptions.paths`.
    entries: PathEntries,
}

impl TsPaths {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Resolve a bare import specifier through the alias table to a file path.
    /// Only the best-matching pattern's targets are tried, as TypeScript does.
    pub fn resolve(&self, specifier: &str) -> Option<PathBuf> {
        let patterns = self.entries.iter().map(|(p, t)| (p.as_str(), t));
        let (targets, captured) = best_match(patterns, specifier, false)?;
        targets.iter().find_map(|target| {
            let rel = target.replace('*', &captured);
            crate::symbols::module_file(&self.base_url.join(rel.trim_start_matches("./")))
        })
    }
}

/// The pattern that matches `specifier` best, with its value and the text its
/// `*` captured: an exact pattern first, then the wildcard with the longest
/// prefix. TypeScript (`paths`, `typesVersions`) keeps the first of two equal
/// prefixes; Node (`exports`) prefers the longer pattern, so `./*.js` beats
/// `./*` — `longer_pattern_wins_ties` selects that.
pub(crate) fn best_match<'a, T>(
    patterns: impl Iterator<Item = (&'a str, T)>,
    specifier: &str,
    longer_pattern_wins_ties: bool,
) -> Option<(T, String)> {
    let mut best: Option<((usize, usize), T, String)> = None;
    for (pattern, value) in patterns {
        let Some(captured) = match_pattern(pattern, specifier) else {
            continue;
        };
        let rank = match pattern.find('*') {
            Some(star) => (
                star,
                if longer_pattern_wins_ties {
                    pattern.len()
                } else {
                    0
                },
            ),
            None => return Some((value, captured)),
        };
        if best.as_ref().is_none_or(|(r, ..)| rank > *r) {
            best = Some((rank, value, captured));
        }
    }
    best.map(|(_, value, captured)| (value, captured))
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
    let mut found = Inherited::default();
    let config_dir = start
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut chain = Chain {
        config_dir: config_dir.canonicalize().unwrap_or(config_dir),
        visited: std::collections::HashSet::new(),
    };
    collect(&start, explicit.is_some(), &mut chain, &mut found);
    let (paths_dir, entries) = found.paths?;
    Some(TsPaths {
        base_url: found.base_url.unwrap_or(paths_dir),
        entries,
    })
}

/// The options read from the most derived config in an `extends` chain that
/// declares them. `paths` resolve against `baseUrl` when one is set anywhere in
/// the chain, and otherwise against the directory of the config declaring them.
#[derive(Default)]
struct Inherited {
    paths: Option<(PathBuf, PathEntries)>,
    base_url: Option<PathBuf>,
}

/// The `extends` chain being walked from one tsconfig.
struct Chain {
    /// Directory of the config the chain starts from, which TypeScript 5.5
    /// substitutes for `${configDir}` in every config of the chain.
    config_dir: PathBuf,
    visited: std::collections::HashSet<PathBuf>,
}

impl Chain {
    fn expand(&self, value: &str) -> String {
        value.replace("${configDir}", &self.config_dir.to_string_lossy())
    }
}

/// Fill `found` from `path`, then from what it extends, most derived first: in
/// an `extends` array a later config overrides an earlier one. A missing file
/// is only reported when `required`, since most projects have no tsconfig at
/// the default location.
fn collect(path: &Path, required: bool, chain: &mut Chain, found: &mut Inherited) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !chain.visited.insert(path.clone()) {
        return;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return,
        Err(e) => {
            crate::diag::warn(format!("reading {}: {e}", path.display()));
            return;
        }
    };
    let json = match serde_json::from_str::<serde_json::Value>(&strip_jsonc(&raw)) {
        Ok(json) => json,
        Err(e) => {
            crate::diag::warn(format!("parsing {}: {e}", path.display()));
            return;
        }
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let co = json.get("compilerOptions");
    if found.base_url.is_none()
        && let Some(base) = co.and_then(|c| c.get("baseUrl")).and_then(|b| b.as_str())
    {
        let base_url = dir.join(chain.expand(base));
        found.base_url = Some(base_url.canonicalize().unwrap_or(base_url));
    }
    if found.paths.is_none()
        && let Some(paths) = co.and_then(|c| c.get("paths")).and_then(|p| p.as_object())
        && !paths.is_empty()
    {
        let entries = paths
            .iter()
            .map(|(k, v)| {
                let targets = v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(|t| chain.expand(t)))
                            .collect()
                    })
                    .unwrap_or_default();
                (k.clone(), targets)
            })
            .collect();
        found.paths = Some((dir.to_path_buf(), entries));
    }
    let parents: Vec<&str> = match json.get("extends") {
        Some(serde_json::Value::String(e)) => vec![e.as_str()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|e| e.as_str()).rev().collect(),
        _ => Vec::new(),
    };
    for parent in parents {
        match extends_path(dir, &chain.expand(parent)) {
            Some(parent) => collect(&parent, true, chain, found),
            None => crate::diag::warn(format!(
                "{}: cannot find the extended tsconfig {parent:?}",
                path.display()
            )),
        }
    }
}

/// The file an `extends` entry names: a path relative to the config, or a
/// package (`@tsconfig/node20`, `@tsconfig/node20/tsconfig.json`) found in a
/// `node_modules` above it.
fn extends_path(dir: &Path, entry: &str) -> Option<PathBuf> {
    let with_json = |p: PathBuf| {
        if p.is_file() || p.extension().is_some_and(|e| e == "json") {
            p
        } else {
            let mut j = p.into_os_string();
            j.push(".json");
            PathBuf::from(j)
        }
    };
    if entry.starts_with('.') || Path::new(entry).is_absolute() {
        return Some(with_json(dir.join(entry)));
    }
    let (pkg, subpath) = crate::symbols::split_package_specifier(entry);
    let pkg_dir = crate::symbols::find_package_dir(dir, &pkg)?;
    if !subpath.is_empty() {
        return Some(with_json(pkg_dir.join(subpath)));
    }
    let declared = std::fs::read_to_string(pkg_dir.join("package.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|json| json.get("tsconfig")?.as_str().map(str::to_string));
    Some(pkg_dir.join(declared.as_deref().unwrap_or("tsconfig.json")))
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
        assert_eq!(
            match_pattern("@acme/core", "@acme/core"),
            Some(String::new())
        );
        assert_eq!(match_pattern("@acme/core", "@acme/other"), None);
        assert_eq!(
            match_pattern("@acme/raw/*", "@acme/raw/books"),
            Some("books".to_string())
        );
        assert_eq!(match_pattern("@acme/raw/*", "@acme/other/x"), None);
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

    /// Write `files` under a fresh temp dir and return it.
    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nestoa-tsc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    #[test]
    fn the_longest_matching_prefix_wins() {
        let dir = tree(
            "prefix",
            &[
                ("libs/any/x.ts", ""),
                ("libs/core/x.ts", ""),
                ("libs/exact.ts", ""),
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "paths": {
                        "@acme/*": ["libs/any/*"],
                        "@acme/core/*": ["libs/core/*"],
                        "@acme/core/exact": ["libs/exact.ts"]
                    } } }"#,
                ),
            ],
        );
        let paths = load_tsconfig_paths(&dir, None).unwrap();
        assert_eq!(
            paths.resolve("@acme/core/x"),
            Some(dir.join("libs/core/x.ts"))
        );
        assert_eq!(paths.resolve("@acme/x"), Some(dir.join("libs/any/x.ts")));
        assert_eq!(
            paths.resolve("@acme/core/exact"),
            Some(dir.join("libs/exact.ts"))
        );
        // The best pattern's targets are the only ones tried.
        assert_eq!(paths.resolve("@acme/core/missing"), None);
    }

    #[test]
    fn extends_arrays_packages_and_an_inherited_base_url() {
        let dir = tree(
            "extends",
            &[
                ("src/lib/index.ts", ""),
                ("other/lib/index.ts", ""),
                (
                    "node_modules/@tsconfig/base/package.json",
                    r#"{ "name": "@tsconfig/base", "tsconfig": "base.json" }"#,
                ),
                (
                    "node_modules/@tsconfig/base/base.json",
                    r#"{ "compilerOptions": { "strict": true } }"#,
                ),
                (
                    "first.json",
                    r#"{ "compilerOptions": { "baseUrl": "other" } }"#,
                ),
                (
                    "second.json",
                    r#"{ "compilerOptions": { "baseUrl": "src" } }"#,
                ),
                (
                    "svc/tsconfig.json",
                    r#"{
                        "extends": ["@tsconfig/base", "../first.json", "../second"],
                        "compilerOptions": { "paths": { "@lib": ["lib"] } }
                    }"#,
                ),
            ],
        );
        let paths = load_tsconfig_paths(&dir.join("svc"), None).unwrap();
        // `paths` resolve against the `baseUrl` of the last config extended.
        assert_eq!(paths.resolve("@lib"), Some(dir.join("src/lib/index.ts")));
    }

    #[test]
    fn paths_without_a_base_url_resolve_from_their_config() {
        let dir = tree(
            "nobase",
            &[
                ("base/lib.ts", ""),
                (
                    "base/tsconfig.json",
                    r#"{ "compilerOptions": { "paths": { "@lib": ["./lib"] } } }"#,
                ),
                (
                    "svc/tsconfig.json",
                    r#"{ "extends": "../base/tsconfig.json" }"#,
                ),
            ],
        );
        let paths = load_tsconfig_paths(&dir.join("svc"), None).unwrap();
        assert_eq!(paths.resolve("@lib"), Some(dir.join("base/lib.ts")));
    }

    #[test]
    fn resolves_alias_chain_via_extends() {
        let dir = std::env::temp_dir().join(format!("nestoa-tsc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("svc")).unwrap();
        std::fs::create_dir_all(dir.join("libs/core/src")).unwrap();
        std::fs::write(
            dir.join("libs/core/src/index.ts"),
            "export interface Core { a: string; }",
        )
        .unwrap();
        std::fs::write(
            dir.join("tsconfig.base.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@acme/core": ["libs/core/src/index.ts"] } } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("svc/tsconfig.json"),
            r#"{ "extends": "../tsconfig.base.json", "compilerOptions": { "strict": true } }"#,
        )
        .unwrap();

        let paths = load_tsconfig_paths(&dir.join("svc"), None).expect("load");
        assert_eq!(paths.len(), 1);
        let resolved = paths.resolve("@acme/core").expect("resolve alias");
        assert!(
            resolved.ends_with("libs/core/src/index.ts"),
            "got {:?}",
            resolved
        );
        assert!(paths.resolve("@acme/missing").is_none());
    }

    #[test]
    fn config_dir_is_the_directory_of_the_starting_config() {
        let dir = tree(
            "config-dir",
            &[
                ("svc/src/dto/index.ts", ""),
                (
                    "tsconfig.base.json",
                    r#"{ "compilerOptions": { "paths": { "@dto": ["${configDir}/src/dto"] } } }"#,
                ),
                (
                    "svc/tsconfig.json",
                    r#"{ "extends": "../tsconfig.base.json" }"#,
                ),
            ],
        );
        let paths = load_tsconfig_paths(&dir.join("svc"), None).unwrap();
        assert_eq!(
            paths.resolve("@dto"),
            Some(dir.join("svc/src/dto/index.ts"))
        );
    }

    #[test]
    fn a_missing_explicit_tsconfig_or_extends_target_warns() {
        crate::diag::take_warnings();
        let dir = tree(
            "missing",
            &[("tsconfig.json", r#"{ "extends": "./nope.json" }"#)],
        );
        assert!(load_tsconfig_paths(&dir, None).is_none());
        let warnings = crate::diag::take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("nope.json"), "{warnings:?}");

        let complete = tree(
            "missing-after-paths",
            &[(
                "tsconfig.json",
                r#"{ "extends": "./nope.json", "compilerOptions": { "baseUrl": ".", "paths": { "a": ["b"] } } }"#,
            )],
        );
        assert!(load_tsconfig_paths(&complete, None).is_some());
        let warnings = crate::diag::take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("nope.json"), "{warnings:?}");

        assert!(load_tsconfig_paths(&dir, Some(Path::new("absent.json"))).is_none());
        let warnings = crate::diag::take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("absent.json"), "{warnings:?}");

        let empty = tree("no-tsconfig", &[("src/app.ts", "")]);
        assert!(load_tsconfig_paths(&empty, None).is_none());
        assert_eq!(crate::diag::take_warnings(), Vec::<String>::new());
    }
}
