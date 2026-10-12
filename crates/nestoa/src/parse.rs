use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use swc_common::{BytePos, SourceMap, comments::SingleThreadedComments, sync::Lrc};
use swc_ecma_ast::{Decl, DefaultDecl, EsVersion, Module, ModuleDecl, ModuleItem, Stmt};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::jsdoc::DocComments;

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub module: Arc<Module>,
    pub docs: Arc<DocComments>,
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
    pub fn parse_file(&self, path: &Path) -> Result<ParsedFile> {
        let fm = self
            .source_map
            .load_file(path)
            .with_context(|| format!("loading {}", path.display()))?;
        let name = path.to_string_lossy();
        let is_dts = [".d.ts", ".d.mts", ".d.cts"]
            .iter()
            .any(|ext| name.ends_with(ext));
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
            .map_err(|e| anyhow::anyhow!("parse error in {}: {:?}", path.display(), e.kind()))?;
        let mut docs = DocComments::new(&fm.src, fm.start_pos, comments);
        for item in &module.body {
            if let Some((decl, start)) = decl_doc_start(item) {
                docs.add_decl_start(decl, start);
            }
        }
        Ok(ParsedFile {
            path: path.to_path_buf(),
            module: Arc::new(module),
            docs: Arc::new(docs),
        })
    }
}

/// A top-level declaration's span start, and the start of the `export`
/// keyword or decorators in front of it, where its JSDoc sits.
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
