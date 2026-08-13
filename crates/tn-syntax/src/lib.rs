//! Lossless `TypeNative` lexical and concrete syntax infrastructure.

pub mod ast;
mod formatter;
mod incremental;
mod lexer;
mod parser;

pub use formatter::{FormatResult, format};
pub use incremental::{IncrementalDocument, ReparseStats, TextEdit};
pub use lexer::{Lexed, Token, TokenKind, lex, template_interpolation_ranges};
pub use parser::{Parse, SyntaxKind, SyntaxNode, TnLanguage, parse};
