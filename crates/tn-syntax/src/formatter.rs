use crate::{SyntaxKind, SyntaxNode, Token, TokenKind, lex, parse};
use std::ops::Range;
use tn_diagnostics::Diagnostic;

#[derive(Clone, Debug)]
pub struct FormatResult {
    pub output: String,
    pub diagnostics: Vec<Diagnostic>,
}

impl FormatResult {
    pub fn is_success(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

pub fn format(file: &str, bytes: &[u8]) -> FormatResult {
    let lexed = lex(file, bytes);
    if !lexed.diagnostics.is_empty() {
        return FormatResult {
            output: lexed.source.into(),
            diagnostics: lexed.diagnostics,
        };
    }
    let parsed = parse(file, bytes);
    if !parsed.diagnostics.is_empty() {
        return FormatResult {
            output: lexed.source.into(),
            diagnostics: parsed.diagnostics,
        };
    }
    let mut writer = Writer::default();
    let jsx_ranges = parsed
        .syntax()
        .descendants()
        .filter_map(|node| {
            matches!(
                node.kind(),
                SyntaxKind::JSX_ELEMENT
                    | SyntaxKind::JSX_FRAGMENT
                    | SyntaxKind::JSX_OPENING_ELEMENT
                    | SyntaxKind::JSX_CLOSING_ELEMENT
                    | SyntaxKind::JSX_EXPRESSION_CONTAINER
                    | SyntaxKind::JSX_TEXT
            )
            .then_some((node.kind(), syntax_range(&node)))
        })
        .collect::<Vec<_>>();
    let significant = lexed
        .tokens
        .iter()
        .filter(|token| token.kind != TokenKind::Whitespace)
        .collect::<Vec<_>>();
    for (index, token) in significant.iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|previous| significant.get(previous))
            .map(|token| token.kind);
        let next = significant.get(index + 1).map(|token| token.kind);
        let source_text = &lexed.source[token.range.clone()];
        let text = contextual_numeric_text(&significant, index, lexed.source, source_text);
        writer.token_with_context(
            token.kind,
            text,
            previous,
            next,
            jsx_format_context(&jsx_ranges, &significant, token),
        );
    }
    writer.finish_file();
    let output = sort_leading_imports(writer.output);
    FormatResult {
        output,
        diagnostics: Vec::new(),
    }
}

/// Removes an integer/float suffix only when the surrounding syntax already supplies the
/// exact same type.  Explicit suffixes remain lossless in inferred expressions and in all
/// contexts where the formatter cannot prove that they are redundant.
fn contextual_numeric_text<'a>(
    tokens: &[&Token],
    index: usize,
    source: &'a str,
    text: &'a str,
) -> &'a str {
    let token = tokens[index];
    if !matches!(
        token.kind,
        TokenKind::IntegerLiteral | TokenKind::FloatLiteral
    ) {
        return text;
    }
    let Some((suffix_start, suffix)) = numeric_suffix(text, token.kind) else {
        return text;
    };
    if contextual_numeric_suffix(tokens, index, source).is_some_and(|expected| expected == suffix) {
        &text[..suffix_start]
    } else {
        text
    }
}

fn numeric_suffix(text: &str, kind: TokenKind) -> Option<(usize, &str)> {
    const INTEGER_SUFFIXES: [&str; 13] = [
        "i128", "u128", "isize", "usize", "number", "i64", "u64", "i32", "u32", "i16", "u16", "i8",
        "u8",
    ];
    const FLOAT_SUFFIXES: [&str; 2] = ["f64", "f32"];
    let suffixes: &[&str] = match kind {
        TokenKind::IntegerLiteral => &INTEGER_SUFFIXES,
        TokenKind::FloatLiteral => &FLOAT_SUFFIXES,
        _ => return None,
    };
    suffixes.iter().find_map(|suffix| {
        text.strip_suffix(suffix)
            .filter(|digits| !digits.is_empty())
            .map(|digits| (digits.len(), *suffix))
    })
}

fn contextual_numeric_suffix<'a>(
    tokens: &[&Token],
    index: usize,
    source: &'a str,
) -> Option<&'a str> {
    let previous = index
        .checked_sub(1)
        .and_then(|position| tokens.get(position));
    if previous.is_some_and(|token| token.kind == TokenKind::Equal) {
        return declaration_numeric_type(tokens, index - 1, source);
    }
    if previous.is_some_and(|token| token.kind == TokenKind::Return) {
        return enclosing_return_type(tokens, index, source);
    }
    // Fixed-array lengths have an implicit `usize` type.  Keep this special case deliberately
    // narrow so a semicolon in an unrelated expression never erases an explicit suffix.
    if previous.is_some_and(|token| token.kind == TokenKind::Semicolon) {
        let mut cursor = index.saturating_sub(2);
        while cursor > 0 {
            match tokens[cursor].kind {
                TokenKind::LeftBracket => return Some("usize"),
                TokenKind::Semicolon | TokenKind::LeftBrace | TokenKind::RightBrace => break,
                _ => cursor -= 1,
            }
        }
    }
    None
}

fn enclosing_return_type<'a>(tokens: &[&Token], index: usize, source: &'a str) -> Option<&'a str> {
    let mut openings = Vec::new();
    let mut stack = Vec::new();
    for (position, token) in tokens.iter().enumerate().take(index) {
        match token.kind {
            TokenKind::LeftBrace => stack.push(position),
            TokenKind::RightBrace => {
                stack.pop();
            }
            _ => {}
        }
    }
    openings.extend(stack.into_iter().rev());
    for open in openings {
        let mut end = open;
        if let Some(throws) = (0..open).rev().find(|position| {
            tokens[*position].kind == TokenKind::Throws
                && tokens[*position + 1..open]
                    .iter()
                    .find(|token| {
                        matches!(token.kind, TokenKind::LeftBrace | TokenKind::RightBrace)
                    })
                    .is_none()
        }) {
            end = throws;
        }
        let Some(type_end) = end.checked_sub(1) else {
            continue;
        };
        let Some(colon) = type_end.checked_sub(1) else {
            continue;
        };
        if tokens[type_end].kind == TokenKind::Identifier
            && tokens[colon].kind == TokenKind::Colon
            && colon > 0
            && tokens[colon - 1].kind == TokenKind::RightParen
        {
            return Some(&source[tokens[type_end].range.clone()]);
        }
    }
    None
}

fn declaration_numeric_type<'a>(
    tokens: &[&Token],
    equal_index: usize,
    source: &'a str,
) -> Option<&'a str> {
    let mut cursor = equal_index;
    while cursor > 0 {
        cursor -= 1;
        match tokens[cursor].kind {
            TokenKind::Colon => {
                let type_index = cursor + 1;
                if type_index + 1 != equal_index || tokens[type_index].kind != TokenKind::Identifier
                {
                    return None;
                }
                // A declaration annotation has an identifier immediately before the colon and
                // a const/let/static keyword before that identifier.  This avoids treating an
                // object-literal property (`{ value: 1i32 }`) as a typed declaration.
                if cursor < 2
                    || tokens[cursor - 1].kind != TokenKind::Identifier
                    || !tokens[..cursor - 1].iter().rev().any(|token| {
                        matches!(
                            token.kind,
                            TokenKind::Const | TokenKind::Let | TokenKind::Static
                        )
                    })
                {
                    return None;
                }
                return Some(&source[tokens[type_index].range.clone()]);
            }
            TokenKind::Semicolon | TokenKind::LeftBrace | TokenKind::RightBrace => return None,
            _ => {}
        }
    }
    None
}

fn sort_leading_imports(output: String) -> String {
    let mut lines = output.lines().collect::<Vec<_>>();
    let import_count = lines
        .iter()
        .take_while(|line| line.starts_with("import "))
        .count();
    if import_count < 2 {
        return output;
    }
    lines[..import_count].sort_by(|left, right| import_key(left).cmp(&import_key(right)));
    let mut sorted = String::new();
    for line in &lines[..import_count] {
        sorted.push_str(line);
        sorted.push('\n');
    }
    if lines.len() > import_count {
        sorted.push('\n');
        for (index, line) in lines[import_count..].iter().enumerate() {
            sorted.push_str(line);
            if index + import_count + 1 < lines.len() || output.ends_with('\n') {
                sorted.push('\n');
            }
        }
    }
    sorted
}

fn import_key(line: &str) -> (u8, &str, &str) {
    let specifier = line
        .rsplit_once('"')
        .and_then(|(before, _)| before.rsplit_once('"').map(|(_, value)| value))
        .unwrap_or(line);
    let group = u8::from(!specifier.starts_with("std/"));
    (group, specifier, line)
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct Writer {
    output: String,
    indent: usize,
    line_start: bool,
    pending_space: bool,
    inline_braces: Vec<bool>,
    placeholder_mode: u8,
    jsx_indent_stack: Vec<bool>,
    jsx_object_pending: bool,
    jsx_object_depth: usize,
    jsx_spread_open: bool,
}

#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct FormatContext {
    jsx: bool,
    opening: bool,
    closing: bool,
    expression: bool,
    expression_opening: bool,
    expression_closing: bool,
    text: bool,
    fragment: bool,
}

impl Writer {
    fn token_with_context(
        &mut self,
        kind: TokenKind,
        text: &str,
        previous: Option<TokenKind>,
        next: Option<TokenKind>,
        context: FormatContext,
    ) {
        if context.jsx {
            self.jsx_token(kind, text, previous, next, context);
            return;
        }
        self.normal_token(kind, text, previous, next);
    }

    fn normal_token(
        &mut self,
        kind: TokenKind,
        text: &str,
        previous: Option<TokenKind>,
        next: Option<TokenKind>,
    ) {
        match kind {
            TokenKind::Whitespace => {}
            TokenKind::LineComment => {
                if !self.line_start {
                    self.space();
                }
                self.write(text.trim_end());
                self.newline();
            }
            TokenKind::BlockComment => {
                if !self.line_start {
                    self.space();
                }
                self.write_block_comment(text);
                if text.starts_with("/**") || text.contains('\n') {
                    self.newline();
                } else {
                    self.pending_space = true;
                }
            }
            TokenKind::LeftBrace => {
                if next == Some(TokenKind::LeftBrace) && self.placeholder_mode == 0 {
                    self.placeholder_mode = 1;
                    self.write("{");
                    return;
                }
                if previous == Some(TokenKind::LeftBrace) && self.placeholder_mode == 1 {
                    self.placeholder_mode = 2;
                    self.write("{");
                    return;
                }
                let inline = previous == Some(TokenKind::Import);
                self.inline_braces.push(inline);
                if inline {
                    self.space();
                    self.write("{");
                    self.pending_space = true;
                    return;
                }
                if !self.line_start
                    && !matches!(
                        previous,
                        Some(TokenKind::LeftParen | TokenKind::LeftBracket)
                    )
                {
                    self.space();
                }
                self.write("{");
                self.indent += 1;
                self.newline();
            }
            TokenKind::RightBrace => {
                if next == Some(TokenKind::RightBrace) && self.placeholder_mode == 2 {
                    self.placeholder_mode = 3;
                    self.write("}");
                    return;
                }
                if previous == Some(TokenKind::RightBrace) && self.placeholder_mode == 3 {
                    self.placeholder_mode = 0;
                    self.write("}");
                    return;
                }
                if self.inline_braces.pop().unwrap_or(false) {
                    if previous == Some(TokenKind::LeftBrace) {
                        self.trim_space();
                    } else {
                        self.space();
                    }
                    self.write("}");
                    self.pending_space = true;
                    return;
                }
                self.indent = self.indent.saturating_sub(1);
                if !self.line_start {
                    self.newline();
                }
                self.write("}");
                match next {
                    Some(
                        TokenKind::Else
                        | TokenKind::Catch
                        | TokenKind::Semicolon
                        | TokenKind::Comma
                        | TokenKind::RightParen
                        | TokenKind::RightBracket,
                    ) => self.pending_space = true,
                    Some(TokenKind::RightBrace) | None => self.newline(),
                    Some(_) => {
                        self.newline();
                        if self.indent == 0 {
                            self.newline();
                        }
                    }
                }
            }
            TokenKind::Semicolon => {
                self.trim_space();
                self.write(";");
                if matches!(
                    next,
                    Some(TokenKind::IntegerLiteral | TokenKind::RightBracket)
                ) {
                    self.pending_space = true;
                } else {
                    self.newline();
                }
            }
            TokenKind::Comma => {
                self.trim_space();
                self.write(",");
                if matches!(next, Some(TokenKind::RightBrace))
                    && matches!(previous, Some(TokenKind::RightBrace))
                {
                    self.newline();
                } else {
                    self.pending_space = true;
                }
            }
            TokenKind::Colon => {
                self.trim_space();
                self.write(":");
                self.pending_space = true;
            }
            kind @ (TokenKind::Dot
            | TokenKind::QuestionDot
            | TokenKind::RightParen
            | TokenKind::RightBracket) => {
                self.trim_space();
                self.write(text);
                if kind == TokenKind::RightParen
                    && matches!(
                        next,
                        Some(
                            TokenKind::Export
                                | TokenKind::Async
                                | TokenKind::Unsafe
                                | TokenKind::Abstract
                                | TokenKind::Final
                                | TokenKind::Function
                                | TokenKind::Struct
                                | TokenKind::Class
                                | TokenKind::Interface
                                | TokenKind::Enum
                        )
                    )
                {
                    self.newline();
                }
            }
            TokenKind::LeftParen => {
                if previous == Some(TokenKind::Colon)
                    || matches!(
                        previous,
                        Some(
                            TokenKind::If
                                | TokenKind::Await
                                | TokenKind::While
                                | TokenKind::For
                                | TokenKind::Match
                                | TokenKind::Macro
                                | TokenKind::Catch
                        )
                    )
                {
                    self.space();
                } else {
                    self.trim_space();
                }
                self.write("(");
            }
            TokenKind::LeftBracket => {
                if matches!(
                    previous,
                    Some(
                        TokenKind::Public
                            | TokenKind::Protected
                            | TokenKind::Private
                            | TokenKind::Static
                            | TokenKind::Abstract
                            | TokenKind::Override
                            | TokenKind::Move
                            | TokenKind::Unsafe
                            | TokenKind::Async
                            | TokenKind::Const
                            | TokenKind::Let
                    )
                ) || previous == Some(TokenKind::Colon)
                {
                    self.space();
                } else {
                    self.trim_space();
                }
                self.write("[");
            }
            TokenKind::Less | TokenKind::Greater if is_generic_delimiter(kind, previous, next) => {
                if kind == TokenKind::Greater && previous == Some(TokenKind::Greater) {
                    self.space();
                } else {
                    self.trim_space();
                }
                self.write(text);
            }
            TokenKind::Star if previous == Some(TokenKind::Function) => {
                self.trim_space();
                self.write("*");
                self.pending_space = true;
            }
            TokenKind::Amp if next == Some(TokenKind::Mut) || is_prefix_reference(previous) => {
                self.space();
                self.write("&");
            }
            TokenKind::Star if matches!(next, Some(TokenKind::Mut | TokenKind::Const)) => {
                self.space();
                self.write("*");
            }
            kind if is_operator(kind) => {
                self.space();
                self.write(text);
                self.pending_space = true;
            }
            TokenKind::At => {
                if !self.line_start {
                    self.newline();
                }
                self.write("@");
            }
            _ => {
                if previous.is_some_and(needs_word_separator) && needs_word_separator(kind) {
                    self.space();
                }
                self.write(text);
                if matches!(
                    kind,
                    TokenKind::Import
                        | TokenKind::Export
                        | TokenKind::Const
                        | TokenKind::Declare
                        | TokenKind::Let
                        | TokenKind::Static
                        | TokenKind::Mut
                        | TokenKind::Readonly
                        | TokenKind::Function
                        | TokenKind::Struct
                        | TokenKind::Class
                        | TokenKind::Interface
                        | TokenKind::Enum
                        | TokenKind::Impl
                        | TokenKind::Extern
                        | TokenKind::Type
                        | TokenKind::Return
                        | TokenKind::Throw
                        | TokenKind::New
                        | TokenKind::Async
                        | TokenKind::Unsafe
                        | TokenKind::Public
                        | TokenKind::Protected
                        | TokenKind::Private
                        | TokenKind::Abstract
                        | TokenKind::Final
                        | TokenKind::Override
                        | TokenKind::Move
                        | TokenKind::Dyn
                        | TokenKind::From
                        | TokenKind::As
                        | TokenKind::InstanceOf
                        | TokenKind::Extends
                        | TokenKind::Implements
                        | TokenKind::Of
                        | TokenKind::Where
                        | TokenKind::Throws
                        | TokenKind::Case
                ) {
                    self.pending_space = true;
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn jsx_token(
        &mut self,
        kind: TokenKind,
        text: &str,
        previous: Option<TokenKind>,
        next: Option<TokenKind>,
        context: FormatContext,
    ) {
        if context.expression {
            match kind {
                TokenKind::LeftBrace if context.expression_opening => {
                    self.trim_space();
                    self.write("{");
                    if next == Some(TokenKind::LeftBrace) {
                        self.jsx_object_pending = true;
                    }
                }
                TokenKind::LeftBrace if self.jsx_object_pending => {
                    self.trim_space();
                    self.write("{");
                    self.jsx_object_pending = false;
                    self.jsx_object_depth = 1;
                    self.space();
                }
                TokenKind::LeftBrace if self.jsx_object_depth > 0 => {
                    self.normal_token(kind, text, previous, next);
                }
                TokenKind::RightBrace
                    if self.jsx_object_depth > 0 && next == Some(TokenKind::RightBrace) =>
                {
                    self.trim_space();
                    self.space();
                    self.write("}");
                    self.jsx_object_depth = self.jsx_object_depth.saturating_sub(1);
                }
                TokenKind::RightBrace if context.expression_closing => {
                    self.trim_space();
                    self.write("}");
                    if matches!(next, Some(TokenKind::Identifier | TokenKind::From)) {
                        self.pending_space = true;
                    }
                    if matches!(next, Some(TokenKind::Less))
                        && self.jsx_indent_stack.last().copied().unwrap_or(false)
                    {
                        self.newline();
                    }
                }
                TokenKind::Equal => {
                    self.trim_space();
                    self.write("=");
                }
                _ => self.normal_token(kind, text, previous, next),
            }
            return;
        }
        if context.text {
            self.normal_token(kind, text, previous, next);
            return;
        }
        if context.closing && kind == TokenKind::Less {
            let multiline = self.jsx_indent_stack.pop().unwrap_or(false);
            if multiline {
                self.indent = self.indent.saturating_sub(1);
                if !self.line_start {
                    self.newline();
                }
            }
            self.write("<");
            return;
        }
        if context.closing {
            match kind {
                TokenKind::Slash | TokenKind::Greater | TokenKind::Dot | TokenKind::Minus => {
                    self.trim_space();
                    self.write(text);
                    if kind == TokenKind::Greater
                        && matches!(next, Some(TokenKind::Less))
                        && self.jsx_indent_stack.last().copied().unwrap_or(false)
                    {
                        self.newline();
                    }
                }
                _ => self.normal_token(kind, text, previous, next),
            }
            return;
        }
        if context.opening || context.fragment {
            match kind {
                TokenKind::LeftBrace if next == Some(TokenKind::Ellipsis) => {
                    self.space();
                    self.write("{");
                    self.jsx_spread_open = true;
                }
                TokenKind::RightBrace if self.jsx_spread_open => {
                    self.trim_space();
                    self.write("}");
                    self.jsx_spread_open = false;
                    self.pending_space = true;
                }
                TokenKind::Less => {
                    self.write("<");
                }
                TokenKind::Greater => {
                    self.trim_space();
                    self.write(">");
                    if context.opening && previous != Some(TokenKind::Slash) {
                        let multiline = next == Some(TokenKind::Less);
                        self.jsx_indent_stack.push(multiline);
                        if multiline {
                            self.indent += 1;
                            self.newline();
                        }
                    } else if context.fragment && previous == Some(TokenKind::Less) {
                        let multiline = next == Some(TokenKind::Less);
                        self.jsx_indent_stack.push(multiline);
                        if multiline {
                            self.indent += 1;
                            self.newline();
                        }
                    }
                }
                TokenKind::Slash => {
                    if previous != Some(TokenKind::Less) {
                        self.space();
                    }
                    self.write("/");
                }
                TokenKind::Equal => {
                    self.trim_space();
                    self.write("=");
                }
                TokenKind::Dot | TokenKind::Minus => {
                    self.trim_space();
                    self.write(text);
                }
                _ => self.normal_token(kind, text, previous, next),
            }
            return;
        }
        self.normal_token(kind, text, previous, next);
    }

    fn write_block_comment(&mut self, text: &str) {
        for (index, line) in text.lines().enumerate() {
            if index > 0 {
                self.newline();
            }
            self.write(line.trim_end());
        }
    }

    fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.line_start {
            for _ in 0..self.indent {
                self.output.push_str("  ");
            }
            self.line_start = false;
        }
        if self.pending_space && !self.output.ends_with([' ', '\n']) {
            self.output.push(' ');
        }
        self.pending_space = false;
        self.output.push_str(text);
    }

    fn space(&mut self) {
        if !self.line_start {
            self.pending_space = true;
        }
    }

    fn trim_space(&mut self) {
        self.pending_space = false;
        while self.output.ends_with(' ') {
            self.output.pop();
        }
    }

    fn newline(&mut self) {
        self.trim_space();
        if !self.output.ends_with('\n') {
            self.output.push('\n');
        }
        self.line_start = true;
    }

    fn finish_file(&mut self) {
        self.trim_space();
        while self.output.ends_with("\n\n") {
            self.output.pop();
        }
        if !self.output.is_empty() && !self.output.ends_with('\n') {
            self.output.push('\n');
        }
    }
}

fn syntax_range(node: &SyntaxNode) -> Range<usize> {
    usize::from(node.text_range().start())..usize::from(node.text_range().end())
}

fn jsx_format_context(
    ranges: &[(SyntaxKind, Range<usize>)],
    tokens: &[&Token],
    token: &Token,
) -> FormatContext {
    let contains = |kind| {
        ranges.iter().any(|(candidate, range)| {
            *candidate == kind && range.start <= token.range.start && range.end >= token.range.end
        })
    };
    let expression_range = ranges
        .iter()
        .filter_map(|(kind, range)| {
            (*kind == SyntaxKind::JSX_EXPRESSION_CONTAINER
                && range.start <= token.range.start
                && range.end >= token.range.end)
                .then_some(range)
        })
        .min_by_key(|range| range.end.saturating_sub(range.start));
    let expression_opening = expression_range.is_some_and(|range| {
        token.kind == TokenKind::LeftBrace
            && tokens
                .iter()
                .find(|candidate| {
                    candidate.range.start >= range.start && candidate.range.end <= range.end
                })
                .is_some_and(|candidate| {
                    candidate.range.start == token.range.start
                        && candidate.range.end == token.range.end
                })
    });
    let expression_closing = expression_range.is_some_and(|range| {
        token.kind == TokenKind::RightBrace
            && tokens
                .iter()
                .rev()
                .find(|candidate| {
                    candidate.range.start >= range.start && candidate.range.end <= range.end
                })
                .is_some_and(|candidate| {
                    candidate.range.start == token.range.start
                        && candidate.range.end == token.range.end
                })
    });
    FormatContext {
        jsx: contains(SyntaxKind::JSX_ELEMENT) || contains(SyntaxKind::JSX_FRAGMENT),
        opening: contains(SyntaxKind::JSX_OPENING_ELEMENT),
        closing: contains(SyntaxKind::JSX_CLOSING_ELEMENT),
        expression: expression_range.is_some(),
        expression_opening,
        expression_closing,
        text: contains(SyntaxKind::JSX_TEXT),
        fragment: contains(SyntaxKind::JSX_FRAGMENT),
    }
}

fn needs_word_separator(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::IntegerLiteral
            | TokenKind::FloatLiteral
            | TokenKind::StringLiteral
            | TokenKind::CharacterLiteral
            | TokenKind::TemplateLiteral
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Undefined
            | TokenKind::SelfValue
            | TokenKind::Super
            | TokenKind::Abstract
            | TokenKind::As
            | TokenKind::Async
            | TokenKind::Await
            | TokenKind::Break
            | TokenKind::Case
            | TokenKind::Catch
            | TokenKind::Class
            | TokenKind::Const
            | TokenKind::Declare
            | TokenKind::Constructor
            | TokenKind::Continue
            | TokenKind::Crate
            | TokenKind::Default
            | TokenKind::Dyn
            | TokenKind::Else
            | TokenKind::Enum
            | TokenKind::Export
            | TokenKind::Extension
            | TokenKind::Extends
            | TokenKind::Extern
            | TokenKind::Final
            | TokenKind::For
            | TokenKind::From
            | TokenKind::Function
            | TokenKind::If
            | TokenKind::Impl
            | TokenKind::Implements
            | TokenKind::Import
            | TokenKind::InstanceOf
            | TokenKind::Interface
            | TokenKind::Let
            | TokenKind::Lifetime
            | TokenKind::Match
            | TokenKind::Macro
            | TokenKind::Mod
            | TokenKind::Move
            | TokenKind::Mut
            | TokenKind::New
            | TokenKind::Null
            | TokenKind::Of
            | TokenKind::Override
            | TokenKind::Private
            | TokenKind::Protected
            | TokenKind::Public
            | TokenKind::Pub
            | TokenKind::Record
            | TokenKind::Readonly
            | TokenKind::Return
            | TokenKind::Static
            | TokenKind::Struct
            | TokenKind::Switch
            | TokenKind::Throw
            | TokenKind::Throws
            | TokenKind::Try
            | TokenKind::Type
            | TokenKind::This
            | TokenKind::Unknown
            | TokenKind::Unsafe
            | TokenKind::Use
            | TokenKind::Using
            | TokenKind::Where
            | TokenKind::While
            | TokenKind::Yield
    )
}

fn is_operator(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::EqualEqualEqual
            | TokenKind::BangEqualEqual
            | TokenKind::AsQuestion
            | TokenKind::LessEqual
            | TokenKind::GreaterEqual
            | TokenKind::AmpAmp
            | TokenKind::PipePipe
            | TokenKind::ShiftLeft
            | TokenKind::ShiftRight
            | TokenKind::PlusEqual
            | TokenKind::MinusEqual
            | TokenKind::StarEqual
            | TokenKind::SlashEqual
            | TokenKind::PercentEqual
            | TokenKind::AmpEqual
            | TokenKind::PipeEqual
            | TokenKind::CaretEqual
            | TokenKind::ShiftLeftEqual
            | TokenKind::ShiftRightEqual
            | TokenKind::QuestionQuestion
            | TokenKind::FatArrow
            | TokenKind::Equal
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::Amp
            | TokenKind::Pipe
            | TokenKind::Caret
            | TokenKind::Less
            | TokenKind::Greater
    )
}

fn is_prefix_reference(previous: Option<TokenKind>) -> bool {
    previous.is_none_or(|kind| {
        matches!(
            kind,
            TokenKind::Colon
                | TokenKind::LeftParen
                | TokenKind::LeftBracket
                | TokenKind::Comma
                | TokenKind::Equal
                | TokenKind::Return
                | TokenKind::Throw
                | TokenKind::As
                | TokenKind::FatArrow
                | TokenKind::Question
                | TokenKind::QuestionQuestion
                | TokenKind::Semicolon
                | TokenKind::LeftBrace
        )
    })
}

fn is_generic_delimiter(
    kind: TokenKind,
    previous: Option<TokenKind>,
    next: Option<TokenKind>,
) -> bool {
    match kind {
        TokenKind::Less => previous.is_some_and(|kind| {
            matches!(
                kind,
                TokenKind::Identifier | TokenKind::Greater | TokenKind::Function
            )
        }),
        TokenKind::Greater => next.is_some_and(|kind| {
            matches!(
                kind,
                TokenKind::LeftParen
                    | TokenKind::LeftBrace
                    | TokenKind::Comma
                    | TokenKind::Semicolon
                    | TokenKind::Dot
                    | TokenKind::Greater
            )
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_is_idempotent_and_preserves_non_trivia_tokens() {
        let source = b"function  main( ) : void{/* keep */const x:i32=1+2;const inferred=3i32+4i32;console.log(x);}";
        let first = format("test.tn", source);
        assert!(first.is_success());
        let second = format("test.tn", first.output.as_bytes());
        assert_eq!(first.output, second.output);

        let original = lex("test.tn", source);
        let formatted = lex("test.tn", first.output.as_bytes());
        let token_text = |lexed: &crate::Lexed<'_>| {
            lexed
                .tokens
                .iter()
                .filter(|token| !token.kind.is_trivia())
                .map(|token| (token.kind, lexed.source[token.range.clone()].to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(token_text(&original), token_text(&formatted));
    }

    #[test]
    fn formats_tnx_jsx_idempotently_without_spacing_inside_tags() {
        let source = b"function App(): Element { return (<View style={{ flex: 1, gap: 12 }}><Text>Hello</Text><Button onPress={() => save()}>Save</Button></View>); }\n";
        let first = format("app.tnx", source);
        assert!(first.is_success(), "{:?}", first.diagnostics);
        assert!(first.output.contains("<View style={{ flex: 1, gap: 12 }}"));
        assert!(first.output.contains("<Text>Hello</Text>"));
        assert!(
            first.output.contains("onPress={() => save()}") || first.output.contains("onPress =")
        );
        assert_eq!(
            first.output,
            format("app.tnx", first.output.as_bytes()).output
        );
    }

    #[test]
    fn formats_tnx_jsx_spreads_without_block_layout() {
        let source = b"function App(): Element { return <View {...props} enabled />; }\n";
        let first = format("app.tnx", source);
        assert!(first.is_success(), "{:?}", first.diagnostics);
        assert!(first.output.contains("<View {...props} enabled />"));
        assert_eq!(
            first.output,
            format("app.tnx", first.output.as_bytes()).output
        );
    }

    #[test]
    fn formats_tnx_expression_children_and_binding_patterns() {
        let source = b"function App(): Element { const [value, setValue] = useState<i32>(0i32); return <Text> { value }</Text>; }\n";
        let first = format("app.tnx", source);
        assert!(first.is_success(), "{:?}", first.diagnostics);
        assert!(first.output.contains("const [value, setValue]"));
        assert!(first.output.contains("<Text>{value}</Text>"));
        assert_eq!(
            first.output,
            format("app.tnx", first.output.as_bytes()).output
        );
    }

    #[test]
    fn elides_only_redundant_contextual_numeric_suffixes() {
        let formatted = format(
            "numeric-context.tn",
            b"function answer(): i32 { return 1i32; } function main(): void { const typed: i32 = 42i32; const inferred = 7i32; const wrong: i64 = 9i32; const values: [i32; 2usize] = [1, 2]; }",
        );
        assert!(formatted.is_success(), "{:?}", formatted.diagnostics);
        assert!(formatted.output.contains("typed: i32 = 42;"));
        assert!(formatted.output.contains("return 1;"));
        assert!(formatted.output.contains("inferred = 7i32;"));
        assert!(formatted.output.contains("wrong: i64 = 9i32;"));
        assert!(formatted.output.contains("2]"));
        assert!(!formatted.output.contains("2usize"));
        assert_eq!(
            formatted.output,
            format("numeric-context.tn", formatted.output.as_bytes()).output
        );
    }

    #[test]
    fn imports_are_sorted_by_group_and_specifier() {
        let source = br#"import { z } from "./z";
import { b } from "std/z";
import { a } from "std/a";
import { y } from "./a";
function main(): void {}
"#;
        let formatted = format("imports.tn", source);
        assert_eq!(
            formatted.output,
            r#"import { a } from "std/a";
import { b } from "std/z";
import { y } from "./a";
import { z } from "./z";

function main(): void {
}
"#
        );
    }

    #[test]
    fn nested_generic_closers_remain_separate_tokens() {
        let source = b"const value: Arc<Mutex<Database > > = undefined;";
        let formatted = format("nested-generics.tn", source);
        assert!(formatted.is_success());
        assert!(formatted.output.contains("> >"));
        assert_eq!(
            formatted.output,
            format("nested-generics.tn", formatted.output.as_bytes()).output
        );
    }

    #[test]
    fn formats_canonical_foreign_declaration_blocks_idempotently() {
        let formatted = format(
            "foreign.tn",
            b"declare extern \"C\"{function puts(text:* mut u8):void;}type Callback=extern \"C\" function(i32):void;",
        );
        assert!(formatted.is_success(), "{:?}", formatted.diagnostics);
        assert!(formatted.output.starts_with("declare extern \"C\" {\n"));
        assert!(
            formatted
                .output
                .contains("type Callback = extern \"C\" function(i32): void;")
        );
        assert_eq!(
            formatted.output,
            format("foreign.tn", formatted.output.as_bytes()).output
        );
    }

    #[test]
    fn formats_pointer_reference_and_symbol_method_spelling() {
        let formatted = format(
            "canonical-types.tn",
            b"interface Disposable{[Symbol.dispose]():void;}class Resource{public async [Symbol.asyncDispose]():Promise<void,never>{const borrowed:& i32=undefined;const shared:& mut i32=undefined;const mutable:* mut i32=undefined;const immutable:* const i32=undefined;return;}}",
        );
        assert!(formatted.is_success(), "{:?}", formatted.diagnostics);
        assert!(
            formatted
                .output
                .contains("public async [Symbol.asyncDispose]()")
        );
        assert!(formatted.output.contains("shared: &mut i32"));
        assert!(formatted.output.contains("borrowed: &i32"));
        assert!(formatted.output.contains("mutable: *mut i32"));
        assert!(formatted.output.contains("immutable: *const i32"));
        assert!(!formatted.output.contains("& mut"));
        assert!(!formatted.output.contains("* mut"));
        assert!(!formatted.output.contains("* const"));
    }

    #[test]
    fn formats_generator_markers_yields_and_async_iteration() {
        let formatted = format(
            "generators.tn",
            b"async function*events():AsyncIterable<i32>{yield 1i32;for await(const value of events()){value;}}",
        );
        assert!(formatted.is_success(), "{:?}", formatted.diagnostics);
        assert!(formatted.output.contains("async function* events()"));
        assert!(formatted.output.contains("yield 1i32;"));
        assert!(
            formatted
                .output
                .contains("for await (const value of events())")
        );
    }

    #[test]
    fn rejects_obsolete_source_macros_before_formatting() {
        let formatted = format(
            "macros.tn",
            b"macro getter(name:identifier,field:identifier,value:type){public {{name}}():{{value}}{return this.{{field}};}} @Expand(getter,getValue,value,i32)struct Counter{public value:i32;}",
        );
        assert!(!formatted.is_success(), "source macros must be rejected");
        assert!(
            formatted
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.condition.as_str() == "SYNTAX_EXCLUDED_CONSTRUCT")
        );
    }
}
