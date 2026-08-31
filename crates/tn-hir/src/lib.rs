//! Resolved `TypeNative` high-level intermediate representation.

mod module_graph;
mod semantic;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tn_diagnostics::SourceSpan;

pub use module_graph::{ModuleGraphError, load_module_graph, load_module_graph_with_jsx_runtime};
pub use semantic::lower_program;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ModuleId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct DeclarationId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct TypeId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MemberId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirLocalId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Namespace {
    Type,
    Value,
    Method,
    Lifetime,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeclarationKind {
    Const,
    Static,
    TypeAlias,
    Function,
    Struct,
    Class,
    Interface,
    Enum,
    Impl,
    ExternBlock,
    ExternStruct,
    ExternFunction,
}

impl DeclarationKind {
    pub const fn namespace(self) -> Option<Namespace> {
        match self {
            Self::Const | Self::Static | Self::Function | Self::ExternFunction => {
                Some(Namespace::Value)
            }
            Self::TypeAlias
            | Self::Struct
            | Self::Class
            | Self::Interface
            | Self::Enum
            | Self::ExternStruct => Some(Namespace::Type),
            Self::Impl | Self::ExternBlock => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attribute {
    pub name: String,
    pub arguments: Vec<String>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Declaration {
    pub id: DeclarationId,
    pub module: ModuleId,
    pub kind: DeclarationKind,
    pub name: Option<String>,
    pub exported: bool,
    pub attributes: Vec<Attribute>,
    pub span: SourceSpan,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportName {
    pub imported: String,
    pub local: String,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ImportClause {
    SideEffect,
    Named(Vec<ImportName>),
    Namespace { local: String, span: SourceSpan },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Import {
    pub specifier: String,
    pub target: ModuleId,
    pub clause: ImportClause,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Module {
    pub id: ModuleId,
    pub path: PathBuf,
    pub source: String,
    pub imports: Vec<Import>,
    pub declarations: Vec<Declaration>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModuleGraph {
    pub root: PathBuf,
    pub standard_library: PathBuf,
    pub runtime_root: Option<PathBuf>,
    pub jsx_runtime: Option<String>,
    pub jsx_runtime_module: Option<ModuleId>,
    pub entry: ModuleId,
    pub modules: Vec<Module>,
}

impl ModuleGraph {
    pub fn module(&self, id: ModuleId) -> Option<&Module> {
        self.modules.iter().find(|module| module.id == id)
    }

    pub fn declaration(&self, id: DeclarationId) -> Option<&Declaration> {
        self.modules
            .iter()
            .flat_map(|module| &module.declarations)
            .find(|declaration| declaration.id == id)
    }

    pub fn is_bundled_module(&self, module: ModuleId, relative_path: &str) -> bool {
        self.module(module).is_some_and(|module| {
            module.path == self.standard_library.join(relative_path)
                || self
                    .runtime_root
                    .as_ref()
                    .is_some_and(|root| module.path == root.join(relative_path))
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum PrimitiveType {
    Bool,
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    Char,
    Void,
    Never,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Type {
    Primitive(PrimitiveType),
    String,
    Str,
    Promise {
        result: Box<Type>,
        error: Box<Type>,
        effects: Vec<DeclarationId>,
    },
    Nominal(DeclarationId, Vec<Type>),
    Optional(Box<Type>),
    Array(Box<Type>, u64),
    Slice(Box<Type>),
    Tuple(Vec<Type>),
    Reference {
        mutable: bool,
        lifetime: String,
        referent: Box<Type>,
    },
    RawPointer {
        mutable: bool,
        pointee: Box<Type>,
    },
    Function(FunctionType),
    Template(Vec<Type>),
    DynamicInterface(DeclarationId, Vec<Type>),
    Generic(String),
    Lifetime(String),
    ErrorUnion(Vec<DeclarationId>),
    Error,
    Unknown,
}

/// Returns the statically known error declarations carried by a Promise error type.
///
/// Generic error parameters are intentionally left unresolved until a concrete
/// specialization is available. Callers may pass the prior effect set so a
/// substitution that remains generic does not discard information.
pub fn promise_effects(error: &Type, prior: &[DeclarationId]) -> Vec<DeclarationId> {
    match error {
        Type::Nominal(id, _) => vec![*id],
        Type::Generic(_) | Type::Error => prior.to_vec(),
        _ => Vec::new(),
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FunctionType {
    pub parameters: Vec<Type>,
    pub result: Box<Type>,
    pub effects: Vec<DeclarationId>,
    pub generics: Vec<GenericConstraint>,
    pub is_async: bool,
    pub is_unsafe: bool,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct GenericConstraint {
    pub name: String,
    pub namespace: Namespace,
    pub bounds: Vec<GenericBound>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Visibility {
    Private,
    Protected,
    Public,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GenericParameter {
    pub name: String,
    pub namespace: Namespace,
    pub bounds: Vec<GenericBound>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum GenericBound {
    Interface(DeclarationId, Vec<Type>),
    Static,
    Outlives(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BindingPattern {
    pub kind: BindingPatternKind,
    pub default: Option<SourceSpan>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BindingPatternKind {
    Identifier {
        name: String,
        mutable: bool,
    },
    Array {
        elements: Vec<Option<BindingPattern>>,
        rest: Option<Box<BindingPattern>>,
    },
    Object {
        properties: Vec<BindingProperty>,
        rest: Option<Box<BindingPattern>>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BindingProperty {
    pub key: String,
    pub pattern: BindingPattern,
    pub span: SourceSpan,
}

impl BindingPattern {
    pub fn identifier(name: String, mutable: bool, span: SourceSpan) -> Self {
        Self {
            kind: BindingPatternKind::Identifier { name, mutable },
            default: None,
            span,
        }
    }

    pub fn primary_name(&self) -> Option<&str> {
        match &self.kind {
            BindingPatternKind::Identifier { name, .. } if name != "_" => Some(name),
            BindingPatternKind::Array { elements, rest } => elements
                .iter()
                .flatten()
                .find_map(Self::primary_name)
                .or_else(|| rest.as_deref().and_then(Self::primary_name)),
            BindingPatternKind::Object { properties, rest } => properties
                .iter()
                .find_map(|property| property.pattern.primary_name())
                .or_else(|| rest.as_deref().and_then(Self::primary_name)),
            BindingPatternKind::Identifier { .. } => None,
        }
    }

    pub fn is_simple_identifier(&self) -> bool {
        matches!(self.kind, BindingPatternKind::Identifier { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub ty: Type,
    pub pattern: BindingPattern,
    pub default: Option<SourceSpan>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Function {
    pub parameters: Vec<Parameter>,
    pub result: Type,
    pub effects: Vec<DeclarationId>,
    pub generics: Vec<GenericParameter>,
    pub is_async: bool,
    pub is_generator: bool,
    pub is_unsafe: bool,
    pub body_start: u32,
    pub body_end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub id: MemberId,
    pub name: String,
    pub ty: Type,
    pub visibility: Visibility,
    pub readonly: bool,
    pub optional: bool,
    pub has_initializer: bool,
    pub span: SourceSpan,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum ReceiverMode {
    Shared,
    Mutable,
    Move,
    Static,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Method {
    pub id: MemberId,
    pub name: String,
    pub attributes: Vec<Attribute>,
    pub function: Function,
    pub visibility: Visibility,
    pub receiver: ReceiverMode,
    pub is_abstract: bool,
    pub is_override: bool,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnumField {
    pub name: Option<String>,
    pub ty: Type,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnumVariant {
    pub id: MemberId,
    pub name: String,
    pub fields: Vec<EnumField>,
    pub discriminant: Option<i128>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DefinitionData {
    Constant {
        ty: Type,
        mutable_static: bool,
    },
    TypeAlias(Type),
    Function(Function),
    Struct {
        c_layout: bool,
        interfaces: Vec<Type>,
        fields: Vec<Field>,
        methods: Vec<Method>,
    },
    Enum {
        repr: Option<PrimitiveType>,
        interfaces: Vec<Type>,
        variants: Vec<EnumVariant>,
        methods: Vec<Method>,
    },
    Interface {
        methods: Vec<Method>,
    },
    Class {
        base: Option<DeclarationId>,
        interfaces: Vec<Type>,
        fields: Vec<Field>,
        constructor: Option<Method>,
        methods: Vec<Method>,
        is_abstract: bool,
    },
    Implementation {
        interface: Option<Type>,
        target: Type,
        methods: Vec<Method>,
        is_unsafe: bool,
    },
    Extern {
        functions: Vec<Method>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub declaration: DeclarationId,
    pub generics: Vec<GenericParameter>,
    pub data: DefinitionData,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub graph: ModuleGraph,
    pub definitions: Vec<Definition>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum BodyOwner {
    Declaration(DeclarationId),
    Member {
        declaration: DeclarationId,
        member: MemberId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirLocal {
    pub id: HirLocalId,
    pub name: String,
    pub ty: Type,
    pub mutable: bool,
    pub origin: SourceSpan,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirExpressionId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirStatementId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirClosureId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirTemplateId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirBindingPatternId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HirJsxId(pub u32);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirBindingPattern {
    pub id: HirBindingPatternId,
    pub root: HirLocalId,
    pub bindings: Vec<HirPatternBinding>,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirJsxElement {
    pub id: HirJsxId,
    pub component: Option<HirExpressionId>,
    pub properties: Vec<HirJsxProperty>,
    pub children: Vec<HirJsxChild>,
    pub properties_type: Type,
    pub element_type: Type,
    pub key: Option<HirExpressionId>,
    pub reference: Option<HirExpressionId>,
    pub fragment: bool,
    pub runtime: Option<DeclarationId>,
    pub runtime_signature: Option<FunctionType>,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirJsxProperty {
    pub name: Option<String>,
    pub value: HirJsxValue,
    pub spread: bool,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HirJsxValue {
    Expression(HirExpressionId),
    Boolean(bool),
    String(String),
    Children(Vec<HirJsxChild>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HirJsxChild {
    Element(HirJsxId),
    Expression(HirExpressionId),
    Text { value: String, origin: SourceSpan },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum ResolvedValue {
    Local(HirLocalId),
    Declaration(DeclarationId),
    Member(MemberId),
    StringLength,
    StringByteLength,
    Closure(HirClosureId),
    Template(HirTemplateId),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirCaptureMode {
    SharedBorrow,
    MutableBorrow,
    Move,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirClosureCapture {
    pub local: HirLocalId,
    pub name: String,
    pub ty: Type,
    pub mode: HirCaptureMode,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirClosure {
    pub id: HirClosureId,
    pub function: FunctionType,
    pub parameters: Vec<HirLocalId>,
    pub captures: Vec<HirClosureCapture>,
    pub moved: bool,
    pub body: SourceSpan,
    pub origin: SourceSpan,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirTemplateStorage {
    SharedBorrow,
    Owned,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HirTemplatePart {
    Literal(String),
    Interpolation {
        expression: HirExpressionId,
        ty: Type,
        storage: HirTemplateStorage,
        origin: SourceSpan,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirTemplate {
    pub id: HirTemplateId,
    pub parts: Vec<HirTemplatePart>,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirExpressionKind {
    Literal,
    Conversion(HirConversionKind),
    Value,
    Borrow { mutable: bool },
    Move,
    Unary,
    Binary,
    Conditional,
    Call,
    Member,
    Index,
    Aggregate,
    Closure,
    Cast,
    Switch,
    Await,
    Jsx(HirJsxId),
    Error,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirConversionKind {
    StringLiteralToOwned,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirExpression {
    pub id: HirExpressionId,
    pub kind: HirExpressionKind,
    pub ty: Type,
    pub optional_chain_value: Option<Type>,
    pub effects: Vec<DeclarationId>,
    pub resolution: Option<ResolvedValue>,
    pub children: Vec<HirExpressionId>,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirStatementKind {
    Block,
    Local(HirLocalId),
    Expression,
    Return,
    Yield,
    Throw,
    If,
    While,
    For {
        binding: HirLocalId,
        awaited: bool,
        witness: Option<Box<IterationWitness>>,
    },
    Try,
    Unsafe,
    Break,
    Continue,
    Using {
        local: HirLocalId,
        awaited: bool,
    },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum IterationWitness {
    Declared {
        into_iterator_implementation: DeclarationId,
        into_iterator_method: MemberId,
        iterator_implementation: DeclarationId,
        next_method: MemberId,
        next_receiver: ReceiverMode,
        iterator_type: Type,
        item_type: Type,
    },
    Generator {
        item_type: Type,
        asynchronous: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirStatement {
    pub id: HirStatementId,
    pub kind: HirStatementKind,
    pub expressions: Vec<HirExpressionId>,
    pub children: Vec<HirStatementId>,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirPatternBinding {
    pub local: HirLocalId,
    pub ty: Type,
    pub projection: Vec<HirPatternProjection>,
    pub default: Option<SourceSpan>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirPatternProjection {
    Variant(MemberId),
    Field(u32),
    Index(u32),
    Rest { start: u32 },
    OptionalPayload,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HirPattern {
    pub scrutinee: Type,
    pub constructor: Option<MemberId>,
    pub bindings: Vec<HirPatternBinding>,
    pub guarded: bool,
    pub origin: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BodyHir {
    pub owner: BodyOwner,
    pub locals: Vec<HirLocal>,
    pub parameter_roots: Vec<HirLocalId>,
    pub expressions: Vec<HirExpression>,
    pub patterns: Vec<HirPattern>,
    pub binding_patterns: Vec<HirBindingPattern>,
    pub jsx_elements: Vec<HirJsxElement>,
    pub statements: Vec<HirStatement>,
    pub closures: Vec<HirClosure>,
    pub templates: Vec<HirTemplate>,
    pub roots: Vec<HirStatementId>,
}

impl Program {
    /// Resolves an exported declaration from the configured JSX runtime module, regardless of its
    /// declaration kind. The driver uses this to distinguish a missing export from an export that
    /// exists but is not callable.
    pub fn jsx_runtime_export(&self, operation: &str) -> Option<DeclarationId> {
        let module = self.graph.jsx_runtime_module?;
        self.graph
            .module(module)?
            .declarations
            .iter()
            .find(|declaration| {
                declaration.exported && declaration.name.as_deref() == Some(operation)
            })
            .map(|declaration| declaration.id)
    }

    /// Resolves one of the callable declarations required by the configured JSX runtime.
    ///
    /// The declaration is looked up by resolved module identity, so a source declaration cannot
    /// impersonate a runtime operation merely by using the same name.
    pub fn jsx_runtime_declaration(&self, operation: &str) -> Option<DeclarationId> {
        let module = self.graph.jsx_runtime_module?;
        self.graph
            .module(module)?
            .declarations
            .iter()
            .find(|declaration| {
                declaration.exported
                    && declaration.kind == DeclarationKind::Function
                    && declaration.name.as_deref() == Some(operation)
            })
            .map(|declaration| declaration.id)
    }

    /// Resolves the external symbol name from the private compiler manifest. Ordinary exported
    /// user declarations use their source name; bundled runtime declarations retain their stable
    /// C ABI names without a source decorator.
    pub fn export_name_for_declaration(&self, declaration: DeclarationId) -> String {
        let Some(item) = self.graph.declaration(declaration) else {
            return "exported".into();
        };
        let Some(name) = item.name.as_deref() else {
            return "exported".into();
        };
        if self.graph.is_bundled_module(item.module, "runtime.tn") {
            return runtime_export_name(name);
        }
        if self
            .graph
            .is_bundled_module(item.module, "platform/linux-x86_64.tn")
            || self
                .graph
                .is_bundled_module(item.module, "platform/darwin-arm64.tn")
        {
            return platform_export_name(name);
        }
        name.into()
    }

    /// Returns whether a declaration is in the compiler-approved C-layout manifest.
    pub fn has_c_layout(&self, declaration: DeclarationId) -> bool {
        let Some(item) = self.graph.declaration(declaration) else {
            return false;
        };
        if self.graph.is_bundled_module(item.module, "runtime.tn")
            || self
                .graph
                .is_bundled_module(item.module, "platform/linux-x86_64.tn")
            || self
                .graph
                .is_bundled_module(item.module, "platform/darwin-arm64.tn")
        {
            return true;
        }
        match self
            .definition(declaration)
            .map(|definition| &definition.data)
        {
            Some(DefinitionData::Struct { c_layout, .. }) => *c_layout,
            Some(DefinitionData::Enum { repr, .. }) => repr.is_some(),
            _ => false,
        }
    }

    pub fn implemented_interfaces(&self, declaration: DeclarationId) -> Vec<DeclarationId> {
        let Some(definition) = self.definition(declaration) else {
            return Vec::new();
        };
        let (DefinitionData::Struct { interfaces, .. }
        | DefinitionData::Enum { interfaces, .. }
        | DefinitionData::Class { interfaces, .. }) = &definition.data
        else {
            return Vec::new();
        };
        let mut result = interfaces
            .iter()
            .filter_map(|interface| match interface {
                Type::Nominal(id, _) | Type::DynamicInterface(id, _) => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        result.sort_unstable();
        result.dedup();
        result
    }

    /// Returns the compiler-owned intrinsic operation for a trusted bundled declaration.
    ///
    /// This table is intentionally keyed by resolved module identity and declaration name. User
    /// source cannot opt into an operation by spelling a decorator with the same name.
    pub fn intrinsic_operation_for_declaration(
        &self,
        declaration: DeclarationId,
    ) -> Option<&'static str> {
        let item = self.graph.declaration(declaration)?;
        let name = item.name.as_deref()?;
        let bundled = |relative: &str| self.graph.is_bundled_module(item.module, relative);
        if bundled("runtime.tn") {
            return runtime_intrinsic(name);
        }
        if bundled("platform/linux-x86_64.tn") || bundled("platform/darwin-arm64.tn") {
            return platform_intrinsic(name);
        }
        if bundled("sync.tn") {
            return match name {
                "borrowMut" => Some("borrow_mut_direct"),
                "moveElement" => Some("move_element"),
                "storeElement" => Some("store_element"),
                "dropInitializedElements" => Some("drop_initialized_elements"),
                _ => None,
            };
        }
        if bundled("env.tn") {
            return (name == "checkedU16").then_some("checked_u16");
        }
        if bundled("bytes.tn") {
            return match name {
                "stringFromRaw" => Some("string_from_raw"),
                "strFromRawParts" => Some("str_from_raw_parts"),
                "sliceFromRawParts" => Some("slice_from_raw_parts"),
                "sliceLength" => Some("slice_length"),
                _ => None,
            };
        }
        if bundled("core.tn") {
            return core_intrinsic(name);
        }
        if bundled("string.tn") {
            return match name {
                "fromRaw" => Some("string_from_raw"),
                "borrowShared" => Some("borrow_shared_direct"),
                "strFromRawParts" => Some("str_from_raw_parts"),
                "sliceFromRawParts" => Some("slice_from_raw_parts"),
                "PlatformUnsigned" => Some("usize"),
                "OwnedString" => Some("string"),
                _ => None,
            };
        }
        if bundled("collections.tn") {
            return collections_intrinsic(name);
        }
        if bundled("alloc.tn") {
            return alloc_intrinsic(name);
        }
        if bundled("thread.tn") {
            return (name == "spawnTask").then_some("thread_spawn");
        }
        None
    }

    pub fn definition(&self, declaration: DeclarationId) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.declaration == declaration)
    }

    pub fn intrinsic_type_declaration(&self, ty: &Type) -> Option<DeclarationId> {
        let key = intrinsic_type_key(ty)?;
        self.definitions.iter().find_map(|definition| {
            (self.intrinsic_operation_for_declaration(definition.declaration) == Some(key))
                .then_some(definition.declaration)
        })
    }

    pub fn intrinsic_type_for_declaration(&self, declaration: DeclarationId) -> Option<Type> {
        self.intrinsic_operation_for_declaration(declaration)
            .and_then(intrinsic_type_from_key)
    }
}

fn runtime_intrinsic(name: &str) -> Option<&'static str> {
    match name {
        "isNullIntrinsic" => Some("is_null"),
        "nullPointerIntrinsic" => Some("null_pointer"),
        "borrowElement" | "borrowElementI32" => Some("borrow_element"),
        "sizeOfIntrinsic" => Some("size_of"),
        "storeRawIntrinsic" => Some("store_raw"),
        "platformSockAddrFamily" => Some("platform_sockaddr_family"),
        "platformSocketReuseAddressOption" => Some("platform_socket_reuse_address_option"),
        "platformSocketLevel" => Some("platform_socket_level"),
        "callRaw" => Some("call_raw"),
        "callRawVoid" => Some("call_raw_void"),
        "callRawPointer" => Some("call_raw_pointer"),
        "atomicUsizeLoad" => Some("atomic_usize_load"),
        "atomicUsizeCompareExchange" => Some("atomic_usize_compare_exchange"),
        "atomicFence" => Some("atomic_fence"),
        "byteAddress" => Some("byte_address"),
        "byteAddressI32" => Some("byte_address_i32"),
        "componentIdentity" => Some("component_identity"),
        _ => None,
    }
}

fn platform_intrinsic(name: &str) -> Option<&'static str> {
    match name {
        "platformIsNull" => Some("is_null"),
        "platformSizeOf" => Some("size_of"),
        "platformStoreRaw" => Some("store_raw"),
        "platformDirentByte" => Some("borrow_element"),
        "platformByteAddress" => Some("byte_address"),
        _ => None,
    }
}

fn core_intrinsic(name: &str) -> Option<&'static str> {
    match name {
        "atomicI32Load" => Some("atomic_i32_load"),
        "atomicI32Store" => Some("atomic_i32_store"),
        "atomicI32FetchAdd" => Some("atomic_i32_fetch_add"),
        "atomicI32CompareExchange" => Some("atomic_i32_compare_exchange"),
        "atomicU64Load" => Some("atomic_u64_load"),
        "atomicU64Store" => Some("atomic_u64_store"),
        "atomicU64FetchAdd" => Some("atomic_u64_fetch_add"),
        "atomicU64CompareExchange" => Some("atomic_u64_compare_exchange"),
        "atomicUsizeLoad" => Some("atomic_usize_load"),
        "atomicUsizeStore" => Some("atomic_usize_store"),
        "atomicUsizeFetchAdd" => Some("atomic_usize_fetch_add"),
        "atomicUsizeCompareExchange" => Some("atomic_usize_compare_exchange"),
        _ => None,
    }
}

fn collections_intrinsic(name: &str) -> Option<&'static str> {
    match name {
        "sizeOf" => Some("size_of"),
        "isString" => Some("is_string"),
        "isCopy" => Some("is_copy"),
        "elementInitialized" => Some("element_initialized"),
        "moveElement" => Some("move_element"),
        "dropElement" => Some("drop_element"),
        "dropValue" => Some("drop_value"),
        "storeElement" => Some("store_element"),
        "dropInitializedElements" => Some("drop_initialized_elements"),
        "borrowElement" => Some("borrow_element"),
        "borrowElementMut" => Some("borrow_element_mut"),
        "sliceFromRawParts" => Some("slice_from_raw_parts"),
        _ => None,
    }
}

fn alloc_intrinsic(name: &str) -> Option<&'static str> {
    match name {
        "sizeOf" => Some("size_of"),
        "isNull" | "isNullIntrinsic" => Some("is_null"),
        "nullPointer" | "nullPointerIntrinsic" => Some("null_pointer"),
        "borrowShared" => Some("borrow_shared_storage"),
        "rawAddress" => Some("borrow_mut_direct"),
        "storeRaw" => Some("store_raw"),
        "dropValue" => Some("drop_value"),
        "u64ToUsize" => Some("u64_to_usize"),
        "i32ToUsize" => Some("i32_to_usize"),
        "usizeToU64" => Some("usize_to_u64"),
        "usizeToF32" => Some("usize_to_f32"),
        "f64ToUsize" => Some("f64_to_usize"),
        "byteAddress" => Some("byte_address"),
        "componentIdentity" => Some("component_identity"),
        "borrowCallable" => Some("borrow_callable"),
        "arcClone" => Some("arc_clone"),
        "weakUpgrade" => Some("weak_upgrade"),
        "borrowMut" => Some("borrow_mut_storage"),
        _ => None,
    }
}

fn runtime_export_name(name: &str) -> String {
    let suffix = match name {
        name if name.starts_with("async") => format!("runtime_async_{}", snake_case(&name[5..])),
        "promiseWait" => "runtime_promise_wait".into(),
        "promiseTakeI32" => "runtime_promise_take_i32".into(),
        name if name.starts_with("rawPromise") => {
            format!("promise_{}", snake_case(&name[10..]))
        }
        name if name.starts_with("taskGroup") => {
            format!("task_group_{}", snake_case(&name[9..]))
        }
        "conditionCreate" => "cond_create".into(),
        "conditionWait" => "cond_wait".into(),
        "conditionSignal" => "cond_signal".into(),
        "conditionBroadcast" => "cond_broadcast".into(),
        "conditionDestroy" => "cond_destroy".into(),
        _ => snake_case(name),
    };
    format!("tn_{suffix}")
}

fn platform_export_name(name: &str) -> String {
    let suffix = match name {
        name if name.starts_with("directory") => format!("dir_{}", snake_case(&name[9..])),
        _ => snake_case(name),
    };
    format!("tn_{suffix}")
}

fn snake_case(name: &str) -> String {
    let mut result = String::with_capacity(name.len() + 4);
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 {
                result.push('_');
            }
            result.push(character.to_ascii_lowercase());
        } else {
            result.push(character);
        }
    }
    result
}

fn intrinsic_type_key(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::String => Some("string"),
        Type::Primitive(PrimitiveType::Usize) => Some("usize"),
        _ => None,
    }
}

fn intrinsic_type_from_key(key: &str) -> Option<Type> {
    match key {
        "string" => Some(Type::String),
        "usize" => Some(Type::Primitive(PrimitiveType::Usize)),
        _ => None,
    }
}
