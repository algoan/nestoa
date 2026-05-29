mod config;
mod discover;
mod jsdoc;
mod parse;
mod routes;
mod schema;
mod symbols;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use swc_common::BytePos;

use crate::config::{Config, OutputFormat};
use crate::parse::ParsingContext;
use crate::routes::{to_openapi, Extractor};
use crate::schema::ComponentSchemas;
use crate::symbols::Project;

#[derive(Parser, Debug)]
#[command(name = "nestoa", version, about = "Static OpenAPI generator for NestJS / TypeScript")]
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
        if let Some(fm) = parser.source_map.get_source_file(&swc_common::FileName::Real(
            file.clone(),
        )) {
            file_starts.insert(file.clone(), fm.start_pos);
        }
        project.add(parsed);
    }

    let mut components = ComponentSchemas::default();
    let routes = {
        let mut extractor = Extractor {
            config: &cfg,
            project: &project,
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
    let output_path = cfg.output.clone();
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
