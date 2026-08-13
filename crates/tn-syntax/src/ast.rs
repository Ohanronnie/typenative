//! Checked typed views over the lossless concrete syntax tree.

use crate::{SyntaxKind, SyntaxNode, TnLanguage};
use rowan::ast::AstNode;

macro_rules! ast_node {
    ($name:ident, $kind:expr) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(SyntaxNode);

        impl AstNode for $name {
            type Language = TnLanguage;

            fn can_cast(kind: SyntaxKind) -> bool {
                kind == $kind
            }

            fn cast(node: SyntaxNode) -> Option<Self> {
                Self::can_cast(node.kind()).then_some(Self(node))
            }

            fn syntax(&self) -> &SyntaxNode {
                &self.0
            }
        }
    };
}

ast_node!(SourceFile, SyntaxKind::SOURCE_FILE);
ast_node!(Attribute, SyntaxKind::ATTRIBUTE);
ast_node!(ImportDeclaration, SyntaxKind::IMPORT_DECLARATION);
ast_node!(ConstDeclaration, SyntaxKind::CONST_DECLARATION);
ast_node!(StaticDeclaration, SyntaxKind::STATIC_DECLARATION);
ast_node!(TypeAliasDeclaration, SyntaxKind::TYPE_ALIAS_DECLARATION);
ast_node!(FunctionDeclaration, SyntaxKind::FUNCTION_DECLARATION);
ast_node!(StructDeclaration, SyntaxKind::STRUCT_DECLARATION);
ast_node!(ClassDeclaration, SyntaxKind::CLASS_DECLARATION);
ast_node!(InterfaceDeclaration, SyntaxKind::INTERFACE_DECLARATION);
ast_node!(EnumDeclaration, SyntaxKind::ENUM_DECLARATION);
ast_node!(ImplDeclaration, SyntaxKind::IMPL_DECLARATION);
ast_node!(ExternBlock, SyntaxKind::EXTERN_BLOCK);
ast_node!(MacroDeclaration, SyntaxKind::MACRO_DECLARATION);
ast_node!(FieldDeclaration, SyntaxKind::FIELD_DECLARATION);
ast_node!(MethodDeclaration, SyntaxKind::METHOD_DECLARATION);
ast_node!(ConstructorDeclaration, SyntaxKind::CONSTRUCTOR_DECLARATION);
ast_node!(ParameterList, SyntaxKind::PARAMETER_LIST);
ast_node!(GenericParameterList, SyntaxKind::GENERIC_PARAMETER_LIST);
ast_node!(GenericArgumentList, SyntaxKind::GENERIC_ARGUMENT_LIST);
ast_node!(WhereClause, SyntaxKind::WHERE_CLAUSE);
ast_node!(Type, SyntaxKind::TYPE);
ast_node!(Block, SyntaxKind::BLOCK);
ast_node!(Statement, SyntaxKind::STATEMENT);
ast_node!(Expression, SyntaxKind::EXPRESSION);
ast_node!(Pattern, SyntaxKind::PATTERN);
ast_node!(MatchArm, SyntaxKind::MATCH_ARM);
ast_node!(CatchClause, SyntaxKind::CATCH_CLAUSE);
ast_node!(EnumVariant, SyntaxKind::ENUM_VARIANT);

impl SourceFile {
    pub fn items(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax().children().filter(|node| {
            matches!(
                node.kind(),
                SyntaxKind::IMPORT_DECLARATION
                    | SyntaxKind::CONST_DECLARATION
                    | SyntaxKind::STATIC_DECLARATION
                    | SyntaxKind::TYPE_ALIAS_DECLARATION
                    | SyntaxKind::FUNCTION_DECLARATION
                    | SyntaxKind::STRUCT_DECLARATION
                    | SyntaxKind::CLASS_DECLARATION
                    | SyntaxKind::INTERFACE_DECLARATION
                    | SyntaxKind::ENUM_DECLARATION
                    | SyntaxKind::IMPL_DECLARATION
                    | SyntaxKind::EXTERN_BLOCK
                    | SyntaxKind::MACRO_DECLARATION
            )
        })
    }
}

impl FunctionDeclaration {
    pub fn parameters(&self) -> Option<ParameterList> {
        self.syntax().children().find_map(ParameterList::cast)
    }

    pub fn body(&self) -> Option<Block> {
        self.syntax().children().find_map(Block::cast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_expose_only_matching_complete_nodes() {
        let parsed = crate::parse("test.tn", b"function main(): void {}");
        let file = SourceFile::cast(parsed.syntax()).expect("root is a source file");
        let function = file
            .items()
            .find_map(FunctionDeclaration::cast)
            .expect("function declaration exists");
        assert!(function.parameters().is_some());
        assert!(function.body().is_some());
        assert!(StructDeclaration::cast(function.syntax().clone()).is_none());
    }
}
