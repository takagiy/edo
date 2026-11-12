pub mod cli;
pub mod command;
pub mod diagnostic;
pub mod edit;
pub mod harness;
pub mod hook;
pub mod install;
pub mod installer;
pub mod language;
pub mod lsp;
pub mod matcher;
pub mod mcp;
pub mod outline;
pub mod scope;
pub mod view;

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// Files a glob matched but edo has no language package for, counted by extension.
pub type Skipped = BTreeMap<String, usize>;

/// Text of a report for the agent: MISON, which spends fewer tokens than JSON on the same data.
pub fn report(v: &Value) -> String {
    mison::to_string(&Mison(v))
}

/// A JSON value as MISON writes it. A `Value` has no type to take heights from, so each array takes its
/// height from its elements.
struct Mison<'a>(&'a Value);

impl mison::Encode for Mison<'_> {
    /// Unused: `write` passes each array's height to the writer itself.
    const HEIGHT: usize = 0;

    fn write(&self, w: &mut mison::Writer) {
        match self.0 {
            Value::Null => {}
            Value::Bool(b) => w.literal(if *b { "true" } else { "false" }),
            Value::Number(n) => w.literal(&n.to_string()),
            Value::String(s) => w.string(s),
            Value::Array(xs) => w.seq(height(self.0), xs.len(), |s| xs.iter().for_each(|x| s.element(&Mison(x)))),
            Value::Object(m) => w.record(|r| m.iter().for_each(|(k, v)| r.field(k, &Mison(v)))),
        }
    }
}

/// How deeply arrays nest in `v`: 0 for a value that is not an array.
fn height(v: &Value) -> usize {
    match v {
        Value::Array(xs) => 1 + xs.iter().map(height).max().unwrap_or(0),
        _ => 0,
    }
}

/// `{"reason":"unsupported_language","extensions":{...}}`, shared by every place that reports skipped files.
pub fn skipped_json(skipped: &Skipped) -> Value {
    json!({"reason": "unsupported_language", "extensions": skipped})
}

/// An error reported to the agent as `{"ok":false,"error":code,"message":...}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub skipped: Skipped,
    /// Which of several requested items failed, counting from 1; absent when only one was requested.
    pub item: Option<usize>,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Error { code, message: message.into(), skipped: Skipped::new(), item: None }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({"ok": false, "error": self.code, "message": self.message});
        if !self.skipped.is_empty() {
            v["skipped"] = skipped_json(&self.skipped);
        }
        if let Some(item) = self.item {
            v["item"] = item.into();
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_in_array_field_are_in_parentheses() {
        // Given: ambiguous の報告。matches はレコードの配列
        let v = json!({"ok": false, "error": "ambiguous", "matches": [
            {"range": "2-2", "parents": "fn a()"},
            {"range": "5-5", "parents": "fn b()"},
        ]});
        // When: 報告を書く
        // Then: 各レコードが丸括弧で囲まれ、, で区切られる
        assert_eq!(
            report(&v),
            r#"ok=false error=ambiguous matches=(range=2-2 parents="fn a()"),(range=5-5 parents="fn b()")"#
        );
    }

    #[test]
    fn array_separator_follows_nesting_depth() {
        // Given: 配列の配列、数の配列、空の配列、null を持つ報告
        let v = json!({"a": [[1, 2], [3]], "b": [1, 2], "c": [], "d": null});
        // When: 報告を書く
        // Then: 深さ 2 の配列は ; で、深さ 1 の配列は , で区切られ、空の配列と null は何も書かれない
        assert_eq!(report(&v), "a=1,2;3 b=1,2 c= d=");
    }
}
