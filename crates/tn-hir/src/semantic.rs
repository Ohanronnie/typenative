use crate::{
    BindingPattern, BindingPatternKind, BindingProperty, Declaration, DeclarationId,
    DeclarationKind, Definition, DefinitionData, EnumField, EnumVariant, Field, Function,
    GenericBound, GenericParameter, MemberId, Method, Module, ModuleGraph, ModuleId, Namespace,
    Parameter, PrimitiveType, Program, ReceiverMode, Type, Visibility,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_syntax::{Token, TokenKind, lex};

/// Resolves declaration signatures and lowers them into semantic HIR.
///
/// # Errors
///
/// Returns deterministic diagnostics for unresolved or contextually invalid type names and
/// malformed semantic relationships.
pub fn lower_program(graph: ModuleGraph) -> Result<Program, Vec<Diagnostic>> {
    let resolver = Resolver::new(&graph);
    let mut diagnostics = Vec::new();
    let mut definitions = Vec::new();
    for module in &graph.modules {
        let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
        for declaration in &module.declarations {
            if let Some(definition) = lower_declaration(
                module,
                declaration,
                &lexed.tokens,
                &resolver,
                &mut diagnostics,
            ) {
                definitions.push(definition);
            }
        }
    }
    validate_coherence_and_inheritance(&definitions, &graph, &mut diagnostics);
    definitions.sort_by_key(|definition| definition.declaration);
    if diagnostics.is_empty() {
        Ok(Program { graph, definitions })
    } else {
        Err(diagnostics)
    }
}

struct Resolver {
    names: BTreeMap<(ModuleId, Namespace, String), DeclarationId>,
    interfaces: BTreeSet<DeclarationId>,
    generic_names: BTreeMap<DeclarationId, Vec<Namespace>>,
}

impl Resolver {
    fn new(graph: &ModuleGraph) -> Self {
        let mut names = BTreeMap::new();
        let mut interfaces = BTreeSet::new();
        let mut generic_names = BTreeMap::new();
        for module in &graph.modules {
            let lexed = lex(&module.path.to_string_lossy(), module.source.as_bytes());
            for declaration in &module.declarations {
                if declaration.kind == DeclarationKind::Interface {
                    interfaces.insert(declaration.id);
                }
                if let (Some(namespace), Some(name)) =
                    (declaration.kind.namespace(), declaration.name.as_ref())
                {
                    names.insert((module.id, namespace, name.clone()), declaration.id);
                }
                let names_for_declaration =
                    declaration_generic_names(declaration, &lexed.tokens, &module.source);
                if !names_for_declaration.is_empty() {
                    generic_names.insert(declaration.id, names_for_declaration);
                }
            }
        }
        for module in &graph.modules {
            for import in &module.imports {
                let Some(target) = graph.module(import.target) else {
                    continue;
                };
                if let crate::ImportClause::Named(imported_names) = &import.clause {
                    for imported in imported_names {
                        if let Some(declaration) = target.declarations.iter().find(|declaration| {
                            declaration.exported
                                && declaration.name.as_deref() == Some(&imported.imported)
                        }) && let Some(namespace) = declaration.kind.namespace()
                        {
                            names.insert(
                                (module.id, namespace, imported.local.clone()),
                                declaration.id,
                            );
                        }
                    }
                }
            }
        }
        Self {
            names,
            interfaces,
            generic_names,
        }
    }

    fn resolve(&self, module: ModuleId, namespace: Namespace, name: &str) -> Option<DeclarationId> {
        self.names
            .get(&(module, namespace, name.to_owned()))
            .copied()
    }

    fn elided_lifetime_arguments(&self, declaration: DeclarationId) -> Vec<Type> {
        let Some(parameters) = self.generic_names.get(&declaration) else {
            return Vec::new();
        };
        if parameters
            .iter()
            .all(|namespace| *namespace == Namespace::Lifetime)
        {
            parameters
                .iter()
                .map(|_| Type::Lifetime("scope".into()))
                .collect()
        } else {
            Vec::new()
        }
    }
}

fn declaration_generic_names(
    declaration: &Declaration,
    tokens: &[Token],
    source: &str,
) -> Vec<Namespace> {
    let significant = tokens
        .iter()
        .filter(|token| {
            !token.kind.is_trivia()
                && token.range.start >= declaration.byte_start as usize
                && token.range.end <= declaration.byte_end as usize
        })
        .collect::<Vec<_>>();
    let Some(name) = declaration.name.as_deref() else {
        return Vec::new();
    };
    let Some(name_index) = significant
        .iter()
        .position(|token| &source[token.range.clone()] == name)
    else {
        return Vec::new();
    };
    if significant.get(name_index + 1).map(|token| token.kind) != Some(TokenKind::Less) {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut depth = 1_u32;
    let mut parameter_start = true;
    for token in significant.iter().skip(name_index + 2) {
        match token.kind {
            TokenKind::Less => depth += 1,
            TokenKind::Greater => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            TokenKind::Comma if depth == 1 => parameter_start = true,
            TokenKind::Lifetime if depth == 1 && parameter_start => {
                result.push(Namespace::Lifetime);
                parameter_start = false;
            }
            TokenKind::Identifier if depth == 1 && parameter_start => {
                result.push(Namespace::Type);
                parameter_start = false;
            }
            _ => {}
        }
    }
    result
}

struct Cursor<'source, 'tokens> {
    file: String,
    source: &'source str,
    tokens: Vec<&'tokens Token>,
    index: usize,
    module: ModuleId,
    generics: BTreeMap<String, Namespace>,
    definition_generics: Vec<GenericParameter>,
    allow_self: bool,
}

impl<'source, 'tokens> Cursor<'source, 'tokens> {
    fn new(
        module: &'source Module,
        declaration: &Declaration,
        all_tokens: &'tokens [Token],
    ) -> Self {
        let start = declaration.byte_start as usize;
        let end = declaration.byte_end as usize;
        Self {
            file: module.path.to_string_lossy().into_owned(),
            source: &module.source,
            tokens: all_tokens
                .iter()
                .filter(|token| {
                    !token.kind.is_trivia() && token.range.start >= start && token.range.end <= end
                })
                .collect(),
            index: 0,
            module: module.id,
            generics: BTreeMap::new(),
            definition_generics: Vec::new(),
            allow_self: false,
        }
    }

    fn kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.index).map(|token| token.kind)
    }

    fn nth(&self, offset: usize) -> Option<TokenKind> {
        self.tokens.get(self.index + offset).map(|token| token.kind)
    }

    fn token(&self) -> Option<&Token> {
        self.tokens.get(self.index).copied()
    }

    fn text(&self) -> Option<&str> {
        self.token().map(|token| &self.source[token.range.clone()])
    }

    fn bump(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.index).copied();
        self.index += usize::from(token.is_some());
        token
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.kind() == Some(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn span(&self) -> SourceSpan {
        self.token().map_or_else(
            || {
                SourceSpan::new(
                    &self.file,
                    self.source.len()..self.source.len(),
                    self.source,
                )
            },
            |token| SourceSpan::new(&self.file, token.range.clone(), self.source),
        )
    }

    fn name(&mut self, diagnostics: &mut Vec<Diagnostic>) -> Option<(String, SourceSpan)> {
        if self.kind() != Some(TokenKind::Identifier) {
            diagnostics.push(diag(
                "SEMANTIC_EXPECTED_NAME",
                "expected a semantic name",
                &self.span(),
                "an identifier is required here",
            ));
            return None;
        }
        let span = self.span();
        let name = self.text()?.to_owned();
        self.bump();
        Some((name, span))
    }

    fn skip_balanced(&mut self, open: TokenKind, close: TokenKind) {
        if !self.eat(open) {
            return;
        }
        let mut depth = 1_u32;
        while let Some(kind) = self.kind() {
            self.bump();
            if kind == open {
                depth += 1;
            } else if kind == close {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
        }
    }

    fn balanced_end(&self, open: TokenKind, close: TokenKind) -> Option<u32> {
        if self.kind() != Some(open) {
            return None;
        }
        let mut depth = 0_u32;
        for token in self.tokens.iter().skip(self.index) {
            if token.kind == open {
                depth += 1;
            } else if token.kind == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(u32::try_from(token.range.end).unwrap_or(u32::MAX));
                }
            }
        }
        None
    }
}

fn lower_declaration(
    module: &Module,
    declaration: &Declaration,
    tokens: &[Token],
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Definition> {
    let mut cursor = Cursor::new(module, declaration, tokens);
    let data = match declaration.kind {
        DeclarationKind::Const | DeclarationKind::Static => {
            lower_constant(&mut cursor, declaration.kind, resolver, diagnostics)
        }
        DeclarationKind::TypeAlias => lower_alias(&mut cursor, resolver, diagnostics),
        DeclarationKind::Function => {
            DefinitionData::Function(lower_function(&mut cursor, resolver, diagnostics, false)?)
        }
        DeclarationKind::ExternFunction => {
            cursor.bump();
            cursor.bump();
            cursor.bump();
            DefinitionData::Function(lower_function(&mut cursor, resolver, diagnostics, false)?)
        }
        DeclarationKind::Struct => lower_struct(&mut cursor, declaration.id, resolver, diagnostics),
        DeclarationKind::ExternStruct => {
            lower_extern_struct(&mut cursor, declaration.id, resolver, diagnostics)
        }
        DeclarationKind::Enum => lower_enum(&mut cursor, declaration.id, resolver, diagnostics),
        DeclarationKind::Interface => {
            lower_interface(&mut cursor, declaration.id, resolver, diagnostics)
        }
        DeclarationKind::Class => lower_class(&mut cursor, declaration.id, resolver, diagnostics),
        DeclarationKind::Impl => lower_impl(&mut cursor, declaration.id, resolver, diagnostics),
        DeclarationKind::ExternBlock => {
            lower_extern(&mut cursor, declaration.id, resolver, diagnostics)
        }
    };
    let generics = cursor.definition_generics.clone();
    Some(Definition {
        declaration: declaration.id,
        generics,
        data,
    })
}

fn lower_constant(
    cursor: &mut Cursor<'_, '_>,
    kind: DeclarationKind,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.bump();
    let mutable_static = kind == DeclarationKind::Static && cursor.eat(TokenKind::Mut);
    cursor.name(diagnostics);
    let ty = if cursor.eat(TokenKind::Colon) {
        parse_type(cursor, resolver, diagnostics)
    } else {
        Type::Error
    };
    DefinitionData::Constant { ty, mutable_static }
}

fn lower_alias(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.bump();
    cursor.name(diagnostics);
    capture_definition_generic_parameters(cursor, resolver, diagnostics);
    cursor.eat(TokenKind::Equal);
    DefinitionData::TypeAlias(parse_type(cursor, resolver, diagnostics))
}

fn lower_function(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
    method: bool,
) -> Option<Function> {
    let is_unsafe = cursor.eat(TokenKind::Unsafe);
    let is_async = cursor.eat(TokenKind::Async);
    if !method {
        cursor.eat(TokenKind::Function);
    }
    let is_generator = !method && cursor.eat(TokenKind::Star);
    if method {
        parse_method_name(cursor, diagnostics)?;
    } else {
        cursor.name(diagnostics)?;
    }
    let generics = if method {
        parse_generic_parameters(cursor, resolver, diagnostics)
    } else {
        capture_definition_generic_parameters(cursor, resolver, diagnostics)
    };
    let parameters = parse_parameters(cursor, resolver, diagnostics);
    cursor.eat(TokenKind::Colon);
    let result = parse_type(cursor, resolver, diagnostics);
    let declared_effects = parse_effects(cursor, resolver, diagnostics);
    if is_async && !declared_effects.is_empty() {
        diagnostics.push(diag(
            "TYPE_ASYNC_THROWS_EXCLUDED",
            "async functions encode errors in Promise<T, E>",
            &cursor.span(),
            "remove `throws` and use the Promise error type",
        ));
    }
    let effects = if is_async {
        if let Type::Promise { effects, .. } = &result {
            effects.clone()
        } else {
            Vec::new()
        }
    } else {
        declared_effects
    };
    if !method {
        cursor.definition_generics.clone_from(&generics);
    }
    let body_start = cursor
        .token()
        .filter(|token| token.kind == TokenKind::LeftBrace)
        .map_or(0, |token| {
            u32::try_from(token.range.start).unwrap_or(u32::MAX)
        });
    let body_end = cursor
        .balanced_end(TokenKind::LeftBrace, TokenKind::RightBrace)
        .unwrap_or(body_start);
    Some(Function {
        parameters,
        result,
        effects,
        generics,
        is_async,
        is_generator,
        is_unsafe,
        body_start,
        body_end,
    })
}

fn peek_method_name(cursor: &Cursor<'_, '_>) -> Option<(String, usize, SourceSpan)> {
    match cursor.kind()? {
        TokenKind::Identifier | TokenKind::From => {
            Some((cursor.text()?.to_owned(), 1, cursor.span()))
        }
        TokenKind::LeftBracket
            if cursor.nth(1) == Some(TokenKind::Identifier)
                && cursor.nth(2) == Some(TokenKind::Dot)
                && cursor.nth(3) == Some(TokenKind::Identifier)
                && cursor.nth(4) == Some(TokenKind::RightBracket) =>
        {
            let root = cursor
                .tokens
                .get(cursor.index + 1)
                .map(|token| &cursor.source[token.range.clone()])?;
            let member = cursor
                .tokens
                .get(cursor.index + 3)
                .map(|token| &cursor.source[token.range.clone()])?;
            Some((format!("[{root}.{member}]"), 5, cursor.span()))
        }
        _ => None,
    }
}

fn parse_method_name(
    cursor: &mut Cursor<'_, '_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(String, SourceSpan)> {
    let (name, width, span) = peek_method_name(cursor)?;
    if width == 5 && !matches!(name.as_str(), "[Symbol.dispose]" | "[Symbol.asyncDispose]") {
        diagnostics.push(diag(
            "TYPE_UNSUPPORTED_COMPUTED_METHOD_NAME",
            "computed method names are reserved for standard disposal symbols",
            &span,
            "use `Symbol.dispose`, `Symbol.asyncDispose`, or an ordinary method name",
        ));
    }
    cursor.index += width;
    Some((name, span))
}

fn parse_generic_parameters(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<GenericParameter> {
    let mut parameters = Vec::new();
    if !cursor.eat(TokenKind::Less) {
        return parameters;
    }
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::Greater) {
        let namespace = if cursor.eat(TokenKind::Lifetime) {
            Namespace::Lifetime
        } else if cursor.eat(TokenKind::Throws) {
            Namespace::Value
        } else {
            Namespace::Type
        };
        if let Some((name, span)) = cursor.name(diagnostics) {
            cursor.generics.insert(name.clone(), namespace);
            let parameter_index = parameters.len();
            parameters.push(GenericParameter {
                name,
                namespace,
                bounds: Vec::new(),
                span,
            });
            if cursor.eat(TokenKind::Extends) {
                loop {
                    let bound = parse_primary_type(cursor, resolver, diagnostics);
                    if let Type::Nominal(id, arguments) | Type::DynamicInterface(id, arguments) =
                        bound
                    {
                        parameters[parameter_index]
                            .bounds
                            .push(GenericBound::Interface(id, arguments));
                    } else {
                        diagnostics.push(diag(
                            "TYPE_EXPECTED_GENERIC_BOUND",
                            "generic bounds must name an interface",
                            &cursor.span(),
                            "write `T extends Interface`",
                        ));
                    }
                    if !cursor.eat(TokenKind::Amp) {
                        break;
                    }
                }
            }
        }
        if !cursor.eat(TokenKind::Comma) {
            break;
        }
    }
    cursor.eat(TokenKind::Greater);
    parameters
}

fn capture_definition_generic_parameters(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<GenericParameter> {
    let parameters = parse_generic_parameters(cursor, resolver, diagnostics);
    cursor.definition_generics.clone_from(&parameters);
    parameters
}

fn parse_parameters(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Parameter> {
    let mut parameters = Vec::new();
    if !cursor.eat(TokenKind::LeftParen) {
        return parameters;
    }
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightParen) {
        cursor.eat(TokenKind::Ellipsis);
        let Some(pattern) = parse_binding_pattern(cursor, diagnostics) else {
            break;
        };
        cursor.eat(TokenKind::Colon);
        let ty = parse_type(cursor, resolver, diagnostics);
        let default = if cursor.eat(TokenKind::Equal) {
            skip_binding_default(cursor, &[TokenKind::Comma, TokenKind::RightParen])
        } else {
            pattern.default.clone()
        };
        let name = pattern
            .primary_name()
            .map_or_else(|| format!("$parameter{}", parameters.len()), str::to_owned);
        let span = pattern.span.clone();
        parameters.push(Parameter {
            name,
            ty,
            pattern,
            default,
            span,
        });
        if !cursor.eat(TokenKind::Comma) {
            break;
        }
    }
    cursor.eat(TokenKind::RightParen);
    parameters
}

#[allow(clippy::too_many_lines)]
fn parse_binding_pattern(
    cursor: &mut Cursor<'_, '_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<BindingPattern> {
    let start = cursor.token()?.range.start;
    let mutable = cursor.eat(TokenKind::Mut);
    let mut pattern = match cursor.kind()? {
        TokenKind::Identifier => {
            let (name, span) = cursor.name(diagnostics)?;
            BindingPattern::identifier(name, false, span)
        }
        TokenKind::LeftBracket => {
            cursor.bump();
            let mut elements = Vec::new();
            let mut rest = None;
            while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBracket) {
                if cursor.eat(TokenKind::Comma) {
                    elements.push(None);
                    continue;
                }
                if cursor.eat(TokenKind::Ellipsis) {
                    rest = parse_binding_pattern(cursor, diagnostics).map(Box::new);
                    if cursor.eat(TokenKind::Comma)
                        && cursor.kind() != Some(TokenKind::RightBracket)
                    {
                        diagnostics.push(diag(
                            "SEMANTIC_BINDING_REST_NOT_LAST",
                            "rest binding must be last",
                            &cursor.span(),
                            "move the rest binding to the end of the pattern",
                        ));
                    }
                    break;
                }
                let Some(mut element) = parse_binding_pattern(cursor, diagnostics) else {
                    break;
                };
                if cursor.eat(TokenKind::Equal) {
                    element.default =
                        skip_binding_default(cursor, &[TokenKind::Comma, TokenKind::RightBracket]);
                }
                elements.push(Some(element));
                if !cursor.eat(TokenKind::Comma) {
                    break;
                }
            }
            cursor.eat(TokenKind::RightBracket);
            BindingPattern {
                kind: BindingPatternKind::Array { elements, rest },
                default: None,
                span: span_from_start(cursor, start),
            }
        }
        TokenKind::LeftBrace => {
            cursor.bump();
            let mut properties = Vec::new();
            let mut rest = None;
            while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
                if cursor.eat(TokenKind::Ellipsis) {
                    rest = parse_binding_pattern(cursor, diagnostics).map(Box::new);
                    if cursor.eat(TokenKind::Comma) && cursor.kind() != Some(TokenKind::RightBrace)
                    {
                        diagnostics.push(diag(
                            "SEMANTIC_BINDING_REST_NOT_LAST",
                            "rest binding must be last",
                            &cursor.span(),
                            "move the rest binding to the end of the pattern",
                        ));
                    }
                    break;
                }
                let property_start = cursor.token()?.range.start;
                let (key, key_span) = cursor.name(diagnostics)?;
                let mut property_pattern = if cursor.eat(TokenKind::Colon) {
                    parse_binding_pattern(cursor, diagnostics)?
                } else {
                    BindingPattern::identifier(key.clone(), false, key_span.clone())
                };
                if cursor.eat(TokenKind::Equal) {
                    property_pattern.default =
                        skip_binding_default(cursor, &[TokenKind::Comma, TokenKind::RightBrace]);
                }
                let property_span = SourceSpan::new(
                    &cursor.file,
                    property_start..last_consumed_end(cursor).max(property_start),
                    cursor.source,
                );
                properties.push(BindingProperty {
                    key,
                    pattern: property_pattern,
                    span: property_span,
                });
                if !cursor.eat(TokenKind::Comma) {
                    break;
                }
            }
            cursor.eat(TokenKind::RightBrace);
            BindingPattern {
                kind: BindingPatternKind::Object { properties, rest },
                default: None,
                span: span_from_start(cursor, start),
            }
        }
        _ => {
            diagnostics.push(diag(
                "SEMANTIC_EXPECTED_BINDING_PATTERN",
                "expected a binding pattern",
                &cursor.span(),
                "use an identifier, array pattern, or object pattern",
            ));
            return None;
        }
    };
    if mutable {
        make_pattern_mutable(&mut pattern);
        pattern.span = SourceSpan::new(
            &cursor.file,
            start..pattern.span.byte_end as usize,
            cursor.source,
        );
    }
    Some(pattern)
}

fn make_pattern_mutable(pattern: &mut BindingPattern) {
    match &mut pattern.kind {
        BindingPatternKind::Identifier { mutable, .. } => *mutable = true,
        BindingPatternKind::Array { elements, rest } => {
            for element in elements.iter_mut().flatten() {
                make_pattern_mutable(element);
            }
            if let Some(rest) = rest {
                make_pattern_mutable(rest);
            }
        }
        BindingPatternKind::Object { properties, rest } => {
            for property in properties {
                make_pattern_mutable(&mut property.pattern);
            }
            if let Some(rest) = rest {
                make_pattern_mutable(rest);
            }
        }
    }
}

fn skip_binding_default(cursor: &mut Cursor<'_, '_>, stops: &[TokenKind]) -> Option<SourceSpan> {
    let start = cursor.token()?.range.start;
    let mut delimiters = Vec::new();
    let mut end = start;
    while let Some(token) = cursor.token() {
        if delimiters.is_empty() && stops.contains(&token.kind) {
            break;
        }
        match token.kind {
            TokenKind::LeftParen => delimiters.push(TokenKind::RightParen),
            TokenKind::LeftBracket => delimiters.push(TokenKind::RightBracket),
            TokenKind::LeftBrace => delimiters.push(TokenKind::RightBrace),
            TokenKind::RightParen | TokenKind::RightBracket | TokenKind::RightBrace => {
                if delimiters.last() == Some(&token.kind) {
                    delimiters.pop();
                } else if delimiters.is_empty() {
                    break;
                }
            }
            _ => {}
        }
        end = token.range.end;
        cursor.bump();
    }
    Some(SourceSpan::new(&cursor.file, start..end, cursor.source))
}

fn last_consumed_end(cursor: &Cursor<'_, '_>) -> usize {
    cursor
        .tokens
        .get(cursor.index.saturating_sub(1))
        .map_or(0, |token| token.range.end)
}

fn span_from_start(cursor: &Cursor<'_, '_>, start: usize) -> SourceSpan {
    SourceSpan::new(
        &cursor.file,
        start..last_consumed_end(cursor).max(start),
        cursor.source,
    )
}

fn parse_effects(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<DeclarationId> {
    if !cursor.eat(TokenKind::Throws) {
        return Vec::new();
    }
    let mut effects = Vec::new();
    loop {
        if let Some((name, _span)) = cursor.name(diagnostics)
            && let Some(id) = resolver.resolve(cursor.module, Namespace::Type, &name)
        {
            effects.push(id);
        } else if cursor.kind() != Some(TokenKind::Pipe) {
            diagnostics.push(diag(
                "TYPE_UNRESOLVED_ERROR_EFFECT",
                "unresolved recoverable error type",
                &cursor.span(),
                "effects must name a nominal error type",
            ));
        }
        if !cursor.eat(TokenKind::Pipe) {
            break;
        }
    }
    effects.sort_unstable();
    effects.dedup();
    effects
}

fn parse_type(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Type {
    let primary = parse_primary_type(cursor, resolver, diagnostics);
    if !cursor.eat(TokenKind::Pipe) {
        return primary;
    }

    let mut alternatives = vec![primary];
    let mut has_undefined = false;
    loop {
        if cursor.eat(TokenKind::Undefined) {
            has_undefined = true;
        } else {
            alternatives.push(parse_primary_type(cursor, resolver, diagnostics));
        }
        if !cursor.eat(TokenKind::Pipe) {
            break;
        }
    }
    alternatives.sort();
    alternatives.dedup();
    let value = if alternatives.len() == 1 {
        alternatives.pop().unwrap_or(Type::Error)
    } else {
        Type::Union(alternatives)
    };
    if has_undefined {
        Type::Optional(Box::new(value))
    } else {
        value
    }
}

fn parse_primary_type(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Type {
    match cursor.kind() {
        Some(TokenKind::Amp) => {
            cursor.bump();
            let lifetime = if cursor.kind() == Some(TokenKind::Static) {
                cursor.bump();
                "static".to_owned()
            } else if cursor.kind() == Some(TokenKind::Scope) {
                cursor.bump();
                "scope".to_owned()
            } else if cursor
                .text()
                .is_some_and(|name| cursor.generics.get(name) == Some(&Namespace::Lifetime))
            {
                let lifetime = cursor.text().unwrap_or("scope").to_owned();
                cursor.bump();
                lifetime
            } else {
                "scope".to_owned()
            };
            let mutable = cursor.eat(TokenKind::Mut);
            Type::Reference {
                mutable,
                lifetime,
                referent: Box::new(parse_primary_type(cursor, resolver, diagnostics)),
            }
        }
        Some(TokenKind::Star) => {
            cursor.bump();
            let mutable = cursor.eat(TokenKind::Mut);
            if !mutable {
                cursor.eat(TokenKind::Const);
            }
            Type::RawPointer {
                mutable,
                pointee: Box::new(parse_primary_type(cursor, resolver, diagnostics)),
            }
        }
        Some(TokenKind::LeftBracket) => {
            cursor.bump();
            let element = parse_type(cursor, resolver, diagnostics);
            if cursor.eat(TokenKind::Semicolon) {
                let length = cursor
                    .text()
                    .and_then(parse_unsigned_literal)
                    .unwrap_or_default();
                cursor.bump();
                cursor.eat(TokenKind::RightBracket);
                Type::Array(Box::new(element), length)
            } else {
                cursor.eat(TokenKind::RightBracket);
                Type::Slice(Box::new(element))
            }
        }
        Some(TokenKind::LeftParen) => parse_tuple_or_function_type(cursor, resolver, diagnostics),
        Some(TokenKind::Async) => {
            cursor.bump();
            parse_tuple_or_function_type(cursor, resolver, diagnostics)
        }
        Some(TokenKind::Extern) => parse_foreign_function_type(cursor, resolver, diagnostics),
        Some(TokenKind::Dyn) => {
            let span = cursor.span();
            cursor.bump();
            diagnostics.push(diag(
                "TYPE_EXCLUDED_DYNAMIC_INTERFACE",
                "dynamic interface values are not part of canonical TypeNative",
                &span,
                "use a statically named interface or `unknown`",
            ));
            Type::Error
        }
        Some(TokenKind::Unknown) => {
            cursor.bump();
            Type::Unknown
        }
        Some(TokenKind::Identifier) => parse_named_type(cursor, resolver, diagnostics),
        _ => {
            let span = cursor.span();
            cursor.bump();
            diagnostics.push(diag(
                "TYPE_EXPECTED_TYPE",
                "expected a type",
                &span,
                "a primitive, nominal, reference, pointer, array, tuple, or function type is required",
            ));
            Type::Error
        }
    }
}

fn parse_foreign_function_type(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Type {
    cursor.bump();
    let abi_span = cursor.span();
    if cursor.text() != Some("\"C\"") {
        diagnostics.push(diag(
            "TYPE_UNSUPPORTED_FOREIGN_ABI",
            "only the C foreign ABI is supported",
            &abi_span,
            "use `extern \"C\"`",
        ));
    }
    cursor.bump();
    cursor.eat(TokenKind::Function);
    let parameters = parse_type_list(cursor, resolver, diagnostics);
    cursor.eat(TokenKind::Colon);
    let result = parse_type(cursor, resolver, diagnostics);
    Type::Function(crate::FunctionType {
        parameters,
        result: Box::new(result),
        effects: Vec::new(),
        generics: Vec::new(),
        is_async: false,
        is_unsafe: true,
    })
}

fn parse_named_type(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Type {
    let name = cursor.text().unwrap_or_default().to_owned();
    if name == "Self" {
        let span = cursor.span();
        cursor.bump();
        if cursor.allow_self {
            return Type::Generic(name);
        }
        diagnostics.push(diag(
            "TYPE_SELF_ONLY_INTERFACE_METHOD",
            "Self is available only in interface method signatures",
            &span,
            "use a concrete nominal type or a declared generic parameter here",
        ));
        return Type::Error;
    }
    if let Some(primitive) = primitive(&name) {
        cursor.bump();
        return primitive;
    }
    if name == "Promise" {
        cursor.bump();
        let mut arguments = parse_generic_arguments(cursor, resolver, diagnostics);
        if arguments.len() != 2 {
            diagnostics.push(diag(
                "TYPE_PROMISE_ARITY",
                "Promise requires exactly a result and error type",
                &cursor.span(),
                "write `Promise<T, E>`",
            ));
            return Type::Error;
        }
        let error = arguments.pop().unwrap_or(Type::Error);
        let result = arguments.pop().unwrap_or(Type::Error);
        let effects = match &error {
            Type::Nominal(id, _) => vec![*id],
            Type::Primitive(PrimitiveType::Never) | Type::Error | Type::Generic(_) => Vec::new(),
            _ => {
                diagnostics.push(diag(
                    "TYPE_PROMISE_ERROR_TYPE",
                    "Promise error types must be nominal errors, generic parameters, or never",
                    &cursor.span(),
                    "use a declared error type, generic parameter, or never as the second Promise argument",
                ));
                Vec::new()
            }
        };
        return Type::Promise {
            result: Box::new(result),
            error: Box::new(error),
            effects,
        };
    }
    if let Some(namespace) = cursor.generics.get(&name).copied() {
        cursor.bump();
        return if namespace == Namespace::Lifetime {
            Type::Lifetime(name)
        } else {
            Type::Generic(name)
        };
    }
    let Some((id, _)) = resolve_named_type(cursor, resolver, diagnostics) else {
        return Type::Error;
    };
    let arguments = if cursor.kind() == Some(TokenKind::Less) {
        parse_generic_arguments(cursor, resolver, diagnostics)
    } else {
        resolver.elided_lifetime_arguments(id)
    };
    if resolver.interfaces.contains(&id) {
        Type::DynamicInterface(id, arguments)
    } else {
        Type::Nominal(id, arguments)
    }
}

fn resolve_named_type(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(DeclarationId, String)> {
    let (name, span) = cursor.name(diagnostics)?;
    while cursor.eat(TokenKind::Dot) {
        cursor.name(diagnostics);
    }
    let Some(id) = resolver.resolve(cursor.module, Namespace::Type, &name) else {
        diagnostics.push(diag(
            "TYPE_UNRESOLVED_NAME",
            format!("unresolved type name `{name}`"),
            &span,
            "import or declare this type in the type namespace",
        ));
        return None;
    };
    Some((id, name))
}

fn parse_generic_arguments(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Type> {
    let mut arguments = Vec::new();
    cursor.eat(TokenKind::Less);
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::Greater) {
        if matches!(cursor.kind(), Some(TokenKind::Static | TokenKind::Scope)) {
            let lifetime = cursor.text().unwrap_or("scope").to_owned();
            cursor.bump();
            arguments.push(Type::Lifetime(lifetime));
        } else {
            arguments.push(parse_type(cursor, resolver, diagnostics));
        }
        if !cursor.eat(TokenKind::Comma) {
            break;
        }
    }
    cursor.eat(TokenKind::Greater);
    arguments
}

fn parse_tuple_or_function_type(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Type {
    let elements = parse_type_list(cursor, resolver, diagnostics);
    if cursor.eat(TokenKind::FatArrow) {
        Type::Function(crate::FunctionType {
            parameters: elements,
            result: Box::new(parse_type(cursor, resolver, diagnostics)),
            effects: parse_effects(cursor, resolver, diagnostics),
            generics: Vec::new(),
            is_async: false,
            is_unsafe: false,
        })
    } else if elements.len() == 1 {
        elements.into_iter().next().unwrap_or(Type::Error)
    } else {
        Type::Tuple(elements)
    }
}

fn parse_type_list(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Type> {
    let mut elements = Vec::new();
    if !cursor.eat(TokenKind::LeftParen) {
        return elements;
    }
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightParen) {
        elements.push(parse_type(cursor, resolver, diagnostics));
        if !cursor.eat(TokenKind::Comma) {
            break;
        }
    }
    cursor.eat(TokenKind::RightParen);
    elements
}

fn primitive(name: &str) -> Option<Type> {
    Some(match name {
        "bool" => Type::Primitive(PrimitiveType::Bool),
        "i8" => Type::Primitive(PrimitiveType::I8),
        "i16" => Type::Primitive(PrimitiveType::I16),
        "i32" => Type::Primitive(PrimitiveType::I32),
        "i64" => Type::Primitive(PrimitiveType::I64),
        "i128" => Type::Primitive(PrimitiveType::I128),
        "isize" | "number" => Type::Primitive(PrimitiveType::Isize),
        "u8" => Type::Primitive(PrimitiveType::U8),
        "u16" => Type::Primitive(PrimitiveType::U16),
        "u32" => Type::Primitive(PrimitiveType::U32),
        "u64" => Type::Primitive(PrimitiveType::U64),
        "u128" => Type::Primitive(PrimitiveType::U128),
        "usize" => Type::Primitive(PrimitiveType::Usize),
        "f32" => Type::Primitive(PrimitiveType::F32),
        "f64" => Type::Primitive(PrimitiveType::F64),
        "char" => Type::Primitive(PrimitiveType::Char),
        "void" => Type::Primitive(PrimitiveType::Void),
        "never" => Type::Primitive(PrimitiveType::Never),
        "string" => Type::String,
        "str" => Type::Str,
        _ => return None,
    })
}

fn parse_unsigned_literal(text: &str) -> Option<u64> {
    let digits = text
        .trim_end_matches(|character: char| character.is_ascii_alphabetic())
        .replace('_', "");
    let (radix, digits) = if let Some(digits) = digits.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = digits.strip_prefix("0o") {
        (8, digits)
    } else if let Some(digits) = digits.strip_prefix("0b") {
        (2, digits)
    } else {
        (10, digits.as_str())
    };
    u64::from_str_radix(digits, radix).ok()
}

fn lower_struct(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.bump();
    cursor.name(diagnostics);
    let generics = capture_definition_generic_parameters(cursor, resolver, diagnostics);
    let interfaces = parse_implemented_interfaces(cursor, resolver, diagnostics);
    cursor.definition_generics = generics;
    cursor.eat(TokenKind::LeftBrace);
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        let attributes = parse_attributes(cursor);
        let checkpoint = cursor.index;
        if let Some(mut method) =
            parse_method(cursor, owner, resolver, diagnostics, false, methods.len())
        {
            method.attributes = attributes;
            methods.push(method);
            continue;
        }
        cursor.index = checkpoint;
        if let Some(field) = parse_field(cursor, owner, resolver, diagnostics) {
            fields.push(field);
        } else {
            cursor.bump();
        }
    }
    DefinitionData::Struct {
        c_layout: false,
        interfaces,
        fields,
        methods,
    }
}

fn lower_extern_struct(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.bump();
    cursor.bump();
    cursor.name(diagnostics);
    let generics = capture_definition_generic_parameters(cursor, resolver, diagnostics);
    cursor.definition_generics = generics;
    cursor.eat(TokenKind::LeftBrace);
    let mut fields = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        let checkpoint = cursor.index;
        if let Some(field) = parse_field(cursor, owner, resolver, diagnostics) {
            fields.push(field);
        } else {
            cursor.index = checkpoint;
            cursor.bump();
        }
    }
    DefinitionData::Struct {
        c_layout: true,
        interfaces: Vec::new(),
        fields,
        methods: Vec::new(),
    }
}

fn lower_enum(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.bump();
    cursor.name(diagnostics);
    let generics = capture_definition_generic_parameters(cursor, resolver, diagnostics);
    let repr = if cursor.eat(TokenKind::Colon) {
        match parse_type(cursor, resolver, diagnostics) {
            Type::Primitive(primitive) if is_integer_primitive(&primitive) => Some(primitive),
            _ => None,
        }
    } else {
        None
    };
    let interfaces = parse_implemented_interfaces(cursor, resolver, diagnostics);
    cursor.definition_generics = generics;
    cursor.allow_self = true;
    cursor.eat(TokenKind::LeftBrace);
    let mut variants = Vec::new();
    let mut methods = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        let checkpoint = cursor.index;
        if matches!(
            cursor.kind(),
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
        ) && let Some(method) =
            parse_method(cursor, owner, resolver, diagnostics, false, methods.len())
        {
            methods.push(method);
            continue;
        }
        cursor.index = checkpoint;
        let Some((name, span)) = cursor.name(diagnostics) else {
            cursor.bump();
            continue;
        };
        let mut fields = Vec::new();
        if cursor.eat(TokenKind::LeftParen) {
            while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightParen) {
                let field_span = cursor.span();
                fields.push(EnumField {
                    name: None,
                    ty: parse_type(cursor, resolver, diagnostics),
                    span: field_span,
                });
                if !cursor.eat(TokenKind::Comma) {
                    break;
                }
            }
            cursor.eat(TokenKind::RightParen);
        } else if cursor.eat(TokenKind::LeftBrace) {
            while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
                let field_name = cursor.name(diagnostics);
                cursor.eat(TokenKind::Question);
                cursor.eat(TokenKind::Colon);
                let ty = parse_type(cursor, resolver, diagnostics);
                if let Some((name, span)) = field_name {
                    fields.push(EnumField {
                        name: Some(name),
                        ty,
                        span,
                    });
                }
                cursor.eat(TokenKind::Semicolon);
            }
            cursor.eat(TokenKind::RightBrace);
        }
        let discriminant = if cursor.eat(TokenKind::Equal) {
            let value = cursor.text().and_then(parse_signed_literal);
            cursor.bump();
            value
        } else {
            None
        };
        variants.push(EnumVariant {
            id: member_id(owner, &name, variants.len()),
            name,
            fields,
            discriminant,
            span,
        });
        if !cursor.eat(TokenKind::Comma) {
            break;
        }
    }
    DefinitionData::Enum {
        repr,
        interfaces,
        variants,
        methods,
    }
}

fn is_integer_primitive(primitive: &PrimitiveType) -> bool {
    matches!(
        primitive,
        PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::I128
            | PrimitiveType::Isize
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::U128
            | PrimitiveType::Usize
    )
}

fn parse_implemented_interfaces(
    cursor: &mut Cursor<'_, '_>,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Type> {
    let mut interfaces = Vec::new();
    if !cursor.eat(TokenKind::Implements) {
        return interfaces;
    }
    loop {
        if let Some((id, _)) = resolve_named_type(cursor, resolver, diagnostics) {
            let arguments = if cursor.kind() == Some(TokenKind::Less) {
                parse_generic_arguments(cursor, resolver, diagnostics)
            } else {
                Vec::new()
            };
            interfaces.push(Type::Nominal(id, arguments));
        }
        if !cursor.eat(TokenKind::Comma) {
            break;
        }
    }
    interfaces
}

fn lower_interface(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.bump();
    cursor.name(diagnostics);
    let generics = capture_definition_generic_parameters(cursor, resolver, diagnostics);
    cursor.definition_generics = generics;
    cursor.allow_self = true;
    cursor.eat(TokenKind::LeftBrace);
    let mut methods = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        if let Some(method) =
            parse_method(cursor, owner, resolver, diagnostics, true, methods.len())
        {
            methods.push(method);
        } else {
            cursor.bump();
        }
    }
    DefinitionData::Interface { methods }
}

fn lower_class(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    let is_abstract = cursor.eat(TokenKind::Abstract);
    cursor.eat(TokenKind::Class);
    cursor.name(diagnostics);
    let generics = capture_definition_generic_parameters(cursor, resolver, diagnostics);
    let base = if cursor.eat(TokenKind::Extends) {
        resolve_named_type(cursor, resolver, diagnostics).map(|(id, _)| id)
    } else {
        None
    };
    let mut interfaces = Vec::new();
    if cursor.eat(TokenKind::Implements) {
        loop {
            if let Some((id, _)) = resolve_named_type(cursor, resolver, diagnostics) {
                let arguments = if cursor.kind() == Some(TokenKind::Less) {
                    parse_generic_arguments(cursor, resolver, diagnostics)
                } else {
                    Vec::new()
                };
                interfaces.push(Type::Nominal(id, arguments));
            }
            if !cursor.eat(TokenKind::Comma) {
                break;
            }
        }
    }
    cursor.definition_generics = generics;
    cursor.eat(TokenKind::LeftBrace);
    let mut fields = Vec::new();
    let mut constructor = None;
    let mut methods = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        let attributes = parse_attributes(cursor);
        if let Some(mut parsed) = parse_constructor(cursor, owner, resolver, diagnostics) {
            parsed.attributes = attributes;
            if constructor.replace(parsed).is_some() {
                diagnostics.push(diag(
                    "TYPE_MULTIPLE_CONSTRUCTORS",
                    "a class declares more than one constructor",
                    &cursor.span(),
                    "TypeNative classes have exactly one constructor signature",
                ));
            }
            continue;
        }
        let checkpoint = cursor.index;
        if let Some(mut method) =
            parse_method(cursor, owner, resolver, diagnostics, false, methods.len())
        {
            method.attributes = attributes;
            methods.push(method);
            continue;
        }
        cursor.index = checkpoint;
        if let Some(field) = parse_field(cursor, owner, resolver, diagnostics) {
            fields.push(field);
        } else {
            cursor.bump();
        }
    }
    DefinitionData::Class {
        base,
        interfaces,
        fields,
        constructor,
        methods,
        is_abstract,
    }
}

fn lower_impl(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    let is_unsafe = cursor.eat(TokenKind::Unsafe);
    cursor.eat(TokenKind::Impl);
    let generics = capture_definition_generic_parameters(cursor, resolver, diagnostics);
    let first = parse_type(cursor, resolver, diagnostics);
    let (interface, target) = if cursor.eat(TokenKind::For) {
        (Some(first), parse_type(cursor, resolver, diagnostics))
    } else {
        (None, first)
    };
    cursor.definition_generics = generics;
    cursor.eat(TokenKind::LeftBrace);
    let mut methods = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        let attributes = parse_attributes(cursor);
        if let Some(mut method) =
            parse_method(cursor, owner, resolver, diagnostics, false, methods.len())
        {
            method.attributes = attributes;
            methods.push(method);
        } else {
            cursor.bump();
        }
    }
    DefinitionData::Implementation {
        interface,
        target,
        methods,
        is_unsafe,
    }
}

fn lower_extern(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> DefinitionData {
    cursor.eat(TokenKind::Declare);
    cursor.eat(TokenKind::Extern);
    if cursor.text() != Some("\"C\"") {
        diagnostics.push(diag(
            "TYPE_UNSUPPORTED_FOREIGN_ABI",
            "only the C foreign ABI is supported",
            &cursor.span(),
            "use `declare extern \"C\"`",
        ));
    }
    cursor.bump();
    cursor.eat(TokenKind::LeftBrace);
    let mut functions = Vec::new();
    while cursor.kind().is_some() && cursor.kind() != Some(TokenKind::RightBrace) {
        skip_attributes(cursor);
        if !cursor.eat(TokenKind::Function) {
            cursor.bump();
            continue;
        }
        cursor.index = cursor.index.saturating_sub(1);
        if let Some(mut method) =
            parse_method(cursor, owner, resolver, diagnostics, true, functions.len())
        {
            method.function.is_unsafe = true;
            functions.push(method);
        }
    }
    DefinitionData::Extern { functions }
}

fn parse_field(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Field> {
    let visibility = parse_visibility(cursor);
    if cursor.eat(TokenKind::Static) {
        cursor.eat(TokenKind::Mut);
    }
    let readonly = cursor.eat(TokenKind::Readonly);
    let (name, span) = cursor.name(diagnostics)?;
    if cursor.kind() == Some(TokenKind::Less) || cursor.kind() == Some(TokenKind::LeftParen) {
        return None;
    }
    let optional = cursor.eat(TokenKind::Question);
    if !cursor.eat(TokenKind::Colon) {
        return None;
    }
    let ty = parse_type(cursor, resolver, diagnostics);
    let has_initializer = cursor.eat(TokenKind::Equal);
    if has_initializer {
        skip_expression_until(cursor, &[TokenKind::Semicolon]);
    }
    cursor.eat(TokenKind::Semicolon);
    Some(Field {
        id: member_id(owner, &name, span.byte_start as usize),
        name,
        ty: if optional {
            Type::Optional(Box::new(ty))
        } else {
            ty
        },
        visibility,
        readonly,
        optional,
        has_initializer,
        span,
    })
}

fn parse_method(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
    declaration_only: bool,
    ordinal: usize,
) -> Option<Method> {
    let start = cursor.index;
    let visibility = parse_visibility(cursor);
    let is_static = cursor.eat(TokenKind::Static);
    let is_abstract = cursor.eat(TokenKind::Abstract);
    let is_override = cursor.eat(TokenKind::Override);
    let receiver = if is_static {
        ReceiverMode::Static
    } else if cursor.eat(TokenKind::Mut) {
        ReceiverMode::Mutable
    } else if cursor.eat(TokenKind::Move) {
        ReceiverMode::Move
    } else {
        ReceiverMode::Shared
    };
    let is_unsafe = cursor.eat(TokenKind::Unsafe);
    let is_async = cursor.eat(TokenKind::Async);
    cursor.eat(TokenKind::Function);
    let Some((name, width, span)) = peek_method_name(cursor) else {
        cursor.index = start;
        return None;
    };
    if !matches!(
        cursor.nth(width),
        Some(TokenKind::Less | TokenKind::LeftParen)
    ) {
        cursor.index = start;
        return None;
    }
    let mut function = lower_function(cursor, resolver, diagnostics, true)?;
    function.is_unsafe |= is_unsafe;
    if is_async {
        function.is_async = true;
        if let Type::Promise { effects, .. } = &function.result {
            function.effects.clone_from(effects);
        }
    }
    if cursor.kind() == Some(TokenKind::LeftBrace) {
        cursor.skip_balanced(TokenKind::LeftBrace, TokenKind::RightBrace);
    } else {
        cursor.eat(TokenKind::Semicolon);
    }
    if declaration_only && function.body_start != 0 {
        diagnostics.push(diag(
            "TYPE_INTERFACE_METHOD_BODY",
            "interface and foreign method declarations cannot have a body",
            &span,
            "replace the body with a semicolon",
        ));
    }
    let receiver = if receiver == ReceiverMode::Shared
        && function.body_start != 0
        && method_writes_receiver(&cursor.tokens, &function)
    {
        ReceiverMode::Mutable
    } else {
        receiver
    };
    Some(Method {
        id: member_id(owner, &name, ordinal),
        name,
        attributes: Vec::new(),
        function,
        visibility,
        receiver,
        is_abstract,
        is_override,
        span,
    })
}

fn method_writes_receiver(tokens: &[&Token], function: &Function) -> bool {
    let start = function.body_start as usize;
    let end = function.body_end as usize;
    let body = tokens
        .iter()
        .filter(|token| token.range.start >= start && token.range.end <= end)
        .copied()
        .collect::<Vec<_>>();
    for (index, token) in body.iter().enumerate() {
        if token.kind != TokenKind::This {
            continue;
        }
        let mut cursor = index + 1;
        let mut saw_member = false;
        while let Some(next) = body.get(cursor) {
            match next.kind {
                TokenKind::Dot
                | TokenKind::Identifier
                | TokenKind::LeftBracket
                | TokenKind::RightBracket => {
                    saw_member = true;
                    cursor += 1;
                }
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
                | TokenKind::ShiftRightEqual
                    if saw_member =>
                {
                    return true;
                }
                TokenKind::Semicolon
                | TokenKind::Comma
                | TokenKind::RightParen
                | TokenKind::RightBrace
                | TokenKind::LeftBrace
                | TokenKind::LeftParen => break,
                _ => cursor += 1,
            }
        }
    }
    false
}

fn parse_constructor(
    cursor: &mut Cursor<'_, '_>,
    owner: DeclarationId,
    resolver: &Resolver,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Method> {
    let start = cursor.index;
    let visibility = parse_visibility(cursor);
    if !cursor.eat(TokenKind::Constructor) {
        cursor.index = start;
        return None;
    }
    let span = cursor
        .tokens
        .get(cursor.index.saturating_sub(1))
        .map_or_else(
            || cursor.span(),
            |token| SourceSpan::new(&cursor.file, token.range.clone(), cursor.source),
        );
    let parameters = parse_parameters(cursor, resolver, diagnostics);
    let effects = parse_effects(cursor, resolver, diagnostics);
    let body_start = cursor
        .token()
        .filter(|token| token.kind == TokenKind::LeftBrace)
        .map_or(0, |token| {
            u32::try_from(token.range.start).unwrap_or(u32::MAX)
        });
    let body_end = cursor
        .balanced_end(TokenKind::LeftBrace, TokenKind::RightBrace)
        .unwrap_or(body_start);
    cursor.skip_balanced(TokenKind::LeftBrace, TokenKind::RightBrace);
    Some(Method {
        id: member_id(owner, "constructor", 0),
        name: "constructor".into(),
        attributes: Vec::new(),
        function: Function {
            parameters,
            result: Type::Primitive(PrimitiveType::Void),
            effects,
            generics: Vec::new(),
            is_async: false,
            is_generator: false,
            is_unsafe: false,
            body_start,
            body_end,
        },
        visibility,
        receiver: ReceiverMode::Mutable,
        is_abstract: false,
        is_override: false,
        span,
    })
}

fn parse_visibility(cursor: &mut Cursor<'_, '_>) -> Visibility {
    if cursor.eat(TokenKind::Public) {
        Visibility::Public
    } else if cursor.eat(TokenKind::Protected) {
        Visibility::Protected
    } else {
        cursor.eat(TokenKind::Private);
        Visibility::Private
    }
}

fn skip_attributes(cursor: &mut Cursor<'_, '_>) {
    while cursor.eat(TokenKind::At) {
        cursor.bump();
        cursor.skip_balanced(TokenKind::LeftParen, TokenKind::RightParen);
    }
}

fn parse_attributes(cursor: &mut Cursor<'_, '_>) -> Vec<crate::Attribute> {
    let mut attributes = Vec::new();
    while cursor.kind() == Some(TokenKind::At) {
        let start = cursor.token().map_or(0, |token| token.range.start);
        cursor.bump();
        let Some(name_token) = cursor.token().cloned() else {
            break;
        };
        let name = cursor.source[name_token.range.clone()].to_owned();
        cursor.bump();
        let mut arguments = Vec::new();
        if cursor.kind() == Some(TokenKind::LeftParen) {
            let open = cursor.index;
            let end = cursor
                .balanced_end(TokenKind::LeftParen, TokenKind::RightParen)
                .map_or_else(
                    || name_token.range.end,
                    |end| usize::try_from(end).unwrap_or(usize::MAX),
                );
            arguments.extend(
                cursor.tokens[open + 1..]
                    .iter()
                    .take_while(|token| token.range.start < end)
                    .filter(|token| {
                        !matches!(
                            token.kind,
                            TokenKind::Comma | TokenKind::LeftParen | TokenKind::RightParen
                        )
                    })
                    .map(|token| {
                        cursor.source[token.range.clone()]
                            .trim_matches('"')
                            .to_owned()
                    }),
            );
            cursor.skip_balanced(TokenKind::LeftParen, TokenKind::RightParen);
        }
        attributes.push(crate::Attribute {
            name,
            arguments,
            span: SourceSpan::new(&cursor.file, start..name_token.range.end, cursor.source),
        });
    }
    attributes
}

fn skip_expression_until(cursor: &mut Cursor<'_, '_>, end: &[TokenKind]) {
    let mut delimiters = Vec::new();
    while let Some(kind) = cursor.kind() {
        if delimiters.is_empty() && end.contains(&kind) {
            break;
        }
        match kind {
            TokenKind::LeftParen => delimiters.push(TokenKind::RightParen),
            TokenKind::LeftBracket => delimiters.push(TokenKind::RightBracket),
            TokenKind::LeftBrace => delimiters.push(TokenKind::RightBrace),
            _ if delimiters.last() == Some(&kind) => {
                delimiters.pop();
            }
            _ => {}
        }
        cursor.bump();
    }
}

fn nominal_id(ty: &Type) -> Option<DeclarationId> {
    match ty {
        Type::Nominal(id, _) | Type::DynamicInterface(id, _) => Some(*id),
        _ => None,
    }
}

fn parse_signed_literal(text: &str) -> Option<i128> {
    parse_unsigned_literal(text).map(i128::from)
}

fn member_id(owner: DeclarationId, name: &str, discriminator: usize) -> MemberId {
    let mut hasher = Sha256::new();
    hasher.update(owner.0.to_be_bytes());
    hasher.update(name.len().to_be_bytes());
    hasher.update(name.as_bytes());
    hasher.update(discriminator.to_be_bytes());
    let digest = hasher.finalize();
    MemberId(u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix is eight bytes"),
    ))
}

fn validate_coherence_and_inheritance(
    definitions: &[Definition],
    graph: &ModuleGraph,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut implementations = BTreeMap::new();
    for definition in definitions {
        let is_nominal = matches!(
            definition.data,
            DefinitionData::Struct { .. }
                | DefinitionData::Enum { .. }
                | DefinitionData::Class { .. }
        );
        if is_nominal && let Some(declaration) = graph.declaration(definition.declaration) {
            let (DefinitionData::Struct { interfaces, .. }
            | DefinitionData::Enum { interfaces, .. }
            | DefinitionData::Class { interfaces, .. }) = &definition.data
            else {
                unreachable!("nominal definition has no interface list")
            };
            let mut conformances = BTreeSet::new();
            for interface in interfaces {
                if let Some(interface) = nominal_id(interface)
                    && !conformances.insert(interface)
                {
                    diagnostics.push(diag(
                        "TYPE_INCOHERENT_IMPLEMENTATION",
                        "duplicate interface conformance",
                        &declaration.span,
                        "declare each interface conformance at most once",
                    ));
                }
            }
        }
        if let DefinitionData::Implementation {
            interface: Some(interface),
            target,
            ..
        } = &definition.data
            && let Some(target) = nominal_id(target)
            && let Some(interface_id) = nominal_id(interface)
        {
            if implementations
                .insert((interface.clone(), target), definition.declaration)
                .is_some()
                && let Some(declaration) = graph.declaration(definition.declaration)
            {
                diagnostics.push(diag(
                    "TYPE_INCOHERENT_IMPLEMENTATION",
                    "duplicate interface implementation",
                    &declaration.span,
                    "exactly one implementation may exist for this interface and nominal type",
                ));
            }
            let interface_module = graph.declaration(interface_id).map(|item| item.module);
            let target_module = graph.declaration(target).map(|item| item.module);
            let implementation_module = graph
                .declaration(definition.declaration)
                .map(|item| item.module);
            if implementation_module != interface_module
                && implementation_module != target_module
                && let Some(declaration) = graph.declaration(definition.declaration)
            {
                diagnostics.push(diag(
                    "TYPE_ORPHAN_IMPLEMENTATION",
                    "implementation violates the coherence ownership rule",
                    &declaration.span,
                    "declare the implementation with its interface or nominal type",
                ));
            }
        }
    }
    validate_class_cycles(definitions, graph, diagnostics);
}

fn validate_class_cycles(
    definitions: &[Definition],
    graph: &ModuleGraph,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let bases = definitions
        .iter()
        .filter_map(|definition| match &definition.data {
            DefinitionData::Class {
                base: Some(base), ..
            } => Some((definition.declaration, *base)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for start in bases.keys().copied() {
        let mut seen = BTreeSet::new();
        let mut current = start;
        while let Some(base) = bases.get(&current).copied() {
            if !seen.insert(current) || base == start {
                if let Some(declaration) = graph.declaration(start) {
                    diagnostics.push(diag(
                        "TYPE_CLASS_INHERITANCE_CYCLE",
                        "class inheritance contains a cycle",
                        &declaration.span,
                        "a class cannot transitively extend itself",
                    ));
                }
                break;
            }
            current = base;
        }
    }
}

fn diag(id: &str, message: impl Into<String>, span: &SourceSpan, label: &str) -> Diagnostic {
    Diagnostic::error(
        ConditionId::new(id).expect("static condition is valid"),
        message,
        Label {
            span: span.clone(),
            message: label.into(),
        },
        id.to_ascii_lowercase().replace('_', "/"),
    )
}
