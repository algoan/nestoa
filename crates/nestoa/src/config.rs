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
    /// packages like `@acme/core` that map to in-repo `libs/*/src`). Reads the
    /// project's `tsconfig.json`, following `extends`. Defaults to true.
    #[serde(default = "default_true")]
    pub resolve_tsconfig_paths: bool,

    /// Explicit tsconfig file to read `paths` from (relative to the project root
    /// or absolute). Defaults to `tsconfig.json` in the project root.
    #[serde(default)]
    pub tsconfig: Option<PathBuf>,

    /// node_modules package names (glob patterns) whose TypeScript declarations
    /// should be parsed so their exported types resolve to real schemas instead
    /// of `x-unresolved`. For example `["@acme/*"]` pulls in the typings of
    /// every `@acme` package referenced by a route. Empty by default, so no
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

    /// Extra decorator-name aliases mapped to known handlers, or to the query
    /// parameters the decorator reads. Lets users wire in custom decorators
    /// without modifying the tool. For example, mapping a project-specific
    /// `@MyAuthUser` to the `user` parameter kind tells the tool to drop that
    /// parameter from the route signature.
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

    /// The security scheme of authenticated operations, which
    /// `errorResponses`' `auth`, `authGuards` and `authOptOut` pick out.
    #[serde(default)]
    pub security: Security,

    /// The AsyncAPI document of the service's message events, written beside
    /// the OpenAPI one when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asyncapi: Option<AsyncApi>,
}

/// See `Config::asyncapi`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AsyncApi {
    /// Output file of the AsyncAPI document, relative to the project.
    #[serde(default = "default_asyncapi_output")]
    pub output: PathBuf,
    /// The document's `info`; the top-level `info` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<Info>,
    #[serde(default = "default_content_type")]
    pub default_content_type: String,
    /// Generic envelope types a message payload is unwrapped from, to their
    /// first type argument, at the root of the payload only.
    #[serde(default)]
    pub payload_wrappers: Vec<String>,
    /// The calls that publish a message.
    #[serde(default = "default_publishers")]
    pub publishers: Vec<Publisher>,
}

impl Default for AsyncApi {
    fn default() -> Self {
        Self {
            output: default_asyncapi_output(),
            info: None,
            default_content_type: default_content_type(),
            payload_wrappers: Vec::new(),
            publishers: default_publishers(),
        }
    }
}

impl AsyncApi {
    pub fn output_format(&self) -> OutputFormat {
        format_of(&self.output)
    }
}

/// A method call that publishes a message: `<receiver>.<method>(channel,
/// payload)`. See `AsyncApi::publishers`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Publisher {
    pub method: String,
    /// Type names the receiver (`this.<field>`) must be declared with; any
    /// receiver when empty.
    #[serde(default)]
    pub receiver_types: Vec<String>,
    /// Index of the argument naming the channel.
    #[serde(default)]
    pub channel: usize,
    /// Index of the argument holding the payload.
    #[serde(default = "default_payload_arg")]
    pub payload: usize,
    /// The property of an object-literal payload argument that holds the
    /// message payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_property: Option<String>,
    /// Index of the call's type argument giving the payload type, which wins
    /// over the argument's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_argument: Option<usize>,
}

fn default_asyncapi_output() -> PathBuf {
    PathBuf::from("asyncapi.json")
}

fn default_content_type() -> String {
    "application/json".into()
}

fn default_payload_arg() -> usize {
    1
}

/// `ClientProxy.emit`/`send` and the transport clients extending it.
fn default_publishers() -> Vec<Publisher> {
    let receiver_types: Vec<String> = [
        "ClientProxy",
        "ClientKafka",
        "ClientMqtt",
        "ClientNats",
        "ClientRedis",
        "ClientRMQ",
        "ClientTCP",
    ]
    .map(String::from)
    .to_vec();
    ["emit", "send"]
        .map(|method| Publisher {
            method: method.into(),
            receiver_types: receiver_types.clone(),
            channel: 0,
            payload: default_payload_arg(),
            payload_property: None,
            type_argument: Some(1),
        })
        .to_vec()
}

/// See `Config::security`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Security {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The key of the scheme in `components.securitySchemes`.
    #[serde(default = "default_security_name")]
    pub name: String,
    /// The OpenAPI Security Scheme Object, emitted verbatim.
    #[serde(default = "default_security_scheme")]
    pub scheme: serde_json::Value,
}

impl Default for Security {
    fn default() -> Self {
        Self {
            enabled: true,
            name: default_security_name(),
            scheme: default_security_scheme(),
        }
    }
}

fn default_security_name() -> String {
    "bearerAuth".to_string()
}

fn default_security_scheme() -> serde_json::Value {
    serde_json::json!({ "type": "http", "scheme": "bearer" })
}

/// Configuration for automatic error-response generation. Recovers the
/// `400`/`401`/`403`/`404`/`409`/`413`/`422` responses NestJS produces but that
/// never appear in a handler's return type. Every field defaults to a value
/// grounded in common NestJS setups, so an empty `errorResponses`
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
    /// pipes on `@Param`/`@Query`. Defaults cover the common id/date parsing
    /// pipes (which throw `404`, not `400`) and Nest's built-in parse pipes.
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

    /// The body every error response references in place of the built-in
    /// `{ code, message, status }` object: a project type name, or a schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<ErrorSchema>,
}

/// See `ErrorResponses::schema`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ErrorSchema {
    /// A type declared in the project, lowered like a handler's return type.
    Type(String),
    /// An OpenAPI schema, used verbatim.
    Inline(serde_json::Map<String, serde_json::Value>),
}

impl<'de> Deserialize<'de> for ErrorSchema {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(name) => Ok(Self::Type(name)),
            serde_json::Value::Object(schema) => Ok(Self::Inline(schema)),
            other => Err(serde::de::Error::custom(format!(
                "`schema` must be a type name or an OpenAPI schema object, not {other}"
            ))),
        }
    }
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
            schema: None,
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
    /// Match routes whose full path ends with this suffix (e.g. `/publish`).
    #[serde(default)]
    pub when_path_ends_with: Option<String>,
    /// Match the route whose full path equals this exactly.
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict the match to a single HTTP method (e.g. `post`), so sibling
    /// routes sharing a path — `POST /owners` (which `409`/`422`s) vs
    /// `GET /owners` (which doesn't) — can be targeted independently. Matched
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
    /// request path ends in `/signed`). The supplied schema replaces the route's
    /// response, optionally gated on a path suffix so sibling routes that share
    /// the same interceptor (array-valued `@Get([':id', ':id/signed'])`) keep their
    /// untransformed shape.
    #[serde(default)]
    pub response: Option<ResponseOverride>,
}

/// A response-schema override applied by an interceptor. See `InterceptorConfig::response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseOverride {
    /// Only apply the override to routes whose path ends with this suffix (e.g.
    /// `/signed`). When omitted, the override applies to every route carrying the
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
    /// sees an array). Emitted as `anyOf: [S, { type: array, items: S }]`.
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Info {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// See `Config::decorator_aliases`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum DecoratorAlias {
    Kind(DecoratorAliasKind),
    Reads(DecoratorReads),
}

// By hand rather than `untagged`, whose error names no field.
impl<'de> Deserialize<'de> for DecoratorAlias {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_json::Value::deserialize(d)? {
            v @ serde_json::Value::String(_) => serde_json::from_value(v).map(Self::Kind),
            v => serde_json::from_value(v).map(Self::Reads),
        }
        .map_err(D::Error::custom)
    }
}

/// What a custom parameter decorator reads from the request, for one that
/// isn't a known kind (`@Paginate()` reading `?page` and `?limit`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecoratorReads {
    /// Query parameters keyed by name, emitted after the handler's own.
    pub query: indexmap::IndexMap<String, DeclaredParam>,
}

/// A parameter declared by `DecoratorReads`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclaredParam {
    #[serde(deserialize_with = "schema_object")]
    pub schema: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
    /// A property of the decorator's options argument that renames the
    /// parameter (`@Paginate({ perPageName: 'limit' })`). The key is the name
    /// when the property is absent.
    #[serde(default)]
    pub name_from: Option<String>,
}

fn schema_object<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<serde_json::Map<String, serde_json::Value>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(schema) => Ok(schema),
        other => Err(serde::de::Error::custom(format!(
            "`schema` must be an OpenAPI schema object, not {other}"
        ))),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DecoratorAliasKind {
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

/// Pipe -> rejection status. `ParseObjectIdPipe`/`ParseDatePipe` conventionally
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
            security: Security::default(),
            asyncapi: None,
        }
    }
}

impl Config {
    /// Load `explicit` (a path as given on the command line, so relative to
    /// the process CWD), or else the first `nestoa.config.*` in `project_root`.
    pub fn load(explicit: Option<&Path>, project_root: &Path) -> Result<Self> {
        let path = match explicit {
            Some(p) if !p.is_file() => bail!("config file {} not found", p.display()),
            Some(p) => p.to_path_buf(),
            None => {
                let found = [
                    "nestoa.config.json",
                    "nestoa.config.yaml",
                    "nestoa.config.yml",
                ]
                .iter()
                .map(|n| project_root.join(n))
                .find(|p| p.is_file());
                let Some(found) = found else {
                    return Ok(Config::default());
                };
                found
            }
        };
        let value = load_value(&path, &mut Vec::new())?;
        serde_json::from_value(value).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn output_format(&self) -> OutputFormat {
        self.format.unwrap_or_else(|| format_of(&self.output))
    }
}

fn format_of(output: &Path) -> OutputFormat {
    match output
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("yaml") | Some("yml") => OutputFormat::Yaml,
        _ => OutputFormat::Json,
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
    fn asyncapi_is_off_by_default_and_defaults_its_publishers() {
        assert!(Config::default().asyncapi.is_none());
        let cfg: Config = serde_json::from_str(r#"{ "asyncapi": {} }"#).unwrap();
        let asyncapi = cfg.asyncapi.expect("an empty section enables it");
        assert_eq!(asyncapi.output, PathBuf::from("asyncapi.json"));
        assert_eq!(asyncapi.output_format(), OutputFormat::Json);
        let methods: Vec<&str> = asyncapi
            .publishers
            .iter()
            .map(|p| p.method.as_str())
            .collect();
        assert_eq!(methods, ["emit", "send"]);
        assert!(
            asyncapi.publishers[0]
                .receiver_types
                .contains(&"ClientProxy".to_string())
        );

        let cfg: Config = serde_json::from_str(
            r#"{ "asyncapi": { "output": "events.yaml", "publishers": [
                { "method": "publish", "payloadProperty": "data", "typeArgument": 0 }
            ] } }"#,
        )
        .unwrap();
        let asyncapi = cfg.asyncapi.unwrap();
        assert_eq!(asyncapi.output_format(), OutputFormat::Yaml);
        assert_eq!(
            asyncapi.publishers,
            [Publisher {
                method: "publish".into(),
                receiver_types: Vec::new(),
                channel: 0,
                payload: 1,
                payload_property: Some("data".into()),
                type_argument: Some(0),
            }]
        );
        assert!(serde_json::from_str::<Config>(r#"{ "asyncapi": { "channels": {} } }"#).is_err());
    }

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
    fn parses_decorator_aliases_of_both_forms() {
        let cfg: Config = serde_json::from_str(
            r#"{ "decoratorAliases": {
                "CurrentUser": "user",
                "Paginate": { "query": {
                    "limit": { "schema": { "type": "integer" }, "nameFrom": "perPageName" }
                } }
            } }"#,
        )
        .expect("valid decoratorAliases parse");
        assert_eq!(
            cfg.decorator_aliases["CurrentUser"],
            DecoratorAlias::Kind(DecoratorAliasKind::User)
        );
        let DecoratorAlias::Reads(reads) = &cfg.decorator_aliases["Paginate"] else {
            panic!("expected the object form");
        };
        let limit = &reads.query["limit"];
        assert_eq!(limit.name_from.as_deref(), Some("perPageName"));
        assert!(!limit.required);
        for bad in [
            r#""cookie""#,
            r#"{ "header": {} }"#,
            r#"{ "query": { "page": { "type": "integer" } } }"#,
            r#"{ "query": { "page": { "schema": "integer" } } }"#,
            r#"{ "query": { "page": { "schema": null } } }"#,
        ] {
            let json = format!(r#"{{ "decoratorAliases": {{ "Paginate": {bad} }} }}"#);
            assert!(serde_json::from_str::<Config>(&json).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_response_override() {
        let cfg: Config = serde_json::from_str(
            r#"{ "interceptors": {
                "SigningInterceptor": {
                    "response": {
                        "whenPathEndsWith": "/signed",
                        "schema": { "type": "object", "properties": { "signature": { "type": "string" } } }
                    }
                }
            } }"#,
        )
        .expect("valid response override config parses");
        let ov = cfg.interceptors["SigningInterceptor"]
            .response
            .as_ref()
            .expect("response override present");
        assert_eq!(ov.when_path_ends_with.as_deref(), Some("/signed"));
        assert_eq!(ov.schema["type"], "object");
    }

    #[test]
    fn merge_value_overrides_scalars_and_merges_objects() {
        let mut base = serde_json::json!({
            "output": "openapi.yaml",
            "resolvePackages": ["@acme/*"],
            "info": { "title": "Base", "version": "1.0" }
        });
        merge_value(
            &mut base,
            serde_json::json!({ "info": { "title": "Child" } }),
        );
        assert_eq!(base["output"], "openapi.yaml");
        assert_eq!(base["resolvePackages"][0], "@acme/*");
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
            r#"{ "output": "openapi.yaml", "resolvePackages": ["@acme/*"] }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("services/svc/nestoa.config.json"),
            r#"{ "extends": "../../nestoa.base.json", "info": { "title": "Svc", "version": "2.0" } }"#,
        )
        .unwrap();

        let cfg = Config::load(
            Some(&dir.join("services/svc/nestoa.config.json")),
            &dir.join("services/svc"),
        )
        .expect("config with extends loads");
        assert_eq!(cfg.info.title, "Svc"); // from child
        assert_eq!(cfg.info.version, "2.0"); // from child
        assert_eq!(cfg.resolve_packages, vec!["@acme/*".to_string()]); // from base
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
        let err = Config::load(Some(&dir.join("nestoa.config.json")), &dir);
        assert!(err.is_err(), "missing extends target should error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_explicit_config_errors() {
        let dir = std::env::temp_dir();
        let missing = dir.join(format!("nestoa-missing-{}.json", std::process::id()));
        let err = Config::load(Some(&missing), &dir).expect_err("a missing --config should error");
        assert!(format!("{err:#}").contains("not found"), "{err:#}");
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
        let err = Config::load(Some(&dir.join("nestoa.config.json")), &dir)
            .expect_err("an extends cycle should error");
        let msg = format!("{err:#}");
        assert!(msg.contains("\"extends\" cycle"), "{msg}");
        assert!(msg.contains("b.json -> "), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_info_key_is_rejected() {
        let err = serde_json::from_str::<Config>(r#"{ "info": { "titel": "X" } }"#)
            .expect_err("a misspelt info key should error");
        assert!(err.to_string().contains("titel"), "{err}");
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
                "routes": [ { "whenPathEndsWith": "/publish", "add": [409] } ],
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
            Some("/publish")
        );
        assert_eq!(er.global[0].status, 413);
    }

    #[test]
    fn error_schema_is_a_type_name_or_a_schema_object() {
        let schema = |json: &str| {
            serde_json::from_str::<Config>(&format!(r#"{{ "errorResponses": {json} }}"#))
                .map(|c| c.error_responses.schema)
        };
        assert_eq!(schema("{}").unwrap(), None);
        assert_eq!(
            schema(r#"{ "schema": "HttpError" }"#).unwrap(),
            Some(ErrorSchema::Type("HttpError".into()))
        );
        let Some(ErrorSchema::Inline(inline)) =
            schema(r#"{ "schema": { "type": "object" } }"#).unwrap()
        else {
            panic!("an object is an inline schema");
        };
        assert_eq!(inline["type"], "object");
        for bad in ["42", "true", r#"["HttpError"]"#] {
            let err = schema(&format!(r#"{{ "schema": {bad} }}"#)).expect_err(bad);
            assert!(err.to_string().contains("`schema`"), "{err}");
        }
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
