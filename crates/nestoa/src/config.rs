use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// Directories or files to scan. Defaults to ["src"].
    #[serde(default = "default_include")]
    pub include: Vec<String>,

    /// Glob patterns to exclude.
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,

    /// Output file path for the generated OpenAPI document.
    #[serde(default = "default_output")]
    pub output: PathBuf,

    /// Output format. "json" or "yaml". Inferred from the output extension if omitted.
    #[serde(default)]
    pub format: Option<OutputFormat>,

    /// Global API base path prepended to every route.
    #[serde(default)]
    pub base_path: Option<String>,

    /// Auto-detect `app.setGlobalPrefix(...)` from the bootstrap file (e.g.
    /// `src/main.ts`) and use it as the base path when `basePath` is not set.
    /// Defaults to true.
    #[serde(default = "default_true")]
    pub detect_global_prefix: bool,

    /// OpenAPI document metadata.
    #[serde(default)]
    pub info: Info,

    /// Extra decorator-name aliases mapped to known handlers. Lets users wire in
    /// custom decorators without modifying the tool. For example, mapping a
    /// project-specific `@MyAuthUser` to the `user` parameter kind tells the
    /// tool to drop that parameter from the route signature.
    #[serde(default)]
    pub decorator_aliases: indexmap::IndexMap<String, DecoratorAlias>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Json,
    Yaml,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DecoratorAlias {
    /// Treat as @Body
    Body,
    /// Treat as @Query
    Query,
    /// Treat as @Param
    Param,
    /// Treat as @Headers
    Header,
    /// Synthesised "request user" parameter — dropped from the route signature.
    User,
    /// Drop from the route signature entirely.
    Ignore,
}

fn default_include() -> Vec<String> {
    vec!["src".into()]
}

fn default_exclude() -> Vec<String> {
    vec![
        "**/node_modules/**".into(),
        "**/dist/**".into(),
        "**/*.spec.ts".into(),
        "**/*.test.ts".into(),
        "**/*.e2e-spec.ts".into(),
    ]
}

fn default_output() -> PathBuf {
    PathBuf::from("openapi.json")
}

fn default_title() -> String {
    "API".into()
}

fn default_version() -> String {
    "0.0.0".into()
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include: default_include(),
            exclude: default_exclude(),
            output: default_output(),
            format: None,
            base_path: None,
            detect_global_prefix: true,
            info: Info {
                title: default_title(),
                version: default_version(),
                description: None,
            },
            decorator_aliases: indexmap::IndexMap::new(),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        let candidates: Vec<PathBuf> = if let Some(p) = explicit {
            vec![cwd.join(p)]
        } else {
            ["nestoa.config.json", "nestoa.config.yaml", "nestoa.config.yml"]
                .iter()
                .map(|n| cwd.join(n))
                .collect()
        };
        for path in candidates {
            if path.is_file() {
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let cfg: Config = if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?
                } else {
                    serde_yaml::from_str(&raw)
                        .with_context(|| format!("parsing {}", path.display()))?
                };
                return Ok(cfg);
            }
        }
        Ok(Config::default())
    }

    pub fn output_format(&self) -> OutputFormat {
        if let Some(f) = self.format {
            return f;
        }
        match self
            .output
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("yaml") | Some("yml") => OutputFormat::Yaml,
            _ => OutputFormat::Json,
        }
    }
}
