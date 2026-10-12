use std::collections::HashMap;

use swc_common::BytePos;
use swc_common::comments::{Comment, CommentKind, SingleThreadedComments};

/// Parsed JSDoc block.
#[derive(Debug, Default, Clone)]
pub struct JsDoc {
    pub summary: Option<String>,
    pub description: Option<String>,
    pub deprecated: bool,
    pub deprecated_reason: Option<String>,
    /// `@internal` (or `@private`): the annotated member is implementation-only
    /// and must be omitted from the emitted schema.
    pub internal: bool,
    /// Free-form tags as (name, body) pairs, preserving order.
    pub tags: Vec<(String, String)>,
    /// `@param name desc` lookup.
    pub params: HashMap<String, String>,
    pub example: Option<String>,
    /// `@route-mapping{ 'suffix': TypeName, ... }`: ordered (path-suffix, type
    /// name) pairs letting one method serving several paths declare a distinct
    /// response type per path. Empty when the tag is absent.
    pub route_mapping: Vec<(String, String)>,
    /// `@throws <status> [description]` (aliases `@httpError`, `@exception`):
    /// error responses a handler produces by throwing — including ones thrown
    /// deep in called service code, which no static route analysis can see. Each
    /// entry is (status code, optional description). Empty when the tag is absent.
    pub throws: Vec<(u16, Option<String>)>,
}

impl JsDoc {
    pub fn parse(raw: &str) -> Self {
        let mut doc = JsDoc::default();
        let lines: Vec<&str> = raw
            .lines()
            .map(|l| l.trim_start().trim_start_matches('*').trim_start())
            .collect();
        let mut description_lines: Vec<String> = Vec::new();
        let mut current_tag: Option<(String, Vec<String>)> = None;
        for line in lines {
            let line = line.trim_end();
            if let Some(rest) = line.strip_prefix('@') {
                if let Some((tag_name, tag_body)) = current_tag.take() {
                    doc.consume_tag(&tag_name, tag_body.join("\n").trim());
                }
                // The tag name runs up to the first whitespace or `{`. Allowing
                // `{` as a boundary lets `@route-mapping{ ... }` attach its
                // brace-body with no separating space; a whitespace separator is
                // consumed, while a `{` is kept as the start of the body.
                let name_end = rest
                    .find(|c: char| c.is_whitespace() || c == '{')
                    .unwrap_or(rest.len());
                let name = rest[..name_end].to_string();
                let mut body = &rest[name_end..];
                if let Some(stripped) = body.strip_prefix(char::is_whitespace) {
                    body = stripped;
                }
                current_tag = Some((name, vec![body.to_string()]));
            } else if let Some((_, body)) = current_tag.as_mut() {
                if !line.is_empty() || !body.is_empty() {
                    body.push(line.to_string());
                }
            } else {
                description_lines.push(line.to_string());
            }
        }
        if let Some((tag_name, body)) = current_tag {
            doc.consume_tag(&tag_name, body.join("\n").trim());
        }
        let joined = description_lines.join("\n").trim().to_string();
        if !joined.is_empty() {
            // First non-empty line becomes the summary; the rest becomes the
            // longer description if it differs.
            let mut iter = joined.split("\n\n");
            doc.summary = iter
                .next()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let rest = iter.collect::<Vec<_>>().join("\n\n").trim().to_string();
            if !rest.is_empty() {
                doc.description = Some(rest);
            }
        }
        doc
    }

    fn consume_tag(&mut self, name: &str, body: &str) {
        match name {
            "deprecated" => {
                self.deprecated = true;
                if !body.is_empty() {
                    self.deprecated_reason = Some(body.to_string());
                }
            }
            "internal" | "private" => self.internal = true,
            "example" => self.example = Some(body.to_string()),
            "summary" => self.summary = Some(body.to_string()),
            "description" => self.description = Some(body.to_string()),
            "param" => {
                let mut parts = body.splitn(2, char::is_whitespace);
                if let Some(name) = parts.next() {
                    let desc = parts.next().unwrap_or("").trim().to_string();
                    self.params.insert(name.to_string(), desc);
                }
            }
            "route-mapping" => self.route_mapping = parse_route_mapping(body),
            "throws" | "httpError" | "exception" => {
                if let Some(entry) = parse_throws(body) {
                    self.throws.push(entry);
                }
            }
            _ => {}
        }
        self.tags.push((name.to_string(), body.to_string()));
    }
}

/// Parse a `@route-mapping` body of the form
/// `{ 'suffix': TypeName, 'other/suffix': OtherType }` into ordered
/// (suffix, type name) pairs. Entries are separated by newlines and/or commas;
/// each is `key: value` where the key may be quoted. Malformed entries (no
/// `:`, empty key or value) are skipped so a stray line can't break parsing.
fn parse_route_mapping(body: &str) -> Vec<(String, String)> {
    let body = body.trim();
    let inner = body.strip_prefix('{').unwrap_or(body);
    let inner = inner.strip_suffix('}').unwrap_or(inner);
    let mut out = Vec::new();
    for entry in inner.split(['\n', ',']) {
        let Some((key, val)) = entry.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches(|c| c == '\'' || c == '"').trim();
        let val = val.trim();
        if key.is_empty() || val.is_empty() {
            continue;
        }
        out.push((key.to_string(), val.to_string()));
    }
    out
}

/// Parse a `@throws` body into (status, optional description). Accepts both the
/// JSDoc `{...}` type-brace form (`@throws {409} Conflict`) and a bare leading
/// token (`@throws 409 Conflict`). Returns `None` when the leading token is not
/// a numeric status, so `@throws {ConflictException} ...` (no code) is skipped
/// rather than mis-parsed.
fn parse_throws(body: &str) -> Option<(u16, Option<String>)> {
    let body = body.trim();
    let (code_tok, rest) = if let Some(after) = body.strip_prefix('{') {
        let (inside, rest) = after.split_once('}')?;
        (inside.trim(), rest.trim())
    } else {
        let mut parts = body.splitn(2, char::is_whitespace);
        (
            parts.next().unwrap_or("").trim(),
            parts.next().unwrap_or("").trim(),
        )
    };
    let status: u16 = code_tok.parse().ok()?;
    let desc = (!rest.is_empty()).then(|| rest.to_string());
    Some((status, desc))
}

/// One file's comments, indexed for JSDoc lookup.
#[derive(Debug, Default)]
pub struct DocComments {
    /// Each comment, keyed by the first non-whitespace position after it.
    before: HashMap<BytePos, Comment>,
}

impl DocComments {
    pub fn new(src: &str, file_start: BytePos, comments: SingleThreadedComments) -> Self {
        let (leading, trailing) = comments.take_all();
        let before = [leading, trailing]
            .into_iter()
            .flat_map(|m| m.take().into_values().flatten())
            .map(|c| {
                let hi = (c.span.hi.0 - file_start.0) as usize;
                let gap = src[hi..].len() - src[hi..].trim_start().len();
                (BytePos(c.span.hi.0 + gap as u32), c)
            })
            .collect();
        Self { before }
    }

    /// The JSDoc of the node starting at `lo`: the nearest `/** */` block among
    /// the comments directly before it, as TypeScript picks it.
    pub fn jsdoc(&self, lo: BytePos) -> Option<JsDoc> {
        let mut pos = lo;
        while let Some(c) = self.before.get(&pos) {
            if c.kind == CommentKind::Block
                && let Some(body) = c.text.strip_prefix('*')
            {
                return Some(JsDoc::parse(body));
            }
            pos = c.span.lo;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use swc_common::{FileName, SourceMap, sync::Lrc};
    use swc_ecma_ast::EsVersion;
    use swc_ecma_parser::{StringInput, Syntax, TsSyntax, lexer::Lexer};

    /// The JSDoc of the token starting at `marker`. Only lexes, so a fragment
    /// that isn't a valid module still works.
    fn doc_before(source: &str, marker: &str) -> Option<JsDoc> {
        let cm: Lrc<SourceMap> = Default::default();
        let fm = cm.new_source_file(FileName::Anon.into(), source.to_string());
        let comments = SingleThreadedComments::default();
        let syntax = Syntax::Typescript(TsSyntax {
            decorators: true,
            ..Default::default()
        });
        Lexer::new(
            syntax,
            EsVersion::EsNext,
            StringInput::from(&*fm),
            Some(&comments),
        )
        .for_each(drop);
        let lo = fm.start_pos.0 + source.find(marker).expect("marker") as u32;
        DocComments::new(source, fm.start_pos, comments).jsdoc(BytePos(lo))
    }

    #[test]
    fn plain_block_comment_never_borrows_an_earlier_jsdoc() {
        let source = "/** List */\nlist() {}\n/* plain */\n@Delete(':id')\nremove() {}";
        assert!(doc_before(source, "@Delete").is_none());
    }

    #[test]
    fn plain_block_comment_after_a_jsdoc_is_skipped() {
        let source = "/** Read */\n/* note */\n@Get()\nread() {}";
        let doc = doc_before(source, "@Get").expect("doc");
        assert_eq!(doc.summary.as_deref(), Some("Read"));
    }

    #[test]
    fn comment_opener_in_a_string_or_line_comment_is_ignored() {
        let source = "const marker = \"/*\"; // see /*\n/** Read */\nread() {}";
        let doc = doc_before(source, "read").expect("doc");
        assert_eq!(doc.summary.as_deref(), Some("Read"));
        let url = "const u = \"http://x\"; /** Write */\nwrite() {}";
        let doc = doc_before(url, "write").expect("doc");
        assert_eq!(doc.summary.as_deref(), Some("Write"));
    }

    #[test]
    fn comment_opener_in_a_multi_line_template_is_ignored() {
        let source = "const m = `\n/*`;\n/** Read */\nread() {}";
        let doc = doc_before(source, "read").expect("doc");
        assert_eq!(doc.summary.as_deref(), Some("Read"));
    }

    #[test]
    fn comment_opener_inside_a_jsdoc_is_text() {
        let source = "/** Match a/*b */\nread() {}";
        let doc = doc_before(source, "read").expect("doc");
        assert_eq!(doc.summary.as_deref(), Some("Match a/*b"));
    }

    #[test]
    fn parses_summary_and_param() {
        let raw = "\n * Do the thing\n *\n * @param id the identifier\n * @deprecated use other instead\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(doc.summary.as_deref(), Some("Do the thing"));
        assert!(doc.deprecated);
        assert_eq!(doc.deprecated_reason.as_deref(), Some("use other instead"));
        assert_eq!(
            doc.params.get("id").map(|s| s.as_str()),
            Some("the identifier")
        );
    }

    #[test]
    fn parses_route_mapping_attached_brace() {
        // The brace opens with no space after the tag name, and other tags
        // surround it — both must still parse.
        let raw = "\n * List pets\n * @route-mapping{\n * 'cats': Cat\n * 'dogs': Dog\n * 'birds': Bird\n * }\n * @param id the identifier\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(doc.summary.as_deref(), Some("List pets"));
        assert_eq!(
            doc.route_mapping,
            vec![
                ("cats".to_string(), "Cat".to_string()),
                ("dogs".to_string(), "Dog".to_string()),
                ("birds".to_string(), "Bird".to_string()),
            ]
        );
        // The surrounding tag is unaffected by the new boundary handling.
        assert_eq!(
            doc.params.get("id").map(|s| s.as_str()),
            Some("the identifier")
        );
    }

    #[test]
    fn parses_route_mapping_with_space_and_multi_segment_keys() {
        // A space before the brace, double quotes, and multi-segment suffixes.
        let raw =
            "\n * @route-mapping {\n * \"cats/latest\": Cat,\n * \"dogs/latest\": Dog\n * }\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(
            doc.route_mapping,
            vec![
                ("cats/latest".to_string(), "Cat".to_string()),
                ("dogs/latest".to_string(), "Dog".to_string()),
            ]
        );
    }

    #[test]
    fn no_route_mapping_leaves_field_empty() {
        assert!(
            JsDoc::parse("\n * Just a summary\n ")
                .route_mapping
                .is_empty()
        );
    }

    #[test]
    fn parses_throws_tag_variants() {
        let raw = "\n * Trigger the thing\n * @throws 409 Conflict when already aggregated\n * @throws {422} product disabled\n * @throws 404\n * @exception {ConflictException} no numeric code, skipped\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(
            doc.throws,
            vec![
                (409, Some("Conflict when already aggregated".to_string())),
                (422, Some("product disabled".to_string())),
                (404, None),
            ]
        );
    }

    #[test]
    fn no_throws_leaves_field_empty() {
        assert!(JsDoc::parse("\n * Just a summary\n ").throws.is_empty());
    }

    #[test]
    fn flags_internal_and_private() {
        assert!(JsDoc::parse("\n * Raw data\n * @internal\n ").internal);
        assert!(JsDoc::parse("\n * @private\n ").internal);
        assert!(!JsDoc::parse("\n * Just a description\n ").internal);
    }
}
