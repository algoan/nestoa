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

    /// Resolve TypeScript `compilerOptions.paths` aliases (e.g. workspace
    /// packages like `@algoan/core` that map to in-repo `libs/*/src`). Reads the
    /// project's `tsconfig.json`, following `extends`. Defaults to true.
    #[serde(default = "default_true")]
    pub resolve_tsconfig_paths: bool,

    /// Explicit tsconfig file to read `paths` from (relative to the project root
    /// or absolute). Defaults to `tsconfig.json` in the project root.
    #[serde(default)]
    pub tsconfig: Option<PathBuf>,

    /// node_modules package names (glob patterns) whose TypeScript declarations
    /// should be parsed so their exported types resolve to real schemas instead
    /// of `x-unresolved`. For example `["@algoan/*"]` pulls in the typings of
    /// every `@algoan` package referenced by a route. Empty by default, so no
    /// third-party packages are inlined unless explicitly opted in.
    #[serde(default)]
    pub resolve_packages: Vec<String>,

    /// Extra decorator-name aliases mapped to known handlers. Lets users wire in
    /// custom decorators without modifying the tool. For example, mapping a
    /// project-specific `@MyAuthUser` to the `user` parameter kind tells the
    /// tool to drop that parameter from the route signature.
    #[serde(default)]
    pub decorator_aliases: indexmap::IndexMap<String, DecoratorAlias>,

    /// Per-interceptor handling, keyed by the interceptor class name as written
    /// in `@UseInterceptors(...)`. The response shape is normally inferred from
    /// the interceptor's `NestInterceptor<In, Out>` type arguments; this map
    /// lets users silence pure side-effect interceptors (`ignore`) and declare
    /// request-body reshaping that the type signature cannot express.
    #[serde(default)]
    pub interceptors: indexmap::IndexMap<String, InterceptorConfig>,
}

/// Per-interceptor handling. See `Config::interceptors`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InterceptorConfig {
    /// Pure side-effect interceptor (logging/tracing/scrubbing) that never
    /// touches the wire shape — skip it entirely.
    #[serde(default)]
    pub ignore: bool,
    /// Request-body reshaping not expressed in `NestInterceptor<In, Out>`.
    #[serde(default)]
    pub request_body: Option<RequestBodyTransform>,
    /// Response-body override for interceptors whose wire shape cannot be
    /// recovered from `NestInterceptor<In, Out>` — typically because the
    /// transform is runtime control flow (e.g. signing the body only when the
    /// request path ends in `/jws`). The supplied schema replaces the route's
    /// response, optionally gated on a path suffix so sibling routes that share
    /// the same interceptor (array-valued `@Get([':id', ':id/jws'])`) keep their
    /// untransformed shape.
    #[serde(default)]
    pub response: Option<ResponseOverride>,
}

/// A response-schema override applied by an interceptor. See `InterceptorConfig::response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseOverride {
    /// Only apply the override to routes whose path ends with this suffix (e.g.
    /// `/jws`). When omitted, the override applies to every route carrying the
    /// interceptor.
    #[serde(default)]
    pub when_path_ends_with: Option<String>,
    /// The OpenAPI schema emitted as the route's response body. Inlined verbatim.
    pub schema: serde_json::Value,
}

/// How an interceptor reshapes the incoming request body.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RequestBodyTransform {
    /// The client may send a single object or an array of them (the handler
    /// sees an array). Emitted as `oneOf: [S, { type: array, items: S }]`.
    SingleOrArray,
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
            resolve_tsconfig_paths: true,
            tsconfig: None,
            resolve_packages: Vec::new(),
            info: Info {
                title: default_title(),
                version: default_version(),
                description: None,
            },
            decorator_aliases: indexmap::IndexMap::new(),
            interceptors: indexmap::IndexMap::new(),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        let candidates: Vec<PathBuf> = if let Some(p) = explicit {
            vec![cwd.join(p)]
        } else {
            [
                "nestoa.config.json",
                "nestoa.config.yaml",
                "nestoa.config.yml",
            ]
            .iter()
            .map(|n| cwd.join(n))
            .collect()
        };
        for path in candidates {
            if path.is_file() {
                let value = load_value(&path)?;
                let cfg: Config = serde_json::from_value(value)
                    .with_context(|| format!("parsing {}", path.display()))?;
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

/// Read a config file into a JSON value, resolving an optional `extends` by
/// deep-merging this config on top of the base it extends. `extends` is a path
/// relative to the config file's own directory and may be chained (a base may
/// itself extend another). The `extends` key is stripped from the result so it
/// never reaches `Config`, which denies unknown fields. JSON and YAML configs
/// are both parsed through `serde_json::Value`, giving one merge medium.
fn load_value(path: &Path) -> Result<serde_json::Value> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: serde_json::Value = if path.extension().and_then(|s| s.to_str()) == Some("json")
    {
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
    } else {
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
    };

    let extends = value.as_object_mut().and_then(|map| map.remove("extends"));
    if let Some(extends) = extends {
        let rel = extends
            .as_str()
            .with_context(|| format!("\"extends\" in {} must be a string path", path.display()))?;
        let base_path = path.parent().unwrap_or_else(|| Path::new(".")).join(rel);
        let mut base = load_value(&base_path)
            .with_context(|| format!("resolving \"extends\" from {}", path.display()))?;
        merge_value(&mut base, value);
        return Ok(base);
    }
    Ok(value)
}

/// Deep-merge `over` onto `base`: object keys merge recursively; arrays and
/// scalars from `over` replace their counterpart in `base` wholesale (so
/// list-valued keys like `resolvePackages`/`include` override cleanly).
fn merge_value(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.get_mut(&k) {
                    Some(existing) => merge_value(existing, v),
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
        }
        (base_slot, over) => *base_slot = over,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interceptors_config() {
        let cfg: Config = serde_json::from_str(
            r#"{ "interceptors": {
                "LoggingInterceptor": { "ignore": true },
                "MultiStatusInterceptor": { "requestBody": "single-or-array" }
            } }"#,
        )
        .expect("valid interceptors config parses");
        assert!(cfg.interceptors["LoggingInterceptor"].ignore);
        assert_eq!(
            cfg.interceptors["MultiStatusInterceptor"].request_body,
            Some(RequestBodyTransform::SingleOrArray)
        );
    }

    #[test]
    fn parses_response_override() {
        let cfg: Config = serde_json::from_str(
            r#"{ "interceptors": {
                "SignResponseInterceptor": {
                    "response": {
                        "whenPathEndsWith": "/jws",
                        "schema": { "type": "object", "properties": { "signature": { "type": "string" } } }
                    }
                }
            } }"#,
        )
        .expect("valid response override config parses");
        let ov = cfg.interceptors["SignResponseInterceptor"]
            .response
            .as_ref()
            .expect("response override present");
        assert_eq!(ov.when_path_ends_with.as_deref(), Some("/jws"));
        assert_eq!(ov.schema["type"], "object");
    }

    #[test]
    fn merge_value_overrides_scalars_and_merges_objects() {
        let mut base = serde_json::json!({
            "output": "openapi.yaml",
            "resolvePackages": ["@algoan/*"],
            "info": { "title": "Base", "version": "1.0" }
        });
        merge_value(
            &mut base,
            serde_json::json!({ "info": { "title": "Child" } }),
        );
        assert_eq!(base["output"], "openapi.yaml");
        assert_eq!(base["resolvePackages"][0], "@algoan/*");
        assert_eq!(base["info"]["title"], "Child"); // child overrides
        assert_eq!(base["info"]["version"], "1.0"); // sibling inherited
    }

    #[test]
    fn merge_value_replaces_arrays_wholesale() {
        let mut base = serde_json::json!({ "include": ["src", "lib"] });
        merge_value(&mut base, serde_json::json!({ "include": ["app"] }));
        assert_eq!(base["include"], serde_json::json!(["app"]));
    }

    #[test]
    fn extends_merges_base_config() {
        let dir = std::env::temp_dir().join(format!("nestoa-extends-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("services/svc")).unwrap();
        std::fs::write(
            dir.join("nestoa.base.json"),
            r#"{ "output": "openapi.yaml", "resolvePackages": ["@algoan/*"] }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("services/svc/nestoa.config.json"),
            r#"{ "extends": "../../nestoa.base.json", "info": { "title": "Svc", "version": "2.0" } }"#,
        )
        .unwrap();

        let cfg = Config::load(
            Some(Path::new("nestoa.config.json")),
            &dir.join("services/svc"),
        )
        .expect("config with extends loads");
        assert_eq!(cfg.info.title, "Svc"); // from child
        assert_eq!(cfg.info.version, "2.0"); // from child
        assert_eq!(cfg.resolve_packages, vec!["@algoan/*".to_string()]); // from base
        assert_eq!(cfg.output, PathBuf::from("openapi.yaml")); // from base
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extends_missing_target_errors() {
        let dir =
            std::env::temp_dir().join(format!("nestoa-extends-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("nestoa.config.json"),
            r#"{ "extends": "./does-not-exist.json" }"#,
        )
        .unwrap();
        let err = Config::load(Some(Path::new("nestoa.config.json")), &dir);
        assert!(err.is_err(), "missing extends target should error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_interceptor_key() {
        let err =
            serde_json::from_str::<Config>(r#"{ "interceptors": { "X": { "bogus": true } } }"#);
        assert!(
            err.is_err(),
            "deny_unknown_fields should reject unknown keys"
        );
    }
}
