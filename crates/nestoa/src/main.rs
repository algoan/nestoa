mod config;
mod constval;
mod diag;
mod dialect;
mod discover;
mod errors;
mod globalprefix;
mod interceptors;
mod jsdoc;
mod parse;
mod routes;
mod schema;
mod symbols;
mod tsconfig;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{Config, OpenApiVersion, OutputFormat};
use crate::parse::ParsingContext;
use crate::routes::{Extractor, to_openapi};
use crate::schema::ComponentSchemas;
use crate::symbols::{Project, package_name};

#[derive(Parser, Debug)]
#[command(
    name = "nestoa",
    version,
    about = "Static OpenAPI generator for NestJS / TypeScript"
)]
struct Cli {
    /// Project root to analyse (defaults to the current directory).
    #[arg(default_value = ".")]
    project: PathBuf,

    /// Path to a config file, relative to the current directory (defaults to
    /// nestoa.config.{json,yaml,yml} in the project root).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override the output file path, relative to the current directory.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Override the OpenAPI version to emit.
    #[arg(long, value_enum)]
    openapi_version: Option<OpenApiVersion>,

    /// Print the resolved configuration and exit.
    #[arg(long)]
    print_config: bool,

    /// Exit with an error after writing the output if there was any warning or
    /// any `x-unresolved` type, listing where each unresolved type is.
    #[arg(long)]
    strict: bool,
}

/// Compile the `resolvePackages` glob patterns into a matcher over package names.
fn build_package_allowlist(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat)?);
    }
    Ok(builder.build()?)
}

/// Transitively parse the modules discovered files import from outside
/// `include` — relative files, tsconfig-aliased libs, and the declaration files
/// of allowlisted node_modules packages — into the symbol table, so their
/// exported types resolve to real schemas. Routes still come only from the
/// discovered files.
fn expand_packages(project: &mut Project, parser: &ParsingContext, allow: &GlobSet) {
    let mut worklist: Vec<PathBuf> = project.files.keys().cloned().collect();
    while let Some(file) = worklist.pop() {
        let Some(fs) = project.symbols.get(&file) else {
            continue;
        };
        let mut specs: Vec<String> = Vec::new();
        specs.extend(fs.imports.values().map(|b| b.specifier.clone()));
        specs.extend(fs.re_exports.values().map(|b| b.specifier.clone()));
        specs.extend(fs.star_re_exports.iter().cloned());
        specs.extend(fs.namespace_re_exports.values().cloned());

        for spec in specs {
            let target = if spec.starts_with('.') {
                // Follow relative re-exports so a pulled-in package/lib entry
                // file drags in the modules it re-exports. Re-discovering an
                // already-parsed project file is a no-op (guarded below).
                project.resolver.relative(&file, &spec)
            } else {
                // Workspace alias (tsconfig paths) wins over a node_modules copy;
                // fall back to an allowlisted package's declarations.
                project.resolve_alias(&spec).or_else(|| {
                    if allow.is_match(package_name(&spec)) {
                        project.resolver.package_types(&file, &spec)
                    } else {
                        None
                    }
                })
            };
            let Some(target) = target else { continue };
            if project.files.contains_key(&target) {
                continue;
            }
            match parser.parse_file(&target) {
                Ok(parsed) => {
                    project.add(parsed);
                    worklist.push(target);
                }
                Err(e) => diag::warn(format!("{e:#}; skipping the file")),
            }
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let project_root = cli
        .project
        .canonicalize()
        .with_context(|| format!("resolving project path {}", cli.project.display()))?;
    let mut cfg = Config::load(cli.config.as_deref(), &project_root)?;
    // Paths given on the command line are relative to where nestoa runs; only
    // the config's own `output` is relative to the project.
    if let Some(o) = cli.output {
        cfg.output = std::path::absolute(&o)
            .with_context(|| format!("resolving output path {}", o.display()))?;
    }
    if let Some(v) = cli.openapi_version {
        cfg.openapi_version = v;
    }

    if cli.print_config {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }

    let files = discover::discover_files(&project_root, &cfg.include, &cfg.exclude)?;
    if files.is_empty() {
        anyhow::bail!(
            "no .ts files found in {} matching include patterns: {:?}",
            project_root.display(),
            cfg.include
        );
    }

    let parser = ParsingContext::default();
    let mut project = Project::default();

    for file in &files {
        let parsed = match parser.parse_file(file) {
            Ok(p) => p,
            Err(e) => {
                diag::warn(format!("{e:#}; skipping the file"));
                continue;
            }
        };
        project.add(parsed);
    }

    // Load tsconfig `paths` aliases so workspace package imports (e.g.
    // `@acme/core` -> `libs/core/src`) resolve to in-repo source.
    if cfg.resolve_tsconfig_paths {
        project.tsconfig_paths =
            tsconfig::load_tsconfig_paths(&project_root, cfg.tsconfig.as_deref());
        if let Some(p) = &project.tsconfig_paths {
            eprintln!("nestoa: loaded {} tsconfig path alias(es)", p.len());
        }
    }

    // Pull in the modules the project imports from outside `include`:
    // relative files, tsconfig-aliased workspace libs, and allowlisted
    // node_modules packages, so their exported types resolve to real schemas
    // instead of `x-unresolved`.
    let allow = build_package_allowlist(&cfg.resolve_packages).unwrap_or_else(|e| {
        diag::warn(format!("invalid resolvePackages pattern: {e}"));
        GlobSet::empty()
    });
    expand_packages(&mut project, &parser, &allow);

    // Recover the NestJS global prefix from the bootstrap file (e.g.
    // `app.setGlobalPrefix(...)` in main.ts) unless the user opted out. An
    // explicit `basePath` wins over the detected prefix and its `exclude`.
    // Versioning is always read; `detectGlobalPrefix` only governs the prefix.
    let with_prefix = cfg.base_path.is_none() && cfg.detect_global_prefix;
    let mut bootstrap = globalprefix::detect(&project, &files, &project_root, with_prefix);
    if let Some(prefix) = bootstrap.prefix.take() {
        eprintln!("nestoa: detected global prefix \"{prefix}\" from bootstrap");
        cfg.base_path = Some(prefix);
    }

    let mut components = ComponentSchemas::default();
    let routes = {
        let mut extractor = Extractor {
            config: &cfg,
            bootstrap: &bootstrap,
            project: &project,
            entry_files: &files,
            components: &mut components,
        };
        extractor.extract_all()
    };

    errors::warn_unmatched(
        &cfg.error_responses,
        routes
            .iter()
            .map(|r| (r.method.as_lower(), r.path.as_str())),
    );
    if routes.is_empty() {
        diag::warn("no @Controller-decorated classes with HTTP routes were found");
    }

    let doc = to_openapi(&cfg, &routes, components);
    // Resolve a relative output path against the analysed project dir (where the
    // config was found), not the process CWD — so `nestoa services/foo` writes
    // `services/foo/openapi.yaml` regardless of where it is invoked from.
    let output_path = if cfg.output.is_relative() {
        project_root.join(&cfg.output)
    } else {
        cfg.output.clone()
    };
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let body = match cfg.output_format() {
        OutputFormat::Json => serde_json::to_string_pretty(&doc)?,
        OutputFormat::Yaml => serde_yaml::to_string(&doc)?,
    };
    std::fs::write(&output_path, body)
        .with_context(|| format!("writing {}", output_path.display()))?;
    let schemas = doc
        .get("components")
        .and_then(|c| c.get("schemas"))
        .and_then(|s| s.as_object().map(|o| o.len()))
        .unwrap_or(0);
    let unresolved = diag::unresolved(&doc);
    eprintln!(
        "nestoa: wrote {} ({}, {}{})",
        output_path.display(),
        plural(routes.len(), "route"),
        plural(schemas, "schema"),
        if unresolved.is_empty() {
            String::new()
        } else {
            format!(", {} unresolved", unresolved.len())
        },
    );

    if cli.strict {
        for (pointer, name) in &unresolved {
            eprintln!("unresolved: {name} at {pointer}");
        }
        let warnings = diag::warning_count();
        if warnings > 0 || !unresolved.is_empty() {
            anyhow::bail!(
                "--strict: {} and {}",
                plural(warnings, "warning"),
                plural(unresolved.len(), "unresolved type")
            );
        }
    }
    Ok(())
}

fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_imports_outside_include_are_parsed_without_any_allowlist() {
        let dir = std::env::temp_dir().join(format!("nestoa-expand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("svc/src")).unwrap();
        std::fs::create_dir_all(dir.join("libs/dto")).unwrap();
        std::fs::write(
            dir.join("svc/src/app.ts"),
            "import { User } from '../../libs/dto';",
        )
        .unwrap();
        std::fs::write(
            dir.join("libs/dto/index.ts"),
            "export class User { id: string; }",
        )
        .unwrap();
        let dir = dir.canonicalize().unwrap();
        let app = dir.join("svc/src/app.ts");
        let parser = ParsingContext::default();
        let mut project = Project::default();
        project.add(parser.parse_file(&app).unwrap());

        expand_packages(&mut project, &parser, &GlobSet::empty());
        let (file, _) = project.resolve(&app, "User").expect("User resolves");
        assert_eq!(file, dir.join("libs/dto/index.ts"));
    }
}
