use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use swc_common::{FileName, SourceMap, sync::Lrc};
use swc_ecma_ast::{EsVersion, Module};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub module: Arc<Module>,
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
        let _ = FileName::Real(path.to_path_buf());
        let name = path.to_string_lossy();
        let is_dts = [".d.ts", ".d.mts", ".d.cts"]
            .iter()
            .any(|ext| name.ends_with(ext));
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
            None,
        );
        let mut parser = Parser::new_from(lexer);
        let module = parser
            .parse_module()
            .map_err(|e| anyhow::anyhow!("parse error in {}: {:?}", path.display(), e.kind()))?;
        Ok(ParsedFile {
            path: path.to_path_buf(),
            module: Arc::new(module),
        })
    }
}
