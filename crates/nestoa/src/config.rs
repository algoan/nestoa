use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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

    /// OpenAPI version the document is emitted for. `3.1.0` (the default) is
    /// the dialect the analyser lowers types into; `3.0.3` runs the emitted
    /// document through `dialect::downgrade_to_30` so it validates against the
    /// older schema. See that module for what the downgrade can and cannot
    /// preserve.
    #[serde(default)]
    pub openapi_version: OpenApiVersion,

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

    /// Generic wrapper type names to treat as transparent: each is lowered to
    /// its first type argument, discarding the wrapper. This is the config-driven
    /// counterpart to the built-in passthrough types (`Promise`, `Observable`,
    /// `Partial`, …) and covers project- or library-specific wrappers whose API
    /// surface is just the type they wrap — e.g. mongoose's
    /// `PopulatedDoc<T>` (`T | RawId`), where only the populated `T` is
    /// meaningful. `Foo<T>` with `["Foo"]` here lowers exactly as `T` would.
    #[serde(default)]
    pub passthrough_types: Vec<String>,

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

    /// Automatic error-response generation. Every field has a recon-grounded
    /// default, so an absent `errorResponses` still emits the deterministic
    /// ("Tier A") set. See `ErrorResponses`.
    #[serde(default)]
    pub error_responses: ErrorResponses,
}

/// Configuration for automatic error-response generation. Recovers the
/// `400`/`401`/`403`/`404`/`409`/`413`/`422` responses NestJS produces but that
/// never appear in a handler's return type. Every field defaults to a value
/// grounded in the surveyed `@algoan` services, so an empty `errorResponses`
/// (or none at all) still auto-emits the deterministic set: `401` for guarded
/// routes, `403` for `@Roles`, `400` for validated bodies, and the pipe-mapped
/// `404`/`400`. The map only *tunes* names/scope and *adds* the service-thrown
/// codes a controller signature cannot reveal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ErrorResponses {
    /// Master switch. When false, no error responses are emitted at all.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How authentication is wired, which decides where `401` applies:
    /// - `global`: every route is protected (the app registers an `APP_GUARD`
    ///   auth guard). Emit `401` unless the route opts out.
    /// - `per-controller`: only routes whose controller/method carries a guard
    ///   decorator naming one of `authGuards` (`@UseGuards(AuthGuard)`).
    /// - `none`: no authentication anywhere; never emit `401`.
    #[serde(default)]
    pub auth: AuthScope,

    /// In `per-controller` mode, the guard class names (as written inside
    /// `@UseGuards(...)`) that mark a route as authenticated.
    #[serde(default = "default_auth_guards")]
    pub auth_guards: Vec<String>,

    /// Decorators that opt a route (or its whole controller) out of `401` — e.g.
    /// `@Unprotected()`.
    #[serde(default = "default_auth_opt_out")]
    pub auth_opt_out: Vec<String>,

    /// Decorators whose presence on a route/controller implies `403`. Our role
    /// guard requires a route-level role, so `@Roles({...})` is a reliable local
    /// signal even though the guard itself is global.
    #[serde(default = "default_role_decorators")]
    pub role_decorators: Vec<String>,

    /// Emit `400` for any route with a request-body DTO, on the assumption that
    /// a global `ValidationPipe` validates it (true for every surveyed service).
    #[serde(default = "default_true")]
    pub validation: bool,

    /// Scan each handler's own body for `throw new <HttpException>(...)` and add
    /// the matching status (e.g. a `throw new UnprocessableEntityException()` in
    /// the controller → `422`). Only the handler body is read, not called
    /// services, so it is deterministic and side-effect free. Default true.
    #[serde(default = "default_true")]
    pub scan_throws: bool,

    /// Pipe class name -> the status it throws on invalid input, applied to
    /// pipes on `@Param`/`@Query`. Defaults cover the `@algoan` pipes (which
    /// throw `404`, not `400`) and Nest's built-in parse pipes.
    #[serde(default = "default_pipes")]
    pub pipes: indexmap::IndexMap<String, u16>,

    /// Filter-level codes not derivable from any route construct (e.g. `413`
    /// from an Express body-size limit). Applied to every route, optionally
    /// gated on a path suffix. Empty by default (`500` is intentionally never a
    /// default — it is a processing failure, not a documented outcome).
    #[serde(default)]
    pub global: Vec<GlobalError>,

    /// Per-path additions for service-thrown ("Tier B") errors the controller
    /// signature cannot reveal (the lone `409`, most `422`, business `404`).
    /// Empty by default; the source-level counterpart is the `@throws` JSDoc tag.
    #[serde(default)]
    pub routes: Vec<RouteErrors>,
}

impl Default for ErrorResponses {
    fn default() -> Self {
        Self {
            enabled: true,
            auth: AuthScope::default(),
            auth_guards: default_auth_guards(),
            auth_opt_out: default_auth_opt_out(),
            role_decorators: default_role_decorators(),
            validation: true,
            scan_throws: true,
            pipes: default_pipes(),
            global: Vec::new(),
            routes: Vec::new(),
        }
    }
}

/// How authentication is wired across the service. See `ErrorResponses::auth`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthScope {
    /// Every route is protected by a global guard.
    #[default]
    Global,
    /// Only routes whose controller/method carries an auth-guard decorator.
    PerController,
    /// No authentication; never emit `401`.
    None,
}

/// A filter-level error applied to every route. See `ErrorResponses::global`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalError {
    /// The HTTP status code to emit.
    pub status: u16,
    /// Only apply to routes whose path ends with this suffix (e.g. `/logo`).
    #[serde(default)]
    pub when_path_ends_with: Option<String>,
    /// Response description; a sensible default is used when omitted.
    #[serde(default)]
    pub description: Option<String>,
}

/// Per-path Tier-B error additions. See `ErrorResponses::routes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteErrors {
    /// Match routes whose full path ends with this suffix (e.g. `/aggregate`).
    #[serde(default)]
    pub when_path_ends_with: Option<String>,
    /// Match the route whose full path equals this exactly.
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict the match to a single HTTP method (e.g. `post`), so sibling
    /// routes sharing a path — `POST /customers` (which `409`/`422`s) vs
    /// `GET /customers` (which doesn't) — can be targeted independently. Matched
    /// case-insensitively. When omitted, every method on the path matches.
    #[serde(default)]
    pub method: Option<String>,
    /// Status codes to add to every matching route.
    pub add: Vec<u16>,
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

/// OpenAPI version to emit. See `Config::openapi_version`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OpenApiVersion {
    /// JSON Schema 2020-12 dialect: `type: [T, "null"]`, tuples as
    /// `prefixItems`. What the analyser lowers into natively.
    #[default]
    #[serde(rename = "3.1.0")]
    #[value(name = "3.1.0")]
    V31,
    /// The 3.0.3 subset: `nullable: true`, no `"null"` type, no tuples.
    #[serde(rename = "3.0.3")]
    #[value(name = "3.0.3")]
    V30,
}

impl OpenApiVersion {
    /// The version string written to the document's `openapi` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V31 => "3.1.0",
            Self::V30 => "3.0.3",
        }
    }
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

fn default_auth_guards() -> Vec<String> {
    vec!["AuthGuard".into()]
}

fn default_auth_opt_out() -> Vec<String> {
    vec!["Unprotected".into()]
}

fn default_role_decorators() -> Vec<String> {
    vec!["Roles".into()]
}

/// Pipe -> rejection status. The `@algoan` `ParseObjectIdPipe`/`ParseDatePipe`
/// throw `404` (not the conventional `400`); Nest's built-in parse pipes and
/// `ValidationPipe` throw `400`.
fn default_pipes() -> indexmap::IndexMap<String, u16> {
    let mut m = indexmap::IndexMap::new();
    m.insert("ParseObjectIdPipe".into(), 404);
    m.insert("ParseDatePipe".into(), 404);
    m.insert("ParseEnumPipe".into(), 400);
    m.insert("ParseBoolPipe".into(), 400);
    m.insert("ParseIntPipe".into(), 400);
    m.insert("ParseFloatPipe".into(), 400);
    m.insert("ParseUUIDPipe".into(), 400);
    m.insert("ParseArrayPipe".into(), 400);
    m.insert("ValidationPipe".into(), 400);
    m
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include: default_include(),
            exclude: default_exclude(),
            output: default_output(),
            format: None,
            openapi_version: OpenApiVersion::default(),
            base_path: None,
            detect_global_prefix: true,
            resolve_tsconfig_paths: true,
            tsconfig: None,
            resolve_packages: Vec::new(),
            passthrough_types: Vec::new(),
            info: Info {
                title: default_title(),
                version: default_version(),
                description: None,
            },
            decorator_aliases: indexmap::IndexMap::new(),
            interceptors: indexmap::IndexMap::new(),
            error_responses: ErrorResponses::default(),
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
                let value = load_value(&path, &mut Vec::new())?;
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
/// `chain` holds the files already being loaded, so a cycle is reported
/// instead of recursing forever.
fn load_value(path: &Path, chain: &mut Vec<PathBuf>) -> Result<serde_json::Value> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if let Some(start) = chain.iter().position(|p| *p == key) {
        let cycle: Vec<String> = chain[start..]
            .iter()
            .chain(std::iter::once(&key))
            .map(|p| p.display().to_string())
            .collect();
        bail!("\"extends\" cycle: {}", cycle.join(" -> "));
    }
    chain.push(key);
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
        let mut base = load_value(&base_path, chain)
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
    fn extends_cycle_errors() {
        let dir = std::env::temp_dir().join(format!("nestoa-extends-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("nestoa.config.json"),
            r#"{ "extends": "./b.json" }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.json"),
            r#"{ "extends": "./nestoa.config.json" }"#,
        )
        .unwrap();
        let err = Config::load(Some(Path::new("nestoa.config.json")), &dir)
            .expect_err("an extends cycle should error");
        let msg = format!("{err:#}");
        assert!(msg.contains("\"extends\" cycle"), "{msg}");
        assert!(msg.contains("b.json -> "), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn error_responses_defaults_when_absent() {
        let cfg: Config = serde_json::from_str("{}").expect("empty config parses");
        let er = &cfg.error_responses;
        assert!(er.enabled);
        assert_eq!(er.auth, AuthScope::Global);
        assert_eq!(er.auth_opt_out, vec!["Unprotected".to_string()]);
        assert_eq!(er.role_decorators, vec!["Roles".to_string()]);
        assert!(er.validation);
        assert_eq!(er.pipes.get("ParseObjectIdPipe"), Some(&404));
        assert_eq!(er.pipes.get("ParseEnumPipe"), Some(&400));
    }

    #[test]
    fn error_responses_partial_override_keeps_other_defaults() {
        let cfg: Config = serde_json::from_str(
            r#"{ "errorResponses": {
                "auth": "per-controller",
                "pipes": { "ParseObjectIdPipe": 400 },
                "routes": [ { "whenPathEndsWith": "/aggregate", "add": [409] } ],
                "global": [ { "status": 413, "whenPathEndsWith": "/logo" } ]
            } }"#,
        )
        .expect("partial errorResponses parses");
        let er = &cfg.error_responses;
        assert_eq!(er.auth, AuthScope::PerController);
        assert_eq!(er.pipes.get("ParseObjectIdPipe"), Some(&400)); // overridden
        assert!(er.validation); // default preserved
        assert_eq!(er.auth_opt_out, vec!["Unprotected".to_string()]); // default preserved
        assert_eq!(er.routes[0].add, vec![409]);
        assert_eq!(
            er.routes[0].when_path_ends_with.as_deref(),
            Some("/aggregate")
        );
        assert_eq!(er.global[0].status, 413);
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
