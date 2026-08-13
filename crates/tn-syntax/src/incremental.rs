use crate::{Parse, SyntaxKind, parse};
use std::ops::Range;
use tn_diagnostics::{Diagnostic, SourceSpan};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReparseStats {
    pub reparsed_range: Range<usize>,
    pub reused_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct IncrementalDocument {
    file: String,
    source: String,
    parse: Parse,
}

impl IncrementalDocument {
    pub fn new(file: impl Into<String>, source: String) -> Self {
        let file = file.into();
        let parse = parse(&file, source.as_bytes());
        Self {
            file,
            source,
            parse,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn parse(&self) -> &Parse {
        &self.parse
    }

    /// Applies an edit and reparses the smallest containing top-level declaration when possible.
    ///
    /// # Errors
    ///
    /// Returns an error when the byte range is reversed, out of bounds, or not on UTF-8 character
    /// boundaries.
    pub fn apply_edit(&mut self, edit: TextEdit) -> Result<ReparseStats, InvalidTextEdit> {
        if edit.range.start > edit.range.end
            || edit.range.end > self.source.len()
            || !self.source.is_char_boundary(edit.range.start)
            || !self.source.is_char_boundary(edit.range.end)
        {
            return Err(InvalidTextEdit(edit.range));
        }

        let old_length = self.source.len();
        let root = self.parse.syntax();
        let candidate = root.children().find(|node| {
            node.kind() != SyntaxKind::ATTRIBUTE
                && usize::from(node.text_range().start()) <= edit.range.start
                && usize::from(node.text_range().end()) >= edit.range.end
        });
        self.source
            .replace_range(edit.range.clone(), &edit.replacement);

        let Some(candidate) = candidate else {
            self.parse = parse(&self.file, self.source.as_bytes());
            return Ok(ReparseStats {
                reparsed_range: 0..self.source.len(),
                reused_bytes: 0,
            });
        };

        let old_range =
            usize::from(candidate.text_range().start())..usize::from(candidate.text_range().end());
        let length_delta = if self.source.len() >= old_length {
            isize::try_from(self.source.len() - old_length).ok()
        } else {
            isize::try_from(old_length - self.source.len())
                .ok()
                .map(|difference| -difference)
        };
        let Some(length_delta) = length_delta else {
            self.parse = parse(&self.file, self.source.as_bytes());
            return Ok(ReparseStats {
                reparsed_range: 0..self.source.len(),
                reused_bytes: 0,
            });
        };
        let new_end = old_range.end.saturating_add_signed(length_delta);
        if new_end > self.source.len() || !self.source.is_char_boundary(new_end) {
            self.parse = parse(&self.file, self.source.as_bytes());
            return Ok(ReparseStats {
                reparsed_range: 0..self.source.len(),
                reused_bytes: 0,
            });
        }

        let fragment_source = &self.source[old_range.start..new_end];
        let fragment = parse(&self.file, fragment_source.as_bytes());
        let fragment_root = fragment.syntax();
        let Some(replacement) = fragment_root
            .children()
            .find(|node| node.kind() == candidate.kind())
        else {
            self.parse = parse(&self.file, self.source.as_bytes());
            return Ok(ReparseStats {
                reparsed_range: 0..self.source.len(),
                reused_bytes: 0,
            });
        };

        let green = candidate.replace_with(replacement.green().into_owned());
        let mut diagnostics = self
            .parse
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                let start = diagnostic.primary.span.byte_start as usize;
                let end = diagnostic.primary.span.byte_end as usize;
                end <= old_range.start || start >= old_range.end
            })
            .cloned()
            .collect::<Vec<_>>();
        for diagnostic in &mut diagnostics {
            adjust_diagnostic_after_edit(diagnostic, old_range.end, length_delta, &self.source);
        }
        diagnostics.extend(fragment.diagnostics.into_iter().map(|mut diagnostic| {
            shift_diagnostic(&mut diagnostic, old_range.start, &self.source);
            diagnostic
        }));
        self.parse = Parse { green, diagnostics };
        Ok(ReparseStats {
            reparsed_range: old_range.start..new_end,
            reused_bytes: old_length.saturating_sub(old_range.len()),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidTextEdit(Range<usize>);

impl std::fmt::Display for InvalidTextEdit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid UTF-8 text edit range: {:?}", self.0)
    }
}

impl std::error::Error for InvalidTextEdit {}

fn adjust_diagnostic_after_edit(
    diagnostic: &mut Diagnostic,
    old_end: usize,
    delta: isize,
    source: &str,
) {
    visit_spans(diagnostic, |span| {
        let range = span.byte_start as usize..span.byte_end as usize;
        if range.start >= old_end {
            let start = range.start.saturating_add_signed(delta);
            let end = range.end.saturating_add_signed(delta);
            *span = SourceSpan::new(span.file.clone(), start..end, source);
        }
    });
}

fn shift_diagnostic(diagnostic: &mut Diagnostic, offset: usize, source: &str) {
    visit_spans(diagnostic, |span| {
        let start = span.byte_start as usize + offset;
        let end = span.byte_end as usize + offset;
        *span = SourceSpan::new(span.file.clone(), start..end, source);
    });
}

fn visit_spans(diagnostic: &mut Diagnostic, mut visit: impl FnMut(&mut SourceSpan)) {
    visit(&mut diagnostic.primary.span);
    for label in &mut diagnostic.secondary {
        visit(&mut label.span);
    }
    for edit in &mut diagnostic.edits {
        visit(&mut edit.span);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_token_edit_reparses_only_its_declaration() {
        let source = "const untouched = 1;\nfunction main(): void {\n  const value = 2;\n}\n";
        let mut document = IncrementalDocument::new("test.tn", source.into());
        let offset = source.find("value").expect("edited token exists");
        let stats = document
            .apply_edit(TextEdit {
                range: offset..offset + "value".len(),
                replacement: "result".into(),
            })
            .expect("edit is valid");
        assert!(stats.reparsed_range.start > 0);
        assert!(stats.reparsed_range.len() < document.source().len());
        assert!(stats.reused_bytes >= "const untouched = 1;\n".len());
        assert_eq!(document.parse().syntax().to_string(), document.source());
        assert!(document.parse().is_success());
    }

    #[test]
    fn invalid_edit_range_is_rejected_without_mutation() {
        let mut document = IncrementalDocument::new("test.tn", "const π = 1;\n".into());
        let original = document.source().to_owned();
        assert!(
            document
                .apply_edit(TextEdit {
                    range: 7..8,
                    replacement: "x".into(),
                })
                .is_err()
        );
        assert_eq!(document.source(), original);
    }
}
