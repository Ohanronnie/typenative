//! Structured diagnostics shared by every `TypeNative` compiler surface.

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::ops::Range;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConditionId(String);

impl ConditionId {
    /// Creates a validated stable condition identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidConditionId`] unless the name starts with an ASCII uppercase letter and
    /// contains only ASCII uppercase letters, digits, and underscores.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidConditionId> {
        let id = id.into();
        let valid = !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && id
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_uppercase());
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidConditionId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidConditionId(String);

impl std::fmt::Display for InvalidConditionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "invalid diagnostic condition identifier: {}",
            self.0
        )
    }
}

impl std::error::Error for InvalidConditionId {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub file: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line: u32,
    pub unicode_column: u32,
}

impl SourceSpan {
    pub fn new(file: impl Into<String>, range: Range<usize>, source: &str) -> Self {
        let prefix = &source[..range.start.min(source.len())];
        let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let unicode_column = source[line_start..range.start.min(source.len())]
            .chars()
            .count()
            + 1;
        Self {
            file: file.into(),
            byte_start: u32::try_from(range.start).unwrap_or(u32::MAX),
            byte_end: u32::try_from(range.end).unwrap_or(u32::MAX),
            line: u32::try_from(line).unwrap_or(u32::MAX),
            unicode_column: u32::try_from(unicode_column).unwrap_or(u32::MAX),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub span: SourceSpan,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Applicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub span: SourceSpan,
    pub replacement: String,
    pub applicability: Applicability,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub condition: ConditionId,
    pub severity: Severity,
    pub message: String,
    pub primary: Label,
    #[serde(default)]
    pub secondary: Vec<Label>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub edits: Vec<Edit>,
    pub documentation_key: String,
}

impl Diagnostic {
    pub fn error(
        condition: ConditionId,
        message: impl Into<String>,
        primary: Label,
        documentation_key: impl Into<String>,
    ) -> Self {
        Self {
            condition,
            severity: Severity::Error,
            message: message.into(),
            primary,
            secondary: Vec::new(),
            notes: Vec::new(),
            edits: Vec::new(),
            documentation_key: documentation_key.into(),
        }
    }
}

pub fn render_text(diagnostic: &Diagnostic) -> String {
    let mut rendered = String::new();
    let span = &diagnostic.primary.span;
    let _ = writeln!(
        rendered,
        "{:?}[{}]: {}",
        diagnostic.severity,
        diagnostic.condition.as_str(),
        diagnostic.message
    );
    let _ = writeln!(
        rendered,
        " --> {}:{}:{} (bytes {}..{})",
        span.file, span.line, span.unicode_column, span.byte_start, span.byte_end
    );
    let _ = writeln!(rendered, "  = {}", diagnostic.primary.message);
    for label in &diagnostic.secondary {
        let _ = writeln!(
            rendered,
            "  = {}:{}:{}: {}",
            label.span.file, label.span.line, label.span.unicode_column, label.message
        );
    }
    for note in &diagnostic.notes {
        let _ = writeln!(rendered, "  = note: {note}");
    }
    rendered
}

/// Serializes one structured diagnostic.
///
/// # Errors
///
/// Returns the serializer error when the diagnostic cannot be represented as JSON.
pub fn render_json(diagnostic: &Diagnostic) -> Result<String, serde_json::Error> {
    serde_json::to_string(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_ids_are_stable_machine_names() {
        assert!(ConditionId::new("SYNTAX_INVALID_UTF8").is_ok());
        assert!(ConditionId::new("syntax.invalid").is_err());
        assert!(ConditionId::new("2_BAD").is_err());
    }

    #[test]
    fn source_span_counts_unicode_scalar_columns() {
        let source = "αβ\nγδ";
        let start = source.find('δ').expect("test character exists");
        let span = SourceSpan::new("sample.tn", start..start + 'δ'.len_utf8(), source);
        assert_eq!((span.line, span.unicode_column), (2, 2));
    }

    #[test]
    fn json_renderer_preserves_condition_and_span() {
        let source = "let x = ;";
        let diagnostic = Diagnostic::error(
            ConditionId::new("SYNTAX_EXPECTED_EXPRESSION").expect("valid id"),
            "expected an expression",
            Label {
                span: SourceSpan::new("sample.tn", 8..9, source),
                message: "expression required here".into(),
            },
            "syntax/expected-expression",
        );
        let value: serde_json::Value =
            serde_json::from_str(&render_json(&diagnostic).expect("diagnostic should serialize"))
                .expect("diagnostic should be valid JSON");
        assert_eq!(value["condition"], "SYNTAX_EXPECTED_EXPRESSION");
        assert_eq!(value["primary"]["span"]["line"], 1);
    }
}
