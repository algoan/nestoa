use std::path::{Path, PathBuf};

use anyhow::Result;
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
        if start.is_file() {
            push_if_ts(&start, root, &exclude, &mut out);
            continue;
        }
        for entry in walkdir::WalkDir::new(&start)
            .into_iter()
            .filter_entry(|e| !(e.file_type().is_dir() && is_match(&excluded_dirs, e.path(), root)))
            .filter_map(|e| e.inspect_err(|e| crate::diag::warn(e)).ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            push_if_ts(entry.path(), root, &exclude, &mut out);
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
        out.push(path.to_path_buf());
    }
}

/// Whether `path`, relative to `root` or as is, matches `globs`.
fn is_match(globs: &globset::GlobSet, path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| globs.is_match(rel)) || globs.is_match(path)
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
