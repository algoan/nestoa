use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use swc_ecma_ast::{
    Class, Decl, DefaultDecl, ExportDecl, ExportDefaultDecl, ExportSpecifier, Ident, ImportDecl,
    ImportSpecifier, Module, ModuleDecl, ModuleItem, Stmt, TsEnumDecl, TsInterfaceDecl,
    TsTypeAliasDecl,
};

use crate::parse::ParsedFile;
use crate::tsconfig::best_match;

/// One exported (or top-level) named declaration in a file. Indexed by name
/// within the file.
#[derive(Clone)]
pub enum Symbol {
    Interface(Interface),
    TypeAlias(Shared<TsTypeAliasDecl>),
    Enum(Shared<TsEnumDecl>),
    Class(Shared<Class>),
}

impl Symbol {
    /// The name the declaration itself was given, whatever it is imported as.
    pub fn name(&self) -> String {
        match self {
            Symbol::Interface(i) => i.id.sym.to_string(),
            Symbol::TypeAlias(t) => t.id.sym.to_string(),
            Symbol::Enum(e) => e.id.sym.to_string(),
            Symbol::Class(c) => {
                class_of(c.item()).map_or_else(String::new, |(id, _)| id.sym.to_string())
            }
        }
    }
}

/// Every declaration of one interface in a file, in source order, which
/// TypeScript merges into one. Dereferences to the first.
#[derive(Clone)]
pub struct Interface(Vec<Shared<TsInterfaceDecl>>);

impl Interface {
    pub fn decls(&self) -> impl Iterator<Item = &TsInterfaceDecl> {
        self.0.iter().map(|d| &**d)
    }
}

impl std::ops::Deref for Interface {
    type Target = TsInterfaceDecl;

    fn deref(&self) -> &TsInterfaceDecl {
        &self.0[0]
    }
}

/// A top-level declaration of a parsed module, shared with the module rather
/// than copied out of it. Dereferences to the declaration.
#[derive(Clone)]
pub struct Shared<T: 'static> {
    module: Arc<Module>,
    index: usize,
    get: fn(&ModuleItem) -> Option<&T>,
}

impl<T> Shared<T> {
    fn item(&self) -> &ModuleItem {
        &self.module.body[self.index]
    }
}

impl<T> std::ops::Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        (self.get)(self.item()).expect("a symbol's module item holds its declaration")
    }
}

fn declaration(item: &ModuleItem) -> Option<&Decl> {
    match item {
        ModuleItem::Stmt(Stmt::Decl(decl))
        | ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl { decl, .. })) => Some(decl),
        _ => None,
    }
}

/// A named class declaration, `export default class X` included.
fn class_of(item: &ModuleItem) -> Option<(&Ident, &Class)> {
    match (declaration(item), item) {
        (Some(Decl::Class(c)), _) => Some((&c.ident, &c.class)),
        (
            None,
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(ExportDefaultDecl {
                decl: DefaultDecl::Class(c),
                ..
            })),
        ) => Some((c.ident.as_ref()?, &c.class)),
        _ => None,
    }
}

#[derive(Clone)]
pub struct ImportBinding {
    /// The exported name on the other side (`default` for a default import).
    /// None for a namespace import.
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
    /// Declarations and imports exported under their own name (`export class
    /// X`, `export { X }`). Unused for a declaration file, which exports all.
    pub exported: std::collections::HashSet<String>,
    pub is_declaration_file: bool,
}

impl FileSymbols {
    /// Whether another module importing `name` from this one sees this
    /// module's own declaration or import of it.
    fn exports(&self, name: &str) -> bool {
        self.is_declaration_file || self.exported.contains(name)
    }
}

/// All files indexed by path.
#[derive(Default)]
pub struct Project {
    pub files: HashMap<PathBuf, ParsedFile>,
    pub symbols: HashMap<PathBuf, FileSymbols>,
    /// TypeScript `compilerOptions.paths` aliases, used to resolve workspace
    /// package imports (e.g. `@algoan/core`) to in-repo source.
    pub tsconfig_paths: Option<crate::tsconfig::TsPaths>,
    pub resolver: Resolver,
}

impl Project {
    pub fn add(&mut self, file: ParsedFile) {
        let fs = collect_file_symbols(&file);
        self.symbols.insert(file.path.clone(), fs);
        self.files.insert(file.path.clone(), file);
    }

    /// Resolve a bare specifier through tsconfig `paths` aliases, if any.
    pub fn resolve_alias(&self, specifier: &str) -> Option<PathBuf> {
        memo(&self.resolver.aliases, specifier.to_string(), |s| {
            self.tsconfig_paths.as_ref()?.resolve(s)
        })
    }

    /// Resolve a module specifier (relative, tsconfig-aliased, or node_modules
    /// package) to a file in the symbol table.
    fn resolve_module(&self, file: &Path, specifier: &str) -> Option<PathBuf> {
        if specifier.starts_with('.') {
            return self.resolver.relative(file, specifier);
        }
        self.resolve_alias(specifier)
            .or_else(|| self.resolver.package_types(file, specifier))
    }

    /// The in-repo file a specifier names: relative, or through tsconfig
    /// `paths`, which TypeScript never applies to a relative specifier.
    fn local_module(&self, file: &Path, specifier: &str) -> Option<PathBuf> {
        if specifier.starts_with('.') {
            self.resolver.relative(file, specifier)
        } else {
            self.resolve_alias(specifier)
        }
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
        self.resolve_inner(&module, member, true, &mut std::collections::HashSet::new())
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
                None => Some(target),
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
        self.local_module(file, specifier)
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
        match self.local_module(file, specifier) {
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
        self.resolve_inner(file, name, false, &mut visited)
    }

    /// Resolve a numeric `const` by name, following imports and re-exports the
    /// same way type resolution does. Used for decorator arguments that name a
    /// constant, e.g. `@HttpCode(MULTI_STATUS)` where `MULTI_STATUS = 207` is
    /// declared in (and re-exported from) another module.
    pub fn resolve_const_number(&self, file: &Path, name: &str) -> Option<f64> {
        let mut visited: std::collections::HashSet<(PathBuf, String)> =
            std::collections::HashSet::new();
        self.resolve_const_inner(file, name, false, &mut visited)
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

    /// `from_importer`: `name` is looked up as an export of `file`, so a
    /// declaration or import it doesn't export is skipped.
    fn resolve_const_inner(
        &self,
        file: &Path,
        name: &str,
        from_importer: bool,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<f64> {
        if !visited.insert((file.to_path_buf(), name.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        let visible = !from_importer || fs.exports(name);
        if visible && let Some(v) = fs.const_numbers.get(name) {
            return Some(*v);
        }
        if let Some(local) = fs.local_exports.get(name) {
            return self.resolve_const_inner(file, local, false, visited);
        }
        if let Some(binding) = fs
            .re_exports
            .get(name)
            .or_else(|| fs.imports.get(name).filter(|_| visible))
            && let Some(target) = self.resolve_module(file, &binding.specifier)
        {
            let next_name = binding.imported.clone().unwrap_or_else(|| name.to_string());
            if let Some(found) = self.resolve_const_inner(&target, &next_name, true, visited) {
                return Some(found);
            }
        }
        for spec in &fs.star_re_exports {
            if let Some(target) = self.resolve_module(file, spec)
                && let Some(found) = self.resolve_const_inner(&target, name, true, visited)
            {
                return Some(found);
            }
        }
        None
    }

    /// `from_importer` as in `resolve_const_inner`.
    fn resolve_inner(
        &self,
        file: &Path,
        name: &str,
        from_importer: bool,
        visited: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Option<(PathBuf, Symbol)> {
        if !visited.insert((file.to_path_buf(), name.to_string())) {
            return None;
        }
        let fs = self.symbols.get(file)?;
        let visible = !from_importer || fs.exports(name);
        if visible && let Some(sym) = fs.declared.get(name) {
            return Some((file.to_path_buf(), sym.clone()));
        }
        if let Some(local) = fs.local_exports.get(name) {
            return self.resolve_inner(file, local, false, visited);
        }
        if let Some(binding) = fs
            .re_exports
            .get(name)
            .or_else(|| fs.imports.get(name).filter(|_| visible))
            && let Some(target) = self.resolve_module(file, &binding.specifier)
        {
            let next_name = binding.imported.clone().unwrap_or_else(|| name.to_string());
            if let Some(found) = self.resolve_inner(&target, &next_name, true, visited) {
                return Some(found);
            }
        }
        // `export * from './other'` — search each one.
        for spec in &fs.star_re_exports {
            if let Some(target) = self.resolve_module(file, spec)
                && let Some(found) = self.resolve_inner(&target, name, true, visited)
            {
                return Some(found);
            }
        }
        None
    }
}

pub fn collect_file_symbols(parsed: &ParsedFile) -> FileSymbols {
    let mut fs = FileSymbols {
        is_declaration_file: crate::parse::is_declaration_file(&parsed.path),
        ..FileSymbols::default()
    };
    for (index, item) in parsed.module.body.iter().enumerate() {
        if let Some((name, symbol)) = declared_symbol(&parsed.module, index) {
            match (fs.declared.get_mut(&name), symbol) {
                (Some(Symbol::Interface(merged)), Symbol::Interface(more)) => {
                    merged.0.extend(more.0);
                }
                (_, symbol) => {
                    fs.declared.insert(name, symbol);
                }
            }
        }
        if let Some(name) = default_export_name(item) {
            fs.local_exports.insert("default".to_string(), name);
        }
        match item {
            ModuleItem::Stmt(Stmt::Decl(decl)) => collect_consts(decl, &mut fs),
            ModuleItem::ModuleDecl(md) => match md {
                ModuleDecl::Import(i) => collect_import(i, &mut fs),
                ModuleDecl::ExportDecl(ExportDecl { decl, .. }) => {
                    collect_consts(decl, &mut fs);
                    fs.exported.extend(declared_names(decl));
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
                                } else {
                                    fs.exported.insert(orig_name);
                                }
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

/// The names a declaration introduces.
fn declared_names(decl: &Decl) -> Vec<String> {
    match decl {
        Decl::Class(c) => vec![c.ident.sym.to_string()],
        Decl::TsInterface(i) => vec![i.id.sym.to_string()],
        Decl::TsTypeAlias(t) => vec![t.id.sym.to_string()],
        Decl::TsEnum(e) => vec![e.id.sym.to_string()],
        Decl::Var(v) => v
            .decls
            .iter()
            .filter_map(|d| d.name.as_ident().map(|i| i.id.sym.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The local name `export default …` exports: a named class or interface
/// declaration, or an identifier (`export default Account;`, or a CommonJS
/// declaration file's `export = Account;`).
fn default_export_name(item: &ModuleItem) -> Option<String> {
    match item {
        ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(e)) => match &e.decl {
            DefaultDecl::Class(c) => c.ident.as_ref().map(|i| i.sym.to_string()),
            DefaultDecl::TsInterfaceDecl(i) => Some(i.id.sym.to_string()),
            DefaultDecl::Fn(_) => None,
        },
        ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(
            swc_ecma_ast::ExportDefaultExpr { expr, .. },
        ))
        | ModuleItem::ModuleDecl(ModuleDecl::TsExportAssignment(
            swc_ecma_ast::TsExportAssignment { expr, .. },
        )) => match &**expr {
            swc_ecma_ast::Expr::Ident(i) => Some(i.sym.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// The named class, interface, type alias or enum `module.body[index]`
/// declares, if any.
fn declared_symbol(module: &Arc<Module>, index: usize) -> Option<(String, Symbol)> {
    fn share<T>(
        module: &Arc<Module>,
        index: usize,
        get: fn(&ModuleItem) -> Option<&T>,
    ) -> Shared<T> {
        Shared {
            module: module.clone(),
            index,
            get,
        }
    }
    fn class(item: &ModuleItem) -> Option<&Class> {
        class_of(item).map(|(_, c)| c)
    }
    fn interface(item: &ModuleItem) -> Option<&TsInterfaceDecl> {
        match (declaration(item), item) {
            (Some(Decl::TsInterface(i)), _) => Some(i),
            (
                None,
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(ExportDefaultDecl {
                    decl: DefaultDecl::TsInterfaceDecl(i),
                    ..
                })),
            ) => Some(i),
            _ => None,
        }
    }
    fn alias(item: &ModuleItem) -> Option<&TsTypeAliasDecl> {
        match declaration(item)? {
            Decl::TsTypeAlias(t) => Some(t),
            _ => None,
        }
    }
    fn enumeration(item: &ModuleItem) -> Option<&TsEnumDecl> {
        match declaration(item)? {
            Decl::TsEnum(e) => Some(e),
            _ => None,
        }
    }
    let item = &module.body[index];
    if let Some((ident, _)) = class_of(item) {
        let symbol = Symbol::Class(share(module, index, class));
        return Some((ident.sym.to_string(), symbol));
    }
    if let Some(i) = interface(item) {
        let symbol = Symbol::Interface(Interface(vec![share(module, index, interface)]));
        return Some((i.id.sym.to_string(), symbol));
    }
    Some(match declaration(item)? {
        Decl::TsTypeAlias(t) => (
            t.id.sym.to_string(),
            Symbol::TypeAlias(share(module, index, alias)),
        ),
        Decl::TsEnum(e) => (
            e.id.sym.to_string(),
            Symbol::Enum(share(module, index, enumeration)),
        ),
        _ => return None,
    })
}

/// Record numeric `const NAME = <literal>` so decorator arguments that
/// reference the constant (e.g. `@HttpCode(MULTI_STATUS)`) can be resolved to
/// their value, including across module boundaries.
fn collect_consts(decl: &Decl, fs: &mut FileSymbols) {
    let Decl::Var(v) = decl else { return };
    for d in &v.decls {
        if let (swc_ecma_ast::Pat::Ident(id), Some(init)) = (&d.name, &d.init)
            && let Some(n) = numeric_expr(init)
        {
            fs.const_numbers.insert(id.id.sym.to_string(), n);
        }
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
                        imported: Some("default".to_string()),
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

/// Memoised module resolution: the same modules are imported from many files,
/// and each lookup stats candidate files or reads a `package.json`.
#[derive(Default)]
pub struct Resolver {
    /// Relative import target -> the module file it loads.
    relative: RefCell<HashMap<PathBuf, Option<PathBuf>>>,
    /// tsconfig `paths` specifier -> the module file it loads.
    aliases: RefCell<HashMap<String, Option<PathBuf>>>,
    /// `(directory, package)` -> the nearest `node_modules/<package>` from it.
    package_dirs: RefCell<HashMap<(PathBuf, String), Option<PathBuf>>>,
    /// `(package directory, subpath, import mode)` -> its declaration file.
    package_entries: RefCell<HashMap<(PathBuf, String, ImportMode), Option<PathBuf>>>,
}

impl Resolver {
    /// Resolve `./other` relative to `current_file` to an absolute `.ts` path.
    /// Returns None for package imports (e.g. `@nestjs/common`).
    pub fn relative(&self, current_file: &Path, specifier: &str) -> Option<PathBuf> {
        if !specifier.starts_with('.') {
            return None;
        }
        let candidate = current_file.parent()?.join(specifier);
        memo(&self.relative, candidate, |c| module_file(c))
    }

    /// Locate the TypeScript declaration file for a bare package import
    /// (including subpaths), so its exported types can be parsed.
    pub fn package_types(&self, current_file: &Path, specifier: &str) -> Option<PathBuf> {
        let (pkg, subpath) = split_package_specifier(specifier);
        let pkg_dir = self.package_dir(current_file.parent()?, &pkg)?;
        let key = (pkg_dir, subpath, ImportMode::of(current_file));
        memo(&self.package_entries, key, |(dir, subpath, mode)| {
            package_entry(dir, subpath, *mode)
        })
    }

    /// `find_package_dir`, memoised per directory on the way up.
    fn package_dir(&self, dir: &Path, pkg: &str) -> Option<PathBuf> {
        memo(
            &self.package_dirs,
            (dir.to_path_buf(), pkg.to_string()),
            |_| {
                let candidate = dir.join("node_modules").join(pkg);
                if candidate.is_dir() {
                    Some(candidate)
                } else {
                    self.package_dir(dir.parent()?, pkg)
                }
            },
        )
    }
}

fn memo<K: Eq + Hash, V: Clone>(
    cache: &RefCell<HashMap<K, V>>,
    key: K,
    compute: impl FnOnce(&K) -> V,
) -> V {
    if let Some(hit) = cache.borrow().get(&key) {
        return hit.clone();
    }
    let value = compute(&key);
    cache.borrow_mut().insert(key, value.clone());
    value
}

/// The file an import of `candidate` loads: its TypeScript counterpart when it
/// names a JavaScript file, else with the usual extensions or `/index` appended,
/// else the path itself.
pub(crate) fn module_file(candidate: &Path) -> Option<PathBuf> {
    substituted_file(candidate)
        .or_else(|| {
            [
                ".ts",
                ".tsx",
                ".d.ts",
                "/index.ts",
                "/index.tsx",
                "/index.d.ts",
            ]
            .into_iter()
            .find_map(|ext| existing(with_suffix(candidate, ext)))
        })
        .or_else(|| existing(candidate.to_path_buf()))
}

/// The TypeScript file a `.js`/`.mjs`/`.cjs`/`.jsx` path stands for, as
/// TypeScript substitutes extensions (`a.mjs` -> `a.mts`, then `a.d.mts`).
fn substituted_file(path: &Path) -> Option<PathBuf> {
    const SUBSTITUTIONS: [(&str, &[&str]); 4] = [
        (".mjs", &[".mts", ".d.mts"]),
        (".cjs", &[".cts", ".d.cts"]),
        (".jsx", &[".tsx", ".d.ts"]),
        (".js", &[".ts", ".tsx", ".d.ts"]),
    ];
    let name = path.to_str()?;
    let (stem, replacements) = SUBSTITUTIONS
        .iter()
        .find_map(|(js, ts)| Some((name.strip_suffix(js)?, *ts)))?;
    replacements
        .iter()
        .find_map(|ext| existing(PathBuf::from(format!("{stem}{ext}"))))
}

/// `path` itself when it names a TypeScript file, or its substituted
/// TypeScript file when it names a JavaScript one, whichever exists.
fn typescript_file(path: &Path) -> Option<PathBuf> {
    let is_ts = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "ts" | "tsx" | "mts" | "cts"));
    if is_ts {
        existing(path.to_path_buf())
    } else {
        substituted_file(path)
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(suffix);
    PathBuf::from(p)
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then(|| path.canonicalize().unwrap_or(path))
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

/// What an `exports` entry value resolves to.
enum ExportsTarget {
    Found(PathBuf),
    /// No applicable condition or alternative yields an existing file.
    Unmatched,
    /// A `null` was reached first: the entry is excluded.
    Blocked,
}

/// Resolve a package.json `exports` entry value to a TypeScript file the way
/// TypeScript walks it: conditions and array alternatives in declared order,
/// the first that yields a file wins, and a `null` reached first blocks the
/// whole entry, however deeply nested.
fn dts_from_exports_value(
    v: &serde_json::Value,
    mode: ImportMode,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> ExportsTarget {
    let candidates: Box<dyn Iterator<Item = &serde_json::Value>> = match v {
        serde_json::Value::Null => return ExportsTarget::Blocked,
        serde_json::Value::String(target) => {
            return resolve(target).map_or(ExportsTarget::Unmatched, ExportsTarget::Found);
        }
        serde_json::Value::Array(alternatives) => Box::new(alternatives.iter()),
        serde_json::Value::Object(map) => Box::new(
            map.iter()
                .filter(|(k, _)| condition_applies(k, mode))
                .map(|(_, v)| v),
        ),
        _ => return ExportsTarget::Unmatched,
    };
    for v in candidates {
        match dts_from_exports_value(v, mode, resolve) {
            ExportsTarget::Unmatched => {}
            settled => return settled,
        }
    }
    ExportsTarget::Unmatched
}

/// The module format of an importing file, as far as its extension tells.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ImportMode {
    Esm,
    CommonJs,
    /// A `.ts` file's format depends on tsconfig and the nearest `package.json`.
    Unknown,
}

impl ImportMode {
    fn of(importer: &Path) -> Self {
        match importer.extension().and_then(|e| e.to_str()) {
            Some("mts" | "mjs") => Self::Esm,
            Some("cts" | "cjs") => Self::CommonJs,
            _ => Self::Unknown,
        }
    }
}

/// The `exports` conditions TypeScript enables when resolving declarations:
/// `import` or `require` by the importer's format, both when it is unknown.
fn condition_applies(condition: &str, mode: ImportMode) -> bool {
    match condition.strip_prefix("types@") {
        Some(range) => satisfies_ts(range),
        None => match condition {
            "types" | "node" | "default" => true,
            "import" => mode != ImportMode::CommonJs,
            "require" => mode != ImportMode::Esm,
            _ => false,
        },
    }
}

/// Resolve a package subpath to its declaration entry via the `exports` map:
/// the best-matching key (exact, then the longest `*` pattern prefix, as Node
/// picks it), its `*` substituted into the target, which must exist. A blocked
/// entry gives `None` like an unmatched one, so the package layout still
/// applies: node10 resolution ignores `exports`, and nestoa doesn't read
/// `moduleResolution`.
fn types_from_exports(
    json: &serde_json::Value,
    pkg_dir: &Path,
    subpath: &str,
    mode: ImportMode,
) -> Option<PathBuf> {
    let exports = json.get("exports")?;
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    let target_in = |target: &str, captured: &str| {
        typescript_file(&pkg_dir.join(target.replace('*', captured).trim_start_matches("./")))
    };
    // A string, array or conditions object is shorthand for `{ ".": … }`.
    let shorthand = match exports {
        serde_json::Value::Object(map) => !map.keys().any(|k| k.starts_with('.')),
        _ => true,
    };
    let (value, captured) = if shorthand {
        (key == ".").then(|| (exports, String::new()))?
    } else {
        let map = exports.as_object()?;
        best_match(map.iter().map(|(k, v)| (k.as_str(), v)), &key, true)?
    };
    match dts_from_exports_value(value, mode, &|t| target_in(t, &captured)) {
        ExportsTarget::Found(path) => Some(path),
        ExportsTarget::Unmatched | ExportsTarget::Blocked => None,
    }
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

/// `path` itself when it is a TypeScript file, its substituted TypeScript file
/// when it is a JavaScript one, or with a `.d.ts` / `/index.d.ts` suffix,
/// whichever exists.
fn declaration_file(path: &Path) -> Option<PathBuf> {
    typescript_file(path).or_else(|| {
        [".d.ts", "/index.d.ts"]
            .into_iter()
            .find_map(|ext| existing(with_suffix(path, ext)))
    })
}

/// The declaration file a package's `subpath` (empty for its root) resolves
/// to, from its `package.json`.
fn package_entry(pkg_dir: &Path, subpath: &str, mode: ImportMode) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let in_package = |rel: &str| declaration_file(&pkg_dir.join(rel.trim_start_matches("./")));
    if subpath.is_empty() {
        if let Some(found) = types_from_exports(&json, pkg_dir, "", mode) {
            return Some(found);
        }
        // Without `typings`/`types`, TypeScript looks for the declaration next
        // to `main`; `typesVersions` remaps whichever entry it picks, and the
        // package-root index after it.
        let entry = ["typings", "types", "main"]
            .into_iter()
            .find_map(|k| json.get(k)?.as_str());
        let remapped = |rel: &str| {
            types_from_versions(&json, pkg_dir, rel.trim_start_matches("./"))
                .or_else(|| in_package(rel))
        };
        return entry.and_then(remapped).or_else(|| remapped("index.d.ts"));
    }
    types_from_exports(&json, pkg_dir, subpath, mode)
        .or_else(|| types_from_versions(&json, pkg_dir, subpath))
        .or_else(|| declaration_file(&pkg_dir.join(subpath)))
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

        let root = Resolver::default()
            .package_types(&consumer, "@scope/lib")
            .expect("bare import");
        assert!(root.ends_with("dist/index.d.ts"), "got {:?}", root);
        let sub = Resolver::default()
            .package_types(&consumer, "@scope/lib/sub")
            .expect("subpath import");
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

    /// Write `files` under a fresh temp dir and parse every `.ts` among them
    /// into a project. Returns the (canonical) dir and the project.
    fn project(name: &str, files: &[(&str, &str)]) -> (PathBuf, Project) {
        let dir = std::env::temp_dir().join(format!("nestoa-sym-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, body) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        let parser = crate::parse::ParsingContext::default();
        let mut project = Project::default();
        for (rel, _) in files {
            if rel.ends_with(".ts") {
                project.add(parser.parse_file(&dir.join(rel)).unwrap());
            }
        }
        (dir, project)
    }

    /// The name of the declaration `name` resolves to from `file`.
    fn resolved_name(project: &Project, file: &Path, name: &str) -> Option<String> {
        project.resolve(file, name).map(|(_, s)| s.name())
    }

    #[test]
    fn tsconfig_paths_never_apply_to_relative_specifiers() {
        let (dir, mut project) = project(
            "relative-paths",
            &[
                ("src/app.ts", "import { Ghost } from './ghost';"),
                (
                    "src/types/ghost.ts",
                    "export interface Ghost { a: string; }",
                ),
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "paths": { "*": ["src/types/*"] } } }"#,
                ),
            ],
        );
        project.tsconfig_paths = crate::tsconfig::load_tsconfig_paths(&dir, None);
        assert!(project.resolve_alias("ghost").is_some());
        assert_eq!(
            resolved_name(&project, &dir.join("src/app.ts"), "Ghost"),
            None
        );
    }

    #[test]
    fn default_imports_and_exports_resolve() {
        let (dir, project) = project(
            "default",
            &[
                ("a.dto.ts", "export default class Account { id: string; }"),
                (
                    "card.ts",
                    "export default interface Card { last4: string; }",
                ),
                (
                    "plain.ts",
                    "class Plain { p: string; }\nexport default Plain;",
                ),
                (
                    "legacy.d.ts",
                    "declare class Legacy { l: string; }\nexport = Legacy;",
                ),
                (
                    "barrel.ts",
                    "export { default as Owner } from './a.dto';\nexport { default } from './card';",
                ),
                (
                    "app.ts",
                    "import Acc from './a.dto';\nimport C from './card';\nimport P from './plain';\n\
                     import L from './legacy';\nimport Def, { Owner } from './barrel';",
                ),
            ],
        );
        let app = dir.join("app.ts");
        let names: Vec<_> = ["Acc", "C", "P", "L", "Owner", "Def"]
            .iter()
            .map(|n| resolved_name(&project, &app, n))
            .collect();
        assert_eq!(
            names,
            ["Account", "Card", "Plain", "Legacy", "Account", "Card"].map(|n| Some(n.to_string()))
        );
    }

    #[test]
    fn importers_see_only_what_a_module_exports() {
        let (dir, project) = project(
            "exported",
            &[
                ("real.ts", "export interface Hidden { real: string; }"),
                (
                    "barrel.ts",
                    "interface Hidden { wrong: string; }\nexport * from './real';\n\
                     const LIMIT = 1;\nexport * from './limits';",
                ),
                ("limits.ts", "export const LIMIT = 2;"),
                ("app.ts", "import { Hidden, LIMIT } from './barrel';"),
                ("ambient.d.ts", "declare class Ambient { a: string; }"),
                ("uses-ambient.ts", "import { Ambient } from './ambient';"),
            ],
        );
        let app = dir.join("app.ts");
        let (file, _) = project.resolve(&app, "Hidden").expect("Hidden resolves");
        assert_eq!(file, dir.join("real.ts"));
        assert_eq!(project.resolve_const_number(&app, "LIMIT"), Some(2.0));
        assert_eq!(
            resolved_name(&project, &dir.join("uses-ambient.ts"), "Ambient"),
            Some("Ambient".to_string())
        );
        // Inside its own module, the local declaration still wins.
        let (file, _) = project.resolve(&dir.join("barrel.ts"), "Hidden").unwrap();
        assert_eq!(file, dir.join("barrel.ts"));
    }

    #[test]
    fn symbols_share_the_parsed_declarations() {
        let dir = std::env::temp_dir().join(format!("nestoa-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dto.ts");
        std::fs::write(
            &file,
            "import { X } from './x';\n\
             export default class Account<T> { id: string; }\n\
             export interface Owner { name: string; }\n\
             type Id = string;\n\
             export enum Kind { A = 'a' }\n",
        )
        .unwrap();
        let mut project = Project::default();
        project.add(
            crate::parse::ParsingContext::default()
                .parse_file(&file)
                .unwrap(),
        );

        let Some((_, Symbol::Class(class))) = project.resolve(&file, "Account") else {
            panic!("default-exported class");
        };
        assert!(class.type_params.is_some());
        let names: Vec<_> = ["Account", "Owner", "Id", "Kind"]
            .iter()
            .map(|n| project.resolve(&file, n).map(|(_, s)| s.name()))
            .collect();
        assert_eq!(
            names,
            ["Account", "Owner", "Id", "Kind"].map(|n| Some(n.to_string()))
        );
    }

    #[test]
    fn resolver_reads_a_package_once_for_every_importer() {
        let consumer = package("memo", r#"{ "types": "./index.d.ts" }"#, &["index.d.ts"]);
        let nested = consumer.parent().unwrap().join("deep/file.ts");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        let resolver = Resolver::default();
        let first = resolver.package_types(&consumer, "lib").expect("resolved");

        let pkg = consumer
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("node_modules");
        std::fs::remove_dir_all(pkg).unwrap();
        assert_eq!(resolver.package_types(&nested, "lib"), Some(first));
        assert_eq!(resolver.relative(&consumer, "./gone"), None);
        std::fs::write(consumer.with_file_name("gone.ts"), "").unwrap();
        assert_eq!(resolver.relative(&consumer, "./gone"), None);
    }

    fn resolved(consumer: &Path, specifier: &str) -> Option<String> {
        let path = Resolver::default().package_types(consumer, specifier)?;
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

        let versioned = package(
            "versioned",
            r#"{ "exports": { ".": {
                "types@<4.0": "./old.d.ts",
                "types@>=5.9": "./new.d.ts",
                "types": "./main.d.ts"
            } } }"#,
            &["old.d.ts", "new.d.ts", "main.d.ts"],
        );
        assert_eq!(resolved(&versioned, "lib").as_deref(), Some("new.d.ts"));

        let blocked = package(
            "blocked",
            r#"{ "typings": "./legacy.d.ts", "exports": { ".": {
                "types@>=5.9": null,
                "types": "./main.d.ts"
            } } }"#,
            &["legacy.d.ts", "main.d.ts"],
        );
        assert_eq!(resolved(&blocked, "lib").as_deref(), Some("legacy.d.ts"));

        // A `null` blocks the entry from inside an array or a nested condition too.
        let nested = package(
            "nested",
            r#"{ "typings": "./legacy.d.ts", "exports": {
                ".": { "import": { "types@>=5.9": null }, "types": "./main.d.ts" },
                "./arr": [null, "./main.d.ts"]
            } }"#,
            &["legacy.d.ts", "main.d.ts"],
        );
        assert_eq!(resolved(&nested, "lib").as_deref(), Some("legacy.d.ts"));
        assert_eq!(resolved(&nested, "lib/arr"), None);

        let ordered = package(
            "ordered",
            r#"{ "exports": { ".": {
                "browser": "./browser.d.ts",
                "import": { "types": "./esm.d.ts" },
                "types": "./main.d.ts"
            } } }"#,
            &["browser.d.ts", "esm.d.ts", "main.d.ts"],
        );
        assert_eq!(resolved(&ordered, "lib").as_deref(), Some("esm.d.ts"));

        let sugar = package(
            "sugar",
            r#"{ "exports": { "types": "./main.d.ts" } }"#,
            &["main.d.ts"],
        );
        assert_eq!(resolved(&sugar, "lib").as_deref(), Some("main.d.ts"));
    }

    #[test]
    fn javascript_targets_resolve_to_their_declarations() {
        let substituted = package(
            "substituted",
            r#"{ "exports": {
                ".": { "import": "./dist/index.js", "types": "./legacy/index.d.ts" },
                "./esm": { "import": "./esm/index.mjs", "require": "./cjs/index.cjs" },
                "./cjs": { "import": "./esm/missing.mjs", "require": "./cjs/index.cjs" },
                "./gone": { "types": "./missing.d.ts", "default": "./dist/index.js" },
                "./*": "./dist/*.js"
            } }"#,
            &[
                "dist/index.d.ts",
                "legacy/index.d.ts",
                "esm/index.d.mts",
                "cjs/index.d.cts",
                "dist/a.d.ts",
            ],
        );
        for (specifier, expected) in [
            ("lib", "dist/index.d.ts"),
            ("lib/esm", "esm/index.d.mts"),
            ("lib/cjs", "cjs/index.d.cts"),
            ("lib/gone", "dist/index.d.ts"),
            ("lib/a", "dist/a.d.ts"),
        ] {
            assert_eq!(
                resolved(&substituted, specifier).as_deref(),
                Some(expected),
                "{specifier}"
            );
        }

        // `.d.cts` names its own format, so the `require` branch is the one that applies.
        let importer = substituted.with_file_name("file.cts");
        std::fs::write(&importer, "").unwrap();
        assert_eq!(
            resolved(&importer, "lib/esm").as_deref(),
            Some("cjs/index.d.cts")
        );

        let main = package(
            "main",
            r#"{ "main": "./lib/index.js" }"#,
            &["lib/index.d.ts", "lib/sub.d.ts"],
        );
        assert_eq!(resolved(&main, "lib").as_deref(), Some("lib/index.d.ts"));
        assert_eq!(
            resolved(&main, "lib/lib/sub.js").as_deref(),
            Some("lib/sub.d.ts")
        );

        let versioned_main = package(
            "versioned-main",
            r#"{ "main": "./lib/index.js", "typesVersions": { "*": { "*": ["ts/*"] } } }"#,
            &["ts/index.d.ts", "ts/lib/index.d.ts"],
        );
        assert_eq!(
            resolved(&versioned_main, "lib").as_deref(),
            Some("ts/lib/index.d.ts")
        );

        // A declared but missing entry falls back to `index.d.ts`, never to `main`.
        let missing = package(
            "missing",
            r#"{ "types": "./gone.d.ts", "main": "./lib/index.js" }"#,
            &["index.d.ts", "lib/index.d.ts"],
        );
        assert_eq!(resolved(&missing, "lib").as_deref(), Some("index.d.ts"));

        let dir = main.parent().unwrap();
        for (rel, specifier) in [("dto.ts", "./dto.js"), ("esm.d.mts", "./esm.mjs")] {
            std::fs::write(dir.join(rel), "").unwrap();
            let found = Resolver::default()
                .relative(&main, specifier)
                .expect(specifier);
            assert!(found.ends_with(rel), "{specifier}: {found:?}");
        }
    }

    #[test]
    fn types_versions_remap_the_root_entry() {
        let consumer = package(
            "root",
            r#"{ "types": "./index.d.ts", "typesVersions": { ">=4.0": { "*": ["ts4/*"] } } }"#,
            &["index.d.ts", "ts4/index.d.ts"],
        );
        assert_eq!(
            resolved(&consumer, "lib").as_deref(),
            Some("ts4/index.d.ts")
        );
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
