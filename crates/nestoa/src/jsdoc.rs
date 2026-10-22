use std::collections::HashMap;

use serde_json::Value;
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
        let mut description_lines: Vec<&str> = Vec::new();
        let mut current_tag: Option<(&str, Vec<&str>)> = None;
        let mut in_fence = false;
        for line in raw.lines() {
            let line = strip_leading_star(line).trim_end();
            let tag = if in_fence {
                None
            } else {
                tag_start(line.trim_start())
            };
            let text = match tag {
                Some(("example", body)) => body.trim_start(),
                Some(_) => "",
                None => line.trim_start(),
            };
            // An opening fence has no other backticks (`` ```npm``` is `` is
            // inline code); a closing one is any line starting with them.
            if let Some(after) = text.strip_prefix("```")
                && (in_fence || !after.contains("```"))
            {
                in_fence = !in_fence;
            }
            if let Some((name, body)) = tag {
                if let Some((name, lines)) = current_tag.take() {
                    doc.consume_tag(name, &tag_body(name, &lines));
                }
                current_tag = Some((name, vec![body]));
            } else if let Some((_, lines)) = current_tag.as_mut() {
                lines.push(line);
            } else {
                description_lines.push(line.trim_start());
            }
        }
        if let Some((name, lines)) = current_tag {
            doc.consume_tag(name, &tag_body(name, &lines));
        }
        let joined = description_lines.join("\n").trim().to_string();
        if !joined.is_empty() {
            // The first paragraph is the summary, the rest the description.
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
        // OpenAPI has no field for the reason, so it ends the description.
        if let Some(reason) = &doc.deprecated_reason {
            let note = format!("Deprecated: {reason}");
            doc.description = Some(match doc.description.take() {
                Some(d) => format!("{d}\n\n{note}"),
                None => note,
            });
        }
        doc.render_links();
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
                if let Some((name, desc)) = parse_param(body) {
                    self.params.insert(name.to_string(), desc.to_string());
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

    fn render_links(&mut self) {
        let texts = [&mut self.summary, &mut self.description]
            .into_iter()
            .flatten()
            .chain(self.params.values_mut())
            .chain(self.throws.iter_mut().filter_map(|(_, d)| d.as_mut()));
        for text in texts {
            *text = render_links(text);
        }
    }
}

/// A comment line without its leading `*` and the space after it. A `*`
/// followed by another one is markdown (`/** **Note** … */`), not a gutter.
fn strip_leading_star(line: &str) -> &str {
    let line = line.trim_start();
    match line.strip_prefix('*').filter(|rest| !rest.starts_with('*')) {
        Some(rest) => rest.strip_prefix(' ').unwrap_or(rest),
        None => line,
    }
}

/// The name and body of a tag opening `line`. A tag name starts with a
/// lowercase letter and ends at whitespace, the end of the line or a `{`
/// (`@route-mapping{`), optionally after a `:`, `.` or `,`. So
/// `@nestjs/common`, `@Get()` or a bare `@Injectable` stay text.
fn tag_start(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('@')?;
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    let mut body = &rest[name_end..];
    // `@deprecated: use v2`, `@deprecated.`: punctuation that ends the line or
    // a word, unlike `@example.com`.
    if let Some(after) = body.strip_prefix([':', '.', ','])
        && (after.is_empty() || after.starts_with(char::is_whitespace))
    {
        body = after;
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !(body.is_empty() || body.starts_with(char::is_whitespace) || body.starts_with('{'))
    {
        return None;
    }
    Some((name, body.strip_prefix(char::is_whitespace).unwrap_or(body)))
}

/// A tag's lines as one body. An `@example` keeps its lines' indentation,
/// less the indentation they share; other tags' lines are trimmed.
fn tag_body(name: &str, lines: &[&str]) -> String {
    let (first, rest) = lines.split_first().map_or(("", &[][..]), |(f, r)| (*f, r));
    let body = if name == "example" {
        let indent = rest
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
            .min()
            .unwrap_or(0);
        std::iter::once(first)
            .chain(rest.iter().map(|l| dedent(l, indent)))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        std::iter::once(first)
            .chain(rest.iter().map(|l| l.trim_start()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    body.trim().to_string()
}

/// `line` without its first `indent` characters, all whitespace.
fn dedent(line: &str, indent: usize) -> &str {
    line.char_indices()
        .nth(indent)
        .map_or("", |(i, _)| &line[i..])
}

/// Parse a `@param` body: `[{type}] name [desc]`, where the name may be
/// `[name]` or `[name=default]` and the description may open with `-`.
fn parse_param(body: &str) -> Option<(&str, &str)> {
    let mut rest = body.trim_start();
    if rest.starts_with('{') {
        rest = rest[balanced_end(rest, '{', '}')?..].trim_start();
    }
    let (name, desc) = if rest.starts_with('[') {
        let end = balanced_end(rest, '[', ']')?;
        let inner = &rest[1..end - 1];
        (inner.split('=').next().unwrap_or("").trim(), &rest[end..])
    } else {
        let (name, desc) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        // `@param id: the id`
        (name.strip_suffix(':').unwrap_or(name), desc)
    };
    if name.is_empty() {
        return None;
    }
    let desc = desc.trim();
    let desc = match desc.strip_prefix('-') {
        Some(after) if after.is_empty() || after.starts_with(char::is_whitespace) => after.trim(),
        _ => desc,
    };
    Some((name, desc))
}

/// The end (exclusive) of the `open … close` group `s` starts with.
fn balanced_end(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    s.char_indices().find_map(|(i, c)| {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
        }
        (depth == 0).then_some(i + c.len_utf8())
    })
}

/// Replace each inline `{@link target}`, `{@link target text}` or
/// `{@link target|text}` (also `@linkcode`, `@linkplain`) by its text, or by a
/// markdown link when a URL target has a text. An unclosed, empty or nested
/// tag stays as written.
fn render_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{@link") {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let inner = &after[..end];
        let (tag, target) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
        if !matches!(tag, "@link" | "@linkcode" | "@linkplain")
            || target.trim().is_empty()
            || target.contains('{')
        {
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
            continue;
        }
        let target = target.trim();
        let (target, label) = match target.split_once('|') {
            Some((t, l)) => (t.trim(), l.trim()),
            None => target
                .split_once(char::is_whitespace)
                .map_or((target, ""), |(t, l)| (t, l.trim())),
        };
        out.push_str(&rest[..start]);
        let label = if label.is_empty() { target } else { label };
        if target.contains("://") && label != target {
            out.push_str(&format!("[{label}]({target})"));
        } else if tag == "@linkcode" {
            out.push_str(&format!("`{label}`"));
        } else {
            out.push_str(label);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The value of an `@example`: JSON when it parses (`42`, `true`, `{"a":1}`),
/// the text between single quotes, or else the text itself. A parsed value
/// the schema's `type` doesn't admit stays text, so `@example 12345` on a
/// postcode is a string; so does a scalar when the schema has no `type`
/// (`schema_types` is empty).
pub fn example_value(raw: &str, schema_types: &[&str]) -> Value {
    let parsed = serde_json::from_str::<Value>(raw).ok().or_else(|| {
        raw.strip_prefix('\'')
            .and_then(|r| r.strip_suffix('\''))
            .map(|s| Value::String(s.to_string()))
    });
    match parsed {
        Some(v) if v.is_string() => v,
        Some(v) if has_rounded_integer(&v) => Value::String(raw.to_string()),
        // A `$ref` or a union: its type is unknown here, and a scalar would
        // contradict a string enum or alias.
        Some(v) if schema_types.is_empty() && (v.is_object() || v.is_array()) => v,
        Some(v) if schema_types.iter().any(|t| has_type(&v, t)) => v,
        _ => Value::String(raw.to_string()),
    }
}

/// Whether `v` holds an integer too large for 64 bits, which serde_json rounds
/// to the nearest `f64`.
fn has_rounded_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => n
            .as_f64()
            .is_some_and(|f| n.is_f64() && f.fract() == 0.0 && f.abs() >= 2f64.powi(63)),
        Value::Array(items) => items.iter().any(has_rounded_integer),
        Value::Object(m) => m.values().any(has_rounded_integer),
        _ => false,
    }
}

fn has_type(v: &Value, ty: &str) -> bool {
    match ty {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "string" => v.is_string(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => false,
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
    /// A declaration's start, mapped to where its JSDoc sits: before its
    /// `export` keyword or first decorator, which its span leaves out.
    decl_starts: HashMap<BytePos, BytePos>,
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
        Self {
            before,
            decl_starts: HashMap::new(),
        }
    }

    pub fn add_decl_start(&mut self, decl: BytePos, start: BytePos) {
        if start < decl {
            self.decl_starts.insert(decl, start);
        }
    }

    /// The JSDoc of the node starting at `lo`: the nearest `/** */` block among
    /// the comments directly before it, as TypeScript picks it.
    pub fn jsdoc(&self, lo: BytePos) -> Option<JsDoc> {
        let mut pos = self.decl_starts.get(&lo).copied().unwrap_or(lo);
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

    use serde_json::json;
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
        let raw = "\n * Trigger the thing\n * @throws 409 Conflict when already published\n * @throws {422} author disabled\n * @throws 404\n * @exception {ConflictException} no numeric code, skipped\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(
            doc.throws,
            vec![
                (409, Some("Conflict when already published".to_string())),
                (422, Some("author disabled".to_string())),
                (404, None),
            ]
        );
    }

    #[test]
    fn no_throws_leaves_field_empty() {
        assert!(JsDoc::parse("\n * Just a summary\n ").throws.is_empty());
    }

    #[test]
    fn parses_param_forms() {
        let raw = "\n * @param {string} title the title\n * @param pages - the page count\n * @param [isbn] optional ISBN\n * @param {Array<{a: string}>} [tags=[]] - the tags\n * @param draft\n ";
        let doc = JsDoc::parse(raw);
        let param = |n: &str| doc.params.get(n).map(String::as_str);
        assert_eq!(param("title"), Some("the title"));
        assert_eq!(param("pages"), Some("the page count"));
        assert_eq!(param("isbn"), Some("optional ISBN"));
        assert_eq!(param("tags"), Some("the tags"));
        assert_eq!(param("draft"), Some(""));
        assert_eq!(doc.params.len(), 5);
        let doc = JsDoc::parse(" @param id: the id ");
        assert_eq!(doc.params.get("id").map(String::as_str), Some("the id"));
    }

    #[test]
    fn only_a_tag_shaped_word_opens_a_tag() {
        let raw = "\n * Wraps the client of\n * @nestjs/common and\n * @Injectable services.\n * @deprecated\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(
            doc.summary.as_deref(),
            Some("Wraps the client of\n@nestjs/common and\n@Injectable services.")
        );
        assert!(doc.deprecated);
    }

    #[test]
    fn example_body_is_kept_verbatim() {
        let raw = "\n * Find a book\n * @example\n * @Get(':id')\n * find() {\n *   return 1;\n * }\n * @param id the id\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(
            doc.example.as_deref(),
            Some("@Get(':id')\nfind() {\n  return 1;\n}")
        );
        assert_eq!(doc.params.get("id").map(String::as_str), Some("the id"));
    }

    #[test]
    fn a_code_fence_hides_tags() {
        let raw = "\n * @example\n * ```ts\n * @param x\n * ```\n * @deprecated\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(doc.example.as_deref(), Some("```ts\n@param x\n```"));
        assert!(doc.params.is_empty());
        assert!(doc.deprecated);
    }

    #[test]
    fn a_tag_name_may_end_with_punctuation() {
        let doc = JsDoc::parse("\n * @deprecated: use v2\n ");
        assert_eq!(doc.deprecated_reason.as_deref(), Some("use v2"));
        assert!(JsDoc::parse(" @deprecated. ").deprecated);
        let doc = JsDoc::parse("\n * Mail\n * @example.com domain\n ");
        assert_eq!(doc.summary.as_deref(), Some("Mail\n@example.com domain"));
    }

    #[test]
    fn inline_code_opens_no_fence() {
        let doc = JsDoc::parse("\n * ```npm``` is the tool\n * @deprecated\n ");
        assert_eq!(doc.summary.as_deref(), Some("```npm``` is the tool"));
        assert!(doc.deprecated);
        let doc = JsDoc::parse("\n * @example ```{\"a\": 1}```\n * @deprecated\n ");
        assert_eq!(doc.example.as_deref(), Some("```{\"a\": 1}```"));
        assert!(doc.deprecated);
    }

    #[test]
    fn only_an_example_opens_a_fence_on_its_tag_line() {
        let doc = JsDoc::parse("\n * @deprecated ```old``` is gone\n * @param id the id\n ");
        assert_eq!(doc.params.get("id").map(String::as_str), Some("the id"));
    }

    #[test]
    fn a_leading_bold_keeps_its_stars() {
        let doc = JsDoc::parse("\n * **Note**: read only\n ");
        assert_eq!(doc.summary.as_deref(), Some("**Note**: read only"));
        let doc = JsDoc::parse(" **Note**: read only ");
        assert_eq!(doc.summary.as_deref(), Some("**Note**: read only"));
    }

    #[test]
    fn a_fence_opened_on_the_tag_line_hides_tags() {
        let raw = "\n * @example ```json\n * {\"a\": 1}\n * @param x\n * ```\n * @deprecated\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(
            doc.example.as_deref(),
            Some("```json\n{\"a\": 1}\n@param x\n```")
        );
        assert!(doc.params.is_empty());
        assert!(doc.deprecated);
    }

    #[test]
    fn example_indentation_is_counted_in_characters() {
        let raw = "\n * @example\n *  \u{a0}first\n *   second\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(doc.example.as_deref(), Some("first\nsecond"));
        let raw = "\n * @example\n *   {\n *     \"a\": 1\n *   }\n ";
        let doc = JsDoc::parse(raw);
        assert_eq!(doc.example.as_deref(), Some("{\n  \"a\": 1\n}"));
    }

    #[test]
    fn deprecation_reason_ends_the_description() {
        let doc = JsDoc::parse("\n * Find a book\n * @deprecated Use v2\n ");
        assert_eq!(doc.summary.as_deref(), Some("Find a book"));
        assert_eq!(doc.description.as_deref(), Some("Deprecated: Use v2"));
        let doc = JsDoc::parse("\n * Find\n *\n * Slow.\n * @deprecated Use v2\n ");
        assert_eq!(
            doc.description.as_deref(),
            Some("Slow.\n\nDeprecated: Use v2")
        );
        let doc = JsDoc::parse("\n * Find\n * @deprecated\n ");
        assert_eq!(doc.description, None);
    }

    #[test]
    fn renders_inline_links() {
        assert_eq!(render_links("see {@link Book}"), "see Book");
        assert_eq!(render_links("see {@link Book|the book}"), "see the book");
        assert_eq!(render_links("see {@link Book the book}"), "see the book");
        assert_eq!(render_links("{@linkcode Book.find}"), "`Book.find`");
        assert_eq!(
            render_links("{@link https://example.com/docs Docs} and {@link https://example.com}"),
            "[Docs](https://example.com/docs) and https://example.com"
        );
        assert_eq!(
            render_links("{@linkx Book} {@link Book"),
            "{@linkx Book} {@link Book"
        );
        assert_eq!(render_links("{@link} {@linkcode }"), "{@link} {@linkcode }");
        assert_eq!(render_links("{@link A {@link B} end"), "{@link A B end");
        let doc =
            JsDoc::parse("\n * @param id the {@link Book} id\n * @throws 404 no {@link Book}\n ");
        assert_eq!(
            doc.params.get("id").map(String::as_str),
            Some("the Book id")
        );
        assert_eq!(doc.throws, vec![(404, Some("no Book".to_string()))]);
    }

    #[test]
    fn example_values() {
        assert_eq!(example_value("42", &["number"]), json!(42));
        assert_eq!(example_value("{\"a\": 1}", &[]), json!({"a": 1}));
        assert_eq!(example_value("'hello'", &[]), json!("hello"));
        assert_eq!(example_value("\"hello\"", &["string"]), json!("hello"));
        assert_eq!(
            example_value("hello world", &["number"]),
            json!("hello world")
        );
        assert_eq!(example_value("42", &["string"]), json!("42"));
        assert_eq!(example_value("42", &["string", "number"]), json!(42));
        assert_eq!(example_value("4.5", &["string", "integer"]), json!("4.5"));
        assert_eq!(example_value("null", &["string", "null"]), json!(null));
        assert_eq!(example_value("123", &[]), json!("123"));
        assert_eq!(example_value("true", &[]), json!("true"));
        assert_eq!(example_value("[1]", &[]), json!([1]));
        assert_eq!(example_value("42", &["object"]), json!("42"));
        assert_eq!(example_value("1e3", &["number"]), json!(1000.0));
        assert_eq!(
            example_value("12345678901234567890123", &["number"]),
            json!("12345678901234567890123")
        );
        assert_eq!(
            example_value("[12345678901234567890123]", &["array"]),
            json!("[12345678901234567890123]")
        );
        assert_eq!(
            example_value("{\"a\": 12345678901234567890123}", &[]),
            json!("{\"a\": 12345678901234567890123}")
        );
    }

    #[test]
    fn flags_internal_and_private() {
        assert!(JsDoc::parse("\n * Raw data\n * @internal\n ").internal);
        assert!(JsDoc::parse("\n * @private\n ").internal);
        assert!(!JsDoc::parse("\n * Just a description\n ").internal);
    }
}
