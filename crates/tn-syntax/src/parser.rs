use crate::lexer::{lex_range, template_interpolations};
use crate::{Token, TokenKind, lex};
use rowan::{GreenNode, GreenNodeBuilder, Language};
use std::ops::Range;
use tn_diagnostics::{Applicability, ConditionId, Diagnostic, Edit, Label, SourceSpan};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SyntaxKind(pub u16);

impl SyntaxKind {
    pub const SOURCE_FILE: Self = Self(1_000);
    pub const ERROR: Self = Self(1_001);
    pub const ATTRIBUTE: Self = Self(1_002);
    pub const IMPORT_DECLARATION: Self = Self(1_003);
    pub const CONST_DECLARATION: Self = Self(1_004);
    pub const STATIC_DECLARATION: Self = Self(1_005);
    pub const TYPE_ALIAS_DECLARATION: Self = Self(1_006);
    pub const FUNCTION_DECLARATION: Self = Self(1_007);
    pub const STRUCT_DECLARATION: Self = Self(1_008);
    pub const CLASS_DECLARATION: Self = Self(1_009);
    pub const INTERFACE_DECLARATION: Self = Self(1_010);
    pub const ENUM_DECLARATION: Self = Self(1_011);
    pub const IMPL_DECLARATION: Self = Self(1_012);
    pub const EXTERN_BLOCK: Self = Self(1_013);
    pub const FIELD_DECLARATION: Self = Self(1_014);
    pub const METHOD_DECLARATION: Self = Self(1_015);
    pub const CONSTRUCTOR_DECLARATION: Self = Self(1_016);
    pub const PARAMETER_LIST: Self = Self(1_017);
    pub const GENERIC_PARAMETER_LIST: Self = Self(1_018);
    pub const GENERIC_ARGUMENT_LIST: Self = Self(1_019);
    pub const WHERE_CLAUSE: Self = Self(1_020);
    pub const TYPE: Self = Self(1_021);
    pub const BLOCK: Self = Self(1_022);
    pub const STATEMENT: Self = Self(1_023);
    pub const EXPRESSION: Self = Self(1_024);
    pub const PATTERN: Self = Self(1_025);
    pub const MATCH_ARM: Self = Self(1_026);
    pub const CATCH_CLAUSE: Self = Self(1_027);
    pub const ENUM_VARIANT: Self = Self(1_028);
    pub const SWITCH_ARM: Self = Self(1_029);
    pub const TEST_REGISTRATION: Self = Self(1_030);
    pub const BINDING_PATTERN: Self = Self(1_031);
    pub const BINDING_PROPERTY: Self = Self(1_032);
    pub const JSX_ELEMENT: Self = Self(1_033);
    pub const JSX_FRAGMENT: Self = Self(1_034);
    pub const JSX_OPENING_ELEMENT: Self = Self(1_035);
    pub const JSX_CLOSING_ELEMENT: Self = Self(1_036);
    pub const JSX_NAME: Self = Self(1_037);
    pub const JSX_ATTRIBUTE: Self = Self(1_038);
    pub const JSX_SPREAD_ATTRIBUTE: Self = Self(1_039);
    pub const JSX_EXPRESSION_CONTAINER: Self = Self(1_040);
    pub const JSX_TEXT: Self = Self(1_041);
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TnLanguage {}

impl Language for TnLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        SyntaxKind(raw.0)
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.0)
    }
}

pub type SyntaxNode = rowan::SyntaxNode<TnLanguage>;

#[derive(Clone, Debug)]
pub struct Parse {
    pub(crate) green: GreenNode,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl Parse {
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn is_success(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

pub fn parse(file: &str, bytes: &[u8]) -> Parse {
    let lexed = lex(file, bytes);
    if lexed.source.is_empty() && !bytes.is_empty() && !lexed.diagnostics.is_empty() {
        let mut builder = GreenNodeBuilder::new();
        builder.start_node(TnLanguage::kind_to_raw(SyntaxKind::SOURCE_FILE));
        builder.finish_node();
        return Parse {
            green: builder.finish(),
            diagnostics: lexed.diagnostics,
        };
    }
    let mut parser = Parser {
        file,
        source: lexed.source,
        tokens: &lexed.tokens,
        cursor: 0,
        eof_offset: lexed.source.len(),
        recursion_depth: 0,
        jsx_enabled: file.ends_with(".tnx"),
        builder: GreenNodeBuilder::new(),
        diagnostics: lexed.diagnostics,
    };
    parser.source_file();
    Parse {
        green: parser.builder.finish(),
        diagnostics: parser.diagnostics,
    }
}

struct Parser<'source, 'tokens> {
    file: &'source str,
    source: &'source str,
    tokens: &'tokens [Token],
    cursor: usize,
    eof_offset: usize,
    recursion_depth: u16,
    jsx_enabled: bool,
    builder: GreenNodeBuilder<'static>,
    diagnostics: Vec<Diagnostic>,
}

impl Parser<'_, '_> {
    fn source_file(&mut self) {
        self.start(SyntaxKind::SOURCE_FILE);
        while self.current().is_some() {
            let before = self.cursor;
            self.item();
            if self.cursor == before {
                self.error_current(
                    "SYNTAX_EXPECTED_DECLARATION",
                    "expected a declaration",
                    "a top-level declaration must begin here",
                );
                self.bump();
            }
        }
        self.bump_trivia();
        self.finish();
    }

    fn item(&mut self) {
        if self.at(TokenKind::Import) {
            self.import_declaration();
            return;
        }
        while self.at(TokenKind::At) {
            self.attribute();
        }
        self.eat(TokenKind::Export);
        if self.at(TokenKind::Identifier)
            && self.current_text() == Some("test")
            && self.nth(1) == Some(TokenKind::LeftParen)
        {
            self.test_registration();
            return;
        }
        self.declaration();
    }

    fn test_registration(&mut self) {
        self.start(SyntaxKind::TEST_REGISTRATION);
        self.expect(TokenKind::Identifier);
        self.expect(TokenKind::LeftParen);
        self.expect(TokenKind::StringLiteral);
        self.expect(TokenKind::Comma);
        self.expression();
        self.expect(TokenKind::RightParen);
        self.expect(TokenKind::Semicolon);
        self.finish();
    }

    fn attribute(&mut self) {
        self.start(SyntaxKind::ATTRIBUTE);
        self.expect(TokenKind::At);
        if !self.eat(TokenKind::Identifier) && !self.eat(TokenKind::Unknown) {
            self.expect(TokenKind::Export);
        }
        if self.eat(TokenKind::LeftParen) {
            self.delimited_sequence(TokenKind::RightParen, Parser::attribute_argument);
        }
        self.finish();
    }

    fn attribute_argument(&mut self) {
        if self.at_literal() || self.at(TokenKind::Identifier) || self.at(TokenKind::Unsafe) {
            self.bump();
            while self.eat(TokenKind::Dot) {
                self.expect(TokenKind::Identifier);
            }
        } else {
            self.error_and_recover(
                "SYNTAX_EXPECTED_ATTRIBUTE_ARGUMENT",
                "expected an attribute argument",
                &[TokenKind::Comma, TokenKind::RightParen],
            );
        }
    }

    fn current_text(&self) -> Option<&str> {
        self.current_token_range().map(|range| &self.source[range])
    }

    fn reject_keyword(&mut self, name: &str) {
        self.error_current(
            "SYNTAX_EXCLUDED_CONSTRUCT",
            &format!("`{name}` is not part of canonical TypeNative"),
            "use the canonical TypeNative spelling",
        );
        if self.current().is_some() {
            self.bump();
        }
    }

    fn import_declaration(&mut self) {
        self.start(SyntaxKind::IMPORT_DECLARATION);
        self.expect(TokenKind::Import);
        if self.at(TokenKind::StringLiteral) {
            self.error_current(
                "SYNTAX_EXCLUDED_IMPORT_FORM",
                "side-effect imports are not part of canonical TypeNative",
                "import named declarations with `import { Name } from \"...\"`",
            );
            self.bump();
        } else {
            if self.eat(TokenKind::LeftBrace) {
                if !self.at(TokenKind::RightBrace) {
                    loop {
                        self.expect(TokenKind::Identifier);
                        if self.eat(TokenKind::As) {
                            self.expect(TokenKind::Identifier);
                        }
                        if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightBrace) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RightBrace);
            } else if self.at(TokenKind::Star) {
                self.error_current(
                    "SYNTAX_EXCLUDED_IMPORT_FORM",
                    "namespace imports are not part of canonical TypeNative",
                    "import the required declarations by name",
                );
                self.bump();
                self.expect(TokenKind::As);
                self.expect(TokenKind::Identifier);
            } else {
                self.error_current(
                    "SYNTAX_EXPECTED_IMPORT_CLAUSE",
                    "expected an import clause",
                    "use a named, namespace, or side-effect import",
                );
            }
            self.expect(TokenKind::From);
            self.expect(TokenKind::StringLiteral);
        }
        self.expect(TokenKind::Semicolon);
        self.finish();
    }

    fn declaration(&mut self) {
        match self.current() {
            Some(TokenKind::Const) => self.const_declaration(),
            Some(TokenKind::Static) => self.static_declaration(),
            Some(TokenKind::Type) => self.type_alias_declaration(),
            Some(TokenKind::Unsafe) if self.nth(1) == Some(TokenKind::Impl) => {
                self.excluded_construct("impl");
            }
            Some(TokenKind::Function | TokenKind::Async | TokenKind::Unsafe) => {
                self.function_declaration();
            }
            Some(TokenKind::Struct) => self.struct_declaration(),
            Some(TokenKind::Class | TokenKind::Abstract) => {
                self.class_declaration();
            }
            Some(TokenKind::Interface) => self.interface_declaration(),
            Some(TokenKind::Enum) => self.enum_declaration(),
            Some(
                TokenKind::Impl
                | TokenKind::Where
                | TokenKind::Match
                | TokenKind::Dyn
                | TokenKind::Use
                | TokenKind::Mod
                | TokenKind::Pub
                | TokenKind::Crate
                | TokenKind::Record
                | TokenKind::Extension
                | TokenKind::Derives
                | TokenKind::Final
                | TokenKind::Macro,
            ) => {
                let name = self.current_text().unwrap_or("excluded").to_owned();
                self.excluded_construct(&name);
            }
            Some(TokenKind::Declare) => self.foreign_declaration_block(false),
            Some(TokenKind::Extern) if self.nth(1) == Some(TokenKind::Struct) => {
                self.extern_struct_declaration();
            }
            Some(TokenKind::Extern)
                if self.nth(1) == Some(TokenKind::StringLiteral)
                    && self.nth(2) == Some(TokenKind::Function) =>
            {
                self.extern_function_declaration();
            }
            Some(TokenKind::Extern)
                if self.nth(1) == Some(TokenKind::StringLiteral)
                    && self.nth(2) == Some(TokenKind::LeftBrace) =>
            {
                self.foreign_declaration_block(true);
            }
            _ => self.error_and_recover(
                "SYNTAX_EXPECTED_DECLARATION",
                "expected a declaration",
                &[
                    TokenKind::Semicolon,
                    TokenKind::RightBrace,
                    TokenKind::Import,
                    TokenKind::Const,
                    TokenKind::Function,
                    TokenKind::Struct,
                    TokenKind::Class,
                    TokenKind::Interface,
                    TokenKind::Enum,
                    TokenKind::Declare,
                    TokenKind::Extern,
                ],
            ),
        }
    }

    fn const_declaration(&mut self) {
        self.start(SyntaxKind::CONST_DECLARATION);
        self.expect(TokenKind::Const);
        self.expect(TokenKind::Identifier);
        if self.eat(TokenKind::Colon) {
            self.ty();
        }
        self.expect(TokenKind::Equal);
        self.expression();
        self.expect(TokenKind::Semicolon);
        self.finish();
    }

    fn static_declaration(&mut self) {
        self.start(SyntaxKind::STATIC_DECLARATION);
        self.expect(TokenKind::Static);
        self.eat(TokenKind::Mut);
        self.expect(TokenKind::Identifier);
        self.expect(TokenKind::Colon);
        self.ty();
        self.expect(TokenKind::Equal);
        self.expression();
        self.expect(TokenKind::Semicolon);
        self.finish();
    }

    fn type_alias_declaration(&mut self) {
        self.start(SyntaxKind::TYPE_ALIAS_DECLARATION);
        self.expect(TokenKind::Type);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        self.expect(TokenKind::Equal);
        self.ty();
        self.expect(TokenKind::Semicolon);
        self.finish();
    }

    fn function_declaration(&mut self) {
        self.start(SyntaxKind::FUNCTION_DECLARATION);
        self.eat(TokenKind::Unsafe);
        self.eat(TokenKind::Async);
        self.expect(TokenKind::Function);
        self.eat(TokenKind::Star);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        self.parameter_list();
        self.expect(TokenKind::Colon);
        self.ty();
        if self.at(TokenKind::Throws) {
            self.throws_clause();
        }
        if self.at(TokenKind::Where) {
            self.excluded_construct("where");
        }
        self.block();
        self.finish();
    }

    fn struct_declaration(&mut self) {
        self.start(SyntaxKind::STRUCT_DECLARATION);
        self.expect(TokenKind::Struct);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        if self.eat(TokenKind::Implements) {
            self.type_path();
            while self.eat(TokenKind::Comma) {
                self.type_path();
            }
        }
        if self.at(TokenKind::Derives) {
            self.excluded_construct("derives");
            self.finish();
            return;
        }
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            self.struct_member();
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn extern_struct_declaration(&mut self) {
        self.start(SyntaxKind::STRUCT_DECLARATION);
        self.expect(TokenKind::Extern);
        self.expect(TokenKind::Struct);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            self.field_declaration(false);
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn extern_function_declaration(&mut self) {
        self.start(SyntaxKind::FUNCTION_DECLARATION);
        self.expect(TokenKind::Extern);
        self.expect(TokenKind::StringLiteral);
        self.expect(TokenKind::Function);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        self.parameter_list();
        self.expect(TokenKind::Colon);
        self.ty();
        if self.at(TokenKind::Throws) {
            self.throws_clause();
        }
        self.block();
        self.finish();
    }

    fn class_declaration(&mut self) {
        self.start(SyntaxKind::CLASS_DECLARATION);
        self.eat(TokenKind::Abstract);
        self.expect(TokenKind::Class);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        if self.eat(TokenKind::Extends) {
            self.type_path();
        }
        if self.eat(TokenKind::Implements) {
            self.type_path();
            while self.eat(TokenKind::Comma) {
                self.type_path();
            }
        }
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            self.class_member();
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn class_member(&mut self) {
        while self.at(TokenKind::At) {
            self.attribute();
        }
        self.visibility();
        if self.at(TokenKind::Constructor) {
            self.start(SyntaxKind::CONSTRUCTOR_DECLARATION);
            self.bump();
            self.parameter_list();
            self.throws_clause();
            self.block();
            self.finish();
            return;
        }
        let mut offset = 0;
        while matches!(
            self.nth(offset),
            Some(
                TokenKind::Static
                    | TokenKind::Abstract
                    | TokenKind::Final
                    | TokenKind::Override
                    | TokenKind::Mut
                    | TokenKind::Move
                    | TokenKind::Unsafe
                    | TokenKind::Async
            )
        ) {
            offset += 1;
        }
        if self.method_name_width(offset).is_some_and(|width| {
            matches!(
                self.nth(offset + width),
                Some(TokenKind::LeftParen | TokenKind::Less)
            )
        }) {
            self.method(false);
        } else {
            self.field_declaration(false);
        }
    }

    fn interface_declaration(&mut self) {
        self.start(SyntaxKind::INTERFACE_DECLARATION);
        self.expect(TokenKind::Interface);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        if self.eat(TokenKind::Implements) {
            self.type_path();
            while self.eat(TokenKind::Comma) {
                self.type_path();
            }
        }
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            self.method(true);
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn enum_declaration(&mut self) {
        self.start(SyntaxKind::ENUM_DECLARATION);
        self.expect(TokenKind::Enum);
        self.expect(TokenKind::Identifier);
        self.generic_parameters();
        if self.eat(TokenKind::Colon) {
            self.ty();
        }
        if self.eat(TokenKind::Implements) {
            self.type_path();
            while self.eat(TokenKind::Comma) {
                self.type_path();
            }
        }
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            if matches!(
                self.current(),
                Some(
                    TokenKind::Public
                        | TokenKind::Protected
                        | TokenKind::Private
                        | TokenKind::Static
                        | TokenKind::Mut
                        | TokenKind::Move
                        | TokenKind::Unsafe
                        | TokenKind::Async
                        | TokenKind::Function
                )
            ) {
                self.visibility();
                self.method(false);
                self.ensure_progress(before);
                continue;
            }
            self.start(SyntaxKind::ENUM_VARIANT);
            self.expect(TokenKind::Identifier);
            if self.eat(TokenKind::LeftParen) {
                if !self.at(TokenKind::RightParen) {
                    self.type_sequence(TokenKind::RightParen);
                }
                self.expect(TokenKind::RightParen);
            } else if self.eat(TokenKind::LeftBrace) {
                while self.current().is_some() && !self.at(TokenKind::RightBrace) {
                    let field_before = self.cursor;
                    self.field_declaration(true);
                    self.ensure_progress(field_before);
                }
                self.expect(TokenKind::RightBrace);
            } else if self.eat(TokenKind::Equal) {
                self.expression();
            }
            self.finish();
            if !self.eat(TokenKind::Comma) && !self.at(TokenKind::RightBrace) {
                self.expect(TokenKind::Comma);
            }
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn excluded_construct(&mut self, name: &str) {
        self.error_current(
            "SYNTAX_EXCLUDED_CONSTRUCT",
            &format!("`{name}` is not part of canonical TypeNative"),
            "use the canonical TypeNative spelling",
        );
        self.start(SyntaxKind::ERROR);
        while let Some(kind) = self.current() {
            if kind == TokenKind::Semicolon {
                self.bump();
                break;
            }
            if kind == TokenKind::LeftBrace {
                let mut depth = 0_u32;
                while let Some(nested) = self.current() {
                    self.bump();
                    match nested {
                        TokenKind::LeftBrace => depth += 1,
                        TokenKind::RightBrace => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                break;
            }
            self.bump();
        }
        self.finish();
    }

    fn struct_member(&mut self) {
        while self.at(TokenKind::At) {
            self.attribute();
        }
        self.visibility();
        let mut offset = 0;
        while matches!(
            self.nth(offset),
            Some(
                TokenKind::Static
                    | TokenKind::Abstract
                    | TokenKind::Final
                    | TokenKind::Override
                    | TokenKind::Mut
                    | TokenKind::Move
                    | TokenKind::Unsafe
                    | TokenKind::Async
            )
        ) {
            offset += 1;
        }
        if self.nth(offset) == Some(TokenKind::Constructor) {
            self.excluded_construct("constructor");
        } else if self.method_name_width(offset).is_some_and(|width| {
            matches!(
                self.nth(offset + width),
                Some(TokenKind::LeftParen | TokenKind::Less)
            )
        }) {
            self.method(false);
        } else {
            self.field_declaration(false);
        }
    }

    fn foreign_declaration_block(&mut self, obsolete: bool) {
        self.start(SyntaxKind::EXTERN_BLOCK);
        if obsolete {
            self.obsolete_extern_block_diagnostic();
            self.expect(TokenKind::Extern);
        } else {
            self.expect(TokenKind::Declare);
            self.expect(TokenKind::Extern);
        }
        self.expect(TokenKind::StringLiteral);
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            while self.at(TokenKind::At) {
                self.attribute();
            }
            self.expect(TokenKind::Function);
            self.expect(TokenKind::Identifier);
            self.expect(TokenKind::LeftParen);
            if !self.at(TokenKind::RightParen) {
                if self.eat(TokenKind::Ellipsis) {
                    // Variadic-only declaration.
                } else {
                    loop {
                        self.expect(TokenKind::Identifier);
                        self.expect(TokenKind::Colon);
                        self.ty();
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                        if self.eat(TokenKind::Ellipsis) || self.at(TokenKind::RightParen) {
                            break;
                        }
                    }
                }
            }
            self.expect(TokenKind::RightParen);
            self.expect(TokenKind::Colon);
            self.ty();
            self.expect(TokenKind::Semicolon);
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn field_declaration(&mut self, attributes: bool) {
        self.start(SyntaxKind::FIELD_DECLARATION);
        if attributes {
            while self.at(TokenKind::At) {
                self.attribute();
            }
        }
        self.visibility();
        if self.eat(TokenKind::Static) {
            self.eat(TokenKind::Mut);
        }
        self.eat(TokenKind::Readonly);
        self.expect(TokenKind::Identifier);
        self.eat(TokenKind::Question);
        self.expect(TokenKind::Colon);
        self.ty();
        if self.eat(TokenKind::Equal) {
            self.expression();
        }
        self.expect(TokenKind::Semicolon);
        self.finish();
    }

    fn method(&mut self, signature_only: bool) {
        self.start(SyntaxKind::METHOD_DECLARATION);
        self.eat(TokenKind::Function);
        self.eat(TokenKind::Static);
        if !self.eat(TokenKind::Abstract) && !self.eat(TokenKind::Final) {
            self.eat(TokenKind::Override);
        }
        if self.at(TokenKind::Mut) {
            self.excluded_construct("mut");
            self.finish();
            return;
        }
        self.eat(TokenKind::Move);
        self.eat(TokenKind::Unsafe);
        self.eat(TokenKind::Async);
        self.method_name();
        self.generic_parameters();
        self.parameter_list();
        self.expect(TokenKind::Colon);
        self.ty();
        self.throws_clause();
        if self.at(TokenKind::Where) {
            self.excluded_construct("where");
        }
        if signature_only || self.at(TokenKind::Semicolon) {
            self.expect(TokenKind::Semicolon);
        } else {
            self.block();
        }
        self.finish();
    }

    fn method_name_width(&self, offset: usize) -> Option<usize> {
        match self.nth(offset) {
            Some(TokenKind::Identifier | TokenKind::From) => Some(1),
            Some(TokenKind::LeftBracket)
                if self.nth(offset + 1) == Some(TokenKind::Identifier)
                    && self.nth(offset + 2) == Some(TokenKind::Dot)
                    && self.nth(offset + 3) == Some(TokenKind::Identifier)
                    && self.nth(offset + 4) == Some(TokenKind::RightBracket) =>
            {
                Some(5)
            }
            _ => None,
        }
    }

    fn method_name(&mut self) {
        if self.eat(TokenKind::LeftBracket) {
            self.expect(TokenKind::Identifier);
            self.expect(TokenKind::Dot);
            self.expect(TokenKind::Identifier);
            self.expect(TokenKind::RightBracket);
        } else if !self.eat(TokenKind::Identifier) {
            self.expect(TokenKind::From);
        }
    }

    fn visibility(&mut self) {
        if !self.eat(TokenKind::Public) && !self.eat(TokenKind::Protected) {
            self.eat(TokenKind::Private);
        }
    }

    fn parameter_list(&mut self) {
        self.start(SyntaxKind::PARAMETER_LIST);
        self.expect(TokenKind::LeftParen);
        if !self.at(TokenKind::RightParen) {
            loop {
                self.binding_pattern();
                self.expect(TokenKind::Colon);
                self.ty();
                if self.eat(TokenKind::Equal) {
                    self.expression();
                }
                if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightParen) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RightParen);
        self.finish();
    }

    fn generic_parameters(&mut self) {
        if !self.at(TokenKind::Less) {
            return;
        }
        self.start(SyntaxKind::GENERIC_PARAMETER_LIST);
        self.bump();
        if !self.at(TokenKind::Greater) {
            loop {
                self.eat(TokenKind::Lifetime);
                self.eat(TokenKind::Throws);
                self.expect(TokenKind::Identifier);
                if self.eat(TokenKind::Extends) {
                    self.type_path();
                    while self.eat(TokenKind::Amp) {
                        self.type_path();
                    }
                }
                if !self.eat(TokenKind::Comma) || self.at(TokenKind::Greater) {
                    break;
                }
            }
        }
        self.expect(TokenKind::Greater);
        self.finish();
    }

    fn generic_arguments(&mut self) {
        self.start(SyntaxKind::GENERIC_ARGUMENT_LIST);
        self.expect(TokenKind::Less);
        loop {
            if self.eat(TokenKind::Static) {
                // Explicit lifetime argument.
            } else if self.at(TokenKind::Scope) {
                self.obsolete_lifetime();
            } else {
                self.ty();
            }
            if !self.eat(TokenKind::Comma) || self.at(TokenKind::Greater) {
                break;
            }
        }
        self.expect(TokenKind::Greater);
        self.finish();
    }

    fn throws_clause(&mut self) {
        if self.eat(TokenKind::Throws) {
            self.type_path();
            while self.eat(TokenKind::Pipe) {
                self.type_path();
            }
        }
    }

    fn ty(&mut self) {
        self.start(SyntaxKind::TYPE);
        if !self.enter_recursion() {
            self.finish();
            return;
        }
        match self.current() {
            Some(TokenKind::Amp) => {
                self.bump();
                if self.at(TokenKind::Identifier)
                    && matches!(
                        self.nth(1),
                        Some(TokenKind::Identifier | TokenKind::LeftBracket | TokenKind::Mut)
                    )
                {
                    self.bump();
                }
                if !self.eat(TokenKind::Static) && self.at(TokenKind::Scope) {
                    self.obsolete_lifetime();
                }
                self.eat(TokenKind::Mut);
                self.ty();
            }
            Some(TokenKind::Star) => {
                self.bump();
                if !self.eat(TokenKind::Const) {
                    self.expect(TokenKind::Mut);
                }
                self.ty();
            }
            Some(TokenKind::LeftBracket) => {
                self.bump();
                self.ty();
                if self.eat(TokenKind::Semicolon) {
                    self.expression();
                }
                self.expect(TokenKind::RightBracket);
            }
            Some(TokenKind::LeftParen) => {
                self.bump();
                if !self.at(TokenKind::RightParen) {
                    self.type_sequence(TokenKind::RightParen);
                }
                self.expect(TokenKind::RightParen);
                if self.eat(TokenKind::FatArrow) {
                    self.ty();
                }
            }
            Some(TokenKind::Async) => {
                self.bump();
                self.expect(TokenKind::LeftParen);
                if !self.at(TokenKind::RightParen) {
                    self.type_sequence(TokenKind::RightParen);
                }
                self.expect(TokenKind::RightParen);
                self.expect(TokenKind::FatArrow);
                self.ty();
            }
            Some(TokenKind::Extern) => {
                self.bump();
                self.expect(TokenKind::StringLiteral);
                self.expect(TokenKind::Function);
                self.expect(TokenKind::LeftParen);
                if !self.at(TokenKind::RightParen) {
                    self.type_sequence(TokenKind::RightParen);
                }
                self.expect(TokenKind::RightParen);
                self.expect(TokenKind::Colon);
                self.ty();
            }
            Some(TokenKind::Dyn) => {
                self.reject_keyword("dyn");
            }
            Some(TokenKind::SelfValue) => self.reject_keyword("self"),
            Some(TokenKind::Null) => self.reject_keyword("null"),
            Some(TokenKind::Unknown) => self.bump(),
            Some(TokenKind::Identifier) => self.type_path(),
            _ => self.error_and_recover(
                "SYNTAX_EXPECTED_TYPE",
                "expected a type",
                &[
                    TokenKind::Comma,
                    TokenKind::RightParen,
                    TokenKind::RightBracket,
                    TokenKind::Semicolon,
                    TokenKind::Equal,
                    TokenKind::LeftBrace,
                    TokenKind::Throws,
                ],
            ),
        }
        if self.eat(TokenKind::Pipe) {
            self.expect(TokenKind::Undefined);
        }
        self.throws_clause();
        self.leave_recursion();
        self.finish();
    }

    fn type_path(&mut self) {
        if !self.eat(TokenKind::Identifier) {
            self.expect(TokenKind::Unknown);
        }
        while self.eat(TokenKind::Dot) {
            self.expect(TokenKind::Identifier);
        }
        if self.at(TokenKind::Less) {
            self.generic_arguments();
        }
    }

    fn type_sequence(&mut self, end: TokenKind) {
        loop {
            self.ty();
            if !self.eat(TokenKind::Comma) || self.at(end) {
                break;
            }
        }
    }

    fn block(&mut self) {
        self.start(SyntaxKind::BLOCK);
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            self.statement();
            if self.cursor == before {
                self.bump();
            }
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    #[allow(clippy::too_many_lines)]
    fn statement(&mut self) {
        self.start(SyntaxKind::STATEMENT);
        match self.current() {
            Some(TokenKind::LeftBrace) => self.block(),
            Some(TokenKind::Const | TokenKind::Let) => {
                self.bump();
                self.binding_pattern();
                if self.eat(TokenKind::Colon) {
                    self.ty();
                }
                self.expect(TokenKind::Equal);
                self.expression();
                self.expect(TokenKind::Semicolon);
            }
            Some(TokenKind::Return | TokenKind::Yield) => {
                self.bump();
                if !self.at(TokenKind::Semicolon) {
                    self.expression();
                }
                self.expect(TokenKind::Semicolon);
            }
            Some(TokenKind::Throw) => {
                self.bump();
                self.expression();
                self.expect(TokenKind::Semicolon);
            }
            Some(TokenKind::If) => {
                self.bump();
                self.parenthesized_expression();
                self.statement();
                if self.eat(TokenKind::Else) {
                    self.statement();
                }
            }
            Some(TokenKind::While) => {
                self.bump();
                self.parenthesized_expression();
                self.statement();
            }
            Some(TokenKind::For) => {
                self.bump();
                self.eat(TokenKind::Await);
                self.expect(TokenKind::LeftParen);
                if !self.eat(TokenKind::Const) {
                    self.expect(TokenKind::Let);
                }
                self.binding_pattern();
                self.expect(TokenKind::Of);
                self.expression();
                self.expect(TokenKind::RightParen);
                self.statement();
            }
            Some(TokenKind::Await) if self.nth(1) == Some(TokenKind::Using) => {
                self.bump();
                self.using_statement();
            }
            Some(TokenKind::Using) => self.using_statement(),
            Some(TokenKind::Switch) => {
                self.switch_expression();
                self.eat(TokenKind::Semicolon);
            }
            Some(TokenKind::Try) if self.nth(1) == Some(TokenKind::LeftBrace) => {
                self.bump();
                self.block();
                if !self.at(TokenKind::Catch) {
                    self.error_current(
                        "SYNTAX_EXPECTED_CATCH",
                        "expected a catch clause",
                        "a try statement requires at least one catch clause",
                    );
                }
                while self.at(TokenKind::Catch) {
                    self.start(SyntaxKind::CATCH_CLAUSE);
                    self.bump();
                    self.expect(TokenKind::LeftParen);
                    self.expect(TokenKind::Identifier);
                    self.expect(TokenKind::Colon);
                    self.type_path();
                    self.expect(TokenKind::RightParen);
                    self.block();
                    self.finish();
                }
            }
            Some(TokenKind::Unsafe) if self.nth(1) == Some(TokenKind::LeftBrace) => {
                self.bump();
                self.block();
            }
            Some(TokenKind::Break | TokenKind::Continue) => {
                self.bump();
                self.expect(TokenKind::Semicolon);
            }
            _ => {
                self.expression();
                self.expect(TokenKind::Semicolon);
            }
        }
        self.finish();
    }

    fn parenthesized_expression(&mut self) {
        self.expect(TokenKind::LeftParen);
        self.expression();
        self.expect(TokenKind::RightParen);
    }

    fn using_statement(&mut self) {
        self.expect(TokenKind::Using);
        self.expect(TokenKind::Identifier);
        self.expect(TokenKind::Equal);
        self.expression();
        self.expect(TokenKind::Semicolon);
    }

    fn expression(&mut self) {
        self.start(SyntaxKind::EXPRESSION);
        self.expression_bp(0);
        self.finish();
    }

    fn expression_bp(&mut self, minimum: u8) {
        if !self.enter_recursion() {
            return;
        }
        match self.current() {
            Some(
                TokenKind::Bang
                | TokenKind::Minus
                | TokenKind::Tilde
                | TokenKind::Move
                | TokenKind::Await
                | TokenKind::Star,
            ) => {
                self.bump();
                self.expression_bp(24);
            }
            Some(TokenKind::Amp) => {
                self.bump();
                self.eat(TokenKind::Mut);
                self.expression_bp(24);
            }
            Some(TokenKind::Try) => {
                self.bump();
                self.eat(TokenKind::Await);
                self.expression_bp(24);
            }
            _ => self.primary_expression(),
        }

        loop {
            if self.postfix_operation() {
                continue;
            }
            if self.at(TokenKind::Question) {
                if minimum > 2 {
                    break;
                }
                self.bump();
                self.expression_bp(0);
                self.expect(TokenKind::Colon);
                self.expression_bp(2);
                continue;
            }
            let Some((left, right)) = self.binary_binding_power() else {
                break;
            };
            if left < minimum {
                break;
            }
            self.bump();
            self.expression_bp(right);
        }
        self.leave_recursion();
    }

    fn primary_expression(&mut self) {
        if self.at(TokenKind::TemplateLiteral) {
            self.template_literal();
            return;
        }
        if self.jsx_enabled && self.looks_like_jsx_start() {
            self.jsx_element();
            return;
        }
        if self.at_literal()
            || self.at_any(&[
                TokenKind::Identifier,
                TokenKind::This,
                TokenKind::Super,
                TokenKind::Undefined,
            ])
        {
            self.bump();
            if self.at(TokenKind::LeftBrace) {
                self.object_literal();
            }
            return;
        }
        match self.current() {
            Some(TokenKind::New) => {
                self.bump();
                self.type_path();
                self.argument_list();
            }
            Some(TokenKind::SelfValue) => self.reject_keyword("self"),
            Some(TokenKind::Null) => self.reject_keyword("null"),
            Some(TokenKind::LeftBracket) => {
                self.bump();
                self.expression_sequence(TokenKind::RightBracket);
                self.expect(TokenKind::RightBracket);
            }
            Some(TokenKind::LeftBrace) => self.object_literal(),
            Some(TokenKind::LeftParen) => {
                let lambda_parameters = self.looks_like_parenthesized_lambda();
                self.bump();
                if lambda_parameters && !self.at(TokenKind::RightParen) {
                    loop {
                        self.expect(TokenKind::Identifier);
                        if self.eat(TokenKind::Colon) {
                            self.ty();
                        }
                        if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightParen) {
                            break;
                        }
                    }
                } else if !self.at(TokenKind::RightParen) {
                    self.expression_sequence(TokenKind::RightParen);
                }
                self.expect(TokenKind::RightParen);
                if self.eat(TokenKind::Colon) {
                    self.ty();
                }
                self.throws_clause();
                if self.eat(TokenKind::FatArrow) {
                    if self.at(TokenKind::LeftBrace) {
                        self.block();
                    } else {
                        self.expression_bp(0);
                    }
                }
            }
            Some(TokenKind::Switch) => self.switch_expression(),
            Some(TokenKind::Match) => self.reject_keyword("match"),
            _ => self.error_and_recover(
                "SYNTAX_EXPECTED_EXPRESSION",
                "expected an expression",
                &[
                    TokenKind::Comma,
                    TokenKind::Colon,
                    TokenKind::Semicolon,
                    TokenKind::RightParen,
                    TokenKind::RightBracket,
                    TokenKind::RightBrace,
                ],
            ),
        }
    }

    fn template_literal(&mut self) {
        let Some(range) = self.current_token_range() else {
            return;
        };
        let text = &self.source[range.clone()];
        let expressions = template_interpolations(text)
            .into_iter()
            .map(|expression| (range.start + expression.start)..(range.start + expression.end))
            .collect::<Vec<_>>();
        self.bump();
        for expression in expressions {
            self.diagnostics.extend(parse_expression_fragment(
                self.file,
                self.source,
                expression,
            ));
            if self.diagnostics.len() > 256 {
                self.diagnostics.truncate(256);
                break;
            }
        }
    }

    fn postfix_operation(&mut self) -> bool {
        match self.current() {
            Some(TokenKind::LeftParen) => self.argument_list(),
            Some(TokenKind::Dot | TokenKind::QuestionDot) => {
                self.bump();
                if !self.eat(TokenKind::Identifier) {
                    self.expect(TokenKind::From);
                }
            }
            Some(TokenKind::LeftBracket) => {
                self.bump();
                self.expression_bp(0);
                self.expect(TokenKind::RightBracket);
            }
            Some(TokenKind::As | TokenKind::AsQuestion) => {
                self.bump();
                self.ty();
            }
            Some(TokenKind::Bang) => self.bump(),
            Some(TokenKind::Less) if self.looks_like_generic_call() => {
                self.generic_arguments();
                self.argument_list();
            }
            _ => return false,
        }
        true
    }

    fn looks_like_generic_call(&self) -> bool {
        let mut depth = 0_u32;
        let mut offset = 0;
        while offset < 512
            && let Some(kind) = self.nth(offset)
        {
            match kind {
                TokenKind::Less => depth += 1,
                TokenKind::Greater => {
                    depth -= 1;
                    if depth == 0 {
                        return self.nth(offset + 1) == Some(TokenKind::LeftParen);
                    }
                }
                TokenKind::Semicolon | TokenKind::LeftBrace if depth == 0 => return false,
                _ => {}
            }
            offset += 1;
        }
        false
    }

    fn looks_like_parenthesized_lambda(&self) -> bool {
        let mut depth = 0_u32;
        let mut offset = 0_usize;
        while offset < 512
            && let Some(kind) = self.nth(offset)
        {
            match kind {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        offset += 1;
                        break;
                    }
                }
                _ => {}
            }
            offset += 1;
        }
        if offset >= 512 {
            return false;
        }
        while let Some(kind) = self.nth(offset) {
            match kind {
                TokenKind::FatArrow => return true,
                TokenKind::Colon
                | TokenKind::Throws
                | TokenKind::Identifier
                | TokenKind::Comma
                | TokenKind::Less
                | TokenKind::Greater
                | TokenKind::Pipe
                | TokenKind::Amp
                | TokenKind::Static => offset += 1,
                _ => return false,
            }
        }
        false
    }

    fn argument_list(&mut self) {
        self.expect(TokenKind::LeftParen);
        self.expression_sequence(TokenKind::RightParen);
        self.expect(TokenKind::RightParen);
    }

    fn expression_sequence(&mut self, end: TokenKind) {
        if self.at(end) {
            return;
        }
        loop {
            self.expression_bp(0);
            if !self.eat(TokenKind::Comma) || self.at(end) {
                break;
            }
        }
    }

    fn object_literal(&mut self) {
        self.expect(TokenKind::LeftBrace);
        if !self.at(TokenKind::RightBrace) {
            loop {
                self.expect(TokenKind::Identifier);
                if self.eat(TokenKind::Colon) {
                    self.expression_bp(0);
                }
                if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightBrace) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RightBrace);
    }

    fn jsx_element(&mut self) {
        self.start(SyntaxKind::JSX_ELEMENT);
        if self.nth(1) == Some(TokenKind::Greater) {
            self.jsx_fragment();
        } else {
            self.jsx_named_element();
        }
        self.finish();
    }

    fn jsx_named_element(&mut self) {
        self.start(SyntaxKind::JSX_OPENING_ELEMENT);
        self.expect(TokenKind::Less);
        let opening_name = self.jsx_name();
        while self.current().is_some()
            && !matches!(self.current(), Some(TokenKind::Greater | TokenKind::Slash))
        {
            if self.at(TokenKind::LeftBrace) && self.nth(1) == Some(TokenKind::Ellipsis) {
                self.jsx_spread_attribute();
            } else if self.at_any(&[TokenKind::Identifier, TokenKind::From]) {
                self.jsx_attribute();
            } else {
                self.error_and_recover(
                    "SYNTAX_JSX_EXPECTED_ATTRIBUTE",
                    "expected a JSX attribute",
                    &[TokenKind::Greater, TokenKind::Slash],
                );
            }
        }
        let self_closing = self.eat(TokenKind::Slash);
        self.expect(TokenKind::Greater);
        self.finish();
        if self_closing {
            return;
        }

        while self.current().is_some() {
            let before = self.cursor;
            if self.at(TokenKind::Less) && self.nth(1) == Some(TokenKind::Slash) {
                break;
            }
            if self.at(TokenKind::LeftBrace) {
                self.jsx_expression_container();
            } else if self.jsx_child_starts_element() {
                self.jsx_element();
            } else {
                self.jsx_text();
            }
            self.ensure_progress(before);
        }

        self.start(SyntaxKind::JSX_CLOSING_ELEMENT);
        let closing_start = self.current_range().start;
        self.expect(TokenKind::Less);
        self.expect(TokenKind::Slash);
        let closing_name = self.jsx_name();
        if let (Some(opening_name), Some(closing_name)) = (opening_name, closing_name)
            && opening_name != closing_name
        {
            self.error_at(
                "SYNTAX_JSX_TAG_MISMATCH",
                "JSX closing tag does not match its opening tag",
                closing_start..self.current_range().start,
                "close the element with the same component name",
            );
        }
        self.expect(TokenKind::Greater);
        self.finish();
    }

    fn jsx_fragment(&mut self) {
        self.start(SyntaxKind::JSX_FRAGMENT);
        self.expect(TokenKind::Less);
        self.expect(TokenKind::Greater);
        while self.current().is_some() {
            let before = self.cursor;
            if self.at(TokenKind::Less) && self.nth(1) == Some(TokenKind::Slash) {
                break;
            }
            if self.at(TokenKind::LeftBrace) {
                self.jsx_expression_container();
            } else if self.jsx_child_starts_element() {
                self.jsx_element();
            } else {
                self.jsx_text();
            }
            self.ensure_progress(before);
        }
        self.start(SyntaxKind::JSX_CLOSING_ELEMENT);
        self.expect(TokenKind::Less);
        self.expect(TokenKind::Slash);
        self.expect(TokenKind::Greater);
        self.finish();
        self.finish();
    }

    fn jsx_attribute(&mut self) {
        self.start(SyntaxKind::JSX_ATTRIBUTE);
        self.jsx_name();
        if self.eat(TokenKind::Equal) {
            match self.current() {
                Some(TokenKind::StringLiteral) => {
                    self.bump();
                }
                Some(TokenKind::LeftBrace) => self.jsx_expression_container(),
                _ => self.error_current(
                    "SYNTAX_JSX_EXPECTED_ATTRIBUTE_VALUE",
                    "expected a JSX attribute value",
                    "use a quoted string or a braced expression",
                ),
            }
        }
        self.finish();
    }

    fn jsx_spread_attribute(&mut self) {
        self.start(SyntaxKind::JSX_SPREAD_ATTRIBUTE);
        self.expect(TokenKind::LeftBrace);
        self.expect(TokenKind::Ellipsis);
        self.expression();
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn jsx_expression_container(&mut self) {
        self.start(SyntaxKind::JSX_EXPRESSION_CONTAINER);
        self.expect(TokenKind::LeftBrace);
        if !self.at(TokenKind::RightBrace) {
            self.expression();
        }
        self.expect(TokenKind::RightBrace);
        self.finish();
    }

    fn jsx_text(&mut self) {
        self.start(SyntaxKind::JSX_TEXT);
        let before = self.cursor;
        while self.current().is_some()
            && !self.at(TokenKind::LeftBrace)
            && !self.jsx_child_starts_element()
            && !(self.at(TokenKind::Less) && self.nth(1) == Some(TokenKind::Slash))
        {
            self.bump();
        }
        if self.cursor == before {
            self.error_current(
                "SYNTAX_JSX_EXPECTED_CHILD",
                "expected a JSX child",
                "use text, a braced expression, or a nested element",
            );
            self.bump();
        }
        self.finish();
    }

    fn jsx_name(&mut self) -> Option<String> {
        self.start(SyntaxKind::JSX_NAME);
        let mut name = String::new();
        let mut valid = false;
        if self.at_any(&[TokenKind::Identifier, TokenKind::From]) {
            valid = true;
            name.push_str(self.current_text().unwrap_or_default());
            self.bump();
            loop {
                let separator = if self.eat(TokenKind::Dot) {
                    Some('.')
                } else if self.eat(TokenKind::Minus) {
                    Some('-')
                } else {
                    None
                };
                let Some(separator) = separator else {
                    break;
                };
                name.push(separator);
                if self.at_any(&[TokenKind::Identifier, TokenKind::From]) {
                    name.push_str(self.current_text().unwrap_or_default());
                    self.bump();
                } else {
                    self.error_current(
                        "SYNTAX_JSX_EXPECTED_NAME",
                        "expected a name after the JSX name separator",
                        "complete the component or host element name",
                    );
                    break;
                }
            }
        } else {
            self.error_current(
                "SYNTAX_JSX_EXPECTED_NAME",
                "expected a JSX element name",
                "use an identifier or a component member expression",
            );
        }
        self.finish();
        valid.then_some(name)
    }

    fn looks_like_jsx_start(&self) -> bool {
        self.at(TokenKind::Less)
            && !self.looks_like_generic_call()
            && matches!(
                self.nth(1),
                Some(TokenKind::Identifier | TokenKind::From | TokenKind::Greater)
            )
    }

    fn jsx_child_starts_element(&self) -> bool {
        self.looks_like_jsx_start() && self.nth(1) != Some(TokenKind::Greater)
    }

    fn switch_expression(&mut self) {
        self.expect(TokenKind::Switch);
        if self.at(TokenKind::LeftParen) {
            self.parenthesized_expression();
        } else {
            self.expression_bp(0);
        }
        self.expect(TokenKind::LeftBrace);
        while self.current().is_some() && !self.at(TokenKind::RightBrace) {
            let before = self.cursor;
            self.start(SyntaxKind::SWITCH_ARM);
            if self.eat(TokenKind::Case) {
                self.pattern();
                if self.eat(TokenKind::If) {
                    self.expression();
                }
            } else {
                self.expect(TokenKind::Default);
            }
            self.expect(TokenKind::Colon);
            if self.at(TokenKind::LeftBrace) {
                self.block();
            } else {
                self.expression();
                self.eat(TokenKind::Comma);
            }
            self.finish();
            self.ensure_progress(before);
        }
        self.expect(TokenKind::RightBrace);
    }

    fn pattern(&mut self) {
        self.start(SyntaxKind::PATTERN);
        if !self.enter_recursion() {
            self.finish();
            return;
        }
        if self.at_literal() || self.at(TokenKind::Undefined) {
            self.bump();
        } else if self.eat(TokenKind::Identifier) {
            while self.eat(TokenKind::Dot) {
                self.expect(TokenKind::Identifier);
            }
            if self.eat(TokenKind::LeftParen) {
                if !self.at(TokenKind::RightParen) {
                    loop {
                        self.pattern();
                        if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightParen) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RightParen);
            } else if self.eat(TokenKind::LeftBrace) {
                if !self.at(TokenKind::RightBrace) {
                    loop {
                        self.expect(TokenKind::Identifier);
                        if self.eat(TokenKind::Colon) {
                            self.pattern();
                        }
                        if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightBrace) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RightBrace);
            }
        } else {
            self.error_current(
                "SYNTAX_EXPECTED_PATTERN",
                "expected a pattern",
                "a match arm pattern must begin here",
            );
        }
        self.leave_recursion();
        self.finish();
    }

    fn binding_pattern(&mut self) {
        self.start(SyntaxKind::BINDING_PATTERN);
        if self.eat(TokenKind::Mut) {
            self.binding_pattern();
        } else if self.eat(TokenKind::LeftBracket) {
            if !self.at(TokenKind::RightBracket) {
                loop {
                    if self.eat(TokenKind::Ellipsis) {
                        self.binding_pattern();
                        if self.eat(TokenKind::Comma) && !self.at(TokenKind::RightBracket) {
                            self.error_current(
                                "SYNTAX_BINDING_REST_NOT_LAST",
                                "rest binding must be last",
                                "move the rest binding to the end of the pattern",
                            );
                        }
                        break;
                    }
                    if !self.at(TokenKind::Comma) {
                        self.binding_pattern();
                        if self.eat(TokenKind::Equal) {
                            self.expression();
                        }
                    }
                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightBracket) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RightBracket);
        } else if self.eat(TokenKind::LeftBrace) {
            if !self.at(TokenKind::RightBrace) {
                loop {
                    if self.eat(TokenKind::Ellipsis) {
                        self.binding_pattern();
                        if self.eat(TokenKind::Comma) && !self.at(TokenKind::RightBrace) {
                            self.error_current(
                                "SYNTAX_BINDING_REST_NOT_LAST",
                                "rest binding must be last",
                                "move the rest binding to the end of the pattern",
                            );
                        }
                        break;
                    }
                    self.start(SyntaxKind::BINDING_PROPERTY);
                    self.expect(TokenKind::Identifier);
                    if self.eat(TokenKind::Colon) {
                        self.binding_pattern();
                        if self.eat(TokenKind::Equal) {
                            self.expression();
                        }
                    } else if self.eat(TokenKind::Equal) {
                        self.expression();
                    }
                    self.finish();
                    if !self.eat(TokenKind::Comma) || self.at(TokenKind::RightBrace) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RightBrace);
        } else if self.eat(TokenKind::Identifier) {
        } else {
            self.error_current(
                "SYNTAX_EXPECTED_BINDING_PATTERN",
                "expected a binding pattern",
                "use an identifier, array pattern, or object pattern",
            );
        }
        self.finish();
    }

    fn binary_binding_power(&self) -> Option<(u8, u8)> {
        let power = match self.current()? {
            TokenKind::Equal
            | TokenKind::PlusEqual
            | TokenKind::MinusEqual
            | TokenKind::StarEqual
            | TokenKind::SlashEqual
            | TokenKind::PercentEqual
            | TokenKind::AmpEqual
            | TokenKind::PipeEqual
            | TokenKind::CaretEqual
            | TokenKind::ShiftLeftEqual
            | TokenKind::ShiftRightEqual => (1, 1),
            TokenKind::QuestionQuestion => (3, 4),
            TokenKind::PipePipe => (5, 6),
            TokenKind::AmpAmp => (7, 8),
            TokenKind::Pipe => (9, 10),
            TokenKind::Caret => (11, 12),
            TokenKind::Amp => (13, 14),
            TokenKind::EqualEqualEqual | TokenKind::BangEqualEqual => (15, 16),
            TokenKind::Less
            | TokenKind::LessEqual
            | TokenKind::Greater
            | TokenKind::GreaterEqual
            | TokenKind::InstanceOf => (17, 18),
            TokenKind::ShiftLeft | TokenKind::ShiftRight => (19, 20),
            TokenKind::Plus | TokenKind::Minus => (21, 22),
            TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (23, 24),
            _ => return None,
        };
        Some(power)
    }

    fn at_literal(&self) -> bool {
        self.at_any(&[
            TokenKind::IntegerLiteral,
            TokenKind::FloatLiteral,
            TokenKind::StringLiteral,
            TokenKind::CharacterLiteral,
            TokenKind::TemplateLiteral,
            TokenKind::True,
            TokenKind::False,
        ])
    }

    fn delimited_sequence(&mut self, end: TokenKind, parse_one: impl Fn(&mut Self)) {
        if !self.at(end) {
            loop {
                parse_one(self);
                if !self.eat(TokenKind::Comma) || self.at(end) {
                    break;
                }
            }
        }
        self.expect(end);
    }

    fn start(&mut self, kind: SyntaxKind) {
        self.builder.start_node(TnLanguage::kind_to_raw(kind));
    }

    fn finish(&mut self) {
        self.builder.finish_node();
    }

    fn current(&self) -> Option<TokenKind> {
        self.nth(0)
    }

    fn nth(&self, offset: usize) -> Option<TokenKind> {
        self.tokens[self.cursor..]
            .iter()
            .filter(|token| !token.kind.is_trivia())
            .nth(offset)
            .map(|token| token.kind)
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.current() == Some(kind)
    }

    fn at_any(&self, kinds: &[TokenKind]) -> bool {
        self.current().is_some_and(|kind| kinds.contains(&kind))
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) {
        if !self.eat(kind) {
            self.error_current(
                "SYNTAX_EXPECTED_TOKEN",
                &format!("expected {kind:?}"),
                "required token is missing here",
            );
        }
    }

    fn bump(&mut self) {
        self.bump_trivia();
        if let Some(token) = self.tokens.get(self.cursor) {
            self.emit_token(token);
            self.cursor += 1;
        }
    }

    fn bump_trivia(&mut self) {
        while self
            .tokens
            .get(self.cursor)
            .is_some_and(|token| token.kind.is_trivia())
        {
            let token = &self.tokens[self.cursor];
            self.emit_token(token);
            self.cursor += 1;
        }
    }

    fn emit_token(&mut self, token: &Token) {
        self.builder.token(
            TnLanguage::kind_to_raw(SyntaxKind(token.kind as u16)),
            &self.source[token.range.clone()],
        );
    }

    fn error_current(&mut self, id: &str, message: &str, label: &str) {
        self.start(SyntaxKind::ERROR);
        self.finish();
        if self.diagnostics.len() >= 256 {
            return;
        }
        let range = self.current_range();
        self.diagnostics.push(Diagnostic::error(
            ConditionId::new(id).expect("static condition identifier is valid"),
            message,
            Label {
                span: SourceSpan::new(self.file, range, self.source),
                message: label.into(),
            },
            id.to_ascii_lowercase().replace('_', "/"),
        ));
    }

    fn error_at(&mut self, id: &str, message: &str, range: Range<usize>, label: &str) {
        self.start(SyntaxKind::ERROR);
        self.finish();
        if self.diagnostics.len() >= 256 {
            return;
        }
        self.diagnostics.push(Diagnostic::error(
            ConditionId::new(id).expect("static condition identifier is valid"),
            message,
            Label {
                span: SourceSpan::new(self.file, range, self.source),
                message: label.into(),
            },
            id.to_ascii_lowercase().replace('_', "/"),
        ));
    }

    fn obsolete_extern_block_diagnostic(&mut self) {
        self.start(SyntaxKind::ERROR);
        self.finish();
        if self.diagnostics.len() >= 256 {
            return;
        }
        let range = self.current_range();
        let span = SourceSpan::new(self.file, range.clone(), self.source);
        let mut diagnostic = Diagnostic::error(
            ConditionId::new("SYNTAX_OBSOLETE_EXTERN_BLOCK")
                .expect("static condition identifier is valid"),
            "foreign declaration blocks require `declare extern`",
            Label {
                span,
                message: "insert `declare ` before `extern`".into(),
            },
            "syntax/obsolete-extern-block",
        );
        diagnostic.edits.push(Edit {
            span: SourceSpan::new(self.file, range.start..range.start, self.source),
            replacement: "declare ".into(),
            applicability: Applicability::MachineApplicable,
        });
        self.diagnostics.push(diagnostic);
    }

    fn obsolete_lifetime(&mut self) {
        self.error_current(
            "SYNTAX_OBSOLETE_LIFETIME",
            "`scope` is not a public lifetime category",
            "use lifetime elision or a named `lifetime` parameter",
        );
        self.bump();
    }

    fn error_and_recover(&mut self, id: &str, message: &str, recovery: &[TokenKind]) {
        self.error_current(id, message, "unexpected syntax begins here");
        self.start(SyntaxKind::ERROR);
        while self.current().is_some_and(|kind| !recovery.contains(&kind)) {
            self.bump();
        }
        self.finish();
    }

    fn current_range(&self) -> Range<usize> {
        self.current_token_range()
            .unwrap_or(self.eof_offset..self.eof_offset)
    }

    fn current_token_range(&self) -> Option<Range<usize>> {
        self.tokens[self.cursor..]
            .iter()
            .find(|token| !token.kind.is_trivia())
            .map(|token| token.range.clone())
    }

    fn ensure_progress(&mut self, previous_cursor: usize) {
        if self.cursor == previous_cursor && self.current().is_some() {
            self.start(SyntaxKind::ERROR);
            self.bump();
            self.finish();
        }
    }

    fn enter_recursion(&mut self) -> bool {
        if self.recursion_depth >= 256 {
            self.error_current(
                "SYNTAX_RECURSION_LIMIT",
                "syntax nesting limit exceeded",
                "split this deeply nested construct into smaller expressions",
            );
            if self.current().is_some() {
                self.bump();
            }
            false
        } else {
            self.recursion_depth += 1;
            true
        }
    }

    fn leave_recursion(&mut self) {
        self.recursion_depth = self.recursion_depth.saturating_sub(1);
    }
}

fn parse_expression_fragment(file: &str, source: &str, range: Range<usize>) -> Vec<Diagnostic> {
    let eof_offset = range.end;
    let lexed = lex_range(file, source, range);
    let mut parser = Parser {
        file,
        source,
        tokens: &lexed.tokens,
        cursor: 0,
        eof_offset,
        recursion_depth: 0,
        jsx_enabled: file.ends_with(".tnx"),
        builder: GreenNodeBuilder::new(),
        diagnostics: lexed.diagnostics,
    };
    parser.start(SyntaxKind::SOURCE_FILE);
    parser.expression();
    if parser.current().is_some() {
        parser.error_and_recover(
            "SYNTAX_UNEXPECTED_TEMPLATE_EXPRESSION_TOKEN",
            "unexpected token in template interpolation",
            &[],
        );
    }
    parser.bump_trivia();
    parser.finish();
    let _ = parser.builder.finish();
    parser.diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_parses(source: &str) {
        let parsed = parse("test.tn", source.as_bytes());
        assert!(
            parsed.is_success(),
            "diagnostics: {:#?}",
            parsed.diagnostics()
        );
        assert_eq!(parsed.syntax().to_string(), source);
    }

    #[test]
    fn parses_representative_declarations_losslessly() {
        assert_parses(
            r#"import { run } from "std/async";
@logged
export struct Point<T extends Display> {
  public x: T;
}
function main(): void {
  const value: i32 = 1i32 + 2i32;
  console.log(value);
}
"#,
        );
    }

    #[test]
    fn parses_tnx_jsx_as_dedicated_lossless_nodes() {
        let source = r#"function App(): Element {
  return (
    <View gap={12} enabled>
      <Text>Hello</Text>
      <Button onPress={() => save()}>Save</Button>
      {items}
    </View>
  );
}
"#;
        let parsed = parse("test.tnx", source.as_bytes());
        assert!(
            parsed.is_success(),
            "diagnostics: {:#?}",
            parsed.diagnostics()
        );
        assert_eq!(parsed.syntax().to_string(), source);
        assert!(
            parsed
                .syntax()
                .descendants()
                .any(|node| node.kind() == SyntaxKind::JSX_ELEMENT)
        );
        assert!(
            parsed
                .syntax()
                .descendants()
                .any(|node| node.kind() == SyntaxKind::JSX_EXPRESSION_CONTAINER)
        );
    }

    #[test]
    fn parses_tnx_fragments_and_reports_mismatched_tags() {
        let source = "function App(): Element { return <><Text>Hello</Text></>; }\n";
        let parsed = parse("test.tnx", source.as_bytes());
        assert_eq!(parsed.syntax().to_string(), source);
        assert!(
            parsed
                .syntax()
                .descendants()
                .any(|node| node.kind() == SyntaxKind::JSX_FRAGMENT)
        );

        let mismatched = parse(
            "test.tnx",
            b"function App(): Element { return <View></Text>; }\n",
        );
        assert!(
            mismatched
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.condition.as_str() == "SYNTAX_JSX_TAG_MISMATCH")
        );
    }

    #[test]
    fn keeps_tnx_generic_calls_and_comparisons_out_of_jsx_parsing() {
        let source = r#"
function identity<T>(value: T): T { return value; }
function main(value: i32): bool {
  const result = identity<i32>(value);
  return result < 10;
}
"#;
        let parsed = parse("test.tnx", source.as_bytes());
        assert!(
            parsed.is_success(),
            "diagnostics: {:#?}",
            parsed.diagnostics()
        );
        assert!(
            !parsed
                .syntax()
                .descendants()
                .any(|node| node.kind() == SyntaxKind::JSX_ELEMENT)
        );
    }

    #[test]
    fn parses_named_lifetimes_on_slice_references() {
        assert_parses(
            "struct View<lifetime a> { public bytes: &a [u8]; }\n\
             function retain<lifetime a>(bytes: &a [u8]): View<a> { return { bytes: bytes }; }\n",
        );
    }

    #[test]
    fn parses_symbol_named_disposal_methods_losslessly() {
        assert_parses(
            "interface Disposable { [Symbol.dispose](): void; }\n\
             interface AsyncDisposable { async [Symbol.asyncDispose](): Promise<void, never>; }\n\
             class Resource implements Disposable { public [Symbol.dispose](): void {} }\n",
        );
    }

    #[test]
    fn rejects_public_scope_lifetimes_on_references_and_nominals() {
        let parsed = parse(
            "scope.tn",
            b"struct View<lifetime a> { public value: &a i32; }\n\
              function inspect(value: &scope i32, view: View<scope>): void { value; view; }\n",
        );
        assert_eq!(
            parsed
                .diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.condition.as_str() == "SYNTAX_OBSOLETE_LIFETIME")
                .count(),
            2
        );
    }

    #[test]
    fn parses_canonical_foreign_declaration_blocks_and_function_pointer_types() {
        assert_parses(
            "declare extern \"C\" {\n  function puts(text: * mut u8): void;\n}\n\
             type Callback = extern \"C\" function(i32): void;\n",
        );
    }

    #[test]
    fn parses_canonical_foreign_layouts_and_c_exports() {
        assert_parses(
            "extern struct Pair { left: i32; right: i32; }\n\
             enum Kind: u8 { Zero, Answer = 42, }\n\
             export extern \"C\" function add(left: i32, right: i32): i32 { return left + right; }\n",
        );
    }

    #[test]
    fn rejects_obsolete_foreign_declaration_blocks_with_an_insertion_fix() {
        let source = "extern \"C\" { function puts(text: * mut u8): void; }\n";
        let parsed = parse("obsolete-extern.tn", source.as_bytes());
        let diagnostic = parsed
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.condition.as_str() == "SYNTAX_OBSOLETE_EXTERN_BLOCK")
            .expect("obsolete syntax diagnostic");
        assert_eq!(
            diagnostic.message,
            "foreign declaration blocks require `declare extern`"
        );
        assert_eq!(
            (
                diagnostic.primary.span.byte_start,
                diagnostic.primary.span.byte_end
            ),
            (0, 6)
        );
        assert_eq!(diagnostic.edits.len(), 1);
        assert_eq!(diagnostic.edits[0].replacement, "declare ");
        assert_eq!(
            diagnostic.edits[0].applicability,
            Applicability::MachineApplicable
        );
        assert_eq!(
            (
                diagnostic.edits[0].span.byte_start,
                diagnostic.edits[0].span.byte_end
            ),
            (0, 0)
        );
        assert_eq!(parsed.syntax().to_string(), source);
    }

    #[test]
    fn rejects_malformed_foreign_declaration_members_and_headers() {
        for source in [
            "declare function missingExtern(): void;\n",
            "declare extern { function missingAbi(): void; }\n",
            "declare extern \"C\" { function hasBody(): void {} }\n",
            "declare extern \"C\" { async function asynchronous(): void; }\n",
        ] {
            let parsed = parse("malformed-foreign.tn", source.as_bytes());
            assert!(!parsed.is_success(), "source unexpectedly parsed: {source}");
            assert!(!parsed.diagnostics().is_empty());
            assert_eq!(parsed.syntax().to_string(), source);
        }
    }

    #[test]
    fn distinguishes_tuple_literals_from_parenthesized_lambdas() {
        assert_parses(
            "function tuple(value: i32): (i32, bool) { return (value, true); }\n\
             function closure(): (i32, bool) => i32 { return (value: i32, flag: bool): i32 => value; }\n",
        );
    }

    #[test]
    fn parses_sync_and_async_generator_syntax() {
        assert_parses(
            "function* numbers(): Iterable<i32> { yield 1i32; }\n\
             async function* events(): AsyncIterable<i32> { yield 2i32; }\n\
             async function consume(values: AsyncIterable<i32>): Promise<void, never> { for await (const value of values) { value; } }\n",
        );
    }

    #[test]
    fn recovers_after_missing_semicolon() {
        let parsed = parse(
            "recovery.tn",
            b"const first = 1 const second = 2; function main(): void {}",
        );
        assert!(!parsed.is_success());
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.condition.as_str() == "SYNTAX_EXPECTED_TOKEN")
        );
        assert!(parsed.syntax().to_string().contains("function main"));
    }

    #[test]
    fn parses_each_template_interpolation_as_a_normal_expression() {
        assert_parses(
            r"const message = `value=${call({ nested: `inner=${item}` })}`;
",
        );

        let source = "const message = `value=${item + }`;";
        let parsed = parse("template.tn", source.as_bytes());
        let diagnostic = parsed
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.condition.as_str() == "SYNTAX_EXPECTED_EXPRESSION")
            .expect("malformed interpolation must be diagnosed");
        let closing = source.find('}').expect("interpolation closing brace");
        assert_eq!(
            diagnostic.primary.span.byte_start,
            u32::try_from(closing).expect("fixture offset fits u32")
        );
    }

    #[test]
    fn deeply_nested_input_hits_a_bounded_diagnostic_instead_of_the_stack() {
        let nested = format!(
            "function main(): void {{ const value = {}0i32{}; }}",
            "(".repeat(10_000),
            ")".repeat(10_000)
        );
        let parsed = parse("nested.tn", nested.as_bytes());
        assert!(
            parsed
                .diagnostics()
                .iter()
                .any(|diagnostic| { diagnostic.condition.as_str() == "SYNTAX_RECURSION_LIMIT" })
        );
        assert!(parsed.diagnostics().len() <= 256);
        assert_eq!(parsed.syntax().to_string(), nested);
    }
}
