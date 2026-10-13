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

use crate::parse::ParsedFile;
use crate::symbols::Project;

/// Scan the discovered `files` for `app.setGlobalPrefix(...)` and resolve
/// the first argument to a literal prefix. A `main.ts` is tried first, then
/// the others in sorted order. Files pulled in only for type resolution
/// (workspace libs, packages) are never scanned: a test helper in a lib may
/// set a prefix of its own.
pub fn detect_global_prefix(
    project: &Project,
    files: &[PathBuf],
    project_root: &Path,
) -> Option<String> {
    let mut files: Vec<&PathBuf> = files.iter().collect();
    files.sort_by_key(|f| (f.file_name().is_none_or(|n| n != "main.ts"), *f));
    for file in files {
        let Some(parsed) = project.files.get(file) else {
            continue;
        };
        let mut finder = PrefixCallFinder { arg: None };
        parsed.module.visit_with(&mut finder);
        let Some(arg) = finder.arg else { continue };
        if let Some(prefix) = resolve_prefix_expr(&arg, parsed, project, project_root) {
            return Some(prefix);
        }
    }
    None
}

/// Visitor that captures the first argument of the first `*.setGlobalPrefix(...)`
/// call it encounters anywhere in the module (including inside the bootstrap
/// function body).
struct PrefixCallFinder {
    arg: Option<Expr>,
}

impl Visit for PrefixCallFinder {
    fn visit_call_expr(&mut self, call: &swc_ecma_ast::CallExpr) {
        if self.arg.is_none()
            && let Callee::Expr(callee) = &call.callee
            && let Expr::Member(MemberExpr {
                prop: MemberProp::Ident(p),
                ..
            }) = callee.as_ref()
            && p.sym == "setGlobalPrefix"
            && let Some(first) = call.args.first()
        {
            self.arg = Some((*first.expr).clone());
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
        dir
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
            detect_global_prefix(&project, &files(&project), &root).as_deref(),
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
            detect_global_prefix(&project, &files(&project), &root).as_deref(),
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
            detect_global_prefix(&project, &files(&project), &root).as_deref(),
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
        assert_eq!(
            detect_global_prefix(&project, &files(&project), &root),
            None
        );
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
            detect_global_prefix(&project, &entry, &root).as_deref(),
            Some("api")
        );
        assert_eq!(
            detect_global_prefix(&project, &entry[..1], &root).as_deref(),
            Some("setup")
        );
    }
}
