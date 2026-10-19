// SPDX-License-Identifier: Apache-2.0
//! Term keys (docs/design.md §3.1): an RDF term as 16 bytes, computed from
//! the term alone, kept by the store in uuid columns.
//!
//! The first byte is the term's kind.  Small values are inline in the 15
//! bytes after it: booleans, numbers, dates and durations, strings and blank
//! node labels of a few bytes, blank nodes the engine names.  Any other term
//! is a 120-bit SipHash-2-4 of its text, and kb.terms holds the text.  Equal
//! terms have equal keys and nothing else does (but for a hash collision,
//! some 2^-60 for a billion terms), so the evaluator joins on keys and a key
//! computed for a query's constant, a value it computes or a row an R2RML
//! mapping reads matches the store's without a lookup.
//!
//! Literals of the datatypes oxigraph knows are kept as values, as its
//! EncodedTerm keeps them (oxigraph/lib/oxigraph/src/storage/numeric_encoder.rs):
//! "01"^^xsd:integer is "1"^^xsd:integer, xsd:int is xsd:integer, and so on,
//! so that the store answers as oxigraph's own does.  The key of the default
//! graph is all zeros.
//!
//! The layout is the store's format: changing it changes every key.

use oxrdf::{
    BlankNode, BlankNodeRef, Literal, LiteralRef, NamedNode, NamedNodeRef, Term, TermRef,
};
use oxsdatatypes::{
    Boolean, Date, DateTime, DayTimeDuration, Decimal, Double, Duration, Float, GDay, GMonth,
    GMonthDay, GYear, GYearMonth, Integer, Time, YearMonthDuration,
};
use siphasher::sip128::{Hasher128, SipHasher24};
use spareval::ExpressionTerm;
use std::hash::Hasher;
use std::str::FromStr;

pub type Key = [u8; 16];

pub const DEFAULT_GRAPH: Key = [0; 16];

pub const TAG_DEFAULT_GRAPH: u8 = 0x00;
pub const TAG_IRI: u8 = 0x01;
pub const TAG_BNODE_NUM: u8 = 0x02;
pub const TAG_BNODE_SMALL: u8 = 0x03;
pub const TAG_BNODE_HASH: u8 = 0x04;
pub const TAG_STRING_SMALL: u8 = 0x05;
pub const TAG_STRING_HASH: u8 = 0x06;
pub const TAG_LANG_SMALL: u8 = 0x07;
pub const TAG_LANG_HASH: u8 = 0x08;
pub const TAG_TYPED_HASH: u8 = 0x09;
pub const TAG_BOOLEAN: u8 = 0x0A;
pub const TAG_FLOAT: u8 = 0x0B;
pub const TAG_DOUBLE: u8 = 0x0C;
pub const TAG_INTEGER: u8 = 0x0D;
pub const TAG_DECIMAL: u8 = 0x0E;
pub const TAG_DATETIME: u8 = 0x0F;
pub const TAG_TIME: u8 = 0x10;
pub const TAG_DATE: u8 = 0x11;
pub const TAG_GYEARMONTH: u8 = 0x12;
pub const TAG_GYEAR: u8 = 0x13;
pub const TAG_GMONTHDAY: u8 = 0x14;
pub const TAG_GDAY: u8 = 0x15;
pub const TAG_GMONTH: u8 = 0x16;
pub const TAG_DURATION: u8 = 0x17;
pub const TAG_YEARMONTHDURATION: u8 = 0x18;
pub const TAG_DAYTIMEDURATION: u8 = 0x19;

pub const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
pub const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

const SMALL: usize = 14;

/// A hashed key's dictionary row: kb.terms (value, extra).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictRow {
    pub value: String,
    /// The datatype of a typed literal, the language of a language-tagged
    /// one, empty otherwise.
    pub extra: String,
}

/// Whether the key names its term only through the dictionary.
#[inline]
pub fn is_hashed(key: &Key) -> bool {
    matches!(
        key[0],
        TAG_IRI | TAG_BNODE_HASH | TAG_STRING_HASH | TAG_LANG_HASH | TAG_TYPED_HASH
    )
}

#[inline]
pub fn is_default_graph(key: &Key) -> bool {
    key[0] == TAG_DEFAULT_GRAPH
}

/// Whether the key may be a subject or a graph name: an IRI or a blank node.
#[inline]
pub fn is_resource(key: &Key) -> bool {
    matches!(
        key[0],
        TAG_IRI | TAG_BNODE_NUM | TAG_BNODE_SMALL | TAG_BNODE_HASH
    )
}

#[inline]
pub fn is_iri(key: &Key) -> bool {
    key[0] == TAG_IRI
}

fn hashed(tag: u8, parts: &[&[u8]]) -> Key {
    let mut hasher = SipHasher24::new();
    for part in parts {
        hasher.write(part);
    }
    let hash = u128::from(hasher.finish128()).to_be_bytes();
    let mut key = [0; 16];
    key[0] = tag;
    key[1..].copy_from_slice(&hash[..15]);
    key
}

fn inline(tag: u8, payload: &[u8]) -> Key {
    let mut key = [0; 16];
    key[0] = tag;
    key[1..=payload.len()].copy_from_slice(payload);
    key
}

fn small(tag: u8, bytes: &[u8]) -> Key {
    let mut key = [0; 16];
    key[0] = tag;
    key[1] = bytes.len() as u8;
    key[2..2 + bytes.len()].copy_from_slice(bytes);
    key
}

pub fn iri_key(iri: &str) -> Key {
    hashed(TAG_IRI, &[iri.as_bytes()])
}

pub fn named_node_key(node: NamedNodeRef<'_>) -> (Key, Option<DictRow>) {
    (
        iri_key(node.as_str()),
        Some(DictRow {
            value: node.as_str().to_owned(),
            extra: String::new(),
        }),
    )
}

/// A blank node label the engine can keep inline as a number: lower-case
/// hexadecimal without leading zeros, below 2^120 (oxigraph's numerical blank
/// nodes are the same, below 2^128).
fn bnode_number(id: &str) -> Option<u128> {
    let digits = id.as_bytes();
    if matches!(digits.first(), None | Some(b'0')) || digits.len() > 30 {
        return None;
    }
    let mut value: u128 = 0;
    for digit in digits {
        value = value * 16
            + u128::from(match *digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                _ => return None,
            });
    }
    Some(value)
}

pub fn blank_node_key(node: BlankNodeRef<'_>) -> (Key, Option<DictRow>) {
    let id = node.as_str();
    if let Some(n) = bnode_number(id) {
        (inline(TAG_BNODE_NUM, &n.to_be_bytes()[1..]), None)
    } else if id.len() <= SMALL {
        (small(TAG_BNODE_SMALL, id.as_bytes()), None)
    } else {
        (
            hashed(TAG_BNODE_HASH, &[id.as_bytes()]),
            Some(DictRow {
                value: id.to_owned(),
                extra: String::new(),
            }),
        )
    }
}

/// A fresh blank node the engine names: a random number of 120 bits, kept
/// inline.
pub fn fresh_blank_node() -> BlankNode {
    // at least 2^116, so that every label has 30 digits
    let n: u128 = (rand::random::<u128>() >> 8) | (1 << 116);
    BlankNode::new_unchecked(format!("{n:x}"))
}

fn string_key(value: &str) -> (Key, Option<DictRow>) {
    if value.len() <= SMALL {
        (small(TAG_STRING_SMALL, value.as_bytes()), None)
    } else {
        (
            hashed(TAG_STRING_HASH, &[value.as_bytes()]),
            Some(DictRow {
                value: value.to_owned(),
                extra: String::new(),
            }),
        )
    }
}

fn lang_string_key(value: &str, language: &str) -> (Key, Option<DictRow>) {
    if value.len() + language.len() <= SMALL - 1 {
        let mut key = [0; 16];
        key[0] = TAG_LANG_SMALL;
        key[1] = value.len() as u8;
        key[2] = language.len() as u8;
        key[3..3 + value.len()].copy_from_slice(value.as_bytes());
        key[3 + value.len()..3 + value.len() + language.len()].copy_from_slice(language.as_bytes());
        (key, None)
    } else {
        (
            hashed(TAG_LANG_HASH, &[language.as_bytes(), &[0], value.as_bytes()]),
            Some(DictRow {
                value: value.to_owned(),
                extra: language.to_owned(),
            }),
        )
    }
}

fn typed_key(value: &str, datatype: &str) -> (Key, Option<DictRow>) {
    (
        hashed(TAG_TYPED_HASH, &[datatype.as_bytes(), &[0], value.as_bytes()]),
        Some(DictRow {
            value: value.to_owned(),
            extra: datatype.to_owned(),
        }),
    )
}

/// 16 bytes of a signed number in its last `width` bytes, when they hold it.
fn shrink_i128(bytes: [u8; 16], width: usize) -> Option<Vec<u8>> {
    let cut = 16 - width;
    let sign = if bytes[cut] & 0x80 != 0 { 0xFF } else { 0x00 };
    if bytes[..cut].iter().all(|b| *b == sign) {
        Some(bytes[cut..].to_vec())
    } else {
        None
    }
}

fn grow_i128(bytes: &[u8]) -> [u8; 16] {
    let mut out = if bytes[0] & 0x80 != 0 { [0xFF; 16] } else { [0; 16] };
    out[16 - bytes.len()..].copy_from_slice(bytes);
    out
}

/// A timestamp-like value's 18 bytes (a decimal and a timezone offset) in 15:
/// the decimal's 13 low bytes, when they hold it, and the offset's 2.
fn timestamp_key(tag: u8, bytes: [u8; 18]) -> Option<Key> {
    let value: [u8; 16] = bytes[..16].try_into().ok()?;
    let low = shrink_i128(value, 13)?;
    let mut payload = low;
    payload.extend_from_slice(&bytes[16..18]);
    Some(inline(tag, &payload))
}

fn timestamp_bytes(key: &Key) -> [u8; 18] {
    let mut bytes = [0; 18];
    bytes[..16].copy_from_slice(&grow_i128(&key[1..14]));
    bytes[16..18].copy_from_slice(&key[14..16]);
    bytes
}

fn decimal_key(value: Decimal) -> Option<Key> {
    Some(inline(TAG_DECIMAL, &shrink_i128(value.to_be_bytes(), 15)?))
}

fn day_time_duration_key(value: DayTimeDuration) -> Option<Key> {
    Some(inline(
        TAG_DAYTIMEDURATION,
        &shrink_i128(value.to_be_bytes(), 15)?,
    ))
}

/// A duration's 24 bytes (months as 8, seconds as 16) in 15: months in 4,
/// seconds in 11, when they hold them.
fn duration_key(value: Duration) -> Option<Key> {
    let bytes = value.to_be_bytes();
    let months: [u8; 8] = bytes[..8].try_into().ok()?;
    let months = i64::from_be_bytes(months);
    let months = i32::try_from(months).ok()?;
    let seconds: [u8; 16] = bytes[8..24].try_into().ok()?;
    let seconds = shrink_i128(seconds, 11)?;
    let mut payload = months.to_be_bytes().to_vec();
    payload.extend_from_slice(&seconds);
    Some(inline(TAG_DURATION, &payload))
}

fn duration_value(key: &Key) -> Duration {
    let mut bytes = [0; 24];
    let months = i32::from_be_bytes(key[1..5].try_into().unwrap_or_default());
    bytes[..8].copy_from_slice(&i64::from(months).to_be_bytes());
    bytes[8..24].copy_from_slice(&grow_i128(&key[5..16]));
    Duration::from_be_bytes(bytes)
}

fn double_bytes(value: Double) -> [u8; 8] {
    if f64::from(value).is_nan() {
        f64::NAN.to_be_bytes()
    } else {
        value.to_be_bytes()
    }
}

fn float_bytes(value: Float) -> [u8; 4] {
    if f32::from(value).is_nan() {
        f32::NAN.to_be_bytes()
    } else {
        value.to_be_bytes()
    }
}

/// A literal of a datatype oxigraph keeps as a value, as that value's key;
/// None for any other literal, or one whose value does not fit.
fn value_key(value: &str, datatype: &str) -> Option<Key> {
    let Some(local) = datatype.strip_prefix(XSD) else {
        return None;
    };
    Some(match local {
        "boolean" => inline(
            TAG_BOOLEAN,
            &[u8::from(bool::from(Boolean::from_str(value).ok()?))],
        ),
        "float" => inline(TAG_FLOAT, &float_bytes(Float::from_str(value).ok()?)),
        "double" => inline(TAG_DOUBLE, &double_bytes(Double::from_str(value).ok()?)),
        "integer" | "byte" | "short" | "int" | "long" | "unsignedByte" | "unsignedShort"
        | "unsignedInt" | "unsignedLong" | "positiveInteger" | "negativeInteger"
        | "nonPositiveInteger" | "nonNegativeInteger" => {
            inline(TAG_INTEGER, &Integer::from_str(value).ok()?.to_be_bytes())
        }
        "decimal" => decimal_key(Decimal::from_str(value).ok()?)?,
        "dateTime" | "dateTimeStamp" => {
            timestamp_key(TAG_DATETIME, DateTime::from_str(value).ok()?.to_be_bytes())?
        }
        "time" => timestamp_key(TAG_TIME, Time::from_str(value).ok()?.to_be_bytes())?,
        "date" => timestamp_key(TAG_DATE, Date::from_str(value).ok()?.to_be_bytes())?,
        "gYearMonth" => timestamp_key(
            TAG_GYEARMONTH,
            GYearMonth::from_str(value).ok()?.to_be_bytes(),
        )?,
        "gYear" => timestamp_key(TAG_GYEAR, GYear::from_str(value).ok()?.to_be_bytes())?,
        "gMonthDay" => timestamp_key(
            TAG_GMONTHDAY,
            GMonthDay::from_str(value).ok()?.to_be_bytes(),
        )?,
        "gDay" => timestamp_key(TAG_GDAY, GDay::from_str(value).ok()?.to_be_bytes())?,
        "gMonth" => timestamp_key(TAG_GMONTH, GMonth::from_str(value).ok()?.to_be_bytes())?,
        "duration" => duration_key(Duration::from_str(value).ok()?)?,
        "yearMonthDuration" => inline(
            TAG_YEARMONTHDURATION,
            &YearMonthDuration::from_str(value).ok()?.to_be_bytes(),
        ),
        "dayTimeDuration" => day_time_duration_key(DayTimeDuration::from_str(value).ok()?)?,
        _ => return None,
    })
}

/// The canonical lexical form of a value whose key did not fit: the hash then
/// names the value, not the text it was written with.
fn canonical(value: &str, datatype: &str) -> Option<String> {
    let local = datatype.strip_prefix(XSD)?;
    Some(match local {
        "decimal" => Decimal::from_str(value).ok()?.to_string(),
        "dateTime" | "dateTimeStamp" => DateTime::from_str(value).ok()?.to_string(),
        "time" => Time::from_str(value).ok()?.to_string(),
        "date" => Date::from_str(value).ok()?.to_string(),
        "gYearMonth" => GYearMonth::from_str(value).ok()?.to_string(),
        "gYear" => GYear::from_str(value).ok()?.to_string(),
        "gMonthDay" => GMonthDay::from_str(value).ok()?.to_string(),
        "gDay" => GDay::from_str(value).ok()?.to_string(),
        "gMonth" => GMonth::from_str(value).ok()?.to_string(),
        "duration" => Duration::from_str(value).ok()?.to_string(),
        "dayTimeDuration" => DayTimeDuration::from_str(value).ok()?.to_string(),
        _ => return None,
    })
}

/// The datatype a value's canonical form carries: "dateTimeStamp" is
/// "dateTime", and so on, as oxigraph decodes them.
fn canonical_datatype(datatype: &str) -> &str {
    match datatype.strip_prefix(XSD) {
        Some("dateTimeStamp") => "http://www.w3.org/2001/XMLSchema#dateTime",
        _ => datatype,
    }
}

pub fn literal_key(literal: LiteralRef<'_>) -> (Key, Option<DictRow>) {
    let value = literal.value();
    if let Some(language) = literal.language() {
        return lang_string_key(value, language);
    }
    let datatype = literal.datatype().as_str();
    if datatype == XSD_STRING {
        return string_key(value);
    }
    if let Some(key) = value_key(value, datatype) {
        return (key, None);
    }
    if let Some(value) = canonical(value, datatype) {
        return typed_key(&value, canonical_datatype(datatype));
    }
    typed_key(value, datatype)
}

pub fn term_key(term: TermRef<'_>) -> (Key, Option<DictRow>) {
    match term {
        TermRef::NamedNode(node) => named_node_key(node),
        TermRef::BlankNode(node) => blank_node_key(node),
        TermRef::Literal(literal) => literal_key(literal),
    }
}

/// The key of a value the evaluator computed, without making its term when
/// the value is inline.
pub fn expression_term_key(term: &ExpressionTerm) -> Option<Key> {
    Some(match term {
        ExpressionTerm::BooleanLiteral(v) => inline(TAG_BOOLEAN, &[u8::from(bool::from(*v))]),
        ExpressionTerm::FloatLiteral(v) => inline(TAG_FLOAT, &float_bytes(*v)),
        ExpressionTerm::DoubleLiteral(v) => inline(TAG_DOUBLE, &double_bytes(*v)),
        ExpressionTerm::IntegerLiteral(v) => inline(TAG_INTEGER, &v.to_be_bytes()),
        ExpressionTerm::DecimalLiteral(v) => decimal_key(*v)?,
        ExpressionTerm::DateTimeLiteral(v) => timestamp_key(TAG_DATETIME, v.to_be_bytes())?,
        ExpressionTerm::TimeLiteral(v) => timestamp_key(TAG_TIME, v.to_be_bytes())?,
        ExpressionTerm::DateLiteral(v) => timestamp_key(TAG_DATE, v.to_be_bytes())?,
        ExpressionTerm::GYearMonthLiteral(v) => timestamp_key(TAG_GYEARMONTH, v.to_be_bytes())?,
        ExpressionTerm::GYearLiteral(v) => timestamp_key(TAG_GYEAR, v.to_be_bytes())?,
        ExpressionTerm::GMonthDayLiteral(v) => timestamp_key(TAG_GMONTHDAY, v.to_be_bytes())?,
        ExpressionTerm::GDayLiteral(v) => timestamp_key(TAG_GDAY, v.to_be_bytes())?,
        ExpressionTerm::GMonthLiteral(v) => timestamp_key(TAG_GMONTH, v.to_be_bytes())?,
        ExpressionTerm::DurationLiteral(v) => duration_key(*v)?,
        ExpressionTerm::YearMonthDurationLiteral(v) => {
            inline(TAG_YEARMONTHDURATION, &v.to_be_bytes())
        }
        ExpressionTerm::DayTimeDurationLiteral(v) => day_time_duration_key(*v)?,
        ExpressionTerm::StringLiteral(v) if v.len() <= SMALL => {
            small(TAG_STRING_SMALL, v.as_bytes())
        }
        _ => return None,
    })
}

/// An inline key's value as the evaluator's term, without making an RDF term.
pub fn inline_expression_term(key: &Key) -> Option<ExpressionTerm> {
    Some(match key[0] {
        TAG_BOOLEAN => ExpressionTerm::BooleanLiteral(Boolean::from(key[1] != 0)),
        TAG_FLOAT => ExpressionTerm::FloatLiteral(Float::from_be_bytes(key[1..5].try_into().ok()?)),
        TAG_DOUBLE => {
            ExpressionTerm::DoubleLiteral(Double::from_be_bytes(key[1..9].try_into().ok()?))
        }
        TAG_INTEGER => {
            ExpressionTerm::IntegerLiteral(Integer::from_be_bytes(key[1..9].try_into().ok()?))
        }
        TAG_DECIMAL => ExpressionTerm::DecimalLiteral(Decimal::from_be_bytes(grow_i128(&key[1..]))),
        TAG_DATETIME => ExpressionTerm::DateTimeLiteral(DateTime::from_be_bytes(timestamp_bytes(key))),
        TAG_TIME => ExpressionTerm::TimeLiteral(Time::from_be_bytes(timestamp_bytes(key))),
        TAG_DATE => ExpressionTerm::DateLiteral(Date::from_be_bytes(timestamp_bytes(key))),
        TAG_GYEARMONTH => {
            ExpressionTerm::GYearMonthLiteral(GYearMonth::from_be_bytes(timestamp_bytes(key)))
        }
        TAG_GYEAR => ExpressionTerm::GYearLiteral(GYear::from_be_bytes(timestamp_bytes(key))),
        TAG_GMONTHDAY => {
            ExpressionTerm::GMonthDayLiteral(GMonthDay::from_be_bytes(timestamp_bytes(key)))
        }
        TAG_GDAY => ExpressionTerm::GDayLiteral(GDay::from_be_bytes(timestamp_bytes(key))),
        TAG_GMONTH => ExpressionTerm::GMonthLiteral(GMonth::from_be_bytes(timestamp_bytes(key))),
        TAG_DURATION => ExpressionTerm::DurationLiteral(duration_value(key)),
        TAG_YEARMONTHDURATION => ExpressionTerm::YearMonthDurationLiteral(
            YearMonthDuration::from_be_bytes(key[1..9].try_into().ok()?),
        ),
        TAG_DAYTIMEDURATION => ExpressionTerm::DayTimeDurationLiteral(
            DayTimeDuration::from_be_bytes(grow_i128(&key[1..])),
        ),
        TAG_STRING_SMALL => {
            let len = usize::from(key[1]).min(SMALL);
            ExpressionTerm::StringLiteral(oxstr::OxString::new_owned(
                std::str::from_utf8(&key[2..2 + len]).ok()?,
            ))
        }
        _ => return None,
    })
}

/// An inline key's term; None for a hashed key, or the default graph's.
pub fn inline_term(key: &Key) -> Option<Term> {
    Some(match key[0] {
        TAG_BNODE_NUM => {
            let mut bytes = [0; 16];
            bytes[1..].copy_from_slice(&key[1..]);
            BlankNode::new_unchecked(format!("{:x}", u128::from_be_bytes(bytes))).into()
        }
        TAG_BNODE_SMALL => {
            let len = usize::from(key[1]).min(SMALL);
            BlankNode::new_unchecked(std::str::from_utf8(&key[2..2 + len]).ok()?.to_owned()).into()
        }
        TAG_STRING_SMALL => {
            let len = usize::from(key[1]).min(SMALL);
            Literal::new_simple_literal(std::str::from_utf8(&key[2..2 + len]).ok()?.to_owned()).into()
        }
        TAG_LANG_SMALL => {
            let vlen = usize::from(key[1]);
            let llen = usize::from(key[2]);
            if vlen + llen > SMALL - 1 {
                return None;
            }
            let value = std::str::from_utf8(&key[3..3 + vlen]).ok()?.to_owned();
            let language = std::str::from_utf8(&key[3 + vlen..3 + vlen + llen]).ok()?.to_owned();
            Literal::new_language_tagged_literal_unchecked(value, language).into()
        }
        TAG_BOOLEAN | TAG_FLOAT | TAG_DOUBLE | TAG_INTEGER | TAG_DECIMAL | TAG_DATETIME
        | TAG_TIME | TAG_DATE | TAG_GYEARMONTH | TAG_GYEAR | TAG_GMONTHDAY | TAG_GDAY
        | TAG_GMONTH | TAG_DURATION | TAG_YEARMONTHDURATION | TAG_DAYTIMEDURATION => {
            Term::from(inline_expression_term(key)?)
        }
        _ => return None,
    })
}

/// A hashed key's term, from its dictionary row.
pub fn hashed_term(key: &Key, value: &str, extra: &str) -> Option<Term> {
    Some(match key[0] {
        TAG_IRI => NamedNode::new_unchecked(value.to_owned()).into(),
        TAG_BNODE_HASH => BlankNode::new_unchecked(value.to_owned()).into(),
        TAG_STRING_HASH => Literal::new_simple_literal(value.to_owned()).into(),
        TAG_LANG_HASH => {
            Literal::new_language_tagged_literal_unchecked(value.to_owned(), extra.to_owned()).into()
        }
        TAG_TYPED_HASH => {
            Literal::new_typed_literal(value.to_owned(), NamedNode::new_unchecked(extra.to_owned()))
                .into()
        }
        _ => return None,
    })
}

/// A key as uuid's text, for messages.
pub fn key_text(key: &Key) -> String {
    let h: String = key.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(value: &'static str, datatype: &'static str) -> Literal {
        Literal::new_typed_literal(value, NamedNode::new_unchecked(datatype))
    }

    fn roundtrip(term: Term) -> Term {
        let (key, row) = term_key(term.as_ref());
        match row {
            Some(row) => hashed_term(&key, &row.value, &row.extra).unwrap(),
            None => inline_term(&key).unwrap(),
        }
    }

    #[test]
    fn values_are_canonical() {
        let a = term_key(lit("01", "http://www.w3.org/2001/XMLSchema#integer").as_ref().into()).0;
        let b = term_key(lit("1", "http://www.w3.org/2001/XMLSchema#int").as_ref().into()).0;
        assert_eq!(a, b);
        let c = term_key(lit("1.0", "http://www.w3.org/2001/XMLSchema#decimal").as_ref().into()).0;
        assert_ne!(a, c);
        assert_eq!(
            roundtrip(lit("01", "http://www.w3.org/2001/XMLSchema#int").into()),
            Term::from(Literal::from(1_i64))
        );
    }

    #[test]
    fn terms_roundtrip() {
        for term in [
            Term::from(NamedNode::new_unchecked("http://example.com/s")),
            BlankNode::new_unchecked("b1").into(),
            BlankNode::new_unchecked("a-rather-long-blank-node-label").into(),
            fresh_blank_node().into(),
            Literal::new_simple_literal("short").into(),
            Literal::new_simple_literal("a string longer than fourteen bytes").into(),
            Literal::new_language_tagged_literal_unchecked("chat", "fr").into(),
            Literal::new_language_tagged_literal_unchecked("a longer text", "en-gb").into(),
            lit("true", "http://www.w3.org/2001/XMLSchema#boolean").into(),
            lit("1.5", "http://www.w3.org/2001/XMLSchema#float").into(),
            lit("-2.5E10", "http://www.w3.org/2001/XMLSchema#double").into(),
            lit("NaN", "http://www.w3.org/2001/XMLSchema#double").into(),
            lit("-123456789012", "http://www.w3.org/2001/XMLSchema#integer").into(),
            lit("-12.25", "http://www.w3.org/2001/XMLSchema#decimal").into(),
            lit("2026-10-19T05:28:00Z", "http://www.w3.org/2001/XMLSchema#dateTime").into(),
            lit("2026-10-19T05:28:00.123+03:00", "http://www.w3.org/2001/XMLSchema#dateTime")
                .into(),
            lit("2026-10-19", "http://www.w3.org/2001/XMLSchema#date").into(),
            lit("12:30:00", "http://www.w3.org/2001/XMLSchema#time").into(),
            lit("2026-10", "http://www.w3.org/2001/XMLSchema#gYearMonth").into(),
            lit("P1Y2M3DT4H5M6S", "http://www.w3.org/2001/XMLSchema#duration").into(),
            lit("-P3D", "http://www.w3.org/2001/XMLSchema#dayTimeDuration").into(),
            lit("P14M", "http://www.w3.org/2001/XMLSchema#yearMonthDuration").into(),
            lit("abc", "http://www.w3.org/2001/XMLSchema#integer").into(),
            lit("x", "http://example.com/datatype").into(),
        ] {
            let back = roundtrip(term.clone());
            let (k1, _) = term_key(term.as_ref());
            let (k2, _) = term_key(back.as_ref());
            assert_eq!(k1, k2, "{term} came back as {back}");
        }
    }

    #[test]
    fn expression_terms_agree() {
        for term in [
            lit("42", "http://www.w3.org/2001/XMLSchema#integer"),
            lit("4.2", "http://www.w3.org/2001/XMLSchema#decimal"),
            lit("2026-10-19T05:28:00Z", "http://www.w3.org/2001/XMLSchema#dateTime"),
            lit("P1Y2M3DT4H5M6S", "http://www.w3.org/2001/XMLSchema#duration"),
            Literal::new_simple_literal("abc"),
        ] {
            let (key, _) = literal_key(term.as_ref());
            let expr = ExpressionTerm::from(Term::from(term.clone()));
            assert_eq!(expression_term_key(&expr), Some(key), "{term}");
            assert!(inline_expression_term(&key).is_some());
        }
    }

    #[test]
    fn big_decimals_hash_their_canonical_form() {
        // above 2^119 / 10^18, within oxigraph's i128
        let a = lit("123456789012345678901.5", "http://www.w3.org/2001/XMLSchema#decimal");
        let b = lit("0123456789012345678901.50", "http://www.w3.org/2001/XMLSchema#decimal");
        let (ka, ra) = literal_key(a.as_ref());
        let (kb, _) = literal_key(b.as_ref());
        assert_eq!(ka, kb);
        assert_eq!(ka[0], TAG_TYPED_HASH);
        assert_eq!(ra.unwrap().value, "123456789012345678901.5");
    }
}
