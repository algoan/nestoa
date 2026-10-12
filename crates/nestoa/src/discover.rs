use std::path::{Path, PathBuf};

use anyhow::Result;
use globset::{Glob, GlobSetBuilder};

pub fn discover_files(
    root: &Path,
    includes: &[String],
    excludes: &[String],
) -> Result<Vec<PathBuf>> {
    let mut exclude = GlobSetBuilder::new();
    for pat in excludes {
        exclude.add(Glob::new(pat)?);
    }
    let exclude = exclude.build()?;

    let mut out = Vec::new();
    for inc in includes {
        let start = root.join(inc);
        if !start.exists() {
            continue;
        }
        if start.is_file() {
            push_if_ts(&start, root, &exclude, &mut out);
            continue;
        }
        for entry in walkdir::WalkDir::new(&start)
            .into_iter()
            .filter_map(|e| e.ok())
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
    if let Ok(rel) = path.strip_prefix(root)
        && exclude.is_match(rel)
    {
        return;
    }
    if exclude.is_match(path) {
        return;
    }
    out.push(path.to_path_buf());
}
