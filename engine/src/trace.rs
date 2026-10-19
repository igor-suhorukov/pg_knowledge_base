// SPDX-License-Identifier: Apache-2.0
//! What pg_otel_probe's spans carry of a SPARQL call (docs/design.md §2.8):
//! its text and its plan, each node with its results and its time, made
//! short enough for the attributes' area the probe gives an extension
//! (src/trace.c) -- and, unless raw, with their literals replaced by `?`,
//! as the probe's normalized texts replace a statement's constants.

use std::ffi::{CString, c_char};

use json_event_parser::{JsonEvent, SliceJsonParser};

use crate::error::{EResult, EngineError};
use crate::ffi::KbTrace;
use crate::query::Trace;

/// A SPARQL text, or a plan's labels, with every literal -- a quoted
/// string, a number -- replaced by `?`.  IRIs, prefixed names, variables,
/// blank node labels, keywords and language tags stay; a comment is
/// dropped.  It is a scan of SPARQL's tokens (SPARQL 1.1 §19.8), not a
/// parse: what does not lex is copied as it is.
pub fn normalize(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    // whether a digit at i goes on a name, a variable or a label: ex:item2,
    // ?x1, _:b1, ex:a-2, ex:a.5 -- but not -7
    let in_name = |i: usize| {
        i > 0
            && (word(b[i - 1])
                || matches!(b[i - 1], b':' | b'?' | b'$')
                || (matches!(b[i - 1], b'-' | b'.') && i >= 2 && word(b[i - 2])))
    };
    while i < b.len() {
        let c = b[i];
        match c {
            // an IRI: '<' and no character IRIREF excludes before '>'
            b'<' => {
                let end = b[i + 1..]
                    .iter()
                    .position(|&x| x == b'>' || x <= b' ' || matches!(x, b'<' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\'));
                match end {
                    Some(n) if b[i + 1 + n] == b'>' => {
                        out.push_str(&text[i..i + n + 2]);
                        i += n + 2;
                    }
                    _ => {
                        out.push('<');
                        i += 1;
                    }
                }
            }
            b'"' | b'\'' => {
                let long = b.len() >= i + 3 && b[i + 1] == c && b[i + 2] == c;
                let quote = if long { 3 } else { 1 };
                let mut j = i + quote;
                while j < b.len() {
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == c && (!long || (j + 2 < b.len() && b[j + 1] == c && b[j + 2] == c)) {
                        break;
                    }
                    if !long && (b[j] == b'\n' || b[j] == b'\r') {
                        break;
                    }
                    j += 1;
                }
                out.push('?');
                i = (j + quote).min(b.len());
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            // a number begins where no name, variable or label goes on
            b'0'..=b'9' if !in_name(i) => {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j + 1 < b.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
                    let mut k = j + 1;
                    if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                        k += 1;
                    }
                    if k < b.len() && b[k].is_ascii_digit() {
                        j = k;
                        while j < b.len() && b[j].is_ascii_digit() {
                            j += 1;
                        }
                    }
                }
                out.push('?');
                i = j;
            }
            _ => {
                let len = utf8_len(c);
                out.push_str(&text[i..(i + len).min(b.len())]);
                i += len;
            }
        }
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// At most max bytes of a text, cut at a character boundary, with "…" where
/// it was cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max.saturating_sub(3);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// A node of spareval's explanation
struct Node {
    name: String,
    results: Option<String>,
    seconds: Option<f64>,
    children: Vec<Node>,
}

fn parse_node(parser: &mut SliceJsonParser<'_>) -> Option<Node> {
    let mut node = Node {
        name: String::new(),
        results: None,
        seconds: None,
        children: Vec::new(),
    };
    loop {
        match parser.parse_next().ok()? {
            JsonEvent::EndObject => return Some(node),
            JsonEvent::ObjectKey(k) => match (k.as_ref(), parser.parse_next().ok()?) {
                ("name", JsonEvent::String(s)) => node.name = s.into_owned(),
                ("number of results", JsonEvent::Number(n)) => node.results = Some(n.into_owned()),
                ("duration in seconds", JsonEvent::Number(n)) => node.seconds = n.parse().ok(),
                ("children", JsonEvent::StartArray) => loop {
                    match parser.parse_next().ok()? {
                        JsonEvent::EndArray => break,
                        JsonEvent::StartObject => node.children.push(parse_node(parser)?),
                        _ => return None,
                    }
                },
                (_, JsonEvent::StartObject | JsonEvent::StartArray) => return None,
                _ => (),
            },
            _ => return None,
        }
    }
}

/// The plan of spareval's explanation (QueryExplanation::write_in_json) as
/// lines, a node a line -- its label, its results and its time, children
/// indented -- in at most max bytes, the nodes past them left out and
/// counted; and the plan's nodes.  None when the JSON is not one.
pub fn plan_text(json: &[u8], max: usize, raw: bool) -> Option<(String, i32)> {
    let mut parser = SliceJsonParser::new(json);
    if !matches!(parser.parse_next().ok()?, JsonEvent::StartObject) {
        return None;
    }
    let mut plan = None;
    loop {
        match parser.parse_next().ok()? {
            JsonEvent::EndObject | JsonEvent::Eof => break,
            JsonEvent::ObjectKey(k) => {
                let value = parser.parse_next().ok()?;
                if k == "plan" {
                    if !matches!(value, JsonEvent::StartObject) {
                        return None;
                    }
                    plan = Some(parse_node(&mut parser)?);
                } else if matches!(value, JsonEvent::StartObject | JsonEvent::StartArray) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let plan = plan?;
    let mut lines = Vec::new();
    flatten(&plan, 0, raw, &mut lines);
    let more = |n: usize| format!("\n… {n} node{} more", if n == 1 { "" } else { "s" });
    let mut out = String::new();
    let mut shown = 0;
    while shown < lines.len() {
        let line = &lines[shown];
        let sep = usize::from(!out.is_empty());
        let rest = lines.len() - shown - 1;
        let note = if rest > 0 { more(rest).len() } else { 0 };
        if out.len() + sep + line.len() + note > max {
            break;
        }
        if sep == 1 {
            out.push('\n');
        }
        out.push_str(line);
        shown += 1;
    }
    if shown == 0 {
        // not even the root fits whole: the root, cut
        out = cut(&lines[0], max.saturating_sub(more(lines.len()).len()));
        shown = 1;
    }
    if shown < lines.len() {
        out.push_str(&more(lines.len() - shown));
    }
    Some((out, lines.len() as i32))
}

fn flatten(node: &Node, depth: usize, raw: bool, lines: &mut Vec<String>) {
    let name = if raw { node.name.clone() } else { normalize(&node.name) };
    let mut line = format!("{}{}", "  ".repeat(depth), name);
    if let Some(results) = &node.results {
        line.push_str(&format!(" rows={results}"));
    }
    if let Some(seconds) = node.seconds {
        line.push_str(&format!(" time={:.3}ms", seconds * 1000.0));
    }
    lines.push(line);
    for child in &node.children {
        flatten(child, depth + 1, raw, lines);
    }
}

/// A trace's figures into the C ABI's struct: the operation, the rows, and
/// the plan in at most max_bytes (a string the caller frees), or none.
pub fn fill(out: &mut KbTrace, trace: Option<&Trace>, max_bytes: i32, raw: bool) -> EResult<()> {
    if (out.size as usize) < std::mem::size_of::<KbTrace>() {
        return Err(EngineError::invalid("the trace struct is from an older ABI"));
    }
    out.operation = [0; 16];
    out.rows = 0;
    out.plan_nodes = 0;
    out.plan = std::ptr::null_mut();
    let Some(trace) = trace else {
        return Ok(());
    };
    for (i, b) in trace.operation.bytes().take(15).enumerate() {
        out.operation[i] = b as c_char;
    }
    out.rows = trace.rows;
    if let Some(json) = &trace.plan {
        if let Some((text, nodes)) = plan_text(json, max_bytes.max(64) as usize, raw) {
            out.plan_nodes = nodes;
            out.plan = CString::new(text).unwrap_or_default().into_raw();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized() {
        assert_eq!(
            normalize(r#"SELECT ?x1 WHERE { ?x1 <http://ex/p2> "a \" b"@en ; ex:age 42 ; ex:h 1.5e3 . FILTER(?y > -7) } LIMIT 10 # who"#),
            r#"SELECT ?x1 WHERE { ?x1 <http://ex/p2> ?@en ; ex:age ? ; ex:h ? . FILTER(?y > -?) } LIMIT ? "#
        );
        assert_eq!(normalize("ASK { _:b1 ex:item2 '''long ' text''' }"), "ASK { _:b1 ex:item2 ? }");
        assert_eq!(normalize(r#""2026-10-19"^^xsd:date"#), "?^^xsd:date");
        assert_eq!(normalize("FILTER(?a < ?b)"), "FILTER(?a < ?b)");
        assert_eq!(normalize("ex:άλφα \"βήτα\""), "ex:άλφα ?");
    }

    #[test]
    fn plans() {
        let json = br#"{"planning duration in seconds": 0.001, "plan": {"name": "Project(?n)", "number of results": 3, "duration in seconds": 0.0025, "children": [{"name": "QuadPattern(?p <http://ex/name> \"x\")", "number of results": 3, "children": []}]}}"#;
        let (text, nodes) = plan_text(json, 2048, false).unwrap();
        assert_eq!(nodes, 2);
        assert_eq!(text, "Project(?n) rows=3 time=2.500ms\n  QuadPattern(?p <http://ex/name> ?) rows=3");
        let (short, _) = plan_text(json, 60, true).unwrap();
        assert!(short.len() <= 60, "{short}");
        assert_eq!(short, "Project(?n) rows=3 time=2.500ms\n… 1 node more");
        let (tiny, _) = plan_text(json, 30, true).unwrap();
        assert!(tiny.len() <= 30, "{tiny}");
    }
}
