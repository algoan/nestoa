use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use swc_ecma_ast::{
    ClassDecl, Decl, DefaultDecl, ExportDecl, ExportDefaultDecl, ExportSpecifier, ImportDecl,
    ImportSpecifier, ModuleDecl, ModuleItem, Stmt, TsEnumDecl, TsInterfaceDecl, TsTypeAliasDecl,
};

use crate::parse::ParsedFile;

/// One exported (or top-level) named declaration in a file. Indexed by name
/// within the file.
#[derive(Clone)]
pub enum Symbol {
    Interface(Arc<TsInterfaceDecl>),
    TypeAlias(Arc<TsTypeAliasDecl>),
    Enum(Arc<TsEnumDecl>),
    Class(Arc<ClassDecl>),
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
        if let Some(binding) = fs.imports.get(ns).or_else(|| fs.re_exports.get(ns)) {
            let target = self.resolve_module(file, &binding.specifier)?;
            match &binding.imported {
                // `import * as Ns from "spec"` — the target module is the namespace.
                None if binding.is_namespace => Some(target),
                None => None, // default import — not a namespace
                // `import { Ns } from "spec"` — find namespace `Ns` in the target.
                Some(imported) => self.resolve_namespace_module(&target, imported, visited),
            }
        } else {
            None
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
        // `None` once an unsupported initialiser makes the implicit value
        // unknowable; a later numeric initialiser restores it.
        let mut next = Some(0.0);
        for m in &e.members {
            let value = match m.init.as_deref() {
                Some(init) => numeric_expr(init),
                None => next,
            };
            let name = match &m.id {
                swc_ecma_ast::TsEnumMemberId::Ident(i) => i.sym.to_string(),
                swc_ecma_ast::TsEnumMemberId::Str(s) => s.value.to_string_lossy().into_owned(),
            };
            if name == member {
                return value;
            }
            next = value.map(|v| v + 1.0);
        }
        None
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
                                }
                                // Bare `export { X }` (no src) — X is already in declared/imports.
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

/// Walk up from `current_file` looking for `node_modules/<pkg>`.
fn find_package_dir(current_file: &Path, pkg: &str) -> Option<PathBuf> {
    let mut dir = current_file.parent();
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

/// Resolve a package subpath to its declaration entry via the `exports` map,
/// honouring exact keys and a trailing `/*` wildcard.
fn types_from_exports(json: &serde_json::Value, subpath: &str) -> Option<String> {
    let exports = json.get("exports")?;
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{}", subpath)
    };
    if let Some(v) = exports.get(&key)
        && let Some(found) = dts_from_exports_value(v)
    {
        return Some(found);
    }
    // Wildcard match: `./foo/*` with a `*` in the target path.
    if let serde_json::Value::Object(map) = exports {
        for (k, v) in map {
            if let Some(prefix) = k.strip_suffix("/*") {
                let prefix = prefix.trim_start_matches("./");
                if let Some(rest) = subpath
                    .strip_prefix(prefix)
                    .and_then(|r| r.strip_prefix('/'))
                    && let Some(tmpl) = dts_from_exports_value(v)
                {
                    return Some(tmpl.replace('*', rest));
                }
            }
        }
    }
    None
}

/// Resolve a subpath via the legacy `typesVersions` map.
fn types_from_versions(json: &serde_json::Value, subpath: &str) -> Option<String> {
    let star = json.get("typesVersions")?.get("*")?;
    if let Some(arr) = star.get(subpath).and_then(|v| v.as_array())
        && let Some(first) = arr.first().and_then(|v| v.as_str())
    {
        return Some(first.to_string());
    }
    None
}

/// Locate the TypeScript declaration file for a bare package import (including
/// subpaths), so its exported types can be parsed. Returns the `.d.ts` path.
pub fn resolve_package_types(current_file: &Path, specifier: &str) -> Option<PathBuf> {
    let (pkg, subpath) = split_package_specifier(specifier);
    let pkg_dir = find_package_dir(current_file, &pkg)?;
    let raw = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let rel = if subpath.is_empty() {
        types_from_exports(&json, "")
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
            .unwrap_or_else(|| "index.d.ts".to_string())
    } else {
        types_from_exports(&json, &subpath)
            .or_else(|| types_from_versions(&json, &subpath))
            .or_else(|| {
                [
                    format!("{}.d.ts", subpath),
                    format!("{}/index.d.ts", subpath),
                ]
                .into_iter()
                .find(|cand| pkg_dir.join(cand).is_file())
            })?
    };

    let candidate = pkg_dir.join(rel.trim_start_matches("./"));
    if candidate.is_file() {
        return Some(candidate.canonicalize().unwrap_or(candidate));
    }
    None
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
}
