mod config;
mod discover;
mod globalprefix;
mod interceptors;
mod jsdoc;
mod parse;
mod routes;
mod schema;
mod symbols;
mod tsconfig;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use globset::{Glob, GlobSet, GlobSetBuilder};
use swc_common::BytePos;

use crate::config::{Config, OutputFormat};
use crate::parse::ParsingContext;
use crate::routes::{Extractor, to_openapi};
use crate::schema::ComponentSchemas;
use crate::symbols::{Project, package_name, resolve_module_path, resolve_package_types};

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

    /// Path to a config file (defaults to nestoa.config.{json,yaml,yml} in the project root).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override the output file path.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Print the resolved configuration and exit.
    #[arg(long)]
    print_config: bool,
}

/// Compile the `resolvePackages` glob patterns into a matcher over package names.
fn build_package_allowlist(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat)?);
    }
    Ok(builder.build()?)
}

/// Transitively parse the declaration files of allowlisted node_modules packages
/// (and the relative files they re-export) into the symbol table, so their
/// exported types resolve to real schemas. Only ever adds files under
/// `node_modules`; the project's own sources come solely from discovery.
fn expand_packages(
    project: &mut Project,
    parser: &ParsingContext,
    sources: &mut HashMap<PathBuf, String>,
    file_starts: &mut HashMap<PathBuf, BytePos>,
    allow: &GlobSet,
) {
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
                resolve_module_path(&file, &spec)
            } else {
                // Workspace alias (tsconfig paths) wins over a node_modules copy;
                // fall back to an allowlisted package's declarations.
                project.resolve_alias(&spec).or_else(|| {
                    if allow.is_match(package_name(&spec)) {
                        resolve_package_types(&file, &spec)
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
                    if let Ok(s) = std::fs::read_to_string(&target) {
                        sources.insert(target.clone(), s);
                    }
                    if let Some(fm) = parser
                        .source_map
                        .get_source_file(&swc_common::FileName::Real(target.clone()))
                    {
                        file_starts.insert(target.clone(), fm.start_pos);
                    }
                    project.add(parsed);
                    worklist.push(target);
                }
                Err(e) => eprintln!("warning: {}", e),
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
    if let Some(o) = cli.output {
        cfg.output = o;
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
    let mut sources: HashMap<PathBuf, String> = HashMap::new();
    let mut file_starts: HashMap<PathBuf, BytePos> = HashMap::new();

    for file in &files {
        let parsed = match parser.parse_file(file) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("warning: {}", e);
                continue;
            }
        };
        // Capture source text + start position for JSDoc lookups before storing parsed.
        if let Ok(s) = std::fs::read_to_string(file) {
            sources.insert(file.clone(), s);
        }
        if let Some(fm) = parser
            .source_map
            .get_source_file(&swc_common::FileName::Real(file.clone()))
        {
            file_starts.insert(file.clone(), fm.start_pos);
        }
        project.add(parsed);
    }

    // Load tsconfig `paths` aliases so workspace package imports (e.g.
    // `@algoan/core` -> `libs/core/src`) resolve to in-repo source.
    if cfg.resolve_tsconfig_paths {
        project.tsconfig_paths =
            tsconfig::load_tsconfig_paths(&project_root, cfg.tsconfig.as_deref());
        if let Some(p) = &project.tsconfig_paths {
            eprintln!("nestoa: loaded {} tsconfig path alias(es)", p.len());
        }
    }

    // Pull in TypeScript declarations from allowlisted node_modules packages and
    // tsconfig-aliased workspace libs so their exported types resolve to real
    // schemas instead of `x-unresolved`.
    let has_aliases = project
        .tsconfig_paths
        .as_ref()
        .is_some_and(|p| p.has_entries());
    if !cfg.resolve_packages.is_empty() || has_aliases {
        match build_package_allowlist(&cfg.resolve_packages) {
            Ok(allow) => expand_packages(
                &mut project,
                &parser,
                &mut sources,
                &mut file_starts,
                &allow,
            ),
            Err(e) => eprintln!("warning: invalid resolvePackages pattern: {}", e),
        }
    }

    // Recover the NestJS global prefix from the bootstrap file (e.g.
    // `app.setGlobalPrefix(...)` in main.ts) unless the user pinned `basePath`
    // explicitly or opted out.
    if cfg.base_path.is_none() && cfg.detect_global_prefix {
        if let Some(prefix) = globalprefix::detect_global_prefix(&project, &project_root) {
            eprintln!(
                "nestoa: detected global prefix \"{}\" from bootstrap",
                prefix
            );
            cfg.base_path = Some(prefix);
        }
    }

    let mut components = ComponentSchemas::default();
    let routes = {
        let mut extractor = Extractor {
            config: &cfg,
            project: &project,
            entry_files: &files,
            sources: &sources,
            file_starts: &file_starts,
            components: &mut components,
        };
        extractor.extract_all()
    };

    if routes.is_empty() {
        eprintln!("warning: no @Controller-decorated classes with HTTP routes were found");
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
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }

    let body = match cfg.output_format() {
        OutputFormat::Json => serde_json::to_string_pretty(&doc)?,
        OutputFormat::Yaml => serde_yaml::to_string(&doc)?,
    };
    std::fs::write(&output_path, body)
        .with_context(|| format!("writing {}", output_path.display()))?;
    eprintln!(
        "nestoa: wrote {} ({} route{}, {} schema{})",
        output_path.display(),
        routes.len(),
        if routes.len() == 1 { "" } else { "s" },
        doc.get("components")
            .and_then(|c| c.get("schemas"))
            .and_then(|s| s.as_object().map(|o| o.len()))
            .unwrap_or(0),
        if doc
            .get("components")
            .and_then(|c| c.get("schemas"))
            .and_then(|s| s.as_object().map(|o| o.len() == 1))
            .unwrap_or(false)
        {
            ""
        } else {
            "s"
        },
    );
    Ok(())
}
