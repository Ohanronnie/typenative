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
ast_node!(BindingPattern, SyntaxKind::BINDING_PATTERN);
ast_node!(BindingProperty, SyntaxKind::BINDING_PROPERTY);
ast_node!(JsxElement, SyntaxKind::JSX_ELEMENT);
ast_node!(JsxFragment, SyntaxKind::JSX_FRAGMENT);
ast_node!(JsxOpeningElement, SyntaxKind::JSX_OPENING_ELEMENT);
ast_node!(JsxClosingElement, SyntaxKind::JSX_CLOSING_ELEMENT);
ast_node!(JsxName, SyntaxKind::JSX_NAME);
ast_node!(JsxAttribute, SyntaxKind::JSX_ATTRIBUTE);
ast_node!(JsxSpreadAttribute, SyntaxKind::JSX_SPREAD_ATTRIBUTE);
ast_node!(JsxExpressionContainer, SyntaxKind::JSX_EXPRESSION_CONTAINER);
ast_node!(JsxText, SyntaxKind::JSX_TEXT);

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

impl BindingPattern {
    pub fn nested_patterns(&self) -> impl Iterator<Item = BindingPattern> + '_ {
        self.syntax().children().filter_map(BindingPattern::cast)
    }

    pub fn properties(&self) -> impl Iterator<Item = BindingProperty> + '_ {
        self.syntax().children().filter_map(BindingProperty::cast)
    }
}

impl BindingProperty {
    pub fn pattern(&self) -> Option<BindingPattern> {
        self.syntax().children().find_map(BindingPattern::cast)
    }
}

impl JsxElement {
    pub fn opening_element(&self) -> Option<JsxOpeningElement> {
        self.syntax().children().find_map(JsxOpeningElement::cast)
    }

    pub fn closing_element(&self) -> Option<JsxClosingElement> {
        self.syntax().children().find_map(JsxClosingElement::cast)
    }

    pub fn children(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax().children().filter(|node| {
            matches!(
                node.kind(),
                SyntaxKind::JSX_ELEMENT
                    | SyntaxKind::JSX_EXPRESSION_CONTAINER
                    | SyntaxKind::JSX_TEXT
                    | SyntaxKind::JSX_FRAGMENT
            )
        })
    }
}

impl JsxOpeningElement {
    pub fn name(&self) -> Option<JsxName> {
        self.syntax().children().find_map(JsxName::cast)
    }

    pub fn attributes(&self) -> impl Iterator<Item = SyntaxNode> + '_ {
        self.syntax().children().filter(|node| {
            matches!(
                node.kind(),
                SyntaxKind::JSX_ATTRIBUTE | SyntaxKind::JSX_SPREAD_ATTRIBUTE
            )
        })
    }
}

impl JsxClosingElement {
    pub fn name(&self) -> Option<JsxName> {
        self.syntax().children().find_map(JsxName::cast)
    }
}

impl JsxName {
    pub fn text(&self) -> String {
        self.syntax().text().to_string()
    }
}

impl JsxAttribute {
    pub fn name(&self) -> Option<JsxName> {
        self.syntax().children().find_map(JsxName::cast)
    }

    pub fn value(&self) -> Option<SyntaxNode> {
        self.syntax().children().next()
    }
}

impl JsxExpressionContainer {
    pub fn expression(&self) -> Option<Expression> {
        self.syntax().children().find_map(Expression::cast)
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
