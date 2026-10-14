//! Reading and writing notebooks with nbformat's exact (de)normalization rules.
//!
//! nbformat does not keep the on-disk structure: on read it joins multi-line
//! strings, drops transient keys and repairs cell ids; on write it splits strings
//! again (but only some of them) and pretty-prints with sorted keys. nbstripout
//! inherits all of this, so nbripout replays the same steps in the same order.

use std::collections::HashSet;
use std::fmt;

use crate::json::{self, Map, Style, Value};
use crate::py;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Jupyter,
    Zeppelin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    V2,
    V3,
    V4 { minor: i64 },
    Zeppelin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input is not UTF-8 encoded JSON (nbformat's `NotJSONError`).
    NotJson(String),
    /// Valid JSON that is not a notebook nbripout can process safely.
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotJson(msg) | Error::Invalid(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

/// A parsed notebook in nbformat's in-memory representation.
#[derive(Debug, Clone)]
pub struct Document {
    pub root: Map,
    pub kind: Kind,
    /// Cell ids were added or de-duplicated while reading. nbformat uses random
    /// ids there, so nbstripout always sees them change when it renumbers cells.
    pub ids_repaired: bool,
}

/// Parse and normalize a notebook the way `nbformat.read(..., NO_CONVERT)` does.
///
/// In Jupyter mode a document without `nbformat` but with `paragraphs` is treated
/// as a Zeppelin notebook; nbstripout crashes on those (for example when `*.zpln`
/// files reach its git filter, which always runs in Jupyter mode).
pub fn read(input: &[u8], mode: Mode) -> Result<Document, Error> {
    let input = input.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(input);
    let text = std::str::from_utf8(input)
        .map_err(|e| Error::NotJson(format!("invalid UTF-8 at byte {}", e.valid_up_to())))?;
    let Value::Object(mut root) = json::parse(text).map_err(|e| Error::NotJson(e.to_string()))?
    else {
        return Err(invalid("top-level JSON value is not an object"));
    };

    let zeppelin = match mode {
        Mode::Zeppelin => true,
        Mode::Jupyter => !root.contains_key("nbformat") && root.contains_key("paragraphs"),
    };
    if zeppelin {
        if !matches!(root.get("paragraphs"), Some(Value::Array(_))) {
            return Err(invalid(
                "not a Zeppelin notebook: 'paragraphs' is missing or not a list",
            ));
        }
        return Ok(Document {
            root,
            kind: Kind::Zeppelin,
            ids_repaired: false,
        });
    }

    let major = version_field(&root, "nbformat", 1)?;
    let minor = version_field(&root, "nbformat_minor", 0)?;
    let mut ids_repaired = false;
    let kind = match major {
        4 => {
            if !matches!(root.get("cells"), Some(Value::Array(_))) {
                return Err(invalid(
                    "invalid notebook: 'cells' is missing or not a list",
                ));
            }
            rejoin_lines_v4(&mut root);
            strip_transient_v4(&mut root);
            if minor >= 5 {
                ids_repaired = normalize_cell_ids(&mut root);
            }
            Kind::V4 { minor }
        }
        2 | 3 => {
            let worksheets_ok = match root.get("worksheets") {
                Some(Value::Array(sheets)) => sheets.iter().all(|ws| {
                    matches!(
                        ws.as_object().and_then(|ws| ws.get("cells")),
                        Some(Value::Array(_))
                    )
                }),
                _ => false,
            };
            if !worksheets_ok {
                return Err(invalid(format!(
                    "invalid v{major} notebook: malformed 'worksheets'"
                )));
            }
            if major == 2 {
                rejoin_lines_legacy(&mut root, |_| "\n");
                Kind::V2
            } else {
                rejoin_lines_legacy(&mut root, v3_separator);
                Kind::V3
            }
        }
        1 => {
            return Err(invalid(
                "nbformat v1 notebooks are not supported; upgrade them with \
                 `jupyter nbconvert --to notebook --inplace`",
            ))
        }
        _ => return Err(invalid(format!("unsupported nbformat version {major}"))),
    };
    Ok(Document {
        root,
        kind,
        ids_repaired,
    })
}

fn version_field(root: &Map, key: &str, default: i64) -> Result<i64, Error> {
    use json::Number;
    match root.get(key) {
        None => Ok(default),
        Some(Value::Number(Number::Int(digits))) => digits
            .parse()
            .map_err(|_| invalid(format!("invalid notebook: '{key}' is out of range"))),
        Some(Value::Number(Number::Float(x))) if x.fract() == 0.0 && x.abs() < 1e15 => {
            Ok(*x as i64)
        }
        Some(_) => Err(invalid(format!(
            "invalid notebook: '{key}' is not an integer"
        ))),
    }
}

impl Document {
    /// Serialize the way `nbformat.write` (or nbstripout's Zeppelin writer) does,
    /// including the trailing newline.
    pub fn render(mut self) -> Vec<u8> {
        let style = match self.kind {
            Kind::V4 { minor } => {
                split_lines_v4(&mut self.root);
                strip_transient_v4(&mut self.root);
                if minor >= 5 {
                    // nbformat.writes() validates (and thereby repairs) ids again.
                    normalize_cell_ids(&mut self.root);
                }
                Style::NBFORMAT_V4
            }
            Kind::V3 => {
                strip_transient_v3(&mut self.root);
                split_lines_legacy(&mut self.root, true);
                Style::NBFORMAT_V3
            }
            // nbformat v2 has no transient keys and splits without line endings.
            Kind::V2 => {
                split_lines_legacy(&mut self.root, false);
                Style::NBFORMAT_V3
            }
            Kind::Zeppelin => Style::ZEPPELIN,
        };
        let mut out = Vec::new();
        json::write(&mut out, &Value::Object(self.root), style);
        out.push(b'\n');
        out
    }

    /// The cell lists of the notebook: `cells` for v4, one per worksheet before.
    pub fn cell_lists_mut(&mut self) -> Vec<&mut Vec<Value>> {
        match self.kind {
            Kind::V4 { .. } => self
                .root
                .get_mut("cells")
                .and_then(Value::as_array_mut)
                .into_iter()
                .collect(),
            Kind::V2 | Kind::V3 => self
                .root
                .get_mut("worksheets")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
                .filter_map(|ws| ws.as_object_mut()?.get_mut("cells")?.as_array_mut())
                .collect(),
            Kind::Zeppelin => Vec::new(),
        }
    }
}

fn cells_mut(root: &mut Map) -> impl Iterator<Item = &mut Map> {
    root.get_mut("cells")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

fn outputs_mut(cell: &mut Map) -> impl Iterator<Item = &mut Map> {
    cell.get_mut("outputs")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

fn is_code_cell(cell: &Map) -> bool {
    cell.get("cell_type").and_then(Value::as_str) == Some("code")
}

/// Replace a list of strings by their concatenation (with `sep`). Lists holding
/// anything but strings are left alone; nbformat would raise on them.
fn join_strings(value: &mut Value, sep: impl Fn(&[Value]) -> &'static str) {
    let Value::Array(items) = value else { return };
    if !items.iter().all(|item| matches!(item, Value::String(_))) {
        return;
    }
    let sep = sep(items);
    let joined = items
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(sep);
    *value = Value::String(joined);
}

fn split_string(value: &mut Value) {
    split_string_with(value, true);
}

fn split_string_with(value: &mut Value, keepends: bool) {
    if let Value::String(s) = value {
        *value = Value::Array(py::splitlines(s, keepends));
    }
}

fn is_json_mime(mime: &str) -> bool {
    mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
}

fn rejoin_mimebundle(bundle: &mut Value) {
    if let Value::Object(bundle) = bundle {
        for (mime, data) in bundle.iter_mut() {
            if !is_json_mime(mime) {
                join_strings(data, |_| "");
            }
        }
    }
}

fn split_mimebundle(bundle: &mut Value) {
    if let Value::Object(bundle) = bundle {
        for (mime, data) in bundle.iter_mut() {
            if mime.starts_with("text/")
                || mime == "application/javascript"
                || mime == "image/svg+xml"
            {
                split_string(data);
            }
        }
    }
}

fn attachments_mut(cell: &mut Map) -> impl Iterator<Item = &mut Value> {
    cell.get_mut("attachments")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|attachments| attachments.values_mut())
}

/// nbformat.v4.rwbase.rejoin_lines
fn rejoin_lines_v4(root: &mut Map) {
    for cell in cells_mut(root) {
        if let Some(source) = cell.get_mut("source") {
            join_strings(source, |_| "");
        }
        attachments_mut(cell).for_each(rejoin_mimebundle);
        if !is_code_cell(cell) {
            continue;
        }
        for output in outputs_mut(cell) {
            let output_type = output.get("output_type").cloned().unwrap_or(Value::Null);
            match output_type.as_str() {
                Some("execute_result" | "display_data") => {
                    if let Some(data) = output.get_mut("data") {
                        rejoin_mimebundle(data);
                    }
                }
                _ if py::truthy(&output_type) => {
                    if let Some(text) = output.get_mut("text") {
                        join_strings(text, |_| "");
                    }
                }
                _ => {}
            }
        }
    }
}

/// nbformat.v4.rwbase.split_lines
fn split_lines_v4(root: &mut Map) {
    for cell in cells_mut(root) {
        if let Some(source) = cell.get_mut("source") {
            split_string(source);
        }
        attachments_mut(cell).for_each(split_mimebundle);
        if !is_code_cell(cell) {
            continue;
        }
        for output in outputs_mut(cell) {
            match output.get("output_type").and_then(Value::as_str) {
                Some("execute_result" | "display_data") => {
                    if let Some(data) = output.get_mut("data") {
                        split_mimebundle(data);
                    }
                }
                Some("stream") => {
                    if let Some(text) = output.get_mut("text") {
                        split_string(text);
                    }
                }
                _ => {}
            }
        }
    }
}

/// nbformat.v4.rwbase.strip_transient
fn strip_transient_v4(root: &mut Map) {
    if let Some(Value::Object(metadata)) = root.get_mut("metadata") {
        for key in ["orig_nbformat", "orig_nbformat_minor", "signature"] {
            metadata.shift_remove(key);
        }
    }
    for cell in cells_mut(root) {
        if let Some(Value::Object(metadata)) = cell.get_mut("metadata") {
            metadata.shift_remove("trusted");
        }
    }
}

/// nbformat's `_normalize`: give cells without an id, and later duplicates of an
/// id, a fresh one. nbformat draws random ids; nbripout derives them from the cell
/// position so that the output is reproducible. Returns whether any id changed.
fn normalize_cell_ids(root: &mut Map) -> bool {
    let Some(Value::Array(cells)) = root.get_mut("cells") else {
        return false;
    };
    let mut taken: HashSet<String> = HashSet::new();
    let mut needs_id = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let Some(cell) = cell.as_object() else {
            continue;
        };
        match cell.get("id") {
            None => needs_id.push(i),
            Some(Value::String(id)) => {
                if !taken.insert(id.clone()) {
                    needs_id.push(i);
                }
            }
            Some(_) => {}
        }
    }
    let repaired = !needs_id.is_empty();
    for i in needs_id {
        let base = i.to_string();
        let id = std::iter::once(base.clone())
            .chain((1..).map(|k| format!("{base}-{k}")))
            .find(|candidate| !taken.contains(candidate))
            .expect("unbounded candidate sequence");
        taken.insert(id.clone());
        if let Some(cell) = cells[i].as_object_mut() {
            cell.insert("id".to_owned(), Value::String(id));
        }
    }
    repaired
}

fn v3_cells_mut(root: &mut Map) -> impl Iterator<Item = &mut Map> {
    root.get_mut("worksheets")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|ws| ws.as_object_mut()?.get_mut("cells")?.as_array_mut())
        .flatten()
        .filter_map(Value::as_object_mut)
}

const V3_MULTILINE_OUTPUTS: [&str; 6] = ["text", "html", "svg", "latex", "javascript", "json"];

/// nbformat.v3.rwbase._join_lines: tolerate lists written by `splitlines()`
/// without line endings.
fn v3_separator(lines: &[Value]) -> &'static str {
    match lines.first().and_then(Value::as_str) {
        Some(first) if first.ends_with(['\n', '\r']) => "",
        _ => "\n",
    }
}

/// nbformat.v2/v3.rwbase.rejoin_lines (they differ only in the separator).
fn rejoin_lines_legacy(root: &mut Map, separator: fn(&[Value]) -> &'static str) {
    for cell in v3_cells_mut(root) {
        if is_code_cell(cell) {
            if let Some(input) = cell.get_mut("input") {
                join_strings(input, separator);
            }
            for output in outputs_mut(cell) {
                for key in V3_MULTILINE_OUTPUTS {
                    if let Some(item) = output.get_mut(key) {
                        join_strings(item, separator);
                    }
                }
            }
        } else {
            for key in ["source", "rendered"] {
                if let Some(item) = cell.get_mut(key) {
                    join_strings(item, separator);
                }
            }
        }
    }
}

/// nbformat.v2/v3.rwbase.split_lines: `splitlines(True)` for v3, `splitlines()`
/// for v2.
fn split_lines_legacy(root: &mut Map, keepends: bool) {
    for cell in v3_cells_mut(root) {
        if is_code_cell(cell) {
            if let Some(input) = cell.get_mut("input") {
                split_string_with(input, keepends);
            }
            for output in outputs_mut(cell) {
                for key in V3_MULTILINE_OUTPUTS {
                    if let Some(item) = output.get_mut(key) {
                        split_string_with(item, keepends);
                    }
                }
            }
        } else {
            for key in ["source", "rendered"] {
                if let Some(item) = cell.get_mut(key) {
                    split_string_with(item, keepends);
                }
            }
        }
    }
}

/// nbformat.v3.rwbase.strip_transient
fn strip_transient_v3(root: &mut Map) {
    root.shift_remove("orig_nbformat");
    root.shift_remove("orig_nbformat_minor");
    for cell in v3_cells_mut(root) {
        if let Some(Value::Object(metadata)) = cell.get_mut("metadata") {
            metadata.shift_remove("trusted");
        }
        cell.shift_remove("trusted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &str) -> String {
        String::from_utf8(read(src.as_bytes(), Mode::Jupyter).unwrap().render()).unwrap()
    }

    #[test]
    fn v4_lines_are_rejoined_and_split_like_nbformat() {
        let src = r#"{"cells": [{"cell_type": "code", "id": "x", "metadata": {"trusted": true},
            "execution_count": null, "source": ["a\n", "b"],
            "outputs": [
              {"output_type": "stream", "name": "stdout", "text": ["1\n", "2\n"]},
              {"output_type": "display_data", "metadata": {},
               "data": {"image/png": ["iVBO\n", "Rw0"], "text/plain": "x\ny",
                        "application/json": {"k": ["a\n", "b"]}}}]}],
            "metadata": {"signature": "s"}, "nbformat": 4, "nbformat_minor": 5}"#;
        let out = roundtrip(src);
        assert!(out.contains("\"image/png\": \"iVBO\\nRw0\""), "{out}");
        assert!(out.contains("\"text/plain\": [\n       \"x\\n\",\n       \"y\"\n      ]"));
        assert!(out.contains("\"k\": [\n        \"a\\n\",\n        \"b\"\n       ]"));
        assert!(!out.contains("trusted") && !out.contains("signature"));
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn missing_and_duplicate_ids_are_repaired_deterministically() {
        let src = r#"{"cells": [
            {"cell_type": "markdown", "metadata": {}, "source": ""},
            {"cell_type": "markdown", "id": "0", "metadata": {}, "source": ""},
            {"cell_type": "markdown", "id": "0", "metadata": {}, "source": ""}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#;
        let doc = read(src.as_bytes(), Mode::Jupyter).unwrap();
        let ids: Vec<_> = doc.root["cells"]
            .as_array()
            .iter()
            .map(|c| c.as_object().unwrap()["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, ["0-1", "0", "2"]);
    }

    #[test]
    fn v2_joins_with_newlines_and_splits_without_line_endings() {
        let src = r#"{"nbformat": 2, "metadata": {"name": "é"}, "worksheets": [{"cells": [
            {"cell_type": "code", "input": ["a\n", "b"], "outputs": [], "language": "python"}]}]}"#;
        let out = roundtrip(src);
        assert!(
            out.contains("\"input\": [\n      \"a\",\n      \"\",\n      \"b\"\n     ]"),
            "{out}"
        );
        assert!(out.contains("\"name\": \"\\u00e9\""));
    }

    #[test]
    fn zeppelin_is_detected_by_content() {
        let doc = read(br#"{"paragraphs": [], "name": "z"}"#, Mode::Jupyter).unwrap();
        assert_eq!(doc.kind, Kind::Zeppelin);
    }

    #[test]
    fn rejects_non_notebooks() {
        assert!(matches!(read(b"[]", Mode::Jupyter), Err(Error::Invalid(_))));
        assert!(matches!(read(b"{", Mode::Jupyter), Err(Error::NotJson(_))));
        assert!(matches!(
            read(b"\xff", Mode::Jupyter),
            Err(Error::NotJson(_))
        ));
        assert!(matches!(
            read(br#"{"nbformat": 5, "cells": []}"#, Mode::Jupyter),
            Err(Error::Invalid(_))
        ));
    }

    impl Value {
        fn as_array(&self) -> &[Value] {
            match self {
                Value::Array(items) => items,
                _ => panic!("not an array"),
            }
        }
    }
}
