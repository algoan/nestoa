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
}

#[derive(Clone, Default)]
pub struct FileSymbols {
    pub declared: HashMap<String, Symbol>,
    pub imports: HashMap<String, ImportBinding>,
    /// `export { Foo } from "./other"` re-exports: local-name -> ImportBinding
    pub re_exports: HashMap<String, ImportBinding>,
    /// `export * from "./other"` re-export targets.
    pub star_re_exports: Vec<String>,
}

/// All files indexed by path.
#[derive(Default)]
pub struct Project {
    pub files: HashMap<PathBuf, ParsedFile>,
    pub symbols: HashMap<PathBuf, FileSymbols>,
}

impl Project {
    pub fn add(&mut self, file: ParsedFile) {
        let fs = collect_file_symbols(&file);
        self.symbols.insert(file.path.clone(), fs);
        self.files.insert(file.path.clone(), file);
    }

    /// Resolve a name referenced inside `file` to the (declaring file, symbol).
    pub fn resolve(&self, file: &Path, name: &str) -> Option<(PathBuf, Symbol)> {
        let mut visited: std::collections::HashSet<(PathBuf, String)> =
            std::collections::HashSet::new();
        self.resolve_inner(file, name, &mut visited)
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
        if let Some(binding) = fs.re_exports.get(name).or_else(|| fs.imports.get(name)) {
            if let Some(target) = resolve_module_path(file, &binding.specifier) {
                let next_name = binding.imported.clone().unwrap_or_else(|| name.to_string());
                if let Some(found) = self.resolve_inner(&target, &next_name, visited) {
                    return Some(found);
                }
            }
        }
        // `export * from './other'` — search each one.
        for spec in &fs.star_re_exports {
            if let Some(target) = resolve_module_path(file, spec) {
                if let Some(found) = self.resolve_inner(&target, name, visited) {
                    return Some(found);
                }
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
                    if let DefaultDecl::Class(cd) = decl {
                        if let Some(ident) = &cd.ident {
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
                }
                ModuleDecl::ExportAll(ea) => {
                    fs.star_re_exports
                        .push(ea.src.value.to_string_lossy().into_owned());
                }
                ModuleDecl::ExportNamed(en) => {
                    let src = en.src.as_ref().map(|s| s.value.to_string_lossy().into_owned());
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
                                        },
                                    );
                                }
                                // Bare `export { X }` (no src) — X is already in declared/imports.
                            }
                            ExportSpecifier::Namespace(_) | ExportSpecifier::Default(_) => {}
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
            fs.declared
                .insert(i.id.sym.to_string(), Symbol::Interface(Arc::new(*i.clone())));
        }
        Decl::TsTypeAlias(t) => {
            fs.declared
                .insert(t.id.sym.to_string(), Symbol::TypeAlias(Arc::new(*t.clone())));
        }
        Decl::TsEnum(e) => {
            fs.declared
                .insert(e.id.sym.to_string(), Symbol::Enum(Arc::new(*e.clone())));
        }
        _ => {}
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
                    },
                );
            }
            ImportSpecifier::Default(d) => {
                fs.imports.insert(
                    d.local.sym.to_string(),
                    ImportBinding {
                        imported: None,
                        specifier: specifier.clone(),
                    },
                );
            }
            ImportSpecifier::Namespace(n) => {
                fs.imports.insert(
                    n.local.sym.to_string(),
                    ImportBinding {
                        imported: None,
                        specifier: specifier.clone(),
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
    for ext in [".ts", ".tsx", "/index.ts", "/index.tsx"] {
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
