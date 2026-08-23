//! Deterministic, declaration-only macro expansion.
//!
//! A macro is deliberately a small token-template facility.  It accepts typed arguments, can
//! add members and validated target attributes, and has no access to the filesystem, process
//! environment, clock, randomness, or native ABI.  Expansion happens before module scanning so
//! generated declarations enter the ordinary parser and semantic pipeline.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_syntax::{Token, TokenKind, lex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParameterKind {
    Identifier,
    Type,
    Literal,
}

#[derive(Clone, Debug)]
struct Parameter {
    name: String,
    kind: ParameterKind,
}

#[derive(Clone, Debug)]
struct MacroDefinition {
    name: String,
    parameters: Vec<Parameter>,
    body: Range<usize>,
    span: SourceSpan,
}

#[derive(Clone, Debug)]
struct ExpansionEdit {
    range: Range<usize>,
    replacement: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Expansion {
    pub(crate) source: String,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// Expands all local `macro` definitions and `@Expand(...)` applications in one source file.
///
/// The expansion language is intentionally declaration-only.  Macro definitions are local to
/// their module; cross-module imports would make expansion order and hygiene depend on module
/// graph traversal.  Every diagnostic produced by this pass points at the original definition or
/// application span, while generated text is subsequently checked by the normal compiler.
#[allow(clippy::too_many_lines)]
pub(crate) fn expand_source(file: &str, source: &str, tokens: &[Token]) -> Expansion {
    let significant = tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let mut definitions = BTreeMap::new();
    let mut edits = Vec::new();
    let mut diagnostics = Vec::new();
    let mut index = 0;
    let mut depth = 0_u32;
    while index < significant.len() {
        let token = significant[index];
        if depth == 0 && token.kind == TokenKind::Macro {
            match parse_definition(file, source, &significant, index) {
                Ok((definition, end)) => {
                    if definitions
                        .insert(definition.name.clone(), definition.clone())
                        .is_some()
                    {
                        diagnostics.push(macro_diagnostic(
                            "MACRO_DUPLICATE_DEFINITION",
                            format!("macro `{}` is declared more than once", definition.name),
                            &definition.span,
                            "give each declaration macro a unique name",
                        ));
                    }
                    let end_offset = significant[end.saturating_sub(1)].range.end;
                    edits.push(ExpansionEdit {
                        range: token.range.start..end_offset,
                        replacement: blank_preserving_newlines(
                            &source[token.range.start..end_offset],
                        ),
                    });
                    index = end;
                    continue;
                }
                Err(diagnostic) => {
                    diagnostics.push(diagnostic);
                    break;
                }
            }
        }
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    if !diagnostics.is_empty() {
        return Expansion {
            source: source.to_owned(),
            diagnostics,
        };
    }

    synthesize_clone_methods(file, source, &significant, &mut edits, &mut diagnostics);
    if !diagnostics.is_empty() {
        return Expansion {
            source: source.to_owned(),
            diagnostics,
        };
    }

    index = 0;
    depth = 0;
    while index < significant.len() {
        let token = significant[index];
        if depth == 0
            && token.kind == TokenKind::At
            && significant
                .get(index + 1)
                .is_some_and(|name| &source[name.range.clone()] == "Expand")
        {
            let Some((attribute_end, args, attribute_span)) =
                parse_application(file, source, &significant, index)
            else {
                diagnostics.push(macro_diagnostic(
                    "MACRO_INVALID_APPLICATION",
                    "`@Expand` requires a macro name and arguments",
                    &span_for(file, source, token.range.clone()),
                    "use `@Expand(name, ...)` on a declaration",
                ));
                index += 1;
                continue;
            };
            let Some(macro_name) = args.first() else {
                diagnostics.push(macro_diagnostic(
                    "MACRO_INVALID_APPLICATION",
                    "`@Expand` requires a macro name",
                    &attribute_span,
                    "use `@Expand(name, ...)`",
                ));
                index = attribute_end;
                continue;
            };
            let Some(definition) = definitions.get(macro_name) else {
                diagnostics.push(macro_diagnostic(
                    "MACRO_UNKNOWN",
                    format!("macro `{macro_name}` is not declared in this module"),
                    &attribute_span,
                    "declare the macro in this module before applying it",
                ));
                index = attribute_end;
                continue;
            };
            let target_start = next_declaration(&significant, attribute_end);
            let Some(target_start) = target_start else {
                diagnostics.push(macro_diagnostic(
                    "MACRO_APPLICATION_TARGET",
                    "`@Expand` must be followed by a declaration",
                    &attribute_span,
                    "apply the macro to a struct, class, interface, or enum",
                ));
                index = attribute_end;
                continue;
            };
            let target_kind = significant[target_start].kind;
            if !matches!(
                target_kind,
                TokenKind::Struct | TokenKind::Class | TokenKind::Interface | TokenKind::Enum
            ) {
                diagnostics.push(macro_diagnostic(
                    "MACRO_APPLICATION_TARGET",
                    "declaration macros may target only nominal declarations",
                    &attribute_span,
                    "apply the macro to a struct, class, interface, or enum",
                ));
                index = attribute_end;
                continue;
            }
            let Some(open_index) = significant[target_start..]
                .iter()
                .position(|candidate| candidate.kind == TokenKind::LeftBrace)
                .map(|offset| target_start + offset)
            else {
                diagnostics.push(macro_diagnostic(
                    "MACRO_APPLICATION_TARGET",
                    "macro target has no complete declaration body",
                    &attribute_span,
                    "complete the target declaration before expanding it",
                ));
                index = attribute_end;
                continue;
            };
            let Some((open_index, close_index)) = balanced_body(&significant, open_index) else {
                diagnostics.push(macro_diagnostic(
                    "MACRO_APPLICATION_TARGET",
                    "macro target has no complete declaration body",
                    &attribute_span,
                    "complete the target declaration before expanding it",
                ));
                index = attribute_end;
                continue;
            };
            let call_args = &args[1..];
            let Some(replacements) = validate_arguments(
                file,
                source,
                definition,
                call_args,
                &attribute_span,
                &mut diagnostics,
            ) else {
                index = attribute_end;
                continue;
            };
            let expanded_body =
                match substitute_body(file, source, definition, &replacements, &attribute_span) {
                    Ok(body) => body,
                    Err(diagnostic) => {
                        diagnostics.push(diagnostic);
                        index = attribute_end;
                        continue;
                    }
                };
            if let Some(diagnostic) =
                sandbox_diagnostic(file, source, &expanded_body, &attribute_span)
            {
                diagnostics.push(diagnostic);
                index = attribute_end;
                continue;
            }
            let (attributes, members) = split_generated_attributes(&expanded_body);
            if let Some(diagnostic) = collision_diagnostic(
                file,
                source,
                &(significant[open_index].range.start..significant[close_index].range.start),
                &members,
                &attribute_span,
            ) {
                diagnostics.push(diagnostic);
                index = attribute_end;
                continue;
            }
            edits.push(ExpansionEdit {
                range: token.range.start..significant[attribute_end.saturating_sub(1)].range.end,
                replacement: blank_preserving_newlines(
                    &source
                        [token.range.start..significant[attribute_end.saturating_sub(1)].range.end],
                ),
            });
            if !attributes.trim().is_empty() {
                edits.push(ExpansionEdit {
                    range: significant[target_start].range.start
                        ..significant[target_start].range.start,
                    replacement: format!("{attributes}\n"),
                });
            }
            if !members.trim().is_empty() {
                edits.push(ExpansionEdit {
                    range: significant[close_index].range.start
                        ..significant[close_index].range.start,
                    replacement: format!("\n{members}\n"),
                });
            }
            index = attribute_end;
            continue;
        }
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    if !diagnostics.is_empty() {
        return Expansion {
            source: source.to_owned(),
            diagnostics,
        };
    }
    edits.sort_by(|left, right| {
        right
            .range
            .start
            .cmp(&left.range.start)
            .then(right.range.end.cmp(&left.range.end))
    });
    let mut expanded = source.to_owned();
    for edit in edits {
        if edit.range.start <= edit.range.end && edit.range.end <= expanded.len() {
            expanded.replace_range(edit.range, &edit.replacement);
        }
    }
    Expansion {
        source: expanded,
        diagnostics,
    }
}

#[allow(clippy::too_many_lines)]
fn synthesize_clone_methods(
    file: &str,
    source: &str,
    tokens: &[&Token],
    edits: &mut Vec<ExpansionEdit>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut index = 0;
    let mut depth = 0_u32;
    while index < tokens.len() {
        let token = tokens[index];
        if depth == 0
            && token.kind == TokenKind::At
            && tokens
                .get(index + 1)
                .is_some_and(|name| &source[name.range.clone()] == "Clone")
        {
            let attribute_end = attribute_end(tokens, index);
            let Some(target_start) = next_declaration(tokens, attribute_end) else {
                index = attribute_end;
                continue;
            };
            let target_kind = tokens[target_start].kind;
            if target_kind == TokenKind::Enum || target_kind == TokenKind::Class {
                let Some(open_index) = tokens[target_start..]
                    .iter()
                    .position(|candidate| candidate.kind == TokenKind::LeftBrace)
                    .map(|offset| target_start + offset)
                else {
                    index = attribute_end;
                    continue;
                };
                let Some((open_index, close_index)) = balanced_body(tokens, open_index) else {
                    index = attribute_end;
                    continue;
                };
                let body = &source[tokens[open_index].range.end..tokens[close_index].range.start];
                if member_names(file, body).contains("clone") {
                    diagnostics.push(macro_diagnostic(
                        "MACRO_NAME_COLLISION",
                        "@Clone would synthesize a `clone` method that already exists",
                        &span_for(file, source, token.range.start..tokens[index + 1].range.end),
                        "remove the explicit clone method or remove @Clone",
                    ));
                    index = attribute_end;
                    continue;
                }
                let Some(name_token) = tokens.get(target_start + 1) else {
                    index = attribute_end;
                    continue;
                };
                let type_end = tokens[target_start..open_index]
                    .iter()
                    .find(|candidate| {
                        matches!(
                            candidate.kind,
                            TokenKind::Extends | TokenKind::Implements | TokenKind::Where
                        )
                    })
                    .map_or(tokens[open_index].range.start, |candidate| {
                        candidate.range.start
                    });
                let result_type = source[name_token.range.start..type_end].trim();
                let replacement = if target_kind == TokenKind::Enum {
                    clone_enum_method(source, tokens, result_type, open_index, close_index)
                } else if tokens
                    .get(target_start.saturating_sub(1))
                    .is_some_and(|candidate| candidate.kind == TokenKind::Final)
                {
                    Some(clone_class_method(
                        source,
                        tokens,
                        result_type,
                        open_index,
                        close_index,
                    ))
                } else {
                    None
                };
                if let Some(replacement) = replacement {
                    edits.push(ExpansionEdit {
                        range: tokens[close_index].range.start..tokens[close_index].range.start,
                        replacement,
                    });
                }
                index = attribute_end;
                continue;
            }
            if target_kind != TokenKind::Struct {
                index = attribute_end;
                continue;
            }
            let Some(open_index) = tokens[target_start..]
                .iter()
                .position(|candidate| candidate.kind == TokenKind::LeftBrace)
                .map(|offset| target_start + offset)
            else {
                index = attribute_end;
                continue;
            };
            let Some((open_index, close_index)) = balanced_body(tokens, open_index) else {
                index = attribute_end;
                continue;
            };
            let body = &source[tokens[open_index].range.end..tokens[close_index].range.start];
            if member_names(file, body).contains("clone") {
                diagnostics.push(macro_diagnostic(
                    "MACRO_NAME_COLLISION",
                    "@Clone would synthesize a `clone` method that already exists",
                    &span_for(file, source, token.range.start..tokens[index + 1].range.end),
                    "remove the explicit clone method or remove @Clone",
                ));
                index = attribute_end;
                continue;
            }
            let Some(name_token) = tokens.get(target_start + 1) else {
                index = attribute_end;
                continue;
            };
            let type_end = tokens[target_start..open_index]
                .iter()
                .find(|candidate| {
                    matches!(
                        candidate.kind,
                        TokenKind::Extends | TokenKind::Implements | TokenKind::Where
                    )
                })
                .map_or(tokens[open_index].range.start, |candidate| {
                    candidate.range.start
                });
            let result_type = source[name_token.range.start..type_end].trim();
            let fields = clone_struct_fields(source, tokens, open_index, close_index);
            if fields.is_empty() {
                edits.push(ExpansionEdit {
                    range: tokens[close_index].range.start..tokens[close_index].range.start,
                    replacement: format!(
                        "\n  public clone(): {result_type} {{\n    return {{}};\n  }}\n"
                    ),
                });
            } else {
                let initializers = fields
                    .iter()
                    .map(|(name, ty)| {
                        let value = if clone_field_is_copy(ty) {
                            format!("this.{name}")
                        } else {
                            format!("this.{name}.clone()")
                        };
                        format!("{name}: {value}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                edits.push(ExpansionEdit {
                    range: tokens[close_index].range.start..tokens[close_index].range.start,
                    replacement: format!(
                        "\n  public clone(): {result_type} {{\n    return {{ {initializers}, }};\n  }}\n"
                    ),
                });
            }
            index = attribute_end;
            continue;
        }
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
}

fn attribute_end(tokens: &[&Token], start: usize) -> usize {
    let mut cursor = start + 2;
    if tokens
        .get(cursor)
        .is_some_and(|token| token.kind == TokenKind::LeftParen)
    {
        let mut depth = 1_u32;
        cursor += 1;
        while let Some(token) = tokens.get(cursor) {
            match token.kind {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cursor += 1;
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
    }
    cursor
}

fn clone_struct_fields(
    source: &str,
    tokens: &[&Token],
    open_index: usize,
    close_index: usize,
) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut index = open_index + 1;
    let mut depth = 0_u32;
    let mut paren_depth = 0_u32;
    while index < close_index {
        let token = tokens[index];
        if depth == 0
            && paren_depth == 0
            && token.kind == TokenKind::Colon
            && index > open_index + 1
            && tokens[index - 1].kind == TokenKind::Identifier
        {
            let name = source[tokens[index - 1].range.clone()].to_owned();
            let type_start = tokens[index].range.end;
            let mut type_end = type_start;
            let mut cursor = index + 1;
            let mut type_depth = 0_u32;
            while cursor < close_index {
                let candidate = tokens[cursor];
                match candidate.kind {
                    TokenKind::LeftBracket | TokenKind::LeftParen | TokenKind::Less => {
                        type_depth += 1;
                    }
                    TokenKind::RightBracket | TokenKind::RightParen | TokenKind::Greater => {
                        type_depth = type_depth.saturating_sub(1);
                    }
                    TokenKind::Semicolon if type_depth == 0 => {
                        type_end = candidate.range.start;
                        break;
                    }
                    _ => {}
                }
                cursor += 1;
            }
            let ty = source[type_start..type_end].trim().to_owned();
            if !ty.is_empty() {
                fields.push((name, ty));
            }
        }
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            TokenKind::LeftParen => paren_depth += 1,
            TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    fields
}

fn clone_field_is_copy(ty: &str) -> bool {
    let normalized = ty.split_whitespace().collect::<String>();
    if let Some(inner) = normalized.strip_suffix("|undefined") {
        return clone_field_is_copy(inner);
    }
    normalized.starts_with('&')
        || normalized.starts_with("*const")
        || normalized.starts_with("*mut")
        || matches!(
            normalized.as_str(),
            "bool"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "number"
                | "f32"
                | "f64"
                | "char"
        )
}

fn clone_class_method(
    source: &str,
    tokens: &[&Token],
    result_type: &str,
    open_index: usize,
    close_index: usize,
) -> String {
    let fields = clone_struct_fields(source, tokens, open_index, close_index);
    let initializers = fields
        .iter()
        .map(|(name, ty)| {
            if clone_field_is_copy(ty) {
                format!("this.{name}")
            } else {
                format!("this.{name}.clone()")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let arguments = if initializers.is_empty() {
        String::new()
    } else {
        format!("{initializers},")
    };
    format!(
        "\n  public clone(): {result_type} {{\n    return new {result_type}({arguments});\n  }}\n"
    )
}

type CloneEnumVariant = (Option<String>, String);
type CloneEnumVariants = Vec<(String, Vec<CloneEnumVariant>)>;

fn clone_enum_method(
    source: &str,
    tokens: &[&Token],
    result_type: &str,
    open_index: usize,
    close_index: usize,
) -> Option<String> {
    let variants = clone_enum_variants(source, tokens, open_index, close_index);
    if variants.is_empty() {
        return None;
    }
    let arms = variants
        .into_iter()
        .map(|(name, fields)| {
            if fields.is_empty() {
                return format!("case {result_type}.{name}: {result_type}.{name},");
            }
            let bindings = fields
                .iter()
                .enumerate()
                .map(|(index, (field_name, _))| {
                    field_name
                        .clone()
                        .unwrap_or_else(|| format!("field{index}"))
                })
                .collect::<Vec<_>>();
            let pattern = if fields.iter().all(|(field_name, _)| field_name.is_some()) {
                let fields = bindings
                    .iter()
                    .map(|name| format!("{name}: {name}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {fields} }}")
            } else {
                format!("({})", bindings.join(", "))
            };
            let values = fields
                .iter()
                .zip(&bindings)
                .map(|((_, ty), binding)| {
                    if clone_field_is_copy(ty) {
                        binding.clone()
                    } else {
                        format!("{binding}.clone()")
                    }
                })
                .collect::<Vec<_>>();
            let constructor = if fields.iter().all(|(field_name, _)| field_name.is_some()) {
                let fields = bindings
                    .iter()
                    .zip(values)
                    .map(|(name, value)| format!("{name}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {fields}, }}")
            } else {
                format!("({},)", values.join(", "))
            };
            format!("case {result_type}.{name} {pattern}: {result_type}.{name} {constructor},")
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "\n  public clone(): {result_type} {{\n    return switch(this) {{ {arms} }};\n  }}\n"
    ))
}

fn clone_enum_variants(
    source: &str,
    tokens: &[&Token],
    open_index: usize,
    close_index: usize,
) -> CloneEnumVariants {
    let mut variants = Vec::new();
    let mut cursor = open_index + 1;
    while cursor < close_index {
        if tokens[cursor].kind == TokenKind::Comma {
            cursor += 1;
            continue;
        }
        let Some(name_token) = tokens.get(cursor) else {
            break;
        };
        if name_token.kind != TokenKind::Identifier {
            cursor += 1;
            continue;
        }
        let name = source[name_token.range.clone()].to_owned();
        cursor += 1;
        let mut fields = Vec::new();
        if tokens
            .get(cursor)
            .is_some_and(|token| token.kind == TokenKind::LeftParen)
        {
            let Some(field_close) =
                matching_delimiter(tokens, cursor, TokenKind::LeftParen, TokenKind::RightParen)
            else {
                break;
            };
            for (start, end) in comma_ranges(tokens, cursor + 1, field_close) {
                if let (Some(first), Some(last)) =
                    (tokens.get(start), tokens.get(end.saturating_sub(1)))
                {
                    let ty = source[first.range.start..last.range.end].trim().to_owned();
                    if !ty.is_empty() {
                        fields.push((None, ty));
                    }
                }
            }
            cursor = field_close + 1;
        } else if tokens
            .get(cursor)
            .is_some_and(|token| token.kind == TokenKind::LeftBrace)
        {
            let Some(field_close) =
                matching_delimiter(tokens, cursor, TokenKind::LeftBrace, TokenKind::RightBrace)
            else {
                break;
            };
            for (start, end) in comma_or_semicolon_ranges(tokens, cursor + 1, field_close) {
                let Some(colon) =
                    (start..end).find(|index| tokens[*index].kind == TokenKind::Colon)
                else {
                    continue;
                };
                let Some(name_token) = tokens.get(start) else {
                    continue;
                };
                let Some(type_start) = tokens.get(colon + 1) else {
                    continue;
                };
                let Some(type_end) = tokens.get(end.saturating_sub(1)) else {
                    continue;
                };
                fields.push((
                    Some(source[name_token.range.clone()].to_owned()),
                    source[type_start.range.start..type_end.range.end]
                        .trim()
                        .to_owned(),
                ));
            }
            cursor = field_close + 1;
        }
        while cursor < close_index && tokens[cursor].kind != TokenKind::Comma {
            cursor += 1;
        }
        variants.push((name, fields));
    }
    variants
}

fn comma_ranges(tokens: &[&Token], start: usize, end: usize) -> Vec<(usize, usize)> {
    separated_ranges(tokens, start, end, TokenKind::Comma)
}

fn comma_or_semicolon_ranges(tokens: &[&Token], start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut cursor = start;
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().take(end).skip(start) {
        match token.kind {
            TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => depth += 1,
            TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
            }
            TokenKind::Comma | TokenKind::Semicolon if depth == 0 => {
                if cursor < index {
                    ranges.push((cursor, index));
                }
                cursor = index + 1;
            }
            _ => {}
        }
    }
    if cursor < end {
        ranges.push((cursor, end));
    }
    ranges
}

fn separated_ranges(
    tokens: &[&Token],
    start: usize,
    end: usize,
    separator: TokenKind,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut cursor = start;
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().take(end).skip(start) {
        match token.kind {
            TokenKind::LeftParen | TokenKind::LeftBracket | TokenKind::LeftBrace => depth += 1,
            TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
            }
            kind if kind == separator && depth == 0 => {
                if cursor < index {
                    ranges.push((cursor, index));
                }
                cursor = index + 1;
            }
            _ => {}
        }
    }
    if cursor < end {
        ranges.push((cursor, end));
    }
    ranges
}

fn matching_delimiter(
    tokens: &[&Token],
    start: usize,
    open: TokenKind,
    close: TokenKind,
) -> Option<usize> {
    if tokens.get(start).map(|token| token.kind) != Some(open) {
        return None;
    }
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token.kind {
            kind if kind == open => depth += 1,
            kind if kind == close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

#[allow(clippy::result_large_err, clippy::too_many_lines)]
fn parse_definition(
    file: &str,
    source: &str,
    tokens: &[&Token],
    start: usize,
) -> Result<(MacroDefinition, usize), Diagnostic> {
    let name_token = tokens.get(start + 1).ok_or_else(|| {
        macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro declaration requires a name",
            &span_for(file, source, tokens[start].range.clone()),
            "use `macro name(...) { ... }`",
        )
    })?;
    if name_token.kind != TokenKind::Identifier {
        return Err(macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro declaration requires an identifier name",
            &span_for(file, source, name_token.range.clone()),
            "use an identifier for the macro name",
        ));
    }
    let Some(left_paren) = tokens.get(start + 2) else {
        return Err(macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro declaration requires a typed parameter list",
            &span_for(file, source, name_token.range.clone()),
            "use `macro name(parameter: type) { ... }`",
        ));
    };
    if left_paren.kind != TokenKind::LeftParen {
        return Err(macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro declaration requires a typed parameter list",
            &span_for(file, source, left_paren.range.clone()),
            "use `macro name(parameter: type) { ... }`",
        ));
    }
    let mut index = start + 3;
    let mut parameters = Vec::new();
    while index < tokens.len() && tokens[index].kind != TokenKind::RightParen {
        let parameter_token = tokens[index];
        let kind_token = tokens.get(index + 2).copied();
        if parameter_token.kind != TokenKind::Identifier
            || tokens.get(index + 1).map(|token| token.kind) != Some(TokenKind::Colon)
            || kind_token.is_none()
        {
            return Err(macro_diagnostic(
                "MACRO_INVALID_PARAMETER",
                "macro parameters must be written as `name: identifier`, `name: type`, or `name: literal`",
                &span_for(file, source, parameter_token.range.clone()),
                "declare one of the three supported typed parameter categories",
            ));
        }
        let kind_token = kind_token.expect("checked above");
        let kind_text = &source[kind_token.range.clone()];
        let Some(kind) = (match kind_text {
            "identifier" => Some(ParameterKind::Identifier),
            "type" => Some(ParameterKind::Type),
            "literal" => Some(ParameterKind::Literal),
            _ => None,
        }) else {
            return Err(macro_diagnostic(
                "MACRO_INVALID_PARAMETER",
                format!("unknown macro parameter category `{kind_text}`"),
                &span_for(file, source, kind_token.range.clone()),
                "use `identifier`, `type`, or `literal`",
            ));
        };
        let name = source[parameter_token.range.clone()].to_owned();
        if parameters
            .iter()
            .any(|parameter: &Parameter| parameter.name == name)
        {
            return Err(macro_diagnostic(
                "MACRO_DUPLICATE_PARAMETER",
                format!("macro parameter `{name}` is declared more than once"),
                &span_for(file, source, parameter_token.range.clone()),
                "give every macro parameter a unique name",
            ));
        }
        parameters.push(Parameter { name, kind });
        index += 3;
        if tokens.get(index).map(|token| token.kind) == Some(TokenKind::Comma) {
            index += 1;
        } else if tokens.get(index).map(|token| token.kind) != Some(TokenKind::RightParen) {
            return Err(macro_diagnostic(
                "MACRO_INVALID_PARAMETER",
                "macro parameters must be comma separated",
                &span_for(file, source, tokens[index].range.clone()),
                "insert a comma between typed parameters",
            ));
        }
    }
    if tokens.get(index).map(|token| token.kind) != Some(TokenKind::RightParen) {
        return Err(macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro parameter list is not closed",
            &span_for(file, source, name_token.range.clone()),
            "close the parameter list before the template body",
        ));
    }
    let body_open = tokens.get(index + 1).copied();
    if body_open.map(|token| token.kind) != Some(TokenKind::LeftBrace) {
        return Err(macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro declaration requires a template body",
            &span_for(file, source, tokens[index].range.clone()),
            "use `{ ... }` for the declaration template",
        ));
    }
    let Some((open_index, close_index)) = balanced_body(tokens, index + 1) else {
        return Err(macro_diagnostic(
            "MACRO_INVALID_DEFINITION",
            "macro template body is not balanced",
            &span_for(
                file,
                source,
                body_open.expect("checked above").range.clone(),
            ),
            "close every template brace",
        ));
    };
    let body_open = tokens[open_index];
    let body_close = tokens[close_index];
    let span = span_for(
        file,
        source,
        tokens[start].range.start..body_close.range.end,
    );
    Ok((
        MacroDefinition {
            name: source[name_token.range.clone()].to_owned(),
            parameters,
            body: body_open.range.end..body_close.range.start,
            span,
        },
        close_index + 1,
    ))
}

fn parse_application(
    file: &str,
    source: &str,
    tokens: &[&Token],
    start: usize,
) -> Option<(usize, Vec<String>, SourceSpan)> {
    let open = tokens.get(start + 2)?;
    if open.kind != TokenKind::LeftParen {
        return None;
    }
    let mut args = Vec::new();
    let mut cursor = start + 3;
    let mut argument_start = cursor;
    let mut depth = 1_u32;
    while cursor < tokens.len() {
        match tokens[cursor].kind {
            TokenKind::LeftParen => depth += 1,
            TokenKind::RightParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if argument_start < cursor {
                        args.push(trimmed_source(
                            source,
                            tokens[argument_start].range.start..tokens[cursor - 1].range.end,
                        ));
                    }
                    let end = cursor + 1;
                    let span = span_for(
                        file,
                        source,
                        tokens[start].range.start..tokens[cursor].range.end,
                    );
                    return Some((end, args, span));
                }
            }
            TokenKind::Comma if depth == 1 => {
                if argument_start < cursor {
                    args.push(trimmed_source(
                        source,
                        tokens[argument_start].range.start..tokens[cursor - 1].range.end,
                    ));
                } else {
                    args.push(String::new());
                }
                argument_start = cursor + 1;
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn next_declaration(tokens: &[&Token], start: usize) -> Option<usize> {
    tokens[start..]
        .iter()
        .position(|token| {
            matches!(
                token.kind,
                TokenKind::Struct | TokenKind::Class | TokenKind::Interface | TokenKind::Enum
            )
        })
        .map(|offset| start + offset)
}

fn balanced_body(tokens: &[&Token], start: usize) -> Option<(usize, usize)> {
    if tokens.get(start).map(|token| token.kind) != Some(TokenKind::LeftBrace) {
        return None;
    }
    let mut depth = 0_u32;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((start, index));
                }
            }
            _ => {}
        }
    }
    None
}

fn validate_arguments(
    file: &str,
    source: &str,
    definition: &MacroDefinition,
    arguments: &[String],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, String>> {
    if arguments.len() != definition.parameters.len() {
        diagnostics.push(macro_diagnostic(
            "MACRO_ARGUMENT_ARITY",
            format!(
                "macro `{}` expects {} argument(s), got {}",
                definition.name,
                definition.parameters.len(),
                arguments.len()
            ),
            span,
            "pass exactly the typed arguments declared by the macro",
        ));
        return None;
    }
    let mut replacements = BTreeMap::new();
    for (parameter, argument) in definition.parameters.iter().zip(arguments) {
        let valid = match parameter.kind {
            ParameterKind::Identifier => is_identifier(source, argument),
            ParameterKind::Type => is_type_fragment(source, argument),
            ParameterKind::Literal => is_literal(source, argument),
        };
        if !valid {
            diagnostics.push(macro_diagnostic(
                "MACRO_ARGUMENT_TYPE",
                format!(
                    "argument for `{}` does not match its typed category",
                    parameter.name
                ),
                span,
                "use the category declared by the macro parameter",
            ));
            return None;
        }
        replacements.insert(parameter.name.clone(), argument.clone());
    }
    let _ = file;
    Some(replacements)
}

#[allow(clippy::result_large_err)]
fn substitute_body(
    file: &str,
    source: &str,
    definition: &MacroDefinition,
    replacements: &BTreeMap<String, String>,
    span: &SourceSpan,
) -> Result<String, Diagnostic> {
    let body = &source[definition.body.clone()];
    let lexed = lex(file, body.as_bytes());
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let mut output = String::new();
    let mut cursor = 0;
    let mut index = 0;
    while index + 4 < significant.len() {
        if significant[index].kind == TokenKind::LeftBrace
            && significant[index + 1].kind == TokenKind::LeftBrace
            && significant[index + 2].kind == TokenKind::Identifier
            && significant[index + 3].kind == TokenKind::RightBrace
            && significant[index + 4].kind == TokenKind::RightBrace
        {
            let start = significant[index].range.start;
            let end = significant[index + 4].range.end;
            output.push_str(&body[cursor..start]);
            let name = &body[significant[index + 2].range.clone()];
            let Some(replacement) = replacements.get(name) else {
                return Err(macro_diagnostic(
                    "MACRO_UNKNOWN_PARAMETER",
                    format!("template references unknown macro parameter `{name}`"),
                    span,
                    "reference only parameters declared by the macro",
                ));
            };
            output.push_str(replacement);
            cursor = end;
            index += 5;
            continue;
        }
        index += 1;
    }
    output.push_str(&body[cursor..]);
    Ok(output)
}

fn split_generated_attributes(body: &str) -> (String, String) {
    let lexed = lex("<macro expansion>", body.as_bytes());
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let mut cursor = 0;
    let mut attributes = String::new();
    while significant.get(cursor).map(|token| token.kind) == Some(TokenKind::At) {
        let start = significant[cursor].range.start;
        let mut end_index = cursor + 2;
        if significant.get(end_index).map(|token| token.kind) == Some(TokenKind::LeftParen) {
            let mut depth = 0_u32;
            while let Some(token) = significant.get(end_index) {
                match token.kind {
                    TokenKind::LeftParen => depth += 1,
                    TokenKind::RightParen => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            end_index += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                end_index += 1;
            }
        }
        let end = significant
            .get(end_index.saturating_sub(1))
            .map_or(start, |token| token.range.end);
        attributes.push_str(body[start..end].trim());
        attributes.push('\n');
        cursor = end_index;
    }
    let member_start = significant
        .get(cursor)
        .map_or(body.len(), |token| token.range.start);
    (attributes, body[member_start..].to_owned())
}

fn sandbox_diagnostic(
    file: &str,
    source: &str,
    body: &str,
    span: &SourceSpan,
) -> Option<Diagnostic> {
    let forbidden = BTreeSet::from([
        "unsafe",
        "declare",
        "extern",
        "Intrinsic",
        "Export",
        "Layout",
        "Copy",
        "Clone",
        "Drop",
        "Send",
        "Sync",
        "Inline",
        "Test",
        "Expand",
        "include",
        "filesystem",
        "network",
        "environment",
        "random",
        "clock",
        "time",
    ]);
    let lexed = lex(file, body.as_bytes());
    for token in lexed.tokens.iter().filter(|token| !token.kind.is_trivia()) {
        let text = &body[token.range.clone()];
        if forbidden.contains(text) {
            return Some(macro_diagnostic(
                "MACRO_SANDBOX_VIOLATION",
                format!("macro template uses forbidden capability `{text}`"),
                span,
                "declaration macros cannot access native, process, or compiler-owned capabilities",
            ));
        }
    }
    let _ = source;
    None
}

fn collision_diagnostic(
    file: &str,
    source: &str,
    target_body: &Range<usize>,
    generated: &str,
    span: &SourceSpan,
) -> Option<Diagnostic> {
    let existing = member_names(file, &source[target_body.clone()]);
    let generated_names = member_names(file, generated);
    if generated_names.iter().any(|name| existing.contains(name)) {
        let name = generated_names
            .iter()
            .find(|name| existing.contains(*name))
            .expect("collision was established");
        return Some(macro_diagnostic(
            "MACRO_NAME_COLLISION",
            format!("macro-generated member `{name}` collides with an existing member"),
            span,
            "rename the generated member or remove the existing declaration",
        ));
    }
    None
}

fn member_names(file: &str, source: &str) -> BTreeSet<String> {
    let lexed = lex(file, source.as_bytes());
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    significant
        .windows(2)
        .filter(|window| {
            window[0].kind == TokenKind::Identifier
                && matches!(window[1].kind, TokenKind::LeftParen | TokenKind::Less)
        })
        .map(|window| source[window[0].range.clone()].to_owned())
        .collect()
}

fn is_identifier(source: &str, argument: &str) -> bool {
    let lexed = lex("<macro argument>", argument.as_bytes());
    let tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    tokens.len() == 1
        && tokens[0].kind == TokenKind::Identifier
        && !argument.is_empty()
        && source.is_char_boundary(argument.len())
}

fn is_type_fragment(_source: &str, argument: &str) -> bool {
    let lexed = lex("<macro type>", argument.as_bytes());
    let tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    !tokens.is_empty()
        && lexed.diagnostics.is_empty()
        && tokens.iter().all(|token| {
            matches!(
                token.kind,
                TokenKind::Identifier
                    | TokenKind::Less
                    | TokenKind::Greater
                    | TokenKind::Comma
                    | TokenKind::Amp
                    | TokenKind::Star
                    | TokenKind::Question
                    | TokenKind::Static
                    | TokenKind::Lifetime
            )
        })
}

fn is_literal(_source: &str, argument: &str) -> bool {
    let lexed = lex("<macro literal>", argument.as_bytes());
    let tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    tokens.len() == 1
        && lexed.diagnostics.is_empty()
        && matches!(
            tokens[0].kind,
            TokenKind::StringLiteral
                | TokenKind::IntegerLiteral
                | TokenKind::FloatLiteral
                | TokenKind::CharacterLiteral
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Undefined
        )
}

fn trimmed_source(source: &str, range: Range<usize>) -> String {
    source[range].trim().to_owned()
}

fn blank_preserving_newlines(source: &str) -> String {
    source
        .chars()
        .map(|character| if character == '\n' { '\n' } else { ' ' })
        .collect()
}

fn span_for(file: &str, source: &str, range: Range<usize>) -> SourceSpan {
    SourceSpan::new(file, range, source)
}

fn macro_diagnostic(
    condition: &str,
    message: impl Into<String>,
    span: &SourceSpan,
    label: impl Into<String>,
) -> Diagnostic {
    Diagnostic::error(
        ConditionId::new(condition).expect("static macro condition"),
        message,
        Label {
            span: span.clone(),
            message: label.into(),
        },
        format!("macro/{}", condition.to_ascii_lowercase().replace('_', "/")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(source: &str) -> Expansion {
        let lexed = lex("macro.tn", source.as_bytes());
        expand_source("macro.tn", source, &lexed.tokens)
    }

    #[test]
    fn expands_typed_member_templates_deterministically() {
        let source = r"
macro getter(name: identifier, field: identifier, value: type) {
  public {{name}}(): {{value}} { return this.{{field}}; }
}
@Expand(getter, value, value, i32)
struct Counter { private value: i32; }
";
        let first = expanded(source);
        let second = expanded(source);
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        assert_eq!(first.source, second.source);
        assert!(first.source.contains("public value(): i32"));
        assert!(!first.source.contains("macro getter"));
    }

    #[test]
    fn reports_sandbox_and_name_collision_diagnostics_at_the_application() {
        let sandbox = expanded(
            "macro forbidden(value: identifier) { unsafe public {{value}}(): void {} }\n@Expand(forbidden, run) struct Counter {}\n",
        );
        assert!(
            sandbox
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.condition.as_str() == "MACRO_SANDBOX_VIOLATION")
        );
        let collision = expanded(
            "macro getter(name: identifier) { public {{name}}(): void {} }\n@Expand(getter, value) struct Counter { value(): void {} }\n",
        );
        assert!(
            collision
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.condition.as_str() == "MACRO_NAME_COLLISION")
        );
    }

    #[test]
    fn synthesizes_a_real_clone_method_for_structs() {
        let expanded = expanded("@Clone struct User { public id: u64; public text: string; }\n");
        assert!(
            expanded.diagnostics.is_empty(),
            "{:?}",
            expanded.diagnostics
        );
        assert!(expanded.source.contains("public clone(): User"));
        assert!(expanded.source.contains("id: this.id"));
        assert!(expanded.source.contains("text: this.text.clone()"));
    }

    #[test]
    fn synthesizes_clone_methods_for_enums_and_final_classes() {
        let enum_expanded = expanded("@Clone enum Value { Number(i32), Text(string), Empty, }\n");
        assert!(
            enum_expanded.diagnostics.is_empty(),
            "{:?}",
            enum_expanded.diagnostics
        );
        assert!(enum_expanded.source.contains("public clone(): Value"));
        let class_expanded = expanded(
            "@Clone final class User { public value: i32; public constructor(value: i32) { this.value = value; } }\n",
        );
        assert!(
            class_expanded.diagnostics.is_empty(),
            "{:?}",
            class_expanded.diagnostics
        );
        assert!(class_expanded.source.contains("public clone(): User"));
    }
}
