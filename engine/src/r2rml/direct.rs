// SPDX-License-Identifier: Apache-2.0
//! The W3C Direct Mapping (https://www.w3.org/TR/rdb-direct-mapping/) of
//! tables, written as an R2RML document that kb.add_mapping() then takes
//! as any other: a row an IRI from its table and its primary key's values,
//! or a blank node without one; its table its class; each column a
//! predicate, <base/Table#Column>; each foreign key a reference,
//! <base/Table#ref-Col1;Col2>, to the row it names.
//!
//! The tables come described as kb.direct_mapping() gathers them from the
//! catalogs (sql/pg_knowledge_base--0.1.sql): a JSON array of {name, table,
//! kind (pg_class.relkind), columns: [{name, type, class}], primary_key:
//! [names], foreign_keys: [{columns, ref_name, ref_table, ref_columns}]}.

use json_event_parser::{JsonEvent, SliceJsonParser};

use crate::error::{EResult, EngineError};

#[derive(Debug, Clone)]
enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    String(String),
    Other,
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self, key: &str) -> EResult<&str> {
        match self.get(key) {
            Some(Json::String(s)) => Ok(s),
            _ => Err(EngineError::invalid(format!("the tables' description lacks \"{key}\""))),
        }
    }

    fn array(&self, key: &str) -> &[Json] {
        match self.get(key) {
            Some(Json::Array(items)) => items,
            _ => &[],
        }
    }

    fn strings(&self, key: &str) -> Vec<String> {
        self.array(key)
            .iter()
            .filter_map(|j| match j {
                Json::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }
}

fn parse_value(parser: &mut SliceJsonParser<'_>, first: JsonEvent<'_>) -> EResult<Json> {
    let syntax = |e: json_event_parser::JsonSyntaxError| EngineError::invalid(format!("invalid JSON: {e}"));
    Ok(match first {
        JsonEvent::String(s) => Json::String(s.into_owned()),
        JsonEvent::Number(_) | JsonEvent::Boolean(_) | JsonEvent::Null => Json::Other,
        JsonEvent::StartArray => {
            let mut items = Vec::new();
            loop {
                match parser.parse_next().map_err(syntax)? {
                    JsonEvent::EndArray => break,
                    event => items.push(parse_value(parser, event)?),
                }
            }
            Json::Array(items)
        }
        JsonEvent::StartObject => {
            let mut fields = Vec::new();
            loop {
                match parser.parse_next().map_err(syntax)? {
                    JsonEvent::EndObject => break,
                    JsonEvent::ObjectKey(k) => {
                        let k = k.into_owned();
                        let event = parser.parse_next().map_err(syntax)?;
                        fields.push((k, parse_value(parser, event)?));
                    }
                    _ => return Err(EngineError::invalid("invalid JSON object")),
                }
            }
            Json::Object(fields)
        }
        _ => return Err(EngineError::invalid("invalid JSON")),
    })
}

fn parse_json(text: &str) -> EResult<Json> {
    let mut parser = SliceJsonParser::new(text.as_bytes());
    let first = parser
        .parse_next()
        .map_err(|e| EngineError::invalid(format!("invalid JSON: {e}")))?;
    parse_value(&mut parser, first)
}

/// The Direct Mapping's percent-encoding of a name or a value in an IRI
/// (its §2.1, as R2RML's IRI-safe strings): anything not unreserved.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') || (c as u32) >= 0xA0 {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// Text in a string template: its braces and backslashes escaped
fn template_text(s: &str) -> String {
    s.replace('\\', "\\\\").replace('{', "\\{").replace('}', "\\}")
}

/// A delimited SQL identifier, as R2RML's rr:column takes it
fn sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A Turtle string literal
fn turtle(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn iri(s: &str) -> String {
    format!("<{s}>")
}

pub fn direct_mapping(tables_json: &str, base_iri: &str) -> EResult<String> {
    let tables = match parse_json(tables_json)? {
        Json::Array(items) => items,
        _ => return Err(EngineError::invalid("the tables' description is not an array")),
    };
    let names: Vec<String> = tables
        .iter()
        .map(|t| t.str("name").map(str::to_owned))
        .collect::<EResult<_>>()?;
    let map_id = |i: usize| format!("<urn:kb:direct:{}>", encode(&names[i]));
    let mut out = String::new();
    out.push_str("@prefix rr: <http://www.w3.org/ns/r2rml#> .\n");
    out.push_str(&format!("# The W3C Direct Mapping of {} table(s), base <{base_iri}>\n\n", tables.len()));
    for (i, table) in tables.iter().enumerate() {
        let name = table.str("name")?;
        let local = table.str("table")?;
        let table_iri = format!("{base_iri}{}", encode(local));
        let pk = table.strings("primary_key");
        let columns: Vec<String> = table
            .array("columns")
            .iter()
            .map(|c| c.str("name").map(str::to_owned))
            .collect::<EResult<_>>()?;
        out.push_str(&format!("{} a rr:TriplesMap ;\n", map_id(i)));
        let keyless_table = table.strings("primary_key").is_empty()
            && matches!(table.get("kind"), Some(Json::String(k)) if k == "r" || k == "p");
        if keyless_table {
            out.push_str(&format!(
                "    rr:logicalTable [ rr:sqlQuery {} ] ;\n",
                turtle(&format!(
                    "SELECT tableoid::text || ':' || ctid::text AS \"kb:row\", * FROM {name}"
                ))
            ));
        } else {
            out.push_str(&format!(
                "    rr:logicalTable [ rr:tableName {} ] ;\n",
                turtle(name)
            ));
        }
        // the row's node: an IRI of its key's values; without a key, a blank
        // node of the row's own, its physical address within the snapshot
        // (two equal rows are two nodes, as the Direct Mapping's are)
        let has_row_id = pk.is_empty() && matches!(table.get("kind"), Some(Json::String(k)) if k == "r" || k == "p");
        let template = if pk.is_empty() {
            if has_row_id {
                "{\"kb:row\"}".to_owned()
            } else {
                columns
                    .iter()
                    .map(|c| format!("{{{}}}", sql_ident(c).replace('{', "\\{").replace('}', "\\}")))
                    .collect::<Vec<_>>()
                    .join(";")
            }
        } else {
            let mut template = template_text(&format!("{table_iri}/"));
            for (k, col) in pk.iter().enumerate() {
                if k > 0 {
                    template.push(';');
                }
                template.push_str(&template_text(&encode(col)));
                template.push('=');
                template.push('{');
                template.push_str(&sql_ident(col).replace('\\', "\\\\").replace('{', "\\{").replace('}', "\\}"));
                template.push('}');
            }
            template
        };
        out.push_str(&format!(
            "    rr:subjectMap [ rr:template {}{} ; rr:class {} ]",
            turtle(&template),
            if pk.is_empty() { " ; rr:termType rr:BlankNode" } else { "" },
            iri(&table_iri)
        ));
        // a column, a predicate
        for col in &columns {
            out.push_str(&format!(
                " ;\n    rr:predicateObjectMap [ rr:predicate {} ; rr:objectMap [ rr:column {} ] ]",
                iri(&format!("{table_iri}#{}", encode(col))),
                turtle(&sql_ident(col))
            ));
        }
        // a foreign key, a reference
        for fk in table.array("foreign_keys") {
            let ref_name = fk.str("ref_name")?;
            let Some(parent) = names.iter().position(|n| n == ref_name) else {
                continue;
            };
            let fk_columns = fk.strings("columns");
            let ref_columns = fk.strings("ref_columns");
            let predicate = format!(
                "{table_iri}#ref-{}",
                fk_columns.iter().map(|c| encode(c)).collect::<Vec<_>>().join(";")
            );
            let joins: Vec<String> = fk_columns
                .iter()
                .zip(&ref_columns)
                .map(|(c, p)| {
                    format!(
                        "[ rr:child {} ; rr:parent {} ]",
                        turtle(&sql_ident(c)),
                        turtle(&sql_ident(p))
                    )
                })
                .collect();
            out.push_str(&format!(
                " ;\n    rr:predicateObjectMap [ rr:predicate {} ; rr:objectMap [ rr:parentTriplesMap {} ; rr:joinCondition {} ] ]",
                iri(&predicate),
                map_id(parent),
                joins.join(", ")
            ));
        }
        out.push_str(" .\n\n");
    }
    Ok(out)
}
