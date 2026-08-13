use crate::{CheckOutput, Project};
use std::collections::BTreeSet;
use tn_diagnostics::{ConditionId, Diagnostic, Label, Severity, SourceSpan};
use tn_hir::ImportClause;
use tn_syntax::lex;

/// Runs semantic checks and deterministic source hygiene checks over a project.
pub fn lint_project(project: &Project) -> CheckOutput {
    let mut output = crate::check_project(project);
    let standard_library = super::standard_library_path();
    let graph = match tn_hir::load_module_graph(&project.root, &project.entry, &standard_library) {
        Ok(graph) => graph,
        Err(error) => {
            output.diagnostics.extend_from_slice(error.diagnostics());
            return output;
        }
    };
    for module in &graph.modules {
        lint_module(module, &mut output.diagnostics);
    }
    output.diagnostics.sort_by(|left, right| {
        left.primary
            .span
            .file
            .cmp(&right.primary.span.file)
            .then(
                left.primary
                    .span
                    .byte_start
                    .cmp(&right.primary.span.byte_start),
            )
            .then(left.condition.as_str().cmp(right.condition.as_str()))
    });
    output
}

fn lint_module(module: &tn_hir::Module, diagnostics: &mut Vec<Diagnostic>) {
    for (line_index, line) in module.source.split_inclusive('\n').enumerate() {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = content.trim_end_matches([' ', '\t']);
        if trimmed.len() != content.len() {
            let start = trimmed.len();
            diagnostics.push(warning(
                "LINT_TRAILING_WHITESPACE",
                "trailing whitespace",
                SourceSpan::new(
                    module.path.to_string_lossy(),
                    line_start(module, line_index) + start
                        ..line_start(module, line_index) + content.len(),
                    &module.source,
                ),
                "remove trailing spaces or tabs",
            ));
        }
    }

    let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
    let tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    for import in &module.imports {
        let ImportClause::Named(names) = &import.clause else {
            continue;
        };
        let used = tokens
            .iter()
            .filter(|token| token.range.start >= import.span.byte_end as usize)
            .map(|token| &module.source[token.range.clone()])
            .collect::<BTreeSet<_>>();
        for name in names {
            if !used.contains(name.local.as_str()) {
                diagnostics.push(warning(
                    "LINT_UNUSED_IMPORT",
                    format!("import `{}` is never used", name.local),
                    name.span.clone(),
                    "remove the import or use the binding",
                ));
            }
        }
    }
}

fn line_start(module: &tn_hir::Module, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    module
        .source
        .match_indices('\n')
        .nth(line.saturating_sub(1))
        .map_or(0, |(index, _)| index + 1)
}

fn warning(
    condition: &str,
    message: impl Into<String>,
    span: SourceSpan,
    label: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        condition: ConditionId::new(condition).expect("static linter condition"),
        severity: Severity::Warning,
        message: message.into(),
        primary: Label {
            span,
            message: label.into(),
        },
        secondary: Vec::new(),
        notes: Vec::new(),
        edits: Vec::new(),
        documentation_key: format!("lint/{}", condition.to_lowercase()),
    }
}
