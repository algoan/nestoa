use std::collections::HashMap;

use swc_common::{BytePos, Span};

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
                let mut parts = rest.splitn(2, char::is_whitespace);
                let name = parts.next().unwrap_or("").to_string();
                let rest = parts.next().unwrap_or("").to_string();
                current_tag = Some((name, vec![rest]));
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
            _ => {}
        }
        self.tags.push((name.to_string(), body.to_string()));
    }
}

/// Look up the JSDoc block immediately preceding `span` in `source`.
/// `file_start` is the global BytePos of byte 0 of the file in the source map.
pub fn jsdoc_before(source: &str, file_start: BytePos, span: Span) -> Option<JsDoc> {
    let span_start = (span.lo.0.saturating_sub(file_start.0)) as usize;
    if span_start == 0 || span_start > source.len() {
        return None;
    }
    let head = &source[..span_start];
    // Scan back, skipping only whitespace and decorators, until we find a /** ... */ block.
    let bytes = head.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        // Trim trailing whitespace
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        // A decorator: skip back over the @Name(...) form. Cheap heuristic: if line ends with ')', skip
        // back to matching '(', then keep going until we find @.
        if i == 0 {
            return None;
        }
        // Try a /** ... */ block
        if i >= 2 && &bytes[i - 2..i] == b"*/" {
            // find matching /**
            let end = i;
            let mut j = i - 2;
            while j >= 3 {
                if &bytes[j - 3..j] == b"/**" {
                    let block = std::str::from_utf8(&bytes[j - 3..end]).ok()?;
                    let inner = block
                        .trim_start_matches("/**")
                        .trim_end_matches("*/")
                        .to_string();
                    return Some(JsDoc::parse(&inner));
                }
                if j == 0 {
                    break;
                }
                j -= 1;
            }
            return None;
        }
        // Walk back over a decorator: must start with `@` and may include (...) args
        // We rewind until we see either a /** end, a `;`/`}`, the start of file, or fail.
        // Decorators may span multiple lines; we use bracket matching for `(...)` if present.
        if bytes[i - 1] == b')' {
            // bracket-match back
            let mut depth: i32 = 0;
            let mut k = i;
            while k > 0 {
                k -= 1;
                match bytes[k] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            // After `(` continues identifier and `@`
            // Walk back through identifier chars and dots
            while k > 0 {
                let c = bytes[k - 1];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.' {
                    k -= 1;
                } else {
                    break;
                }
            }
            if k > 0 && bytes[k - 1] == b'@' {
                i = k - 1;
                continue;
            }
            return None;
        }
        // Or a bare `@Foo` decorator
        // Walk back through identifier
        let mut k = i;
        while k > 0 {
            let c = bytes[k - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.' {
                k -= 1;
            } else {
                break;
            }
        }
        if k < i && k > 0 && bytes[k - 1] == b'@' {
            i = k - 1;
            continue;
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn flags_internal_and_private() {
        assert!(JsDoc::parse("\n * Raw data\n * @internal\n ").internal);
        assert!(JsDoc::parse("\n * @private\n ").internal);
        assert!(!JsDoc::parse("\n * Just a description\n ").internal);
    }
}
