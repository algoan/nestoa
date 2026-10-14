//! Detection of the NestJS global prefix declared in a bootstrap file.
//!
//! NestJS apps set a global route prefix at runtime via
//! `app.setGlobalPrefix(<arg>)` (usually in `src/main.ts`). Because nestoa is a
//! purely static analyser it never runs that code, so the prefix has to be
//! recovered from the source. This module finds the call and resolves its
//! argument back to a literal string, covering the three shapes seen in real
//! codebases:
//!
//! 1. a string literal — `app.setGlobalPrefix('v2')`;
//! 2. a local `const` bound to a string — `const globalPrefix = 'v2'; … app.setGlobalPrefix(globalPrefix)`;
//! 3. a `node-config-ts` member access — `app.setGlobalPrefix(config.globalPrefix)`,
//!    resolved against the project's `config/default.json`.

use std::path::{Path, PathBuf};

use swc_ecma_ast::{
    Callee, Decl, Expr, Lit, MemberExpr, MemberProp, ModuleDecl, ModuleItem, Pat, Stmt,
};
use swc_ecma_visit::{Visit, VisitWith};

use crate::constval::object_value;
use crate::parse::ParsedFile;
use crate::symbols::Project;

/// What the bootstrap file configures about every route's path.
#[derive(Debug, Default)]
pub struct Bootstrap {
    /// `setGlobalPrefix`'s prefix.
    pub prefix: Option<String>,
    /// The routes `setGlobalPrefix`'s `exclude` option leaves unprefixed.
    pub exclude: Vec<PrefixExclude>,
    /// `enableVersioning` with URI versioning, the only kind that shows in
    /// the path.
    pub versioning: Option<UriVersioning>,
}

/// URI versioning: a version `1` adds a `<prefix>1` segment (`v1`) after the
/// global prefix.
#[derive(Debug)]
pub struct UriVersioning {
    pub prefix: String,
    /// The versions of a handler that declares none.
    pub default_versions: Vec<Version>,
}

/// One version a handler answers: a number, or `VERSION_NEUTRAL` (`None`),
/// which adds no segment.
pub type Version = Option<String>;

impl UriVersioning {
    /// The path segment for `version` (`v1`), empty for a neutral one.
    pub fn segment(&self, version: &Version) -> String {
        version
            .as_ref()
            .map(|v| format!("{}{v}", self.prefix))
            .unwrap_or_default()
    }
}

/// The versions a `@Version(...)` argument, a controller's `version` option
/// or `defaultVersion` names: a string, `VERSION_NEUTRAL`, or an array of
/// them.
pub fn versions(project: &Project, file: &Path, expr: &Expr) -> Option<Vec<Version>> {
    let one = |e: &Expr| match e {
        Expr::Ident(i) if i.sym == "VERSION_NEUTRAL" => Some(None),
        e => crate::constval::string(project, file, e).map(Some),
    };
    match crate::constval::unwrap(expr) {
        Expr::Array(a) => a.elems.iter().map(|el| one(&el.as_ref()?.expr)).collect(),
        // `const VERSIONS = ['1', '2']`, evaluated where it is declared.
        Expr::Ident(i) if i.sym != "VERSION_NEUTRAL" => {
            let (decl_file, c) = project.resolve_const(file, &i.sym)?;
            match crate::constval::unwrap(c.init()) {
                init @ Expr::Array(_) => versions(project, &decl_file, init),
                _ => one(expr).map(|v| vec![v]),
            }
        }
        e => one(e).map(|v| vec![v]),
    }
}

/// One `exclude` entry: a route path (OpenAPI form, without surrounding
/// slashes), possibly ending in a wildcard, and the method it is limited to.
#[derive(Debug, PartialEq)]
pub struct PrefixExclude {
    path: String,
    wildcard: bool,
    method: Option<String>,
}

impl Bootstrap {
    /// Whether the global prefix is left off `route` (the controller and
    /// method paths, joined) for `method` (lowercase).
    pub fn excludes(&self, method: &str, route: &str) -> bool {
        let route = unnamed_params(route.trim_matches('/'));
        self.exclude.iter().any(|e| {
            let path_matches = if e.wildcard {
                route == e.path || route.starts_with(&format!("{}/", e.path)) || e.path.is_empty()
            } else {
                route == e.path
            };
            path_matches && e.method.as_deref().is_none_or(|m| m == method)
        })
    }
}

/// `path` with each parameter, whatever its name or constraint, as `:`:
/// an exclusion of `items/:id` covers a route at `items/:itemId(\\d+)`.
fn unnamed_params(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c != ':' {
            continue;
        }
        while chars
            .next_if(|n| n.is_ascii_alphanumeric() || *n == '_')
            .is_some()
        {}
        if chars.next_if_eq(&'(').is_some() {
            let (mut depth, mut escaped) = (1, false);
            for n in chars.by_ref() {
                match n {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
            }
        }
        chars.next_if_eq(&'?');
    }
    out
}

/// Scan the discovered `files` for `app.setGlobalPrefix(...)`. A `main.ts`
/// is tried first, then the others in sorted order. Files pulled in only
/// for type resolution (workspace libs, packages) are never scanned: a test
/// helper in a lib may set a prefix of its own.
/// `with_prefix: false` skips the global prefix (and its warnings), for a
/// config that sets `basePath` or turns prefix detection off.
pub fn detect(
    project: &Project,
    files: &[PathBuf],
    project_root: &Path,
    with_prefix: bool,
) -> Bootstrap {
    let mut files: Vec<&PathBuf> = files.iter().collect();
    files.sort_by_key(|f| (f.file_name().is_none_or(|n| n != "main.ts"), *f));
    // The first `enableVersioning` call decides, even one that leaves paths
    // unversioned (header versioning).
    let versioning = files
        .iter()
        .find_map(|f| Some((*f, call_args(project, f, "enableVersioning")?)))
        .and_then(|(f, args)| uri_versioning(project, f, &args));
    let mut bootstrap = if with_prefix {
        global_prefix(project, &files, project_root)
    } else {
        Bootstrap::default()
    };
    bootstrap.versioning = versioning;
    bootstrap
}

/// `value`, or a warning that `enableVersioning`'s options can't be read.
fn unreadable<T>(value: Option<T>) -> Option<T> {
    if value.is_none() {
        crate::diag::warn("cannot evaluate enableVersioning's options; paths are left unversioned");
    }
    value
}

/// The arguments of the first `*.<method>(...)` call in `file`.
fn call_args(project: &Project, file: &Path, method: &'static str) -> Option<Vec<Expr>> {
    let mut finder = CallFinder { method, args: None };
    project.files.get(file)?.module.visit_with(&mut finder);
    finder.args
}

/// The URI versioning `enableVersioning(args)`, written in `file`, enables:
/// `None` for another type (URI is the default).
fn uri_versioning(project: &Project, file: &Path, args: &[Expr]) -> Option<UriVersioning> {
    let mut versioning = UriVersioning {
        prefix: "v".to_string(),
        default_versions: vec![None],
    };
    // Options that can't be read, or a `type` that isn't plainly
    // `VersioningType.URI`, can't be told apart from header versioning,
    // which leaves paths alone.
    let Some(arg) = args.first() else {
        return Some(versioning);
    };
    let mut holder = None;
    let (file, options) = options_object(project, file, arg, &mut holder)?;
    let file = file.as_path();
    match object_value(options, "type") {
        None => {}
        Some(Expr::Member(MemberExpr {
            prop: MemberProp::Ident(kind),
            ..
        })) if kind.sym == "URI" => {}
        Some(_) => return None,
    }
    // A `prefix` or `defaultVersion` that can't be read would put routes on
    // a path the app doesn't serve: leave paths unversioned instead.
    match object_value(options, "prefix") {
        Some(Expr::Lit(Lit::Bool(b))) if !b.value => versioning.prefix.clear(),
        Some(e) => versioning.prefix = unreadable(crate::constval::string(project, file, &e))?,
        None => {}
    }
    if let Some(e) = object_value(options, "defaultVersion") {
        versioning.default_versions = unreadable(versions(project, file, &e))?;
    }
    Some(versioning)
}

fn global_prefix(project: &Project, files: &[&PathBuf], project_root: &Path) -> Bootstrap {
    for &file in files {
        let Some(parsed) = project.files.get(file) else {
            continue;
        };
        let mut finder = CallFinder {
            method: "setGlobalPrefix",
            args: None,
        };
        parsed.module.visit_with(&mut finder);
        let Some(args) = finder.args else { continue };
        // The first call decides: one that can't be read leaves routes
        // unprefixed rather than taking another file's prefix.
        let Some(prefix) = args
            .first()
            .and_then(|arg| resolve_prefix_expr(arg, parsed, project, project_root))
        else {
            crate::diag::warn(format!(
                "{}: cannot evaluate setGlobalPrefix's prefix; routes are left unprefixed",
                file.display()
            ));
            return Bootstrap::default();
        };
        let exclude = args
            .get(1)
            .map(|options| prefix_excludes(project, file, options))
            .unwrap_or_default();
        return Bootstrap {
            prefix: Some(prefix),
            exclude,
            versioning: None,
        };
    }
    Bootstrap::default()
}

/// The `exclude` entries of `setGlobalPrefix`'s options: `'health'`, or
/// `{ path: 'health', method: RequestMethod.GET }`.
fn unreadable_exclude() -> Vec<PrefixExclude> {
    crate::diag::warn("cannot evaluate setGlobalPrefix's exclude; every route keeps the prefix");
    Vec::new()
}

fn prefix_excludes(project: &Project, file: &Path, options: &Expr) -> Vec<PrefixExclude> {
    let mut holder = None;
    let Some((file, options)) = options_object(project, file, options, &mut holder) else {
        return unreadable_exclude();
    };
    let Some(exclude) = object_value(options, "exclude") else {
        return Vec::new();
    };
    // `exclude: EXCLUDES`, evaluated where the array is declared.
    let (file, entries) = match crate::constval::unwrap(&exclude) {
        Expr::Array(a) => (file, a.clone()),
        Expr::Ident(i) => match project.resolve_const(&file, &i.sym) {
            Some((decl_file, c)) => match crate::constval::unwrap(c.init()) {
                Expr::Array(a) => (decl_file, a.clone()),
                _ => return unreadable_exclude(),
            },
            None => return unreadable_exclude(),
        },
        _ => return unreadable_exclude(),
    };
    let file = file.as_path();
    entries
        .elems
        .iter()
        .flatten()
        .map(|entry| {
            let (path, method) = match entry.expr.as_ref() {
                Expr::Object(o) => {
                    // A method that can't be read drops the exclusion rather
                    // than widening it to every method.
                    let method = match object_value(o, "method") {
                        None => None,
                        Some(Expr::Member(MemberExpr {
                            prop: MemberProp::Ident(m),
                            ..
                        })) => (m.sym != "ALL").then(|| m.sym.to_lowercase()),
                        Some(_) => return None,
                    };
                    (
                        crate::constval::string(project, file, &object_value(o, "path")?)?,
                        method,
                    )
                }
                expr => (crate::constval::string(project, file, expr)?, None),
            };
            let path = path.trim_matches('/');
            let (path, wildcard) = match ["(.*)", "*", "{*splat}"]
                .iter()
                .find_map(|w| path.strip_suffix(w))
            {
                Some(stem) => (stem.trim_end_matches('/'), true),
                None => (path, false),
            };
            Some(PrefixExclude {
                path: unnamed_params(path),
                wildcard,
                method,
            })
        })
        // An entry that can't be read may name a route the prefix is left
        // off: say so, since its route is then documented prefixed.
        .filter_map(|exclude| {
            if exclude.is_none() {
                crate::diag::warn(
                    "cannot evaluate an entry of setGlobalPrefix's exclude; its route keeps the prefix",
                );
            }
            exclude
        })
        .collect()
}

/// An options argument as an object literal, written inline or held in a
/// constant (`as const` and `satisfies` included), with the file its values
/// are evaluated in. `holder` keeps a constant's declaration alive.
fn options_object<'a>(
    project: &Project,
    file: &Path,
    arg: &'a Expr,
    holder: &'a mut Option<crate::symbols::ConstInit>,
) -> Option<(PathBuf, &'a swc_ecma_ast::ObjectLit)> {
    match crate::constval::unwrap(arg) {
        Expr::Object(o) => Some((file.to_path_buf(), o)),
        Expr::Ident(i) => {
            let (decl_file, c) = project.resolve_const(file, &i.sym)?;
            match crate::constval::unwrap(holder.insert(c).init()) {
                Expr::Object(o) => Some((decl_file, o)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Visitor that captures the arguments of the first `*.<method>(...)` call
/// it encounters anywhere in the module (including inside the bootstrap
/// function body).
struct CallFinder {
    method: &'static str,
    args: Option<Vec<Expr>>,
}

impl Visit for CallFinder {
    fn visit_call_expr(&mut self, call: &swc_ecma_ast::CallExpr) {
        if self.args.is_none()
            && let Callee::Expr(callee) = &call.callee
            && let Expr::Member(MemberExpr {
                prop: MemberProp::Ident(p),
                ..
            }) = callee.as_ref()
            && p.sym == self.method
        {
            self.args = Some(call.args.iter().map(|a| (*a.expr).clone()).collect());
        }
        call.visit_children_with(self);
    }
}

fn resolve_prefix_expr(
    expr: &Expr,
    parsed: &ParsedFile,
    project: &Project,
    project_root: &Path,
) -> Option<String> {
    match expr {
        Expr::Lit(Lit::Str(s)) => Some(s.value.to_string_lossy().into_owned()),
        Expr::Tpl(_) => static_tpl(expr),
        Expr::Ident(i) => resolve_local_const(i.sym.as_ref(), parsed),
        Expr::Member(_) => resolve_config_member(expr, parsed, project, project_root),
        _ => None,
    }
}

/// A template literal with no interpolations is just a string, e.g. `` `v2` ``.
fn static_tpl(expr: &Expr) -> Option<String> {
    let Expr::Tpl(t) = expr else { return None };
    if !t.exprs.is_empty() || t.quasis.len() != 1 {
        return None;
    }
    t.quasis[0]
        .cooked
        .as_ref()
        .map(|c| c.to_string_lossy().into_owned())
}

/// Find a top-level `const <name> = "literal"` (or static template) in the file.
fn resolve_local_const(name: &str, parsed: &ParsedFile) -> Option<String> {
    for item in &parsed.module.body {
        let var = match item {
            ModuleItem::Stmt(Stmt::Decl(Decl::Var(v))) => v,
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => match &e.decl {
                Decl::Var(v) => v,
                _ => continue,
            },
            _ => continue,
        };
        for d in &var.decls {
            let Pat::Ident(bi) = &d.name else { continue };
            if bi.id.sym != name {
                continue;
            }
            let Some(init) = &d.init else { continue };
            match init.as_ref() {
                Expr::Lit(Lit::Str(s)) => return Some(s.value.to_string_lossy().into_owned()),
                tpl @ Expr::Tpl(_) => {
                    if let Some(s) = static_tpl(tpl) {
                        return Some(s);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Resolve `config.<key>...` where `config` is imported from `node-config-ts`.
/// The runtime `config` object mirrors the merged config JSON, so the member
/// path maps directly into `config/default.json`.
fn resolve_config_member(
    expr: &Expr,
    parsed: &ParsedFile,
    project: &Project,
    project_root: &Path,
) -> Option<String> {
    let (root, path) = flatten_member(expr)?;
    if path.is_empty() {
        return None;
    }
    let fs = project.symbols.get(&parsed.path)?;
    let binding = fs.imports.get(&root)?;
    if binding.specifier != "node-config-ts" {
        return None;
    }
    let raw = std::fs::read_to_string(project_root.join("config").join("default.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let mut cur = &json;
    for key in &path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(|s| s.to_string())
}

/// Flatten a member-access chain into its root identifier and the list of
/// property names. `config.a.b` → `("config", ["a", "b"])`. Returns `None` for
/// computed (`config["a"]`) or non-identifier roots.
fn flatten_member(expr: &Expr) -> Option<(String, Vec<String>)> {
    match expr {
        Expr::Ident(i) => Some((i.sym.to_string(), Vec::new())),
        Expr::Member(m) => {
            let MemberProp::Ident(prop) = &m.prop else {
                return None;
            };
            let (root, mut path) = flatten_member(&m.obj)?;
            path.push(prop.sym.to_string());
            Some((root, path))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParsingContext;

    fn project_from(root: &Path, files: &[(&str, &str)]) -> Project {
        let parser = ParsingContext::default();
        let mut project = Project::default();
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            project.add(parser.parse_file(&path).unwrap());
        }
        project
    }

    fn files(project: &Project) -> Vec<PathBuf> {
        project.files.keys().cloned().collect()
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nestoa-gp-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn detects_string_literal() {
        let root = temp_root("lit");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "async function bootstrap() { app.setGlobalPrefix('api'); }",
            )],
        );
        assert_eq!(
            detect(&project, &files(&project), &root, true)
                .prefix
                .as_deref(),
            Some("api")
        );
    }

    #[test]
    fn detects_local_const() {
        let root = temp_root("const");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "const globalPrefix: string = 'v2';\nasync function bootstrap() { app.setGlobalPrefix(globalPrefix); }",
            )],
        );
        assert_eq!(
            detect(&project, &files(&project), &root, true)
                .prefix
                .as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn detects_node_config_member() {
        let root = temp_root("nodeconfig");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config/default.json"),
            r#"{ "globalPrefix": "v2" }"#,
        )
        .unwrap();
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "import { config } from 'node-config-ts';\nasync function bootstrap() { app.setGlobalPrefix(config.globalPrefix); }",
            )],
        );
        assert_eq!(
            detect(&project, &files(&project), &root, true)
                .prefix
                .as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn ignores_unrelated_member() {
        let root = temp_root("unrelated");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "import { settings } from './local';\nasync function bootstrap() { app.setGlobalPrefix(settings.prefix); }",
            )],
        );
        assert_eq!(detect(&project, &files(&project), &root, true).prefix, None);
    }

    #[test]
    fn only_entry_files_are_scanned_and_main_ts_comes_first() {
        let root = temp_root("entry");
        let project = project_from(
            &root,
            &[
                ("libs/testing/src/index.ts", "app.setGlobalPrefix('e2e');"),
                ("src/a-setup.ts", "app.setGlobalPrefix('setup');"),
                ("src/main.ts", "app.setGlobalPrefix('api');"),
            ],
        );
        let entry = [root.join("src/a-setup.ts"), root.join("src/main.ts")];
        assert_eq!(
            detect(&project, &entry, &root, true).prefix.as_deref(),
            Some("api")
        );
        assert_eq!(
            detect(&project, &entry[..1], &root, true).prefix.as_deref(),
            Some("setup")
        );
    }

    #[test]
    fn detects_uri_versioning() {
        let root = temp_root("versioning");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "app.enableVersioning({ type: VersioningType.URI, prefix: 'ver', defaultVersion: ['1', VERSION_NEUTRAL] });",
            )],
        );
        let versioning = detect(&project, &files(&project), &root, true)
            .versioning
            .expect("URI versioning");
        assert_eq!(versioning.prefix, "ver");
        assert_eq!(versioning.default_versions, [Some("1".to_string()), None]);
        assert_eq!(versioning.segment(&Some("2".into())), "ver2");
        let consts = temp_root("versioning-const");
        let project = project_from(
            &consts,
            &[(
                "src/main.ts",
                "const VERSIONS = ['1', VERSION_NEUTRAL] satisfies VersionValue[];\napp.enableVersioning({ defaultVersion: VERSIONS });",
            )],
        );
        let versioning = detect(&project, &files(&project), &consts, true)
            .versioning
            .unwrap();
        assert_eq!(versioning.default_versions, [Some("1".to_string()), None]);

        let header = temp_root("versioning-header");
        let project = project_from(
            &header,
            &[(
                "src/main.ts",
                "app.enableVersioning({ type: VersioningType.HEADER, header: 'X-Version' });",
            )],
        );
        assert!(
            detect(&project, &files(&project), &header, true)
                .versioning
                .is_none()
        );

        let through_variable = temp_root("versioning-variable");
        let project = project_from(
            &through_variable,
            &[(
                "src/main.ts",
                "const uri = { type: VersioningType.URI, prefix: 'ver' } as const satisfies VersioningOptions;\n\
                 app.enableVersioning(uri);",
            )],
        );
        let detected = detect(&project, &files(&project), &through_variable, true);
        assert_eq!(
            detected.versioning.map(|v| v.prefix).as_deref(),
            Some("ver")
        );
        let unreadable = temp_root("versioning-unreadable");
        let project = project_from(
            &unreadable,
            &[(
                "src/main.ts",
                "const header = { type: VersioningType.HEADER, header: 'X-Version' };\n\
                 app.enableVersioning(header);",
            )],
        );
        assert!(
            detect(&project, &files(&project), &unreadable, true)
                .versioning
                .is_none()
        );
        let project = project_from(
            &unreadable,
            &[("src/main.ts", "function f(o) { app.enableVersioning(o); }")],
        );
        assert!(
            detect(&project, &files(&project), &unreadable, true)
                .versioning
                .is_none()
        );
    }

    #[test]
    fn the_first_versioning_call_decides_and_imported_options_use_their_module() {
        let root = temp_root("versioning-first");
        let project = project_from(
            &root,
            &[
                (
                    "src/main.ts",
                    "app.enableVersioning({ type: VersioningType.HEADER, header: 'X-V' });",
                ),
                ("src/other.ts", "app.enableVersioning();"),
            ],
        );
        assert!(
            detect(&project, &files(&project), &root, true)
                .versioning
                .is_none()
        );

        let imported = temp_root("versioning-imported");
        let project = project_from(
            &imported,
            &[
                (
                    "src/versioning.ts",
                    "const PREFIX = 'ver';\nconst DEFAULT = '3';\n\
                     export const options = { type: VersioningType.URI, prefix: PREFIX, defaultVersion: DEFAULT };",
                ),
                (
                    "src/main.ts",
                    "import { options } from './versioning';\napp.enableVersioning(options);",
                ),
            ],
        );
        let versioning = detect(&project, &files(&project), &imported, true)
            .versioning
            .expect("URI versioning");
        assert_eq!(versioning.prefix, "ver");
        assert_eq!(versioning.default_versions, [Some("3".to_string())]);
    }

    #[test]
    fn prefix_options_held_in_a_constant() {
        let root = temp_root("prefix-options");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "const options = { exclude: ['health'] } as const;\napp.setGlobalPrefix('api', options);",
            )],
        );
        let bootstrap = detect(&project, &files(&project), &root, true);
        assert!(bootstrap.excludes("get", "/health"));
        assert!(!bootstrap.excludes("get", "/other"));
    }

    #[test]
    fn unreadable_options_never_widen_or_guess() {
        crate::diag::take_warnings();
        let root = temp_root("unreadable-options");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "app.setGlobalPrefix('api', { exclude: [{ path: 'health', method: METHOD }, 'ping', 'items/:id', DYNAMIC] });\n\
                 app.enableVersioning({ prefix: getPrefix() });",
            )],
        );
        let bootstrap = detect(&project, &files(&project), &root, true);
        assert!(!bootstrap.excludes("post", "/health"));
        assert!(bootstrap.excludes("post", "/ping"));
        assert!(bootstrap.excludes("get", "/items/:itemId?"));
        assert!(bootstrap.versioning.is_none());
        assert_eq!(
            crate::diag::take_warnings(),
            [
                "cannot evaluate enableVersioning's options; paths are left unversioned",
                "cannot evaluate an entry of setGlobalPrefix's exclude; its route keeps the prefix",
                "cannot evaluate an entry of setGlobalPrefix's exclude; its route keeps the prefix"
            ]
        );
    }

    #[test]
    fn an_unreadable_prefix_in_main_ts_is_not_replaced() {
        crate::diag::take_warnings();
        let root = temp_root("prefix-unreadable");
        let project = project_from(
            &root,
            &[
                ("src/main.ts", "app.setGlobalPrefix(getPrefix());"),
                ("src/other.ts", "app.setGlobalPrefix('other');"),
            ],
        );
        assert_eq!(detect(&project, &files(&project), &root, true).prefix, None);
        assert_eq!(crate::diag::take_warnings().len(), 1);
    }

    #[test]
    fn an_exclude_array_held_in_a_constant() {
        let root = temp_root("exclude-const");
        let project = project_from(
            &root,
            &[
                (
                    "src/excludes.ts",
                    "const HEALTH = 'health';\nexport const EXCLUDES = [HEALTH];",
                ),
                (
                    "src/main.ts",
                    "import { EXCLUDES } from './excludes';\napp.setGlobalPrefix('api', { exclude: EXCLUDES });",
                ),
            ],
        );
        assert!(detect(&project, &files(&project), &root, true).excludes("get", "/health"));
    }

    #[test]
    fn without_prefix_detection_nothing_about_the_prefix_is_reported() {
        crate::diag::take_warnings();
        let root = temp_root("prefix-off");
        let project = project_from(
            &root,
            &[(
                "src/main.ts",
                "app.setGlobalPrefix(getPrefix());\napp.enableVersioning();",
            )],
        );
        let bootstrap = detect(&project, &files(&project), &root, false);
        assert_eq!(bootstrap.prefix, None);
        assert!(bootstrap.versioning.is_some());
        assert_eq!(crate::diag::take_warnings(), Vec::<String>::new());
    }
}
