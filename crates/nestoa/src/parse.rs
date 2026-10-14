use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use swc_common::{
    BytePos, SourceFile, SourceMap, Spanned, comments::SingleThreadedComments, sync::Lrc,
};
use swc_ecma_ast::{Decl, DefaultDecl, EsVersion, Module, ModuleDecl, ModuleItem, Stmt};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::jsdoc::DocComments;

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub module: Arc<Module>,
    pub docs: Arc<DocComments>,
    /// The file's text, kept from parsing for `x-unresolved` source excerpts.
    pub source: Lrc<SourceFile>,
}

impl ParsedFile {
    /// The source text `span` covers, on one line.
    pub fn text(&self, span: swc_common::Span) -> Option<String> {
        let start = self.source.start_pos.0;
        let lo = span.lo.0.checked_sub(start)? as usize;
        let hi = span.hi.0.checked_sub(start)? as usize;
        let text = self.source.src.get(lo..hi)?;
        Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

pub fn is_declaration_file(path: &Path) -> bool {
    let name = path.to_string_lossy();
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

pub struct ParsingContext {
    pub source_map: Lrc<SourceMap>,
}

impl Default for ParsingContext {
    fn default() -> Self {
        Self {
            source_map: Lrc::new(SourceMap::default()),
        }
    }
}

impl ParsingContext {
    fn describe(&self, path: &Path, e: &swc_ecma_parser::error::Error) -> String {
        let line = self.source_map.lookup_char_pos(e.span().lo).line;
        format!("{}:{line}: {}", path.display(), e.kind().msg())
    }

    pub fn parse_file(&self, path: &Path) -> Result<ParsedFile> {
        let fm = self
            .source_map
            .load_file(path)
            .with_context(|| format!("loading {}", path.display()))?;
        let is_dts = is_declaration_file(path);
        let comments = SingleThreadedComments::default();
        let lexer = Lexer::new(
            Syntax::Typescript(TsSyntax {
                tsx: path.extension().and_then(|s| s.to_str()) == Some("tsx"),
                decorators: true,
                dts: is_dts,
                no_early_errors: true,
                disallow_ambiguous_jsx_like: false,
            }),
            EsVersion::EsNext,
            StringInput::from(&*fm),
            Some(&comments),
        );
        let mut parser = Parser::new_from(lexer);
        let module = parser
            .parse_module()
            .map_err(|e| anyhow::anyhow!("parse error in {}", self.describe(path, &e)))?;
        for e in parser.take_errors() {
            crate::diag::warn(format!(
                "recovered from a syntax error in {}",
                self.describe(path, &e)
            ));
        }
        let mut docs = DocComments::new(&fm.src, fm.start_pos, comments);
        add_decl_starts(&module.body, &mut docs);
        Ok(ParsedFile {
            path: path.to_path_buf(),
            module: Arc::new(module),
            docs: Arc::new(docs),
            source: fm,
        })
    }
}

/// A top-level declaration's span start, and the start of the `export`
/// keyword or decorators in front of it, where its JSDoc sits.
/// Map each declaration's start back to its `export` keyword and decorators,
/// in namespace blocks too.
fn add_decl_starts(items: &[ModuleItem], docs: &mut DocComments) {
    for item in items {
        if let Some((decl, start)) = decl_doc_start(item) {
            docs.add_decl_start(decl, start);
        }
        let decl = match item {
            ModuleItem::Stmt(Stmt::Decl(decl))
            | ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(swc_ecma_ast::ExportDecl {
                decl,
                ..
            })) => decl,
            _ => continue,
        };
        if let Decl::TsModule(m) = decl
            && let Some(swc_ecma_ast::TsNamespaceBody::TsModuleBlock(block)) = &m.body
        {
            add_decl_starts(&block.body, docs);
        }
    }
}

fn decl_doc_start(item: &ModuleItem) -> Option<(BytePos, BytePos)> {
    let (decl, class, start) = match item {
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) => match &e.decl {
            Decl::Class(c) => (c.class.span.lo, Some(&c.class), e.span.lo),
            Decl::TsInterface(i) => (i.span.lo, None, e.span.lo),
            Decl::TsEnum(en) => (en.span.lo, None, e.span.lo),
            Decl::TsTypeAlias(t) => (t.span.lo, None, e.span.lo),
            _ => return None,
        },
        ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(e)) => match &e.decl {
            DefaultDecl::Class(c) => (c.class.span.lo, Some(&c.class), e.span.lo),
            DefaultDecl::TsInterfaceDecl(i) => (i.span.lo, None, e.span.lo),
            DefaultDecl::Fn(_) => return None,
        },
        ModuleItem::Stmt(Stmt::Decl(Decl::Class(c))) => {
            (c.class.span.lo, Some(&c.class), c.class.span.lo)
        }
        _ => return None,
    };
    let decorators = class.into_iter().flat_map(|c| &c.decorators);
    let start = decorators.map(|d| d.span.lo).fold(start, BytePos::min);
    Some((decl, start))
}
