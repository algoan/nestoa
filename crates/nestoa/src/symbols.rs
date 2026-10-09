use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use swc_ecma_ast::{
    ClassDecl, Decl, DefaultDecl, ExportDecl, ExportDefaultDecl, ExportSpecifier, ImportDecl,
    ImportSpecifier, ModuleDecl, ModuleItem, Stmt, TsEnumDecl, TsInterfaceDecl, TsTypeAliasDecl,
};

use crate::parse::ParsedFile;
use crate::tsconfig::best_match;

/// One exported (or top-level) named declaration in a file. Indexed by name
/// within the file.
#[derive(Clone)]
pub enum Symbol {
    Interface(Arc<TsInterfaceDecl>),
    TypeAlias(Arc<TsTypeAliasDecl>),
    Enum(Arc<TsEnumDecl>),
    Class(Arc<ClassDecl>),
}

impl Symbol {
    /// The name the declaration itself was given, whatever it is imported as.
    pub fn name(&self) -> String {
        match self {
            Symbol::Interface(i) => i.id.sym.to_string(),
            Symbol::TypeAlias(t) => t.id.sym.to_string(),
            Symbol::Enum(e) => e.id.sym.to_string(),
            Symbol::Class(c) => c.ident.sym.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct ImportBinding {
    /// The exported name on the other side. None for default imports / namespace imports.
    pub imported: Option<String>,
    /// Raw module specifier (e.g. "./foo", "@nestjs/common").
    pub specifier: String,
    /// `import * as Ns from "..."` — the binding names the whole module.
    pub is_namespace: bool,
}

#[derive(Clone, Default)]
pub struct FileSymbols {
    pub declared: HashMap<String, Symbol>,
    /// Numeric `const` values (`export const MULTI_STATUS = 207`), used to
    /// resolve identifier arguments to decorators like `@HttpCode(MULTI_STATUS)`.
    pub const_numbers: HashMap<String, f64>,
    pub imports: HashMap<String, ImportBinding>,
    /// `export { Foo } from "./other"` re-exports: local-name -> ImportBinding
    pub re_exports: HashMap<String, ImportBinding>,
    /// `export * from "./other"` re-export targets.
    pub star_re_exports: Vec<String>,
    /// `export * as Ns from "./other"` — namespace name -> module specifier.
    pub namespace_re_exports: HashMap<String, String>,
    /// `export { Local as Exported }` without `from`: exported name -> the
    /// local declaration or import it names.
    pub local_exports: HashMap<String, String>,
}

/// All files indexed by path.
#[derive(Default)]
pub struct Project {
    pub files: HashMap<PathBuf, ParsedFile>,
    pub symbols: HashMap<PathBuf, FileSymbols>,
    /// TypeScript `compilerOptions.paths` aliases, used to resolve workspace
    /// package imports (e.g. `@algoan/core`) to in-repo source.
    pub tsconfig_paths: Option<crate::tsconfig::TsPaths>,
}

impl Project {
    pub fn add(&mut self, file: ParsedFile) {
        let fs = collect_file_symbols(&file);
        self.symbols.insert(file.path.clone(), fs);
        self.files.insert(file.path.clone(), file);
    }

    /// Resolve a bare specifier through tsconfig `paths` aliases, if any.
    pub fn resolve_alias(&self, specifier: &str) -> Option<PathBuf> {
        self.tsconfig_paths.as_ref()?.resolve(specifier)
    }

    /// Resolve a module specifier (relative, tsconfig-aliased, or node_modules
    /// package) to a file in the symbol table.
    fn resolve_module(&self, file: &Path, specifier: &str) -> Option<PathBuf> {
        resolve_module_path(file, specifier)
            .or_else(|| self.resolve_alias(specifier))
            .or_else(|| resolve_package_types(file, specifier))
    }

    /// Resolve a namespace-qualified type `Ns.Member` (where `Ns` is a TS
    /// namespace introduced by `import * as Ns`, `import { Ns }` of an
    /// `export * as Ns`, or a local `export * as Ns`) to the declaring symbol.
    pub fn resolve_namespace_member(
        &self,
        file: &Path,
        ns: &str,
        member: &str,
    ) -> Option<(PathBuf, Symbol)> {
        let mut visited = std::collections::HashSet::new();
        let module = self.resolve_namespace_module(file, ns, &mut visited)?;
        self.resolve(&module, member)
    }

    /// Find the module file that namespace `ns` aliases, following imports and
    /// `export * as` re-exports.
    fn resolve_namespace_module(
        &self,
        file: &Path,
        ns: &str,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<PathBuf> {
        if !visited.insert((file.to_path_buf(), ns.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        // Local `export * as Ns from "./mod"`.
        if let Some(spec) = fs.namespace_re_exports.get(ns) {
            return self.resolve_module(file, spec);
        }
        if let Some(local) = fs.local_exports.get(ns) {
            return self.resolve_namespace_module(file, local, visited);
        }
        if let Some(binding) = fs.imports.get(ns).or_else(|| fs.re_exports.get(ns)) {
            let target = self.resolve_module(file, &binding.specifier)?;
            return match &binding.imported {
                // `import * as Ns from "spec"` — the target module is the namespace.
                None if binding.is_namespace => Some(target),
                None => None, // default import — not a namespace
                // `import { Ns } from "spec"` — find namespace `Ns` in the target.
                Some(imported) => self.resolve_namespace_module(&target, imported, visited),
            };
        }
        fs.star_re_exports.iter().find_map(|spec| {
            let target = self.resolve_module(file, spec)?;
            self.resolve_namespace_module(&target, ns, visited)
        })
    }

    /// The package a value binding ultimately comes from, and its exported
    /// name there: `import { PartialType as P } from '@nestjs/swagger'` gives
    /// `("@nestjs/swagger", "PartialType")` for `P`. In-repo modules (relative
    /// or tsconfig-aliased barrels, namespace imports) are followed through
    /// their imports and re-exports; a package specifier ends the walk. `None`
    /// for a name not imported from any package, e.g. declared locally.
    pub fn import_origin(&self, file: &Path, name: &str) -> Option<(String, String)> {
        let mut visited = std::collections::HashSet::new();
        self.import_origin_inner(file, name, &mut visited)
    }

    /// `import_origin` for `ns.member`, where `ns` is a namespace: an
    /// `import * as ns`, or an `export * as ns` reached through in-repo
    /// barrels.
    pub fn namespace_member_origin(
        &self,
        file: &Path,
        ns: &str,
        member: &str,
    ) -> Option<(String, String)> {
        let mut seen = std::collections::HashSet::new();
        let (module_file, specifier) = self.namespace_specifier(file, ns, &mut seen)?;
        let mut visited = std::collections::HashSet::new();
        self.follow_specifier(&module_file, &specifier, member, &mut visited)
    }

    /// The module specifier namespace `ns` stands for, and the file it is
    /// written in.
    fn namespace_specifier(
        &self,
        file: &Path,
        ns: &str,
        seen: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<(PathBuf, String)> {
        if !seen.insert((file.to_path_buf(), ns.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        if let Some(spec) = fs.namespace_re_exports.get(ns) {
            return Some((file.to_path_buf(), spec.clone()));
        }
        if let Some(local) = fs.local_exports.get(ns) {
            return self.namespace_specifier(file, local, seen);
        }
        if let Some(binding) = fs.imports.get(ns).or_else(|| fs.re_exports.get(ns)) {
            if binding.is_namespace {
                return Some((file.to_path_buf(), binding.specifier.clone()));
            }
            let target = self.in_repo_module(file, &binding.specifier)?;
            let exported = binding.imported.as_deref().unwrap_or(ns);
            return self.namespace_specifier(&target, exported, seen);
        }
        fs.star_re_exports.iter().find_map(|spec| {
            let target = self.in_repo_module(file, spec)?;
            self.namespace_specifier(&target, ns, seen)
        })
    }

    /// The parsed in-repo module (relative or tsconfig-aliased) a specifier
    /// names. `None` for a package.
    fn in_repo_module(&self, file: &Path, specifier: &str) -> Option<PathBuf> {
        resolve_module_path(file, specifier)
            .or_else(|| self.resolve_alias(specifier))
            .filter(|t| self.symbols.contains_key(t))
    }

    fn import_origin_inner(
        &self,
        file: &Path,
        name: &str,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<(String, String)> {
        if !visited.insert((file.to_path_buf(), name.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        if let Some(local) = fs.local_exports.get(name) {
            return self.import_origin_inner(file, local, visited);
        }
        if let Some(b) = fs.imports.get(name).or_else(|| fs.re_exports.get(name))
            && !b.is_namespace
        {
            let imported = b.imported.clone().unwrap_or_else(|| name.to_string());
            return self.follow_specifier(file, &b.specifier, &imported, visited);
        }
        fs.star_re_exports
            .iter()
            .find_map(|spec| self.follow_specifier(file, spec, name, visited))
    }

    /// Continue `import_origin` into `specifier`: into the module when it is
    /// an in-repo one nestoa parsed, otherwise stop at the package.
    fn follow_specifier(
        &self,
        file: &Path,
        specifier: &str,
        name: &str,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<(String, String)> {
        let in_repo =
            resolve_module_path(file, specifier).or_else(|| self.resolve_alias(specifier));
        match in_repo {
            Some(target) if self.symbols.contains_key(&target) => {
                self.import_origin_inner(&target, name, visited)
            }
            Some(_) => None,
            None => Some((specifier.to_string(), name.to_string())),
        }
    }

    /// Resolve a name referenced inside `file` to the (declaring file, symbol).
    pub fn resolve(&self, file: &Path, name: &str) -> Option<(PathBuf, Symbol)> {
        let mut visited: std::collections::HashSet<(PathBuf, String)> =
            std::collections::HashSet::new();
        self.resolve_inner(file, name, &mut visited)
    }

    /// Resolve a numeric `const` by name, following imports and re-exports the
    /// same way type resolution does. Used for decorator arguments that name a
    /// constant, e.g. `@HttpCode(MULTI_STATUS)` where `MULTI_STATUS = 207` is
    /// declared in (and re-exported from) another module.
    pub fn resolve_const_number(&self, file: &Path, name: &str) -> Option<f64> {
        let mut visited: std::collections::HashSet<(PathBuf, String)> =
            std::collections::HashSet::new();
        self.resolve_const_inner(file, name, &mut visited)
    }

    /// Resolve the numeric value of `enum_name.member`, following imports the
    /// way type resolution does. A member without an initialiser continues
    /// from the previous numeric one, as in TypeScript.
    pub fn resolve_enum_member_number(
        &self,
        file: &Path,
        enum_name: &str,
        member: &str,
    ) -> Option<f64> {
        let (_, Symbol::Enum(e)) = self.resolve(file, enum_name)? else {
            return None;
        };
        match enum_member_values(&e)
            .into_iter()
            .find(|(name, _)| name == member)?
        {
            (_, Some(EnumValue::Num(v))) => Some(v),
            _ => None,
        }
    }

    fn resolve_const_inner(
        &self,
        file: &Path,
        name: &str,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<f64> {
        if !visited.insert((file.to_path_buf(), name.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        if let Some(v) = fs.const_numbers.get(name) {
            return Some(*v);
        }
        if let Some(local) = fs.local_exports.get(name) {
            return self.resolve_const_inner(file, local, visited);
        }
        if let Some(binding) = fs.re_exports.get(name).or_else(|| fs.imports.get(name))
            && let Some(target) = self.resolve_module(file, &binding.specifier)
        {
            let next_name = binding.imported.clone().unwrap_or_else(|| name.to_string());
            if let Some(found) = self.resolve_const_inner(&target, &next_name, visited) {
                return Some(found);
            }
        }
        for spec in &fs.star_re_exports {
            if let Some(target) = self.resolve_module(file, spec)
                && let Some(found) = self.resolve_const_inner(&target, name, visited)
            {
                return Some(found);
            }
        }
        None
    }

    fn resolve_inner(
        &self,
        file: &Path,
        name: &str,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<(PathBuf, Symbol)> {
        if !visited.insert((file.to_path_buf(), name.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        if let Some(sym) = fs.declared.get(name) {
            return Some((file.to_path_buf(), sym.clone()));
        }
        if let Some(local) = fs.local_exports.get(name) {
            return self.resolve_inner(file, local, visited);
        }
        if let Some(binding) = fs.re_exports.get(name).or_else(|| fs.imports.get(name))
            && let Some(target) = self.resolve_module(file, &binding.specifier)
        {
            let next_name = binding.imported.clone().unwrap_or_else(|| name.to_string());
            if let Some(found) = self.resolve_inner(&target, &next_name, visited) {
                return Some(found);
            }
        }
        // `export * from './other'` — search each one.
        for spec in &fs.star_re_exports {
            if let Some(target) = self.resolve_module(file, spec)
                && let Some(found) = self.resolve_inner(&target, name, visited)
            {
                return Some(found);
            }
        }
        None
    }
}

pub fn collect_file_symbols(parsed: &ParsedFile) -> FileSymbols {
    let mut fs = FileSymbols::default();
    for item in &parsed.module.body {
        match item {
            ModuleItem::Stmt(Stmt::Decl(decl)) => collect_decl(decl, &mut fs),
            ModuleItem::ModuleDecl(md) => match md {
                ModuleDecl::Import(i) => collect_import(i, &mut fs),
                ModuleDecl::ExportDecl(ExportDecl { decl, .. }) => collect_decl(decl, &mut fs),
                ModuleDecl::ExportDefaultDecl(ExportDefaultDecl { decl, .. }) => {
                    if let DefaultDecl::Class(cd) = decl
                        && let Some(ident) = &cd.ident
                    {
                        fs.declared.insert(
                            ident.sym.to_string(),
                            Symbol::Class(Arc::new(ClassDecl {
                                ident: ident.clone(),
                                declare: false,
                                class: cd.class.clone(),
                            })),
                        );
                    }
                }
                ModuleDecl::ExportAll(ea) => {
                    fs.star_re_exports
                        .push(ea.src.value.to_string_lossy().into_owned());
                }
                ModuleDecl::ExportNamed(en) => {
                    let src = en
                        .src
                        .as_ref()
                        .map(|s| s.value.to_string_lossy().into_owned());
                    for spec in &en.specifiers {
                        match spec {
                            ExportSpecifier::Named(n) => {
                                let exported_local = match &n.exported {
                                    Some(e) => module_export_name(e),
                                    None => module_export_name(&n.orig),
                                };
                                let orig_name = module_export_name(&n.orig);
                                if let Some(s) = &src {
                                    fs.re_exports.insert(
                                        exported_local,
                                        ImportBinding {
                                            imported: Some(orig_name),
                                            specifier: s.clone(),
                                            is_namespace: false,
                                        },
                                    );
                                } else if exported_local != orig_name {
                                    fs.local_exports.insert(exported_local, orig_name);
                                }
                                // Bare `export { X }` — X is already in declared/imports.
                            }
                            ExportSpecifier::Namespace(ns) => {
                                // `export * as Ns from "./other"`.
                                if let Some(s) = &src {
                                    fs.namespace_re_exports
                                        .insert(module_export_name(&ns.name), s.clone());
                                }
                            }
                            ExportSpecifier::Default(_) => {}
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    fs
}

fn collect_decl(decl: &Decl, fs: &mut FileSymbols) {
    match decl {
        Decl::Class(c) => {
            fs.declared
                .insert(c.ident.sym.to_string(), Symbol::Class(Arc::new(c.clone())));
        }
        Decl::TsInterface(i) => {
            fs.declared.insert(
                i.id.sym.to_string(),
                Symbol::Interface(Arc::new(*i.clone())),
            );
        }
        Decl::TsTypeAlias(t) => {
            fs.declared.insert(
                t.id.sym.to_string(),
                Symbol::TypeAlias(Arc::new(*t.clone())),
            );
        }
        Decl::TsEnum(e) => {
            fs.declared
                .insert(e.id.sym.to_string(), Symbol::Enum(Arc::new(*e.clone())));
        }
        Decl::Var(v) => {
            // Record numeric `const NAME = <literal>` so decorator arguments that
            // reference the constant (e.g. `@HttpCode(MULTI_STATUS)`) can be
            // resolved to their value, including across module boundaries.
            for d in &v.decls {
                if let (swc_ecma_ast::Pat::Ident(id), Some(init)) = (&d.name, &d.init)
                    && let Some(n) = numeric_expr(init)
                {
                    fs.const_numbers.insert(id.id.sym.to_string(), n);
                }
            }
        }
        _ => {}
    }
}

/// The value of an enum member, as far as it can be known statically.
#[derive(Debug, Clone, PartialEq)]
pub enum EnumValue {
    Str(String),
    Num(f64),
}

/// Each member of an enum with its value, following TypeScript: a string or
/// (possibly negative) numeric initializer is taken as written, and a member
/// without one continues from the previous numeric member, starting at 0.
/// `None` for a computed initializer, and for an implicit member after one
/// or after a string member, whose value nestoa cannot know. In an ambient
/// (`declare`) non-const enum TypeScript treats every implicit member as
/// computed, so those are `None` too.
pub fn enum_member_values(e: &TsEnumDecl) -> Vec<(String, Option<EnumValue>)> {
    let implicit_known = !e.declare || e.is_const;
    let mut next = implicit_known.then_some(0.0);
    e.members
        .iter()
        .map(|m| {
            let name = match &m.id {
                swc_ecma_ast::TsEnumMemberId::Ident(i) => i.sym.to_string(),
                swc_ecma_ast::TsEnumMemberId::Str(s) => s.value.to_string_lossy().into_owned(),
            };
            let value = match m.init.as_deref() {
                Some(swc_ecma_ast::Expr::Lit(swc_ecma_ast::Lit::Str(s))) => {
                    Some(EnumValue::Str(s.value.to_string_lossy().into_owned()))
                }
                Some(init) => numeric_expr(init).map(EnumValue::Num),
                None => next.map(EnumValue::Num),
            };
            next = match &value {
                Some(EnumValue::Num(v)) if implicit_known => Some(v + 1.0),
                _ => None,
            };
            (name, value)
        })
        .collect()
}

/// The numeric value of an expression that is a number literal, optionally
/// negated (`207`, `-1`). Returns `None` for anything else.
fn numeric_expr(expr: &swc_ecma_ast::Expr) -> Option<f64> {
    use swc_ecma_ast::{Expr, Lit, UnaryOp};
    match expr {
        Expr::Lit(Lit::Num(n)) => Some(n.value),
        Expr::Unary(u) if matches!(u.op, UnaryOp::Minus) => numeric_expr(&u.arg).map(|v| -v),
        _ => None,
    }
}

fn collect_import(i: &ImportDecl, fs: &mut FileSymbols) {
    let specifier = i.src.value.to_string_lossy().into_owned();
    for spec in &i.specifiers {
        match spec {
            ImportSpecifier::Named(n) => {
                let local = n.local.sym.to_string();
                let imported = n
                    .imported
                    .as_ref()
                    .map(module_export_name)
                    .unwrap_or_else(|| local.clone());
                fs.imports.insert(
                    local,
                    ImportBinding {
                        imported: Some(imported),
                        specifier: specifier.clone(),
                        is_namespace: false,
                    },
                );
            }
            ImportSpecifier::Default(d) => {
                fs.imports.insert(
                    d.local.sym.to_string(),
                    ImportBinding {
                        imported: None,
                        specifier: specifier.clone(),
                        is_namespace: false,
                    },
                );
            }
            ImportSpecifier::Namespace(n) => {
                fs.imports.insert(
                    n.local.sym.to_string(),
                    ImportBinding {
                        imported: None,
                        specifier: specifier.clone(),
                        is_namespace: true,
                    },
                );
            }
        }
    }
}

fn module_export_name(n: &swc_ecma_ast::ModuleExportName) -> String {
    match n {
        swc_ecma_ast::ModuleExportName::Ident(i) => i.sym.to_string(),
        swc_ecma_ast::ModuleExportName::Str(s) => s.value.to_string_lossy().into_owned(),
    }
}

/// Resolve `./other` relative to `current_file` to an absolute `.ts` path.
/// Returns None for package imports (e.g. `@nestjs/common`).
pub fn resolve_module_path(current_file: &Path, specifier: &str) -> Option<PathBuf> {
    if !specifier.starts_with('.') {
        return None;
    }
    let base = current_file.parent()?;
    let candidate = base.join(specifier);
    for ext in [
        ".ts",
        ".tsx",
        ".d.ts",
        "/index.ts",
        "/index.tsx",
        "/index.d.ts",
    ] {
        let mut p = candidate.clone().into_os_string();
        p.push(ext);
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p.canonicalize().unwrap_or(p));
        }
    }
    if candidate.is_file() {
        return Some(candidate.canonicalize().unwrap_or(candidate));
    }
    None
}

/// Split a bare module specifier into `(package_name, subpath)`.
/// `"@algoan/aggregator-raw-data/budget-insight"` -> `("@algoan/aggregator-raw-data", "budget-insight")`.
/// `"lodash"` -> `("lodash", "")`.
pub fn split_package_specifier(specifier: &str) -> (String, String) {
    let parts: Vec<&str> = specifier
        .splitn(if specifier.starts_with('@') { 3 } else { 2 }, '/')
        .collect();
    if specifier.starts_with('@') {
        let pkg = format!(
            "{}/{}",
            parts.first().copied().unwrap_or(""),
            parts.get(1).copied().unwrap_or("")
        );
        (pkg, parts.get(2).copied().unwrap_or("").to_string())
    } else {
        (
            parts.first().copied().unwrap_or("").to_string(),
            parts.get(1).copied().unwrap_or("").to_string(),
        )
    }
}

/// The package name portion of a bare specifier (e.g. `@algoan/foo/bar` -> `@algoan/foo`).
pub fn package_name(specifier: &str) -> String {
    split_package_specifier(specifier).0
}

/// Walk up from `start_dir` looking for `node_modules/<pkg>`.
pub(crate) fn find_package_dir(start_dir: &Path, pkg: &str) -> Option<PathBuf> {
    let mut dir = Some(start_dir);
    while let Some(d) = dir {
        let candidate = d.join("node_modules").join(pkg);
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Recursively search a package.json `exports` entry value for a `.d.ts` path,
/// preferring an explicit `types`/`typings` condition.
fn dts_from_exports_value(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if s.ends_with(".d.ts") => Some(s.clone()),
        serde_json::Value::Array(alternatives) => {
            alternatives.iter().find_map(dts_from_exports_value)
        }
        serde_json::Value::Object(map) => {
            for key in ["types", "typings"] {
                if let Some(found) = map.get(key).and_then(dts_from_exports_value) {
                    return Some(found);
                }
            }
            // Fall back to any nested condition that yields a .d.ts.
            map.values().find_map(dts_from_exports_value)
        }
        _ => None,
    }
}

/// Resolve a package subpath to its declaration entry via the `exports` map:
/// the best-matching key (exact, then the longest `*` pattern prefix, as Node
/// picks it), its `*` substituted into the target.
fn types_from_exports(json: &serde_json::Value, subpath: &str) -> Option<String> {
    let exports = json.get("exports")?;
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    // A string, array or conditions object is shorthand for `{ ".": … }`.
    let serde_json::Value::Object(map) = exports else {
        return (key == ".")
            .then(|| dts_from_exports_value(exports))
            .flatten();
    };
    if !map.keys().any(|k| k.starts_with('.')) {
        return (key == ".")
            .then(|| dts_from_exports_value(exports))
            .flatten();
    }
    let (value, captured) = best_match(map.iter().map(|(k, v)| (k.as_str(), v)), &key, true)?;
    Some(dts_from_exports_value(value)?.replace('*', &captured))
}

/// The TypeScript version `typesVersions` ranges are evaluated against.
const TS_VERSION: [u64; 3] = [5, 9, 0];

/// Resolve a subpath via the legacy `typesVersions` map: the first version
/// range `TS_VERSION` satisfies, as TypeScript picks it, then each target of
/// its best-matching pattern in order.
fn types_from_versions(json: &serde_json::Value, pkg_dir: &Path, subpath: &str) -> Option<PathBuf> {
    let map = json
        .get("typesVersions")?
        .as_object()?
        .iter()
        .find(|(range, _)| satisfies_ts(range))?
        .1
        .as_object()?;
    let (targets, captured) = best_match(map.iter().map(|(k, v)| (k.as_str(), v)), subpath, false)?;
    targets
        .as_array()?
        .iter()
        .filter_map(|t| t.as_str())
        .find_map(|t| {
            let target = pkg_dir.join(t.replace('*', &captured).trim_start_matches("./"));
            declaration_file(&target)
        })
}

/// Whether `TS_VERSION` satisfies a semver range: `||` alternatives of
/// space-separated comparators (`>=4.2 <5`, `*`, `~4.1`, `^4.1`, `4.2.x`). An
/// unparsable comparator never holds.
fn satisfies_ts(range: &str) -> bool {
    range.split("||").any(|alt| {
        let alt = alt.trim();
        alt.is_empty() || alt.split_whitespace().all(comparator_holds)
    })
}

fn comparator_holds(comparator: &str) -> bool {
    let (op, version) = ["<=", ">=", "<", ">", "=", "~", "^"]
        .into_iter()
        .find_map(|op| comparator.strip_prefix(op).map(|v| (op, v)))
        .unwrap_or(("", comparator));
    let mut parts: Vec<u64> = Vec::new();
    for part in version.trim_start_matches('v').split('.') {
        match part {
            "x" | "X" | "*" => break,
            _ => match part.parse() {
                Ok(n) => parts.push(n),
                Err(_) => return false,
            },
        }
    }
    let given = parts.len().min(3);
    let mut bound = [0u64; 3];
    bound[..given].copy_from_slice(&parts[..given]);
    let ts = TS_VERSION;
    let same_prefix = |n: usize| ts[..n] == bound[..n];
    // A partial version stands for every version it prefixes, so `>5` means
    // above all of 5.x: from 6.0.0 on, and `<=5` below 6.0.0.
    let past_partial = || {
        let mut next = bound;
        if given > 0 {
            next[given - 1] += 1;
        }
        next
    };
    match op {
        _ if given == 0 => matches!(op, "" | "=" | ">=" | "<=" | "~" | "^"),
        "<" => ts < bound,
        "<=" if given < 3 => ts < past_partial(),
        "<=" => ts <= bound,
        ">" if given < 3 => ts >= past_partial(),
        ">" => ts > bound,
        ">=" => ts >= bound,
        // `~1.2` allows patch updates, `~1` minor ones.
        "~" => ts >= bound && same_prefix(given.clamp(1, 2)),
        // `^1.2` allows anything below the next major (or minor, for `^0.x`).
        "^" => ts >= bound && same_prefix(if bound[0] == 0 { 2 } else { 1 }),
        // A bare or `=` version matches every version it is a prefix of.
        _ => same_prefix(given),
    }
}

/// `path` itself when it is a `.ts` file, or with a `.d.ts` / `/index.d.ts`
/// suffix, whichever exists.
fn declaration_file(path: &Path) -> Option<PathBuf> {
    let as_is = path.extension().is_some_and(|e| e == "ts").then_some("");
    as_is
        .into_iter()
        .chain([".d.ts", "/index.d.ts"])
        .find_map(|ext| {
            let mut p = path.as_os_str().to_os_string();
            p.push(ext);
            let p = PathBuf::from(p);
            p.is_file().then(|| p.canonicalize().unwrap_or(p))
        })
}

/// Locate the TypeScript declaration file for a bare package import (including
/// subpaths), so its exported types can be parsed. Returns the `.d.ts` path.
pub fn resolve_package_types(current_file: &Path, specifier: &str) -> Option<PathBuf> {
    let (pkg, subpath) = split_package_specifier(specifier);
    let pkg_dir = find_package_dir(current_file.parent()?, &pkg)?;
    let raw = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let in_package = |rel: String| {
        let candidate = pkg_dir.join(rel.trim_start_matches("./"));
        candidate
            .is_file()
            .then(|| candidate.canonicalize().unwrap_or(candidate))
    };
    if subpath.is_empty() {
        let rel = types_from_exports(&json, "")
            .or_else(|| {
                json.get("typings")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .or_else(|| {
                json.get("types")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "index.d.ts".to_string());
        return in_package(rel);
    }
    types_from_exports(&json, &subpath)
        .and_then(in_package)
        .or_else(|| types_from_versions(&json, &pkg_dir, &subpath))
        .or_else(|| declaration_file(&pkg_dir.join(&subpath)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_scoped_and_plain_specifiers() {
        assert_eq!(
            split_package_specifier("@algoan/aggregator-raw-data/budget-insight"),
            (
                "@algoan/aggregator-raw-data".to_string(),
                "budget-insight".to_string()
            )
        );
        assert_eq!(
            split_package_specifier("@algoan/aggregator-raw-data"),
            ("@algoan/aggregator-raw-data".to_string(), String::new())
        );
        assert_eq!(
            split_package_specifier("lodash/fp"),
            ("lodash".to_string(), "fp".to_string())
        );
        assert_eq!(package_name("lodash"), "lodash");
    }

    #[test]
    fn resolves_package_types_via_exports_and_subpath() {
        let dir = std::env::temp_dir().join(format!("nestoa-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("node_modules/@scope/lib");
        std::fs::create_dir_all(pkg.join("dist/sub")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{
                "name": "@scope/lib",
                "exports": {
                    ".": { "types": "./dist/index.d.ts", "default": "./dist/index.js" },
                    "./sub": { "types": "./dist/sub/index.d.ts" }
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("dist/index.d.ts"),
            "export interface Root { a: string; }",
        )
        .unwrap();
        std::fs::write(
            pkg.join("dist/sub/index.d.ts"),
            "export interface Sub { b: number; }",
        )
        .unwrap();

        // A consumer file sitting next to node_modules.
        let consumer = dir.join("src/file.ts");
        std::fs::create_dir_all(consumer.parent().unwrap()).unwrap();
        std::fs::write(&consumer, "").unwrap();

        let root = resolve_package_types(&consumer, "@scope/lib").expect("bare import");
        assert!(root.ends_with("dist/index.d.ts"), "got {:?}", root);
        let sub = resolve_package_types(&consumer, "@scope/lib/sub").expect("subpath import");
        assert!(sub.ends_with("dist/sub/index.d.ts"), "got {:?}", sub);
    }

    /// Write a package under a fresh temp dir and return a consumer file
    /// sitting next to its `node_modules`.
    fn package(name: &str, manifest: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nestoa-pkg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("node_modules/lib");
        for rel in files {
            let path = pkg.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "export interface X { a: string; }").unwrap();
        }
        std::fs::write(pkg.join("package.json"), manifest).unwrap();
        let consumer = dir.join("src/file.ts");
        std::fs::create_dir_all(consumer.parent().unwrap()).unwrap();
        std::fs::write(&consumer, "").unwrap();
        consumer
    }

    fn resolved(consumer: &Path, specifier: &str) -> Option<String> {
        let path = resolve_package_types(consumer, specifier)?;
        let pkg = consumer
            .parent()?
            .parent()?
            .join("node_modules/lib")
            .canonicalize()
            .ok()?;
        Some(path.strip_prefix(pkg).ok()?.to_string_lossy().into_owned())
    }

    #[test]
    fn exports_patterns_match_like_node() {
        let consumer = package(
            "exports",
            r#"{ "exports": {
                "./*": { "types": "./dist/*.d.ts" },
                "./*.js": { "types": "./js/*.d.ts" },
                "./deep/*": { "types": "./types/deep/*.d.ts" },
                "./hidden/*": null
            } }"#,
            &[
                "dist/a.d.ts",
                "types/deep/b.d.ts",
                "hidden/c.d.ts",
                "js/d.d.ts",
            ],
        );
        // Same prefix: the longer pattern wins, whatever the key order.
        assert_eq!(
            resolved(&consumer, "lib/d.js").as_deref(),
            Some("js/d.d.ts")
        );
        assert_eq!(resolved(&consumer, "lib/a").as_deref(), Some("dist/a.d.ts"));
        assert_eq!(
            resolved(&consumer, "lib/deep/b").as_deref(),
            Some("types/deep/b.d.ts")
        );
        // `null` excludes the subpath from `exports`; the plain layout still applies.
        assert_eq!(
            resolved(&consumer, "lib/hidden/c").as_deref(),
            Some("hidden/c.d.ts")
        );

        let sugar = package(
            "sugar",
            r#"{ "exports": { "types": "./main.d.ts" } }"#,
            &["main.d.ts"],
        );
        assert_eq!(resolved(&sugar, "lib").as_deref(), Some("main.d.ts"));
    }

    #[test]
    fn types_versions_ranges() {
        for (range, holds) in [
            ("*", true),
            (">=4.2", true),
            ("<4.0", false),
            (">=3.1 <4.0 || >=5", true),
            ("~5.9", true),
            ("~5.8", false),
            ("^5.1", true),
            ("^4.9", false),
            ("5.x", true),
            ("5.9", true),
            ("4", false),
            (">5", false),
            (">5.8", true),
            (">5.9", false),
            ("<=5", true),
            ("<=5.8", false),
            (">5.9.0", false),
            ("<=5.9.0", true),
            ("latest", false),
        ] {
            assert_eq!(satisfies_ts(range), holds, "{range}");
        }
    }

    #[test]
    fn types_versions_patterns_and_fallback_targets() {
        let consumer = package(
            "versions",
            r#"{ "typesVersions": {
                "<4.0": { "*": ["legacy/*"] },
                ">=4.2": {
                    "sub/*": ["missing/*", "types/sub/*"],
                    "*": ["dist/*"]
                }
            } }"#,
            &[
                "dist/a.d.ts",
                "dist/dir/index.d.ts",
                "types/sub/b.d.ts",
                "legacy/a.d.ts",
            ],
        );
        // `<4.0` comes first but is for an older TypeScript.
        assert_eq!(resolved(&consumer, "lib/a").as_deref(), Some("dist/a.d.ts"));
        assert_eq!(
            resolved(&consumer, "lib/dir").as_deref(),
            Some("dist/dir/index.d.ts")
        );
        assert_eq!(
            resolved(&consumer, "lib/sub/b").as_deref(),
            Some("types/sub/b.d.ts")
        );
    }
}
