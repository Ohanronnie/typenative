use crate::{
    Attribute, Declaration, DeclarationId, DeclarationKind, Import, ImportClause, ImportName,
    Module, ModuleGraph, ModuleId,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};
use tn_syntax::{Token, TokenKind, lex, parse};

/// Loads, parses, and resolves the complete local and standard-library module graph.
///
/// # Errors
///
/// Returns all syntax and resolution diagnostics together, or an I/O error when a source file
/// cannot be read.
#[allow(clippy::too_many_lines)]
pub fn load_module_graph(
    root: &Path,
    entry: &Path,
    standard_library: &Path,
) -> Result<ModuleGraph, ModuleGraphError> {
    let root = normalize_existing(root)?;
    let entry = normalize_existing(entry)?;
    let standard_library = normalize_existing(standard_library)?;
    let runtime_root = entry.parent().and_then(|parent| {
        if parent.file_name().is_some_and(|name| name == "platform") {
            parent.parent().map(Path::to_path_buf)
        } else if parent.file_name().is_some_and(|name| name == "runtime") {
            Some(parent.to_path_buf())
        } else {
            None
        }
    });
    let mut pending = VecDeque::from([entry.clone()]);
    let string_prelude = standard_library.join("string.tn");
    if string_prelude.is_file() {
        pending.push_back(normalize_existing(&string_prelude)?);
    }
    let collections_prelude = standard_library.join("collections.tn");
    if collections_prelude.is_file() {
        pending.push_back(normalize_existing(&collections_prelude)?);
    }
    let mut discovered = BTreeSet::new();
    let mut raw_modules = BTreeMap::new();
    let mut diagnostics = Vec::new();

    while let Some(path) = pending.pop_front() {
        if !discovered.insert(path.clone()) {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let file = path.to_string_lossy();
        let lexed = lex(&file, &bytes);
        let parsed = parse(&file, &bytes);
        diagnostics.extend_from_slice(parsed.diagnostics());
        if lexed.source.is_empty() && !bytes.is_empty() {
            continue;
        }
        let raw = scan_module(&path, lexed.source, &lexed.tokens);
        for import in &raw.imports {
            match resolve_specifier(&path, &import.specifier, &standard_library) {
                Ok(target) => pending.push_back(target),
                Err(message) => diagnostics.push(diagnostic(
                    "RESOLVE_INVALID_MODULE_SPECIFIER",
                    message,
                    &import.span,
                    "module specifier cannot be resolved",
                )),
            }
        }
        raw_modules.insert(path, raw);
    }

    let mut modules = Vec::with_capacity(raw_modules.len());
    for (path, raw) in raw_modules {
        let id = module_id(&path, &root, &standard_library);
        let mut imports = Vec::new();
        for raw_import in raw.imports {
            if let Ok(target) = resolve_specifier(&path, &raw_import.specifier, &standard_library) {
                imports.push(Import {
                    specifier: raw_import.specifier,
                    target: module_id(&target, &root, &standard_library),
                    clause: raw_import.clause,
                    span: raw_import.span,
                });
            }
        }
        let declarations = raw
            .declarations
            .into_iter()
            .map(|raw| Declaration {
                id: declaration_id(id, raw.kind, raw.name.as_deref(), raw.span.byte_start),
                module: id,
                kind: raw.kind,
                name: raw.name,
                exported: raw.exported,
                attributes: raw.attributes,
                span: raw.span,
                byte_start: raw.byte_start,
                byte_end: raw.byte_end,
            })
            .collect();
        modules.push(Module {
            id,
            path,
            source: raw.source,
            imports,
            declarations,
        });
    }
    let entry_id = module_id(&entry, &root, &standard_library);
    let graph = ModuleGraph {
        root,
        standard_library,
        runtime_root,
        entry: entry_id,
        modules,
    };
    validate_bindings(&graph, &mut diagnostics);
    if diagnostics.is_empty() {
        Ok(graph)
    } else {
        Err(ModuleGraphError::Diagnostics(diagnostics))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModuleGraphError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("module graph contains resolution diagnostics")]
    Diagnostics(Vec<Diagnostic>),
}

impl ModuleGraphError {
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Io(_) => &[],
            Self::Diagnostics(diagnostics) => diagnostics,
        }
    }
}

struct RawModule {
    source: String,
    imports: Vec<RawImport>,
    declarations: Vec<RawDeclaration>,
}

struct RawImport {
    specifier: String,
    clause: ImportClause,
    span: SourceSpan,
}

struct RawDeclaration {
    kind: DeclarationKind,
    name: Option<String>,
    exported: bool,
    attributes: Vec<Attribute>,
    span: SourceSpan,
    byte_start: u32,
    byte_end: u32,
}

fn scan_module(path: &Path, source: &str, tokens: &[Token]) -> RawModule {
    let significant = tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .collect::<Vec<_>>();
    let file = path.to_string_lossy();
    let mut imports = Vec::new();
    let mut declarations = Vec::new();
    let mut index = 0;
    let mut depth = 0_u32;
    let mut exported = false;
    let mut attributes = Vec::new();
    while index < significant.len() {
        let token = significant[index];
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => depth = depth.saturating_sub(1),
            TokenKind::Export if depth == 0 => exported = true,
            TokenKind::At if depth == 0 => {
                if let Some(attribute) = scan_attribute(&significant, index, source, &file) {
                    attributes.push(attribute);
                }
            }
            TokenKind::Import if depth == 0 => {
                let end = significant[index..]
                    .iter()
                    .position(|candidate| candidate.kind == TokenKind::Semicolon)
                    .map_or(significant.len(), |offset| index + offset + 1);
                if let Some(import) = scan_import(&file, source, &significant[index..end]) {
                    imports.push(import);
                }
                index = end;
                exported = false;
                continue;
            }
            kind if depth == 0 && declaration_kind_at(&significant, index, kind).is_some() => {
                let kind = declaration_kind_at(&significant, index, kind)
                    .expect("guard established declaration kind");
                let name = declaration_name(&significant, index, kind)
                    .map(|candidate| source[candidate.range.clone()].to_owned());
                let byte_end = declaration_end(&significant, index, kind, source.len());
                declarations.push(RawDeclaration {
                    kind,
                    name,
                    exported,
                    attributes: std::mem::take(&mut attributes),
                    span: SourceSpan::new(&*file, token.range.clone(), source),
                    byte_start: u32::try_from(declaration_start(&significant, index))
                        .unwrap_or(u32::MAX),
                    byte_end: u32::try_from(byte_end).unwrap_or(u32::MAX),
                });
                exported = false;
                while index < significant.len() && significant[index].range.start < byte_end {
                    index += 1;
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    RawModule {
        source: source.to_owned(),
        imports,
        declarations,
    }
}

fn scan_attribute(tokens: &[&Token], index: usize, source: &str, file: &str) -> Option<Attribute> {
    let name = tokens.get(index + 1)?;
    if !matches!(
        name.kind,
        TokenKind::Identifier | TokenKind::Unknown | TokenKind::Export
    ) {
        return None;
    }
    let mut arguments = Vec::new();
    let mut cursor = index + 2;
    if tokens
        .get(cursor)
        .is_some_and(|token| token.kind == TokenKind::LeftParen)
    {
        cursor += 1;
        let mut depth = 1_u32;
        while let Some(token) = tokens.get(cursor) {
            match token.kind {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::Comma if depth == 1 => {}
                kind if depth == 1 && !kind.is_trivia() => {
                    arguments.push(source[token.range.clone()].trim_matches('"').to_owned());
                }
                _ => {}
            }
            cursor += 1;
        }
    }
    Some(Attribute {
        name: source[name.range.clone()].to_owned(),
        arguments,
        span: SourceSpan::new(file, tokens[index].range.start..name.range.end, source),
    })
}

fn declaration_start(tokens: &[&Token], index: usize) -> usize {
    let mut start = index;
    while start > 0
        && matches!(
            tokens[start - 1].kind,
            TokenKind::Unsafe | TokenKind::Async | TokenKind::Abstract | TokenKind::Final
        )
    {
        start -= 1;
    }
    tokens[start].range.start
}

fn declaration_name<'tokens>(
    tokens: &'tokens [&Token],
    index: usize,
    kind: DeclarationKind,
) -> Option<&'tokens Token> {
    if matches!(kind, DeclarationKind::Impl | DeclarationKind::ExternBlock) {
        return None;
    }
    tokens[index + 1..]
        .iter()
        .take(8)
        .copied()
        .find(|token| token.kind == TokenKind::Identifier)
}

fn declaration_end(
    tokens: &[&Token],
    start: usize,
    kind: DeclarationKind,
    source_length: usize,
) -> usize {
    if matches!(
        kind,
        DeclarationKind::Const | DeclarationKind::Static | DeclarationKind::TypeAlias
    ) {
        return tokens[start..]
            .iter()
            .find(|token| token.kind == TokenKind::Semicolon)
            .map_or(source_length, |token| token.range.end);
    }
    let Some(open_offset) = tokens[start..]
        .iter()
        .position(|token| token.kind == TokenKind::LeftBrace)
    else {
        return source_length;
    };
    let mut depth = 0_u32;
    for token in &tokens[start + open_offset..] {
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return token.range.end;
                }
            }
            _ => {}
        }
    }
    source_length
}

fn scan_import(file: &str, source: &str, tokens: &[&Token]) -> Option<RawImport> {
    let specifier_token = tokens
        .iter()
        .rev()
        .find(|token| token.kind == TokenKind::StringLiteral)?;
    let quoted = &source[specifier_token.range.clone()];
    let specifier = quoted[1..quoted.len().saturating_sub(1)].to_owned();
    let span = SourceSpan::new(file, specifier_token.range.clone(), source);
    let clause = if tokens
        .get(1)
        .is_some_and(|token| token.kind == TokenKind::StringLiteral)
    {
        ImportClause::SideEffect
    } else if tokens
        .get(1)
        .is_some_and(|token| token.kind == TokenKind::Star)
    {
        let local = tokens
            .iter()
            .find(|token| token.kind == TokenKind::Identifier)?;
        ImportClause::Namespace {
            local: source[local.range.clone()].to_owned(),
            span: SourceSpan::new(file, local.range.clone(), source),
        }
    } else {
        let from = tokens
            .iter()
            .position(|token| token.kind == TokenKind::From)?;
        let mut names = Vec::new();
        let mut index = 2;
        while index < from {
            if tokens[index].kind != TokenKind::Identifier {
                index += 1;
                continue;
            }
            let imported = source[tokens[index].range.clone()].to_owned();
            let imported_span = SourceSpan::new(file, tokens[index].range.clone(), source);
            index += 1;
            let local = if tokens
                .get(index)
                .is_some_and(|token| token.kind == TokenKind::As)
            {
                index += 1;
                let Some(token) = tokens.get(index) else {
                    break;
                };
                let name = source[token.range.clone()].to_owned();
                index += 1;
                name
            } else {
                imported.clone()
            };
            names.push(ImportName {
                imported,
                local,
                span: imported_span,
            });
        }
        ImportClause::Named(names)
    };
    Some(RawImport {
        specifier,
        clause,
        span,
    })
}

const fn declaration_kind(kind: TokenKind) -> Option<DeclarationKind> {
    match kind {
        TokenKind::Const => Some(DeclarationKind::Const),
        TokenKind::Static => Some(DeclarationKind::Static),
        TokenKind::Type => Some(DeclarationKind::TypeAlias),
        TokenKind::Function => Some(DeclarationKind::Function),
        TokenKind::Struct => Some(DeclarationKind::Struct),
        TokenKind::Class => Some(DeclarationKind::Class),
        TokenKind::Interface => Some(DeclarationKind::Interface),
        TokenKind::Enum => Some(DeclarationKind::Enum),
        TokenKind::Impl => Some(DeclarationKind::Impl),
        TokenKind::Declare => Some(DeclarationKind::ExternBlock),
        _ => None,
    }
}

fn declaration_kind_at(
    tokens: &[&Token],
    index: usize,
    kind: TokenKind,
) -> Option<DeclarationKind> {
    if kind != TokenKind::Extern {
        return declaration_kind(kind);
    }
    match (
        tokens.get(index + 1).map(|token| token.kind),
        tokens.get(index + 2).map(|token| token.kind),
    ) {
        (Some(TokenKind::Struct), _) => Some(DeclarationKind::ExternStruct),
        (Some(TokenKind::StringLiteral), Some(TokenKind::Function)) => {
            Some(DeclarationKind::ExternFunction)
        }
        _ => None,
    }
}

fn resolve_specifier(
    importer: &Path,
    specifier: &str,
    standard_library: &Path,
) -> Result<PathBuf, String> {
    if specifier.contains('\\')
        || Path::new(specifier)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tn"))
    {
        return Err(format!(
            "module specifier must omit the .tn suffix: {specifier}"
        ));
    }
    let candidate = if let Some(relative) = specifier.strip_prefix("std/") {
        standard_library.join(relative).with_extension("tn")
    } else if specifier.starts_with("./") || specifier.starts_with("../") {
        importer
            .parent()
            .unwrap_or(Path::new("."))
            .join(specifier)
            .with_extension("tn")
    } else {
        return Err(format!(
            "bare package specifiers are not supported: {specifier}"
        ));
    };
    normalize_existing(&candidate)
        .map_err(|_| format!("module does not resolve to one source file: {specifier}"))
}

fn normalize_existing(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if canonical.is_file()
        && canonical
            .extension()
            .is_none_or(|extension| extension != "tn")
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "module source does not use the .tn suffix",
        ));
    }
    Ok(canonical)
}

fn normalized_identity(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            Component::RootDir => Some("/".into()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn stable_hash(parts: &[&str]) -> u64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.len().to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix is eight bytes"),
    )
}

fn logical_identity(path: &Path, root: &Path, standard_library: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(standard_library) {
        return format!("std/{}", normalized_identity(relative));
    }
    if let Ok(relative) = path.strip_prefix(root) {
        return format!("project/{}", normalized_identity(relative));
    }
    normalized_identity(path)
}

fn module_id(path: &Path, root: &Path, standard_library: &Path) -> ModuleId {
    ModuleId(stable_hash(&[&logical_identity(
        path,
        root,
        standard_library,
    )]))
}

fn declaration_id(
    module: ModuleId,
    kind: DeclarationKind,
    name: Option<&str>,
    offset: u32,
) -> DeclarationId {
    DeclarationId(stable_hash(&[
        &module.0.to_string(),
        &format!("{kind:?}"),
        name.unwrap_or(""),
        &offset.to_string(),
    ]))
}

fn validate_bindings(graph: &ModuleGraph, diagnostics: &mut Vec<Diagnostic>) {
    for module in &graph.modules {
        let mut locals = BTreeMap::new();
        for declaration in &module.declarations {
            let Some(name) = declaration.name.as_deref() else {
                continue;
            };
            let Some(namespace) = declaration.kind.namespace() else {
                continue;
            };
            if locals.insert((namespace, name), declaration).is_some() {
                diagnostics.push(diagnostic(
                    "RESOLVE_DUPLICATE_DECLARATION",
                    format!("duplicate declaration of {name}"),
                    &declaration.span,
                    "this name is already declared in the same namespace",
                ));
            }
        }
        let mut imported_locals = BTreeSet::new();
        for import in &module.imports {
            let Some(target) = graph.module(import.target) else {
                continue;
            };
            if let ImportClause::Named(names) = &import.clause {
                for name in names {
                    let exported = target.declarations.iter().any(|declaration| {
                        declaration.exported && declaration.name.as_deref() == Some(&name.imported)
                    });
                    if !exported {
                        diagnostics.push(diagnostic(
                            "RESOLVE_MISSING_EXPORT",
                            format!(
                                "module {} does not export {}",
                                import.specifier, name.imported
                            ),
                            &name.span,
                            "no accessible exported declaration has this name",
                        ));
                    }
                    if !imported_locals.insert(name.local.as_str()) {
                        diagnostics.push(diagnostic(
                            "RESOLVE_AMBIGUOUS_IMPORT",
                            format!("multiple imports bind {}", name.local),
                            &name.span,
                            "rename one import with `as`",
                        ));
                    }
                }
            }
        }
    }
}

fn diagnostic(id: &str, message: impl Into<String>, span: &SourceSpan, label: &str) -> Diagnostic {
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
