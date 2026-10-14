//! The stripping rules of nbstripout's `strip_output` and `strip_zeppelin_output`.

use std::fmt;

use crate::json::{Map, Value};
use crate::notebook::{Document, Error, Kind};
use crate::py;

/// Metadata keys nbstripout removes by default.
pub const DEFAULT_EXTRA_KEYS: [&str; 8] = [
    "metadata.signature",
    "metadata.widgets",
    "cell.metadata.collapsed",
    "cell.metadata.ExecuteTime",
    "cell.metadata.execution",
    "cell.metadata.heading_collapsed",
    "cell.metadata.hidden",
    "cell.metadata.scrolled",
];

/// An output type filter such as `stream` or `stream:stderr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputType {
    output_type: String,
    name: Option<String>,
}

impl std::str::FromStr for OutputType {
    type Err = String;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let mut parts = spec.split(':');
        let output_type = parts.next().unwrap_or_default().to_owned();
        let name = parts.next().map(str::to_owned);
        if parts.next().is_some() {
            return Err(format!(
                "invalid output type `{spec}`: expected `output_type` or `output_type:name`"
            ));
        }
        Ok(OutputType { output_type, name })
    }
}

impl fmt::Display for OutputType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{}:{name}", self.output_type),
            None => f.write_str(&self.output_type),
        }
    }
}

impl OutputType {
    fn matches(&self, output: &Value) -> bool {
        let Some(output) = output.as_object() else {
            return false;
        };
        let field = |key: &str| output.get(key).and_then(Value::as_str);
        field("output_type") == Some(self.output_type.as_str())
            && self
                .name
                .as_deref()
                .map_or(true, |name| field("name") == Some(name))
    }
}

/// Keys to pop from the notebook metadata and from every cell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraKeys {
    pub metadata: Vec<String>,
    pub cell: Vec<String>,
}

impl ExtraKeys {
    /// Sort `metadata.*` and `cell.*` keys into their namespaces. Returns the
    /// keys that fit neither, which nbstripout ignores with a warning.
    pub fn parse<'a>(keys: impl IntoIterator<Item = &'a str>) -> (ExtraKeys, Vec<&'a str>) {
        let mut parsed = ExtraKeys::default();
        let mut invalid = Vec::new();
        for key in keys {
            match key.split_once('.') {
                Some(("metadata", rest)) => parsed.metadata.push(rest.to_owned()),
                Some(("cell", rest)) => parsed.cell.push(rest.to_owned()),
                _ => invalid.push(key),
            }
        }
        (parsed, invalid)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `None` defers to the notebook's `metadata.keep_output`.
    pub keep_output: Option<bool>,
    pub keep_count: bool,
    pub keep_id: bool,
    pub extra_keys: ExtraKeys,
    pub drop_empty_cells: bool,
    pub drop_tagged_cells: Vec<String>,
    pub strip_init_cells: bool,
    pub drop_output_types: Vec<OutputType>,
    pub keep_output_types: Vec<OutputType>,
    pub max_size: i64,
}

/// Strip a notebook in place. Returns whether anything changed, with the same
/// meaning as nbstripout's `nb_orig != nb_stripped`.
pub fn strip(doc: &mut Document, opts: &Options) -> Result<bool, Error> {
    match doc.kind {
        Kind::Zeppelin => Ok(strip_zeppelin(&mut doc.root)),
        Kind::V2 | Kind::V3 | Kind::V4 { .. } => strip_jupyter(doc, opts),
    }
}

fn strip_zeppelin(root: &mut Map) -> bool {
    let mut changed = false;
    let paragraphs = root.get_mut("paragraphs").and_then(Value::as_array_mut);
    for paragraph in paragraphs.into_iter().flatten() {
        if let Some(results) = paragraph.as_object_mut().and_then(|p| p.get_mut("results")) {
            changed |= !matches!(results, Value::Object(m) if m.is_empty());
            *results = Value::Object(Map::new());
        }
    }
    changed
}

/// `dict.pop(key)` where `key` is a `.`-delimited path (nbstripout's `pop_recursive`).
/// A literal key containing dots takes precedence over the nested path.
fn pop_recursive(map: &mut Map, key: &str) -> bool {
    if map.shift_remove(key).is_some() {
        return true;
    }
    let Some((head, tail)) = key.split_once('.') else {
        return false;
    };
    match map.get_mut(head) {
        Some(Value::Object(child)) => pop_recursive(child, tail),
        _ => false,
    }
}

/// `any(line.strip() for line in cell.get('source', []))`.
fn has_content(source: Option<&Value>) -> bool {
    match source {
        Some(Value::String(s)) => !s.chars().all(py::is_space),
        Some(Value::Array(lines)) => lines
            .iter()
            .any(|line| line.as_str().is_some_and(|s| !s.chars().all(py::is_space))),
        _ => false,
    }
}

fn tags(cell: &Map) -> Option<&Value> {
    cell.get("metadata")?.as_object()?.get("tags")
}

/// nbstripout's `determine_keep_output`.
fn keep_cell_output(cell: &Map, default: bool, strip_init_cells: bool) -> Result<bool, Error> {
    let Some(metadata) = cell.get("metadata") else {
        return Ok(default);
    };
    let Some(metadata) = metadata.as_object() else {
        return Ok(default);
    };
    if let Some(init_cell) = metadata.get("init_cell") {
        return Ok(py::truthy(init_cell) && !strip_init_cells);
    }
    let keep_meta = metadata.get("keep_output");
    let keep_meta_value = keep_meta.is_some_and(py::truthy);
    let keep_tag = metadata
        .get("tags")
        .is_some_and(|tags| py::contains(tags, "keep_output"));
    if keep_meta.is_some() && keep_tag && !keep_meta_value {
        return Err(Error::Invalid(
            "cell metadata contradicts tags: `keep_output` is false, but `keep_output` in tags"
                .to_owned(),
        ));
    }
    if keep_meta.is_some() || keep_tag {
        return Ok(keep_meta_value || keep_tag);
    }
    Ok(default)
}

fn strip_jupyter(doc: &mut Document, opts: &Options) -> Result<bool, Error> {
    let kind = doc.kind;
    let legacy = matches!(kind, Kind::V2 | Kind::V3);
    // A repaired cell is either renumbered or dropped below; both are changes.
    let mut changed = doc.ids_repaired && !opts.keep_id;

    let keep_output = match opts.keep_output {
        Some(keep) => keep,
        None => doc
            .root
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|m| m.get("keep_output"))
            .is_some_and(py::truthy),
    };

    if let Some(Value::Object(metadata)) = doc.root.get_mut("metadata") {
        for key in &opts.extra_keys.metadata {
            changed |= pop_recursive(metadata, key);
        }
    }

    let mut index = 0usize;
    for cells in doc.cell_lists_mut() {
        if let Some(i) = cells.iter().position(|c| !matches!(c, Value::Object(_))) {
            return Err(Error::Invalid(format!(
                "invalid notebook: cell {i} is not an object"
            )));
        }

        let before = cells.len();
        cells.retain(|cell| {
            let cell = cell.as_object().expect("checked above");
            if opts.drop_empty_cells {
                // nbstripout looks at `source` only, which drops every v2/v3 code
                // cell (their text lives in `input`). Look where the text is.
                let field =
                    if legacy && cell.get("cell_type").and_then(Value::as_str) == Some("code") {
                        "input"
                    } else {
                        "source"
                    };
                if !has_content(cell.get(field)) {
                    return false;
                }
            }
            // nbstripout's closure-in-a-loop bug only honors the last tag; drop a
            // cell carrying any of them, as documented.
            !opts
                .drop_tagged_cells
                .iter()
                .any(|tag| tags(cell).is_some_and(|tags| py::contains(tags, tag)))
        });
        changed |= cells.len() != before;

        for cell in cells.iter_mut() {
            let cell = cell.as_object_mut().expect("checked above");
            // nbformat v2 turns PNG/JPEG data of code cells into `bytes`.
            let bytes_images =
                kind == Kind::V2 && cell.get("cell_type").and_then(Value::as_str) == Some("code");
            changed |= strip_cell(cell, index, keep_output, bytes_images, opts)?;
            index += 1;
        }
    }
    Ok(changed)
}

fn set_null(value: &mut Value) -> bool {
    let changed = !value.is_null();
    *value = Value::Null;
    changed
}

/// Extra characters `get_size` counts for image data held as `bytes`
/// (`len(str(b'...'))` instead of `len('...')`).
fn bytes_overhead(output: &Value) -> i64 {
    let Some(output) = output.as_object() else {
        return 0;
    };
    ["png", "jpeg"]
        .iter()
        .filter_map(|key| output.get(*key)?.as_str())
        .map(|data| (py::bytes_repr_len(data) - data.chars().count()) as i64)
        .sum()
}

fn strip_cell(
    cell: &mut Map,
    index: usize,
    keep_output: bool,
    bytes_images: bool,
    opts: &Options,
) -> Result<bool, Error> {
    let mut changed = false;
    let keep_this = keep_cell_output(cell, keep_output, opts.strip_init_cells)?;

    if let Some(outputs) = cell.get_mut("outputs") {
        let Value::Array(outputs) = outputs else {
            return Err(Error::Invalid(format!(
                "invalid notebook: outputs of cell {index} are not a list"
            )));
        };
        let before = outputs.len();
        if !keep_this || !opts.keep_output_types.is_empty() {
            outputs.retain(|output| {
                let limit = if bytes_images {
                    opts.max_size.saturating_sub(bytes_overhead(output))
                } else {
                    opts.max_size
                };
                py::size_at_most(output, limit)
                    || opts.keep_output_types.iter().any(|t| t.matches(output))
            });
        }
        if !opts.keep_count {
            for output in outputs.iter_mut().filter_map(Value::as_object_mut) {
                if let Some(count) = output.get_mut("execution_count") {
                    changed |= set_null(count);
                }
            }
        }
        if !opts.drop_output_types.is_empty() {
            outputs.retain(|output| !opts.drop_output_types.iter().any(|t| t.matches(output)));
        }
        changed |= outputs.len() != before;
    }

    if !opts.keep_count {
        for key in ["prompt_number", "execution_count"] {
            if let Some(count) = cell.get_mut(key) {
                changed |= set_null(count);
            }
        }
    }

    if !opts.keep_id {
        if let Some(id) = cell.get_mut("id") {
            let new_id = index.to_string();
            changed |= id.as_str() != Some(new_id.as_str());
            *id = Value::String(new_id);
        }
    }

    for key in &opts.extra_keys.cell {
        changed |= pop_recursive(cell, key);
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::{read, Mode};

    fn options() -> Options {
        let (extra_keys, _) = ExtraKeys::parse(DEFAULT_EXTRA_KEYS);
        Options {
            extra_keys,
            ..Options::default()
        }
    }

    fn run(src: &str, opts: &Options) -> (bool, String) {
        let mut doc = read(src.as_bytes(), Mode::Jupyter).unwrap();
        let changed = strip(&mut doc, opts).unwrap();
        (changed, String::from_utf8(doc.render()).unwrap())
    }

    const NB: &str = r#"{"cells": [
        {"cell_type": "code", "execution_count": 3, "id": "abc", "metadata": {"collapsed": true, "tags": ["t1"]},
         "outputs": [{"output_type": "stream", "name": "stdout", "text": "hi\n"},
                     {"output_type": "execute_result", "execution_count": 3, "metadata": {}, "data": {"text/plain": "3"}}],
         "source": "1 + 2"},
        {"cell_type": "markdown", "id": "def", "metadata": {"tags": ["t2"]}, "source": "  \n"},
        {"cell_type": "code", "execution_count": 4, "id": "ghi", "metadata": {"tags": ["keep_output"]},
         "outputs": [{"output_type": "error", "ename": "E", "evalue": "v", "traceback": []}], "source": "raise"}],
        "metadata": {"widgets": {}, "kernelspec": {"name": "python3"}}, "nbformat": 4, "nbformat_minor": 5}"#;

    #[test]
    fn strips_outputs_counts_ids_and_metadata() {
        let (changed, out) = run(NB, &options());
        assert!(changed);
        assert!(
            !out.contains("\"hi\\n\"") && !out.contains("collapsed") && !out.contains("widgets")
        );
        assert!(out.contains("\"id\": \"0\"") && out.contains("\"id\": \"2\""));
        assert!(
            out.contains("\"ename\": \"E\""),
            "keep_output tag keeps outputs"
        );
        assert!(!out.contains("\"execution_count\": 3") && !out.contains("\"execution_count\": 4"));
    }

    #[test]
    fn stripping_is_idempotent() {
        let (_, once) = run(NB, &options());
        let (changed, twice) = run(&once, &options());
        assert!(!changed);
        assert_eq!(once, twice);
    }

    #[test]
    fn drops_cells_with_any_listed_tag_and_empty_cells() {
        let opts = Options {
            drop_tagged_cells: vec!["t1".into(), "nope".into()],
            drop_empty_cells: true,
            ..options()
        };
        let (_, out) = run(NB, &opts);
        assert_eq!(out.matches("\"cell_type\"").count(), 1);
        assert!(
            out.contains("\"id\": \"0\""),
            "ids are renumbered after dropping"
        );
    }

    #[test]
    fn output_type_filters() {
        let opts = Options {
            keep_output: Some(true),
            drop_output_types: vec!["stream:stdout".parse().unwrap()],
            ..options()
        };
        let (_, out) = run(NB, &opts);
        assert!(!out.contains("\"hi\\n\"") && out.contains("\"text/plain\""));

        let opts = Options {
            keep_output_types: vec!["stream".parse().unwrap()],
            ..options()
        };
        let (_, out) = run(NB, &opts);
        assert!(out.contains("\"hi\\n\"") && !out.contains("\"text/plain\""));
        assert!("a:b:c".parse::<OutputType>().is_err());
    }

    #[test]
    fn contradicting_keep_output_is_an_error() {
        let src = r#"{"cells": [{"cell_type": "code", "metadata": {"keep_output": false, "tags": ["keep_output"]},
            "outputs": [], "source": "", "execution_count": null}], "metadata": {}, "nbformat": 4, "nbformat_minor": 4}"#;
        let mut doc = read(src.as_bytes(), Mode::Jupyter).unwrap();
        assert!(strip(&mut doc, &options()).is_err());
    }

    #[test]
    fn pop_recursive_prefers_literal_dotted_keys() {
        let Value::Object(mut m) = crate::json::parse(r#"{"a.b": 1, "a": {"b": 2}}"#).unwrap()
        else {
            unreachable!()
        };
        assert!(pop_recursive(&mut m, "a.b"));
        assert!(m["a"].as_object().unwrap().contains_key("b"));
        assert!(pop_recursive(&mut m, "a.b"));
        assert!(!pop_recursive(&mut m, "a.b"));
    }
}
