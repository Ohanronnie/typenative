use logos::{Lexer, Logos};
use std::ops::Range;
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};

#[repr(u16)]
#[derive(Logos, Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[logos(error = LexError)]
pub enum TokenKind {
    #[regex(r"[ \t\r\n]+")]
    Whitespace,
    #[regex(r"//[^\r\n]*")]
    LineComment,
    #[token("/*", lex_block_comment)]
    BlockComment,

    #[token("abstract")]
    Abstract,
    #[token("as")]
    As,
    #[token("async")]
    Async,
    #[token("await")]
    Await,
    #[token("break")]
    Break,
    #[token("case")]
    Case,
    #[token("catch")]
    Catch,
    #[token("class")]
    Class,
    #[token("const")]
    Const,
    #[token("constructor")]
    Constructor,
    #[token("continue")]
    Continue,
    #[token("crate")]
    Crate,
    #[token("declare")]
    Declare,
    #[token("default")]
    Default,
    #[token("dyn")]
    Dyn,
    #[token("else")]
    Else,
    #[token("enum")]
    Enum,
    #[token("export")]
    Export,
    #[token("extends")]
    Extends,
    #[token("extern")]
    Extern,
    #[token("false")]
    False,
    #[token("final")]
    Final,
    #[token("for")]
    For,
    #[token("from")]
    From,
    #[token("function")]
    Function,
    #[token("if")]
    If,
    #[token("impl")]
    Impl,
    #[token("implements")]
    Implements,
    #[token("import")]
    Import,
    #[token("instanceof")]
    InstanceOf,
    #[token("interface")]
    Interface,
    #[token("let")]
    Let,
    #[token("lifetime")]
    Lifetime,
    #[token("match")]
    Match,
    #[token("macro")]
    Macro,
    #[token("mod")]
    Mod,
    #[token("move")]
    Move,
    #[token("mut")]
    Mut,
    #[token("new")]
    New,
    #[token("null")]
    Null,
    #[token("of")]
    Of,
    #[token("override")]
    Override,
    #[token("private")]
    Private,
    #[token("protected")]
    Protected,
    #[token("public")]
    Public,
    #[token("pub")]
    Pub,
    #[token("record")]
    Record,
    #[token("readonly")]
    Readonly,
    #[token("return")]
    Return,
    #[token("scope")]
    Scope,
    #[token("static")]
    Static,
    #[token("struct")]
    Struct,
    #[token("super")]
    Super,
    #[token("self")]
    SelfValue,
    #[token("switch")]
    Switch,
    #[token("this")]
    This,
    #[token("throw")]
    Throw,
    #[token("throws")]
    Throws,
    #[token("true")]
    True,
    #[token("try")]
    Try,
    #[token("type")]
    Type,
    #[token("undefined")]
    Undefined,
    #[token("unknown")]
    Unknown,
    #[token("unsafe")]
    Unsafe,
    #[token("use")]
    Use,
    #[token("using")]
    Using,
    #[token("where")]
    Where,
    #[token("while")]
    While,
    #[token("yield")]
    Yield,
    #[token("extension")]
    Extension,

    #[regex(r"[_\p{XID_Start}][_\p{XID_Continue}]*")]
    Identifier,
    #[regex(r"0[bB][01](?:_?[01])*[A-Za-z0-9]*")]
    #[regex(r"0[oO][0-7](?:_?[0-7])*[A-Za-z0-9]*")]
    #[regex(r"0[xX][0-9A-Fa-f](?:_?[0-9A-Fa-f])*[A-Za-z0-9]*")]
    #[regex(r"[0-9](?:_?[0-9])*(?:[A-Za-z][A-Za-z0-9]*)?")]
    IntegerLiteral,
    #[regex(
        r"[0-9](?:_?[0-9])*\.(?:[0-9](?:_?[0-9])*)?(?:[eE][+-]?[0-9](?:_?[0-9])*)?(?:f32|f64)?"
    )]
    #[regex(r"[0-9](?:_?[0-9])*(?:[eE][+-]?[0-9](?:_?[0-9])*)(?:f32|f64)?")]
    FloatLiteral,
    #[token("\"", lex_string)]
    StringLiteral,
    #[token("'", lex_character)]
    CharacterLiteral,
    #[token("`", lex_template)]
    TemplateLiteral,

    #[token("===")]
    EqualEqualEqual,
    #[token("!==")]
    BangEqualEqual,
    #[token(">>=")]
    ShiftRightEqual,
    #[token("<<=")]
    ShiftLeftEqual,
    #[token("?.")]
    QuestionDot,
    #[token("??")]
    QuestionQuestion,
    #[token("=>")]
    FatArrow,
    #[token("<=")]
    LessEqual,
    #[token(">=")]
    GreaterEqual,
    #[token("&&")]
    AmpAmp,
    #[token("||")]
    PipePipe,
    #[token("<<")]
    ShiftLeft,
    #[token(">>")]
    ShiftRight,
    #[token("+=")]
    PlusEqual,
    #[token("-=")]
    MinusEqual,
    #[token("*=")]
    StarEqual,
    #[token("/=")]
    SlashEqual,
    #[token("%=")]
    PercentEqual,
    #[token("&=")]
    AmpEqual,
    #[token("|=")]
    PipeEqual,
    #[token("^=")]
    CaretEqual,
    #[token("as?")]
    AsQuestion,
    #[token("...")]
    Ellipsis,

    #[token("@")]
    At,
    #[token("{")]
    LeftBrace,
    #[token("}")]
    RightBrace,
    #[token("(")]
    LeftParen,
    #[token(")")]
    RightParen,
    #[token("[")]
    LeftBracket,
    #[token("]")]
    RightBracket,
    #[token(";")]
    Semicolon,
    #[token(":")]
    Colon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token("?")]
    Question,
    #[token("=")]
    Equal,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("&")]
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("!")]
    Bang,
    #[token("~")]
    Tilde,
    #[token("<")]
    Less,
    #[token(">")]
    Greater,
    #[regex(r".", priority = 0)]
    ErrorToken,
}

impl TokenKind {
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LexError {
    #[default]
    UnexpectedCharacter,
    UnterminatedBlockComment,
    UnterminatedString,
    UnterminatedCharacter,
    UnterminatedTemplate,
    UnbalancedTemplateInterpolation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct Lexed<'source> {
    pub source: &'source str,
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Tokenizes source bytes while retaining every trivia byte.
///
/// # Panics
///
/// Panics only if a compiler-owned static diagnostic identifier is invalid, which is an internal
/// compiler defect rather than an input-dependent condition.
pub fn lex<'source>(file: &str, bytes: &'source [u8]) -> Lexed<'source> {
    let Ok(source) = std::str::from_utf8(bytes) else {
        let error = std::str::from_utf8(bytes).expect_err("invalid UTF-8 was established");
        let start = error.valid_up_to();
        let end = start + error.error_len().unwrap_or(1);
        let lossy = String::from_utf8_lossy(bytes);
        let condition = ConditionId::new("SYNTAX_INVALID_UTF8").expect("static condition is valid");
        return Lexed {
            source: "",
            tokens: Vec::new(),
            diagnostics: vec![Diagnostic::error(
                condition,
                "source file is not valid UTF-8",
                Label {
                    span: SourceSpan::new(file, start..end, &lossy),
                    message: "invalid byte sequence starts here".into(),
                },
                "syntax/invalid-utf8",
            )],
        };
    };

    lex_source(file, source, source, 0)
}

pub(crate) fn lex_range<'source>(
    file: &str,
    source: &'source str,
    range: Range<usize>,
) -> Lexed<'source> {
    lex_source(file, source, &source[range.clone()], range.start)
}

fn lex_source<'source>(
    file: &str,
    source: &'source str,
    fragment: &str,
    base: usize,
) -> Lexed<'source> {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();
    let mut lexer = TokenKind::lexer(fragment);
    while let Some(result) = lexer.next() {
        let local_range = lexer.span();
        let range = (local_range.start + base)..(local_range.end + base);
        match result {
            Ok(kind) => {
                if let Some(issue) = validate_token(kind, &fragment[local_range]) {
                    diagnostics.push(literal_diagnostic(file, source, &range, issue));
                }
                tokens.push(Token { kind, range });
            }
            Err(error) => {
                diagnostics.push(lex_diagnostic(file, source, range.clone(), error));
                tokens.push(Token {
                    kind: TokenKind::ErrorToken,
                    range,
                });
            }
        }
    }
    Lexed {
        source,
        tokens,
        diagnostics,
    }
}

#[derive(Clone, Copy)]
struct TokenIssue {
    id: &'static str,
    message: &'static str,
    label: &'static str,
    offset: usize,
    length: usize,
}

fn validate_token(kind: TokenKind, text: &str) -> Option<TokenIssue> {
    match kind {
        TokenKind::IntegerLiteral => validate_integer(text),
        TokenKind::FloatLiteral => validate_float(text),
        TokenKind::StringLiteral => validate_escapes(text, false).err(),
        TokenKind::CharacterLiteral => validate_character(text),
        TokenKind::TemplateLiteral => validate_template(text),
        TokenKind::ErrorToken => Some(TokenIssue {
            id: "SYNTAX_UNEXPECTED_CHARACTER",
            message: "unexpected character",
            label: "this character does not begin a TypeNative token",
            offset: 0,
            length: text.len(),
        }),
        _ => None,
    }
}

fn validate_integer(text: &str) -> Option<TokenIssue> {
    let suffixes = [
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        "number",
    ];
    let (base, start) = if text.starts_with("0b") || text.starts_with("0B") {
        (2, 2)
    } else if text.starts_with("0o") || text.starts_with("0O") {
        (8, 2)
    } else if text.starts_with("0x") || text.starts_with("0X") {
        (16, 2)
    } else {
        (10, 0)
    };
    let digit_valid = |byte: u8| match base {
        2 => matches!(byte, b'0' | b'1'),
        8 => matches!(byte, b'0'..=b'7'),
        10 => byte.is_ascii_digit(),
        16 => byte.is_ascii_hexdigit(),
        _ => false,
    };
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && (digit_valid(bytes[end]) || bytes[end] == b'_') {
        end += 1;
    }
    let digits = &text[start..end];
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return Some(TokenIssue {
            id: "SYNTAX_INVALID_NUMERIC_SEPARATOR",
            message: "invalid numeric separator placement",
            label: "underscores must occur singly between digits",
            offset: start,
            length: digits.len().max(1),
        });
    }
    let suffix = &text[end..];
    if !suffix.is_empty() && !suffixes.contains(&suffix) {
        return Some(TokenIssue {
            id: "SYNTAX_INVALID_INTEGER_SUFFIX",
            message: "invalid integer literal suffix",
            label: "this suffix is not a TypeNative integer type",
            offset: end,
            length: suffix.len(),
        });
    }
    None
}

fn validate_float(text: &str) -> Option<TokenIssue> {
    if text.contains("__") || text.starts_with('_') || text.ends_with('_') {
        return Some(TokenIssue {
            id: "SYNTAX_INVALID_NUMERIC_SEPARATOR",
            message: "invalid numeric separator placement",
            label: "underscores must occur singly between digits",
            offset: text.find("__").unwrap_or(0),
            length: 2,
        });
    }
    if let Some(index) = text.find('f') {
        let suffix = &text[index..];
        if !matches!(suffix, "f32" | "f64") {
            return Some(TokenIssue {
                id: "SYNTAX_INVALID_FLOAT_SUFFIX",
                message: "invalid floating-point literal suffix",
                label: "floating-point suffix must be f32 or f64",
                offset: index,
                length: suffix.len(),
            });
        }
    }
    None
}

fn validate_character(text: &str) -> Option<TokenIssue> {
    if let Err(issue) = validate_escapes(text, false) {
        return Some(issue);
    }
    let inner = &text[1..text.len().saturating_sub(1)];
    let mut scalars = 0;
    let mut chars = inner.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            let Some(escaped) = chars.next() else {
                break;
            };
            if escaped == 'x' {
                chars.next();
                chars.next();
            } else if escaped == 'u' {
                for next in chars.by_ref() {
                    if next == '}' {
                        break;
                    }
                }
            }
        }
        scalars += 1;
    }
    if scalars == 1 {
        None
    } else {
        Some(TokenIssue {
            id: "SYNTAX_INVALID_CHARACTER_LENGTH",
            message: "character literal must contain exactly one Unicode scalar value",
            label: "this literal does not decode to one scalar value",
            offset: 1,
            length: inner.len().max(1),
        })
    }
}

fn validate_escapes(text: &str, template: bool) -> Result<(), TokenIssue> {
    let bytes = text.as_bytes();
    let mut index = 1;
    let end = bytes.len().saturating_sub(1);
    while index < end {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        let escape_start = index;
        index += 1;
        let Some(byte) = bytes.get(index).copied() else {
            return Err(invalid_escape(escape_start, 1));
        };
        match byte {
            b'\\' | b'"' | b'\'' | b'n' | b'r' | b't' | b'0' => index += 1,
            b'`' if template => index += 1,
            b'$' if template && bytes.get(index + 1) == Some(&b'{') => index += 2,
            b'x' => {
                let valid = bytes
                    .get(index + 1..index + 3)
                    .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit));
                if !valid {
                    return Err(invalid_escape(escape_start, 4.min(end - escape_start)));
                }
                index += 3;
            }
            b'u' if bytes.get(index + 1) == Some(&b'{') => {
                let digits_start = index + 2;
                let Some(relative_end) = bytes[digits_start..end]
                    .iter()
                    .position(|byte| *byte == b'}')
                else {
                    return Err(invalid_escape(escape_start, end - escape_start));
                };
                let digits_end = digits_start + relative_end;
                let digits = &text[digits_start..digits_end];
                let scalar = u32::from_str_radix(digits, 16)
                    .ok()
                    .and_then(char::from_u32);
                if digits.is_empty() || digits.len() > 6 || scalar.is_none() {
                    return Err(invalid_escape(escape_start, digits_end + 1 - escape_start));
                }
                index = digits_end + 1;
            }
            _ => return Err(invalid_escape(escape_start, 2)),
        }
    }
    Ok(())
}

const fn invalid_escape(offset: usize, length: usize) -> TokenIssue {
    TokenIssue {
        id: "SYNTAX_INVALID_ESCAPE",
        message: "invalid literal escape sequence",
        label: "this escape is not valid in the literal",
        offset,
        length,
    }
}

fn validate_template(text: &str) -> Option<TokenIssue> {
    let scan = scan_template(text.as_bytes(), 0).ok()?;
    let mut start = 1;
    for expression in scan.interpolations {
        let end = expression.start.saturating_sub(2);
        if let Err(mut issue) = validate_template_chunk(text, start, end) {
            issue.offset += start;
            return Some(issue);
        }
        start = expression.end + 1;
    }
    let end = scan.end.saturating_sub(1);
    validate_template_chunk(text, start, end)
        .err()
        .map(|mut issue| {
            issue.offset += start;
            issue
        })
}

fn validate_template_chunk(text: &str, start: usize, end: usize) -> Result<(), TokenIssue> {
    let mut quoted = String::with_capacity(end.saturating_sub(start) + 2);
    quoted.push('`');
    quoted.push_str(&text[start..end]);
    quoted.push('`');
    validate_escapes(&quoted, true).map_err(|mut issue| {
        issue.offset = issue.offset.saturating_sub(1);
        issue
    })
}

fn literal_diagnostic(
    file: &str,
    source: &str,
    token_range: &Range<usize>,
    issue: TokenIssue,
) -> Diagnostic {
    let start = token_range.start + issue.offset;
    let end = (start + issue.length).min(token_range.end);
    Diagnostic::error(
        ConditionId::new(issue.id).expect("static condition is valid"),
        issue.message,
        Label {
            span: SourceSpan::new(file, start..end, source),
            message: issue.label.into(),
        },
        issue.id.to_ascii_lowercase().replace('_', "/"),
    )
}

fn lex_diagnostic(file: &str, source: &str, range: Range<usize>, error: LexError) -> Diagnostic {
    let range = if error == LexError::UnbalancedTemplateInterpolation {
        source[range.clone()]
            .find("${")
            .map_or(range.clone(), |relative| {
                (range.start + relative)..(range.start + relative + 2)
            })
    } else {
        range
    };
    let (id, message, label) = match error {
        LexError::UnexpectedCharacter => (
            "SYNTAX_UNEXPECTED_CHARACTER",
            "unexpected character",
            "this character does not begin a TypeNative token",
        ),
        LexError::UnterminatedBlockComment => (
            "SYNTAX_UNTERMINATED_BLOCK_COMMENT",
            "unterminated block comment",
            "comment starts here and has no matching delimiter",
        ),
        LexError::UnterminatedString => (
            "SYNTAX_UNTERMINATED_STRING",
            "unterminated string literal",
            "string literal has no closing quote",
        ),
        LexError::UnterminatedCharacter => (
            "SYNTAX_UNTERMINATED_CHARACTER",
            "unterminated character literal",
            "character literal has no closing quote",
        ),
        LexError::UnterminatedTemplate => (
            "SYNTAX_UNTERMINATED_TEMPLATE",
            "unterminated template literal",
            "template literal has no closing backtick",
        ),
        LexError::UnbalancedTemplateInterpolation => (
            "SYNTAX_UNBALANCED_TEMPLATE_INTERPOLATION",
            "unbalanced template interpolation",
            "this interpolation has no matching closing brace",
        ),
    };
    Diagnostic::error(
        ConditionId::new(id).expect("static condition is valid"),
        message,
        Label {
            span: SourceSpan::new(file, range, source),
            message: label.into(),
        },
        id.to_ascii_lowercase().replace('_', "/"),
    )
}

fn lex_block_comment(lexer: &mut Lexer<'_, TokenKind>) -> Result<(), LexError> {
    let remainder = lexer.remainder().as_bytes();
    let mut depth = 1_u32;
    let mut index = 0;
    while index + 1 < remainder.len() {
        match &remainder[index..index + 2] {
            b"/*" => {
                depth += 1;
                index += 2;
            }
            b"*/" => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    lexer.bump(index);
                    return Ok(());
                }
            }
            _ => index += 1,
        }
    }
    lexer.bump(remainder.len());
    Err(LexError::UnterminatedBlockComment)
}

fn lex_string(lexer: &mut Lexer<'_, TokenKind>) -> Result<(), LexError> {
    lex_quoted(lexer, b'"', LexError::UnterminatedString)
}

fn lex_character(lexer: &mut Lexer<'_, TokenKind>) -> Result<(), LexError> {
    lex_quoted(lexer, b'\'', LexError::UnterminatedCharacter)
}

fn lex_quoted(
    lexer: &mut Lexer<'_, TokenKind>,
    delimiter: u8,
    error: LexError,
) -> Result<(), LexError> {
    let remainder = lexer.remainder().as_bytes();
    let mut escaped = false;
    for (index, byte) in remainder.iter().copied().enumerate() {
        if byte == b'\n' || byte == b'\r' {
            lexer.bump(index);
            return Err(error);
        }
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == delimiter {
            lexer.bump(index + 1);
            return Ok(());
        }
    }
    lexer.bump(remainder.len());
    Err(error)
}

fn lex_template(lexer: &mut Lexer<'_, TokenKind>) -> Result<(), LexError> {
    let mut input = Vec::with_capacity(lexer.remainder().len() + 1);
    input.push(b'`');
    input.extend_from_slice(lexer.remainder().as_bytes());
    match scan_template(&input, 0) {
        Ok(scan) => {
            lexer.bump(scan.end.saturating_sub(1));
            Ok(())
        }
        Err(error) => {
            lexer.bump(lexer.remainder().len());
            Err(error)
        }
    }
}

#[derive(Debug)]
pub(crate) struct TemplateScan {
    pub end: usize,
    pub interpolations: Vec<Range<usize>>,
}

pub(crate) fn template_interpolations(text: &str) -> Vec<Range<usize>> {
    scan_template(text.as_bytes(), 0)
        .map(|scan| scan.interpolations)
        .unwrap_or_default()
}

/// Returns the byte ranges of expressions embedded in a complete template literal token.
#[must_use]
pub fn template_interpolation_ranges(text: &str) -> Vec<Range<usize>> {
    template_interpolations(text)
}

fn scan_template(bytes: &[u8], start: usize) -> Result<TemplateScan, LexError> {
    let mut index = start + 1;
    let mut interpolations = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'`' => {
                return Ok(TemplateScan {
                    end: index + 1,
                    interpolations,
                });
            }
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                let expression_start = index + 2;
                let expression_end = scan_interpolation(bytes, expression_start)?;
                interpolations.push(expression_start..expression_end);
                index = expression_end + 1;
            }
            _ => index += 1,
        }
    }
    Err(LexError::UnterminatedTemplate)
}

fn scan_interpolation(bytes: &[u8], start: usize) -> Result<usize, LexError> {
    let mut index = start;
    let mut depth = 1_u32;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' => index = skip_quoted(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index);
            }
            b'`' => {
                index = scan_template(bytes, index)
                    .map_err(|_| LexError::UnbalancedTemplateInterpolation)?
                    .end;
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    Err(LexError::UnbalancedTemplateInterpolation)
}

fn skip_quoted(bytes: &[u8], start: usize) -> usize {
    let delimiter = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == delimiter {
            return index + 1;
        } else {
            index += 1;
        }
    }
    index
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 2;
    let mut depth = 1_u32;
    while index + 1 < bytes.len() {
        if &bytes[index..index + 2] == b"/*" {
            depth += 1;
            index += 2;
        } else if &bytes[index..index + 2] == b"*/" {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            index += 1;
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        lex("test.tn", source.as_bytes())
            .tokens
            .into_iter()
            .filter_map(|token| (!token.kind.is_trivia()).then_some(token.kind))
            .collect()
    }

    #[test]
    fn retains_nested_comments_as_one_token() {
        let lexed = lex("test.tn", b"/* outer /* nested */ done */ const");
        assert!(lexed.diagnostics.is_empty());
        assert_eq!(lexed.tokens[0].kind, TokenKind::BlockComment);
        assert_eq!(lexed.tokens[0].range, 0..29);
        assert_eq!(kinds(lexed.source), vec![TokenKind::Const]);
    }

    #[test]
    fn rejects_invalid_utf8_before_tokenization() {
        let lexed = lex("bad.tn", &[0x66, 0x80, 0x6f]);
        assert!(lexed.tokens.is_empty());
        assert_eq!(
            lexed.diagnostics[0].condition.as_str(),
            "SYNTAX_INVALID_UTF8"
        );
    }

    #[test]
    fn recognizes_unicode_identifiers_and_keywords() {
        assert_eq!(
            kinds("const Αθήνα = 1;"),
            vec![
                TokenKind::Const,
                TokenKind::Identifier,
                TokenKind::Equal,
                TokenKind::IntegerLiteral,
                TokenKind::Semicolon
            ]
        );
    }

    #[test]
    fn reserves_declare_as_a_keyword() {
        assert_eq!(kinds("declare"), vec![TokenKind::Declare]);
        assert_ne!(kinds("declare"), vec![TokenKind::Identifier]);
    }

    #[test]
    fn scans_literals_without_losing_spelling() {
        let source = "\"hi\\n\" 'λ' `x=${value}`";
        let lexed = lex("literal.tn", source.as_bytes());
        assert!(lexed.diagnostics.is_empty());
        assert_eq!(
            kinds(source),
            vec![
                TokenKind::StringLiteral,
                TokenKind::CharacterLiteral,
                TokenKind::TemplateLiteral
            ]
        );
        let reconstructed: String = lexed
            .tokens
            .iter()
            .map(|token| &source[token.range.clone()])
            .collect();
        assert_eq!(reconstructed, source);
    }

    #[test]
    fn template_interpolation_balances_normal_language_tokens() {
        let source = r#"`a ${call({ value: "}" }, `nested ${other}`) /* } */} z`"#;
        let lexed = lex("template.tn", source.as_bytes());
        assert!(lexed.diagnostics.is_empty(), "{:#?}", lexed.diagnostics);
        assert_eq!(kinds(source), vec![TokenKind::TemplateLiteral]);
        assert_eq!(
            template_interpolations(source)
                .into_iter()
                .map(|range| &source[range])
                .collect::<Vec<_>>(),
            vec![r#"call({ value: "}" }, `nested ${other}`) /* } */"#]
        );
    }

    #[test]
    fn unbalanced_template_interpolation_is_localized() {
        let source = "const value = `before ${name`;";
        let lexed = lex("template.tn", source.as_bytes());
        let diagnostic = &lexed.diagnostics[0];
        assert_eq!(
            diagnostic.condition.as_str(),
            "SYNTAX_UNBALANCED_TEMPLATE_INTERPOLATION"
        );
        let start = source.find("${").expect("interpolation opener");
        assert_eq!(
            diagnostic.primary.span.byte_start,
            u32::try_from(start).expect("fixture offset fits u32")
        );
        assert_eq!(
            diagnostic.primary.span.byte_end,
            u32::try_from(start + 2).expect("fixture offset fits u32")
        );
    }

    #[test]
    fn diagnoses_invalid_escapes_and_character_width() {
        let invalid_escape = lex("escape.tn", br#""bad\q""#);
        assert_eq!(
            invalid_escape.diagnostics[0].condition.as_str(),
            "SYNTAX_INVALID_ESCAPE"
        );
        let invalid_character = lex("character.tn", b"'ab'");
        assert_eq!(
            invalid_character.diagnostics[0].condition.as_str(),
            "SYNTAX_INVALID_CHARACTER_LENGTH"
        );
    }

    #[test]
    fn diagnoses_invalid_numeric_suffixes() {
        let lexed = lex("number.tn", b"123wat");
        assert_eq!(
            lexed.diagnostics[0].condition.as_str(),
            "SYNTAX_INVALID_INTEGER_SUFFIX"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_fixed_token_form_has_a_lexer_fixture() {
        let cases = [
            ("abstract", TokenKind::Abstract),
            ("as", TokenKind::As),
            ("async", TokenKind::Async),
            ("await", TokenKind::Await),
            ("break", TokenKind::Break),
            ("case", TokenKind::Case),
            ("catch", TokenKind::Catch),
            ("class", TokenKind::Class),
            ("const", TokenKind::Const),
            ("constructor", TokenKind::Constructor),
            ("continue", TokenKind::Continue),
            ("declare", TokenKind::Declare),
            ("dyn", TokenKind::Dyn),
            ("else", TokenKind::Else),
            ("enum", TokenKind::Enum),
            ("export", TokenKind::Export),
            ("extends", TokenKind::Extends),
            ("extern", TokenKind::Extern),
            ("false", TokenKind::False),
            ("final", TokenKind::Final),
            ("for", TokenKind::For),
            ("from", TokenKind::From),
            ("function", TokenKind::Function),
            ("if", TokenKind::If),
            ("impl", TokenKind::Impl),
            ("implements", TokenKind::Implements),
            ("import", TokenKind::Import),
            ("instanceof", TokenKind::InstanceOf),
            ("interface", TokenKind::Interface),
            ("let", TokenKind::Let),
            ("lifetime", TokenKind::Lifetime),
            ("match", TokenKind::Match),
            ("move", TokenKind::Move),
            ("mut", TokenKind::Mut),
            ("new", TokenKind::New),
            ("null", TokenKind::Null),
            ("of", TokenKind::Of),
            ("override", TokenKind::Override),
            ("private", TokenKind::Private),
            ("protected", TokenKind::Protected),
            ("public", TokenKind::Public),
            ("readonly", TokenKind::Readonly),
            ("return", TokenKind::Return),
            ("scope", TokenKind::Scope),
            ("static", TokenKind::Static),
            ("struct", TokenKind::Struct),
            ("super", TokenKind::Super),
            ("self", TokenKind::SelfValue),
            ("throw", TokenKind::Throw),
            ("throws", TokenKind::Throws),
            ("true", TokenKind::True),
            ("try", TokenKind::Try),
            ("type", TokenKind::Type),
            ("undefined", TokenKind::Undefined),
            ("unsafe", TokenKind::Unsafe),
            ("where", TokenKind::Where),
            ("while", TokenKind::While),
            ("===", TokenKind::EqualEqualEqual),
            ("!==", TokenKind::BangEqualEqual),
            (">>=", TokenKind::ShiftRightEqual),
            ("<<=", TokenKind::ShiftLeftEqual),
            ("?.", TokenKind::QuestionDot),
            ("??", TokenKind::QuestionQuestion),
            ("=>", TokenKind::FatArrow),
            ("<=", TokenKind::LessEqual),
            (">=", TokenKind::GreaterEqual),
            ("&&", TokenKind::AmpAmp),
            ("||", TokenKind::PipePipe),
            ("<<", TokenKind::ShiftLeft),
            (">>", TokenKind::ShiftRight),
            ("+=", TokenKind::PlusEqual),
            ("-=", TokenKind::MinusEqual),
            ("*=", TokenKind::StarEqual),
            ("/=", TokenKind::SlashEqual),
            ("%=", TokenKind::PercentEqual),
            ("&=", TokenKind::AmpEqual),
            ("|=", TokenKind::PipeEqual),
            ("^=", TokenKind::CaretEqual),
            ("as?", TokenKind::AsQuestion),
            ("...", TokenKind::Ellipsis),
            ("@", TokenKind::At),
            ("{", TokenKind::LeftBrace),
            ("}", TokenKind::RightBrace),
            ("(", TokenKind::LeftParen),
            (")", TokenKind::RightParen),
            ("[", TokenKind::LeftBracket),
            ("]", TokenKind::RightBracket),
            (";", TokenKind::Semicolon),
            (":", TokenKind::Colon),
            (",", TokenKind::Comma),
            (".", TokenKind::Dot),
            ("?", TokenKind::Question),
            ("=", TokenKind::Equal),
            ("+", TokenKind::Plus),
            ("-", TokenKind::Minus),
            ("*", TokenKind::Star),
            ("/", TokenKind::Slash),
            ("%", TokenKind::Percent),
            ("&", TokenKind::Amp),
            ("|", TokenKind::Pipe),
            ("^", TokenKind::Caret),
            ("!", TokenKind::Bang),
            ("~", TokenKind::Tilde),
            ("<", TokenKind::Less),
            (">", TokenKind::Greater),
        ];
        for (text, expected) in cases {
            let lexed = lex("token.tn", text.as_bytes());
            assert!(lexed.diagnostics.is_empty(), "failed to lex {text:?}");
            assert_eq!(lexed.tokens.len(), 1, "fixture {text:?}");
            assert_eq!(lexed.tokens[0].kind, expected, "fixture {text:?}");
        }
    }
}
