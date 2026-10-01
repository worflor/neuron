// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Pure, bounded transforms for explicitly selected Unicode clipboard text.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};

pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_OPERATIONS: usize = 32;
pub const MAX_PREVIEW_BYTES: usize = 8192;
const MAX_REGEX_SIZE: usize = 1024 * 1024;

/// One deterministic clipboard operation. The order in `ClipboardTransform::ops` is significant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum TransformOp {
    Uppercase,
    Lowercase,
    Titlecase,
    Trim,
    TrimLines,
    LinesReverse,
    LinesSort,
    LinesUnique,
    JsonPretty,
    JsonCompact,
    CsvToJson,
    JsonToCsv,
    Base64Encode,
    Base64Decode,
    UrlEncode,
    UrlDecode,
    Regex { pattern: String, replace: String },
}

/// One named byte range selected in an example string for deterministic regex generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariableSelection {
    pub start_byte: usize,
    pub end_byte: usize,
    pub name: String,
    pub kind: String,
    pub delimiter: String,
}

/// An ordered transform chain stored in an action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardTransform {
    pub ops: Vec<TransformOp>,
}

/// Failure categories the action and inline preview can present independently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransformError {
    TooManyOperations,
    InputTooLarge,
    OutputTooLarge,
    InvalidText(String),
    InvalidOperation(String),
}

impl std::fmt::Display for TransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyOperations => write!(f, "at most {MAX_OPERATIONS} operations are allowed"),
            Self::InputTooLarge => write!(f, "clipboard text exceeds {MAX_TEXT_BYTES} bytes"),
            Self::OutputTooLarge => write!(f, "transform output exceeds {MAX_TEXT_BYTES} bytes"),
            Self::InvalidText(message) | Self::InvalidOperation(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for TransformError {}

/// Apply every operation in memory, returning a complete plain-text result or one failure.
pub fn apply(input: &str, ops: &[TransformOp]) -> Result<String, TransformError> {
    if input.len() > MAX_TEXT_BYTES {
        return Err(TransformError::InputTooLarge);
    }
    if ops.len() > MAX_OPERATIONS {
        return Err(TransformError::TooManyOperations);
    }
    let mut current = input.to_owned();
    for op in ops {
        current = apply_one(&current, op)?;
        if current.len() > MAX_TEXT_BYTES {
            return Err(TransformError::OutputTooLarge);
        }
    }
    Ok(current)
}

pub fn validate_regex(pattern: &str, replace: &str) -> Result<(), TransformError> {
    let regex = compile_regex(pattern)?;
    validate_replacement(&regex, replace)
}

/// Generate the concrete Rust-regex sample pattern from a fixed prefix, selected text, and suffix.
pub fn pattern_from_example(prefix: &str, segment: &str, suffix: &str, name: &str, kind: &str, delimiter: &str) -> Result<String, TransformError> {
    if segment.is_empty() { return Err(TransformError::InvalidOperation("select a non-empty sample segment".into())); }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(TransformError::InvalidOperation("capture names start with a letter or underscore and use letters, digits, or underscore".into()));
    }
    let capture = match kind {
        "literal" => escape_regex_literal(segment),
        "digits" => r"\d+".into(),
        "word" => r"\w+".into(),
        "until-delimiter" => {
            let mut chars = delimiter.chars();
            let Some(delimiter) = chars.next() else { return Err(TransformError::InvalidOperation("choose a delimiter".into())); };
            if chars.next().is_some() { return Err(TransformError::InvalidOperation("delimiter must be one character".into())); }
            format!("[^{0}]+", escape_class_char(delimiter))
        }
        _ => return Err(TransformError::InvalidOperation("unknown example capture kind".into())),
    };
    Ok(format!("{}(?P<{name}>{capture}){}", escape_regex_literal(prefix), escape_regex_literal(suffix)))
}

/// Generate one regex with several ordered, named ranges and escaped fixed text between them.
pub fn pattern_from_selections(sample: &str, variables: &[VariableSelection]) -> Result<String, TransformError> {
    if variables.is_empty() { return Err(TransformError::InvalidOperation("mark at least one sample variable".into())); }
    let mut names = std::collections::HashSet::new();
    let mut pattern = String::new();
    let mut cursor = 0usize;
    for variable in variables {
        let start = variable.start_byte;
        let end = variable.end_byte;
        if start < cursor || start >= end || end > sample.len()
            || !sample.is_char_boundary(start) || !sample.is_char_boundary(end)
        {
            return Err(TransformError::InvalidOperation("variable ranges must be non-empty, ordered UTF-8 boundaries".into()));
        }
        validate_capture_name(&variable.name)?;
        if !names.insert(variable.name.as_str()) { return Err(TransformError::InvalidOperation("capture names must be unique".into())); }
        pattern.push_str(&escape_regex_literal(&sample[cursor..start]));
        let fragment = variable_fragment(&sample[start..end], &variable.name, &variable.kind, &variable.delimiter)?;
        pattern.push_str(&format!("(?P<{}>{fragment})", variable.name));
        cursor = end;
    }
    pattern.push_str(&escape_regex_literal(&sample[cursor..]));
    Ok(pattern)
}

/// Build an example pattern from byte offsets reported by Slint's native text selection.
/// Offsets must land on UTF-8 boundaries and select at least one scalar value.
pub fn pattern_from_selection(sample: &str, start: usize, end: usize, name: &str, kind: &str, delimiter: &str) -> Result<String, TransformError> {
    pattern_from_selections(sample, &[VariableSelection { start_byte: start, end_byte: end, name: name.into(), kind: kind.into(), delimiter: delimiter.into() }])
}

fn validate_capture_name(name: &str) -> Result<(), TransformError> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(TransformError::InvalidOperation("capture names start with a letter or underscore and use letters, digits, or underscore".into()));
    }
    Ok(())
}

fn variable_fragment(segment: &str, name: &str, kind: &str, delimiter: &str) -> Result<String, TransformError> {
    validate_capture_name(name)?;
    match kind {
        "literal" => Ok(escape_regex_literal(segment)),
        "digits" => Ok(r"\d+".into()),
        "word" => Ok(r"\w+".into()),
        "until-delimiter" => {
            let mut chars = delimiter.chars();
            let Some(delimiter) = chars.next() else { return Err(TransformError::InvalidOperation("choose a delimiter".into())); };
            if chars.next().is_some() { return Err(TransformError::InvalidOperation("delimiter must be one character".into())); }
            Ok(format!("[^{0}]+", escape_class_char(delimiter)))
        }
        _ => Err(TransformError::InvalidOperation("unknown example capture kind".into())),
    }
}

/// Preview one regex against a bounded user-provided sample; no clipboard access occurs.
pub fn preview_regex(sample: &str, pattern: &str, replace: &str) -> Result<(bool, String), TransformError> {
    if sample.len() > MAX_PREVIEW_BYTES { return Err(TransformError::InputTooLarge); }
    let regex = compile_regex(pattern)?;
    validate_replacement(&regex, replace)?;
    let matched = regex.is_match(sample);
    let output = regex_replace(sample, pattern, replace)?;
    Ok((matched, output.chars().take(512).collect()))
}

fn escape_regex_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' ) { out.push('\\'); }
        out.push(ch);
    }
    out
}

fn escape_class_char(ch: char) -> String {
    if matches!(ch, '\\' | ']' | '^' | '-') { format!("\\{ch}") } else { ch.to_string() }
}

fn apply_one(input: &str, op: &TransformOp) -> Result<String, TransformError> {
    match op {
        TransformOp::Uppercase => bounded_collect(input.chars().flat_map(char::to_uppercase)),
        TransformOp::Lowercase => bounded_collect(input.chars().flat_map(char::to_lowercase)),
        TransformOp::Titlecase => titlecase(input),
        TransformOp::Trim => Ok(input.trim().to_owned()),
        TransformOp::TrimLines => {
            let (lines, newline, terminal) = split_lines(input)?;
            let mut out = BoundedString::new();
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    out.push_str(newline)?;
                }
                out.push_str(line.trim())?;
            }
            if terminal && !lines.is_empty() {
                out.push_str(newline)?;
            }
            Ok(out.finish())
        }
        TransformOp::LinesReverse | TransformOp::LinesSort | TransformOp::LinesUnique => {
            transform_lines(input, op)
        }
        TransformOp::JsonPretty => json_format(input, true),
        TransformOp::JsonCompact => json_format(input, false),
        TransformOp::CsvToJson => csv_to_json(input),
        TransformOp::JsonToCsv => json_to_csv(input),
        TransformOp::Base64Encode => {
            let output_len = input.len().saturating_add(2) / 3 * 4;
            if output_len > MAX_TEXT_BYTES {
                return Err(TransformError::OutputTooLarge);
            }
            Ok(STANDARD.encode(input.as_bytes()))
        }
        TransformOp::Base64Decode => {
            let bytes = STANDARD
                .decode(input.trim())
                .map_err(|e| TransformError::InvalidText(format!("invalid base64: {e}")))?;
            String::from_utf8(bytes)
                .map_err(|e| TransformError::InvalidText(format!("base64 is not UTF-8: {e}")))
        }
        TransformOp::UrlEncode => url_encode(input),
        TransformOp::UrlDecode => url_decode(input),
        TransformOp::Regex { pattern, replace } => regex_replace(input, pattern, replace),
    }
}

fn bounded_collect<I: Iterator<Item = char>>(chars: I) -> Result<String, TransformError> {
    let mut output = BoundedString::new();
    for ch in chars {
        output.push_char(ch)?;
    }
    Ok(output.finish())
}

fn titlecase(input: &str) -> Result<String, TransformError> {
    let mut output = BoundedString::new();
    let mut in_word = false;
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            if in_word {
                for lower in ch.to_lowercase() {
                    output.push_char(lower)?;
                }
            } else {
                for upper in ch.to_uppercase() {
                    output.push_char(upper)?;
                }
            }
            in_word = true;
        } else {
            output.push_char(ch)?;
            in_word = false;
        }
    }
    Ok(output.finish())
}

fn split_lines(input: &str) -> Result<(Vec<&str>, &'static str, bool), TransformError> {
    let has_crlf = input.contains("\r\n");
    if has_crlf && input.replace("\r\n", "").contains('\n') {
        return Err(TransformError::InvalidOperation("line operations require consistent LF or CRLF endings".into()));
    }
    let newline = if has_crlf { "\r\n" } else { "\n" };
    let terminal = input.ends_with('\n');
    let body = if terminal { &input[..input.len() - 1] } else { input };
    let body = if newline == "\r\n" { body.strip_suffix('\r').unwrap_or(body) } else { body };
    let lines = if body.is_empty() && input.is_empty() { Vec::new() } else {
        body.split(newline).collect()
    };
    Ok((lines, newline, terminal))
}

fn transform_lines(input: &str, op: &TransformOp) -> Result<String, TransformError> {
    let (lines, newline, terminal) = split_lines(input)?;
    let mut owned: Vec<&str> = lines;
    match op {
        TransformOp::LinesReverse => owned.reverse(),
        TransformOp::LinesSort => owned.sort_unstable(),
        TransformOp::LinesUnique => {
            let mut seen = std::collections::HashSet::new();
            owned.retain(|line| seen.insert(*line));
        }
        _ => unreachable!(),
    }
    let mut out = BoundedString::new();
    for (index, line) in owned.iter().enumerate() {
        if index > 0 {
            out.push_str(newline)?;
        }
        out.push_str(line)?;
    }
    if terminal && !owned.is_empty() {
        out.push_str(newline)?;
    }
    Ok(out.finish())
}

fn json_format(input: &str, pretty: bool) -> Result<String, TransformError> {
    let value: Value = serde_json::from_str(input)
        .map_err(|e| TransformError::InvalidText(format!("invalid JSON: {e}")))?;
    let mut out = BoundedVec::new();
    if pretty {
        serde_json::to_writer_pretty(&mut out, &value)
    } else {
        serde_json::to_writer(&mut out, &value)
    }
    .map_err(map_json_write_error)?;
    String::from_utf8(out.0).map_err(|e| TransformError::InvalidText(e.to_string()))
}

fn csv_to_json(input: &str) -> Result<String, TransformError> {
    let mut reader = csv::ReaderBuilder::new().has_headers(true).from_reader(input.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| TransformError::InvalidText(format!("invalid CSV header: {e}")))?
        .clone();
    if headers.is_empty() || headers.iter().any(str::is_empty) {
        return Err(TransformError::InvalidText("CSV needs non-empty headers".into()));
    }
    let mut unique = std::collections::HashSet::new();
    if headers.iter().any(|header| !unique.insert(header)) {
        return Err(TransformError::InvalidText("CSV headers must be unique".into()));
    }
    let mut out = BoundedVec::new();
    out.write_all(b"[").map_err(map_write_error)?;
    let mut first_row = true;
    for record in reader.records() {
        let record = record.map_err(|e| TransformError::InvalidText(format!("invalid CSV: {e}")))?;
        if record.len() != headers.len() {
            return Err(TransformError::InvalidText("CSV rows must match the header width".into()));
        }
        if !first_row { out.write_all(b",").map_err(map_write_error)?; }
        first_row = false;
        out.write_all(b"{").map_err(map_write_error)?;
        for (index, (key, value)) in headers.iter().zip(record.iter()).enumerate() {
            if index > 0 { out.write_all(b",").map_err(map_write_error)?; }
            serde_json::to_writer(&mut out, key).map_err(map_json_write_error)?;
            out.write_all(b":").map_err(map_write_error)?;
            serde_json::to_writer(&mut out, value).map_err(map_json_write_error)?;
        }
        out.write_all(b"}").map_err(map_write_error)?;
    }
    out.write_all(b"]").map_err(map_write_error)?;
    String::from_utf8(out.0).map_err(|e| TransformError::InvalidText(e.to_string()))
}

fn json_to_csv(input: &str) -> Result<String, TransformError> {
    let value: Value = serde_json::from_str(input)
        .map_err(|e| TransformError::InvalidText(format!("invalid JSON: {e}")))?;
    let rows = value.as_array().ok_or_else(|| TransformError::InvalidText(
        "JSON-to-CSV expects an array of rectangular objects".into(),
    ))?;
    if rows.is_empty() {
        return Err(TransformError::InvalidText("JSON-to-CSV needs at least one object row".into()));
    }
    let headers: Vec<String> = rows[0].as_object().ok_or_else(|| TransformError::InvalidText(
        "JSON-to-CSV expects object rows".into(),
    ))?.keys().cloned().collect();
    for row in rows {
        let object = row.as_object().ok_or_else(|| TransformError::InvalidText(
            "JSON-to-CSV expects object rows".into(),
        ))?;
        if object.len() != headers.len() || headers.iter().any(|key| !object.contains_key(key)) {
            return Err(TransformError::InvalidText("JSON rows must have identical fields".into()));
        }
        if object.values().any(|v| !v.is_null() && !v.is_string() && !v.is_boolean() && !v.is_number()) {
            return Err(TransformError::InvalidText("JSON-to-CSV values must be scalar".into()));
        }
    }
    let mut out = BoundedVec::new();
    {
        let mut writer = csv::WriterBuilder::new().from_writer(&mut out);
        writer.write_record(&headers).map_err(map_csv_error)?;
        for row in rows {
            let object = row.as_object().ok_or_else(|| TransformError::InvalidText("invalid object row".into()))?;
            let cells: Vec<String> = headers.iter().map(|key| scalar_csv(object.get(key).unwrap_or(&Value::Null))).collect();
            writer.write_record(&cells).map_err(map_csv_error)?;
        }
        writer.flush().map_err(map_write_error)?;
    }
    String::from_utf8(out.0).map_err(|e| TransformError::InvalidText(e.to_string()))
}

fn scalar_csv(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn compile_regex(pattern: &str) -> Result<regex::Regex, TransformError> {
    regex::RegexBuilder::new(pattern)
        .size_limit(MAX_REGEX_SIZE)
        .dfa_size_limit(MAX_REGEX_SIZE)
        .build()
        .map_err(|e| TransformError::InvalidOperation(format!("invalid regex: {e}")))
}

fn validate_replacement(regex: &regex::Regex, replace: &str) -> Result<(), TransformError> {
    let names: std::collections::HashSet<&str> = regex.capture_names().flatten().collect();
    let captures = regex.captures_len();
    let bytes = replace.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' { index += 1; continue; }
        index += 1;
        if index < bytes.len() && bytes[index] == b'$' { index += 1; continue; }
        let (key, end) = replacement_reference(replace, index)?;
        if key.bytes().all(|b| b.is_ascii_digit()) {
            let number: usize = key.parse().unwrap_or(usize::MAX);
            if number >= captures { return Err(TransformError::InvalidOperation(format!("regex has no capture ${key}"))); }
        } else if !names.contains(key) {
            return Err(TransformError::InvalidOperation(format!("regex has no named capture ${{{key}}}")));
        }
        index = end;
    }
    Ok(())
}

fn regex_replace(input: &str, pattern: &str, replace: &str) -> Result<String, TransformError> {
    let regex = compile_regex(pattern)?;
    validate_replacement(&regex, replace)?;
    let mut out = BoundedString::new();
    let mut last = 0;
    for captures in regex.captures_iter(input) {
        let whole = captures.get(0).ok_or_else(|| TransformError::InvalidOperation("regex match has no whole capture".into()))?;
        out.push_str(&input[last..whole.start()])?;
        append_replacement(&mut out, &captures, replace)?;
        last = whole.end();
        if whole.is_empty() && last == input.len() { break; }
    }
    out.push_str(&input[last..])?;
    Ok(out.finish())
}

fn append_replacement(out: &mut BoundedString, captures: &regex::Captures<'_>, replace: &str) -> Result<(), TransformError> {
    let bytes = replace.as_bytes();
    let mut literal_start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' { index += 1; continue; }
        out.push_str(&replace[literal_start..index])?;
        index += 1;
        if index < bytes.len() && bytes[index] == b'$' {
            out.push_char('$')?;
            index += 1;
            literal_start = index;
            continue;
        }
        let (key, end) = replacement_reference(replace, index)?;
        if key.bytes().all(|b| b.is_ascii_digit()) {
            let number = key.parse::<usize>().unwrap_or(usize::MAX);
            if let Some(value) = captures.get(number) { out.push_str(value.as_str())?; }
        } else if let Some(value) = captures.name(key) {
            out.push_str(value.as_str())?;
        }
        index = end;
        literal_start = index;
    }
    out.push_str(&replace[literal_start..])
}

fn replacement_reference(replace: &str, mut index: usize) -> Result<(&str, usize), TransformError> {
    let bytes = replace.as_bytes();
    if index < bytes.len() && bytes[index] == b'{' {
        let start = index + 1;
        let close = bytes[start..].iter().position(|b| *b == b'}').map(|p| start + p)
            .ok_or_else(|| TransformError::InvalidOperation("unterminated regex replacement capture".into()))?;
        if start == close { return Err(TransformError::InvalidOperation("empty regex replacement capture".into())); }
        return Ok((&replace[start..close], close + 1));
    }
    let start = index;
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') { index += 1; }
    if start == index { return Err(TransformError::InvalidOperation("use $$, $1, $name, or ${name} in regex replacements".into())); }
    Ok((&replace[start..index], index))
}

fn url_encode(input: &str) -> Result<String, TransformError> {
    let mut out = BoundedString::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push_char(byte as char)?;
        } else {
            out.push_str(&format!("%{byte:02X}"))?;
        }
    }
    Ok(out.finish())
}

fn url_decode(input: &str) -> Result<String, TransformError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len().min(MAX_TEXT_BYTES));
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() { return Err(TransformError::InvalidText("incomplete URL escape".into())); }
            let hi = hex(bytes[index + 1]).ok_or_else(|| TransformError::InvalidText("invalid URL escape".into()))?;
            let lo = hex(bytes[index + 2]).ok_or_else(|| TransformError::InvalidText("invalid URL escape".into()))?;
            out.push((hi << 4) | lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
        if out.len() > MAX_TEXT_BYTES { return Err(TransformError::OutputTooLarge); }
    }
    String::from_utf8(out).map_err(|e| TransformError::InvalidText(format!("URL component is not UTF-8: {e}")))
}

fn hex(byte: u8) -> Option<u8> {
    match byte { b'0'..=b'9' => Some(byte - b'0'), b'a'..=b'f' => Some(byte - b'a' + 10), b'A'..=b'F' => Some(byte - b'A' + 10), _ => None }
}

struct BoundedString(String);

impl BoundedString {
    fn new() -> Self { Self(String::new()) }
    fn push_str(&mut self, value: &str) -> Result<(), TransformError> {
        if self.0.len().saturating_add(value.len()) > MAX_TEXT_BYTES { return Err(TransformError::OutputTooLarge); }
        self.0.push_str(value);
        Ok(())
    }
    fn push_char(&mut self, value: char) -> Result<(), TransformError> {
        if self.0.len().saturating_add(value.len_utf8()) > MAX_TEXT_BYTES { return Err(TransformError::OutputTooLarge); }
        self.0.push(value);
        Ok(())
    }
    fn finish(self) -> String { self.0 }
}

struct BoundedVec(Vec<u8>);

impl BoundedVec { fn new() -> Self { Self(Vec::new()) } }

impl Write for BoundedVec {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_TEXT_BYTES { return Err(io::Error::other("output limit")); }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn map_write_error(error: io::Error) -> TransformError {
    if error.to_string() == "output limit" { TransformError::OutputTooLarge }
    else { TransformError::InvalidText(error.to_string()) }
}

fn map_json_write_error(error: serde_json::Error) -> TransformError {
    if error.is_io() && error.to_string().contains("output limit") { TransformError::OutputTooLarge }
    else { TransformError::InvalidText(error.to_string()) }
}

fn map_csv_error(error: csv::Error) -> TransformError {
    if error.to_string().contains("output limit") { TransformError::OutputTooLarge }
    else { TransformError::InvalidText(format!("invalid CSV: {error}")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ops_keep_crlf_and_terminal_newline() {
        assert_eq!(apply("b\r\na\r\nb\r\n", &[TransformOp::LinesUnique]).unwrap(), "b\r\na\r\n");
        assert_eq!(apply("b\na", &[TransformOp::LinesReverse]).unwrap(), "a\nb");
    }

    #[test]
    fn unicode_case_and_encoding_round_trip() {
        assert_eq!(apply("hÉLLO world", &[TransformOp::Titlecase]).unwrap(), "Héllo World");
        let encoded = apply("a b/é", &[TransformOp::UrlEncode]).unwrap();
        assert_eq!(encoded, "a%20b%2F%C3%A9");
        assert_eq!(apply(&encoded, &[TransformOp::UrlDecode]).unwrap(), "a b/é");
    }

    #[test]
    fn json_csv_round_trip_and_validation() {
        let csv = "name,n\r\n\"Ada, L\",37\r\n";
        let json = apply(csv, &[TransformOp::CsvToJson]).unwrap();
        assert_eq!(apply(&json, &[TransformOp::JsonToCsv]).unwrap(), "n,name\n37,\"Ada, L\"\n");
        assert!(apply("[{\"a\":1},{\"b\":2}]", &[TransformOp::JsonToCsv]).is_err());
    }

    #[test]
    fn regex_replacements_validate_names_and_numbers() {
        let op = TransformOp::Regex { pattern: "(?P<name>\\w+)-(\\d+)".into(), replace: "${name}:$2:$$".into() };
        assert_eq!(apply("x-12 y-3", &[op]).unwrap(), "x:12:$ y:3:$");
        assert_eq!(apply("x-12", &[TransformOp::Regex { pattern: "(?P<name>x)-(\\d+)".into(), replace: "$name-${2}x".into() }]).unwrap(), "x-12x");
        assert_eq!(apply("é", &[TransformOp::Regex { pattern: "(?P<char>.)".into(), replace: "${char}".into() }]).unwrap(), "é");
        assert_eq!(apply("b", &[TransformOp::Regex { pattern: "(a)?b".into(), replace: "<$1>".into() }]).unwrap(), "<>");
        assert_eq!(apply("ab", &[TransformOp::Regex { pattern: "(?:)".into(), replace: "!".into() }]).unwrap(), "!a!b!");
        assert_eq!(apply("abc", &[TransformOp::Regex { pattern: "z".into(), replace: "$0".into() }]).unwrap(), "abc");
        assert!(validate_regex("(x)", "$1x").is_err());
        assert!(validate_regex("(x)", "$2").is_err());
        assert!(validate_regex("(?P<x>x)", "${missing}").is_err());
    }

    #[test]
    fn limits_stop_growth_before_append() {
        assert_eq!(apply("a".repeat(MAX_TEXT_BYTES + 1).as_str(), &[]), Err(TransformError::InputTooLarge));
        let expand = TransformOp::Regex { pattern: "(.)".into(), replace: "${1}${1}".into() };
        assert_eq!(apply(&"a".repeat(MAX_TEXT_BYTES), &[expand]), Err(TransformError::OutputTooLarge));
    }

    #[test]
    fn example_patterns_escape_fixed_text_and_preview_without_clipboard() {
        let pattern = pattern_from_example("date: ", "202602", " end", "date", "digits", "").unwrap();
        assert_eq!(pattern, r"date: (?P<date>\d+) end");
        let (matched, output) = preview_regex("date: 202602 end", &pattern, "${date}").unwrap();
        assert!(matched);
        assert_eq!(output, "202602");
        let literal = pattern_from_example("a.", "x+", ".z", "part", "literal", "").unwrap();
        assert_eq!(literal, r"a\.(?P<part>x\+)\.z");
        assert!(pattern_from_example("", "x", "", "1bad", "word", "").is_err());
    }

    #[test]
    fn multiple_example_variables_preserve_unicode_boundaries_and_names() {
        let sample = "id=42 · name=Zoë";
        let pattern = pattern_from_selections(sample, &[
            VariableSelection { start_byte: 3, end_byte: 5, name: "id".into(), kind: "digits".into(), delimiter: "".into() },
            VariableSelection { start_byte: 14, end_byte: 18, name: "person".into(), kind: "literal".into(), delimiter: "".into() },
        ]).unwrap();
        assert_eq!(pattern, r"id=(?P<id>\d+) · name=(?P<person>Zoë)");
        assert!(preview_regex(sample, &pattern, "${person}:${id}").unwrap().0);
        let invalid = [
            VariableSelection { start_byte: 3, end_byte: 5, name: "same".into(), kind: "digits".into(), delimiter: "".into() },
            VariableSelection { start_byte: 14, end_byte: 18, name: "same".into(), kind: "word".into(), delimiter: "".into() },
        ];
        assert!(pattern_from_selections(sample, &invalid).is_err());
        assert!(pattern_from_selection("é", 1, 2, "x", "literal", "").is_err());
    }
}
