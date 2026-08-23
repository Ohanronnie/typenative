//! Resolved `TypeNative` high-level intermediate representation.

mod macros;
mod module_graph;
mod semantic;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tn_diagnostics::SourceSpan;

pub use module_graph::{ModuleGraphError, load_module_graph};
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
    Macro,
}

impl DeclarationKind {
    pub const fn namespace(self) -> Option<Namespace> {
        match self {
            Self::Const | Self::Static | Self::Function => Some(Namespace::Value),
            Self::TypeAlias | Self::Struct | Self::Class | Self::Interface | Self::Enum => {
                Some(Namespace::Type)
            }
            Self::Impl | Self::ExternBlock | Self::Macro => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AttributeKind {
    Copy,
    Clone,
    Drop,
    Conform,
    Sealed,
    Layout,
    Export,
    Intrinsic,
    Inline,
    Test,
    Expand,
    Unknown(String),
}

impl AttributeKind {
    pub fn parse(name: &str) -> Self {
        match name {
            "Copy" => Self::Copy,
            "Clone" => Self::Clone,
            "Drop" => Self::Drop,
            "Conform" => Self::Conform,
            "Sealed" => Self::Sealed,
            "Layout" => Self::Layout,
            "Export" => Self::Export,
            "Intrinsic" => Self::Intrinsic,
            "Inline" => Self::Inline,
            "Test" => Self::Test,
            "Expand" => Self::Expand,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Copy => "Copy",
            Self::Clone => "Clone",
            Self::Drop => "Drop",
            Self::Conform => "Conform",
            Self::Sealed => "Sealed",
            Self::Layout => "Layout",
            Self::Export => "Export",
            Self::Intrinsic => "Intrinsic",
            Self::Inline => "Inline",
            Self::Test => "Test",
            Self::Expand => "Expand",
            Self::Unknown(name) => name,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attribute {
    pub kind: AttributeKind,
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
pub struct Parameter {
    pub name: String,
    pub ty: Type,
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
    pub function: Function,
    pub visibility: Visibility,
    pub receiver: ReceiverMode,
    pub is_abstract: bool,
    pub is_final: bool,
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
        fields: Vec<Field>,
        methods: Vec<Method>,
    },
    Enum {
        variants: Vec<EnumVariant>,
        methods: Vec<Method>,
    },
    Interface {
        methods: Vec<Method>,
        is_sealed: bool,
    },
    Class {
        base: Option<DeclarationId>,
        interfaces: Vec<Type>,
        fields: Vec<Field>,
        constructor: Option<Method>,
        methods: Vec<Method>,
        is_abstract: bool,
        is_final: bool,
        is_sealed: bool,
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
pub enum ResolvedValue {
    Local(HirLocalId),
    Declaration(DeclarationId),
    Member(MemberId),
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
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum HirPatternProjection {
    Variant(MemberId),
    Field(u32),
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
    pub expressions: Vec<HirExpression>,
    pub patterns: Vec<HirPattern>,
    pub statements: Vec<HirStatement>,
    pub closures: Vec<HirClosure>,
    pub templates: Vec<HirTemplate>,
    pub roots: Vec<HirStatementId>,
}

impl Program {
    pub fn definition(&self, declaration: DeclarationId) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.declaration == declaration)
    }

    pub fn intrinsic_type_declaration(&self, ty: &Type) -> Option<DeclarationId> {
        let key = intrinsic_type_key(ty)?;
        self.definitions.iter().find_map(|definition| {
            let declaration = self.graph.declaration(definition.declaration)?;
            declaration
                .attributes
                .iter()
                .any(|attribute| {
                    attribute.kind == AttributeKind::Intrinsic
                        && attribute.arguments.as_slice() == [key]
                })
                .then_some(definition.declaration)
        })
    }

    pub fn intrinsic_type_for_declaration(&self, declaration: DeclarationId) -> Option<Type> {
        let declaration = self.graph.declaration(declaration)?;
        declaration.attributes.iter().find_map(|attribute| {
            (attribute.kind == AttributeKind::Intrinsic)
                .then(|| attribute.arguments.first())
                .flatten()
                .and_then(|key| intrinsic_type_from_key(key))
        })
    }
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
