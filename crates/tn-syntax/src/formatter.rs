use crate::{TokenKind, lex, parse};
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
        let text = &lexed.source[token.range.clone()];
        writer.token(token.kind, text, previous, next);
    }
    writer.finish_file();
    let output = sort_leading_imports(writer.output);
    FormatResult {
        output,
        diagnostics: Vec::new(),
    }
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
struct Writer {
    output: String,
    indent: usize,
    line_start: bool,
    pending_space: bool,
    inline_braces: Vec<bool>,
    placeholder_mode: u8,
}

impl Writer {
    fn token(
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
                self.newline();
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
                if matches!(
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
                ) {
                    self.space();
                } else {
                    self.trim_space();
                }
                self.write("(");
            }
            TokenKind::LeftBracket => {
                self.trim_space();
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
        let source = b"function  main( ) : void{/* keep */const x:i32=1i32+2i32;console.log(x);}";
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
