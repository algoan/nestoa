use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};

pub fn discover_files(
    root: &Path,
    includes: &[String],
    excludes: &[String],
) -> Result<Vec<PathBuf>> {
    let mut exclude = GlobSetBuilder::new();
    // `dir/**` excludes everything under a directory matching `dir`, so the
    // walk skips those directories instead of listing node_modules or dist.
    let mut excluded_dirs = GlobSetBuilder::new();
    for pat in excludes {
        exclude.add(Glob::new(pat)?);
        if let Some(dir) = pat.strip_suffix("/**") {
            excluded_dirs.add(Glob::new(dir)?);
        }
    }
    let exclude = exclude.build()?;
    let excluded_dirs = excluded_dirs.build()?;

    let root = &root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let mut out = Vec::new();
    for inc in includes {
        let start = root.join(inc);
        if !start.exists() {
            if inc.contains(['*', '?', '[', '{']) {
                crate::diag::warn(format!(
                    "include entry {inc:?} is a glob; include takes paths, use exclude to filter"
                ));
            } else {
                crate::diag::warn(format!("include path {} does not exist", start.display()));
            }
            continue;
        }
        let start = start
            .canonicalize()
            .with_context(|| format!("resolving {}", start.display()))?;
        // Globs match paths relative to the project, so a `dist` or
        // `node_modules` above it never excludes it; an include outside the
        // project (`../libs/dto`) is matched relative to itself.
        let base = if start.starts_with(root) {
            root.to_path_buf()
        } else if start.is_file() {
            start.parent().unwrap_or(&start).to_path_buf()
        } else {
            start.clone()
        };
        if start.is_file() {
            push_if_ts(&start, &base, &exclude, &mut out);
            continue;
        }
        for entry in walkdir::WalkDir::new(&start)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                !(e.file_type().is_dir() && is_match(&excluded_dirs, e.path(), &base))
            })
            .filter_map(|e| e.inspect_err(|e| crate::diag::warn(e)).ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            push_if_ts(entry.path(), &base, &exclude, &mut out);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn push_if_ts(path: &Path, root: &Path, exclude: &globset::GlobSet, out: &mut Vec<PathBuf>) {
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return;
    };
    if !matches!(ext, "ts" | "tsx" | "mts" | "cts") {
        return;
    }
    if !is_match(exclude, path, root) {
        // The resolver keys modules by canonical path; a symlinked file is
        // kept under its target.
        out.push(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    }
}

/// Whether `path`, relative to `base`, matches `globs`.
fn is_match(globs: &globset::GlobSet, path: &Path, base: &Path) -> bool {
    globs.is_match(path.strip_prefix(base).unwrap_or(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_directories_are_skipped() {
        let root = std::env::temp_dir().join(format!("nestoa-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for rel in [
            "src/app.ts",
            "src/node_modules/pkg/index.d.ts",
            "src/feature/dist/out.ts",
            "src/feature/distance.ts",
            "src/feature/feature.spec.ts",
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let root = root.canonicalize().unwrap();
        let excludes = ["**/node_modules/**", "**/dist/**", "**/*.spec.ts"].map(String::from);
        let found = discover_files(&root, &["src".to_string()], &excludes).unwrap();
        let rel: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap())
            .collect();
        assert_eq!(
            rel,
            [
                Path::new("src/app.ts"),
                Path::new("src/feature/distance.ts")
            ]
        );
    }

    #[test]
    fn paths_are_canonical_and_matched_below_the_project() {
        let top = std::env::temp_dir().join(format!("nestoa-discover-dist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        for rel in [
            "dist/svc/src/app.ts",
            "dist/svc/src/app.spec.ts",
            "dist/libs/dto/user.ts",
            "dist/shared/linked.ts",
        ] {
            let path = top.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let top = top.canonicalize().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(top.join("dist/shared"), top.join("dist/svc/src/shared"))
            .unwrap();
        let excludes = ["**/dist/**", "**/*.spec.ts"].map(String::from);
        let found = discover_files(
            &top.join("dist/svc"),
            &["src".into(), "../libs/dto".into()],
            &excludes,
        )
        .unwrap();
        let mut expected = vec![
            top.join("dist/libs/dto/user.ts"),
            top.join("dist/svc/src/app.ts"),
        ];
        if cfg!(unix) {
            expected.insert(1, top.join("dist/shared/linked.ts"));
        }
        assert_eq!(found, expected);
    }

    #[test]
    fn missing_and_glob_include_entries_warn() {
        crate::diag::take_warnings();
        let root = std::env::temp_dir();
        let found = discover_files(&root, &["nestoa-absent".into(), "src/**".into()], &[]).unwrap();
        assert!(found.is_empty());
        let warnings = crate::diag::take_warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("does not exist"), "{warnings:?}");
        assert!(warnings[1].contains("is a glob"), "{warnings:?}");
    }
}
