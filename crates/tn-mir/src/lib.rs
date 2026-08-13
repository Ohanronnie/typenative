//! Target-independent generic MIR, deterministic rendering, and validation.

mod drop_elaboration;
mod error_lowering;
mod monomorphize;
mod validate;

use serde::{Deserialize, Serialize};
use tn_diagnostics::SourceSpan;
use tn_hir::{
    DeclarationId, FunctionType, HirClosureId, HirTemplateId, MemberId, ReceiverMode, Type,
};

pub use drop_elaboration::{DropSemantics, elaborate_drops};
pub use error_lowering::lower_typed_errors;
pub use monomorphize::{
    Callable, DropImplementation, GenericBody, Instance, MonomorphizationError, MonomorphizedBody,
    monomorphize, monomorphize_with_drops,
};
pub use validate::{MirValidationError, validate};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct LocalId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct BasicBlockId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RegionId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum BorrowKind {
    Shared,
    Mutable,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Projection {
    Field { index: u32, ty: Type },
    Dereference,
    Index(LocalId),
    Downcast(u32),
    BaseClass(DeclarationId),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Place {
    pub local: LocalId,
    pub projection: Vec<Projection>,
}

impl Place {
    pub const fn local(local: LocalId) -> Self {
        Self {
            local,
            projection: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Constant {
    Bool(bool),
    Integer {
        value: i128,
        ty: Type,
    },
    Float {
        bits: u64,
        ty: Type,
    },
    Character(char),
    String(String),
    Undefined(Type),
    Function(DeclarationId, Type),
    Method {
        owner: DeclarationId,
        member: MemberId,
        ty: Type,
    },
    Constructor {
        owner: DeclarationId,
        member: Option<MemberId>,
        ty: Type,
    },
}

impl Constant {
    pub fn ty(&self) -> Type {
        match self {
            Self::Bool(_) => Type::Primitive(tn_hir::PrimitiveType::Bool),
            Self::Integer { ty, .. }
            | Self::Float { ty, .. }
            | Self::Undefined(ty)
            | Self::Function(_, ty)
            | Self::Method { ty, .. }
            | Self::Constructor { ty, .. } => ty.clone(),
            Self::Character(_) => Type::Primitive(tn_hir::PrimitiveType::Char),
            Self::String(_) => Type::Reference {
                mutable: false,
                lifetime: "static".into(),
                referent: Box::new(Type::Str),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Operand {
    Copy(Place),
    Move(Place),
    Constant(Constant),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    ShiftLeft,
    ShiftRight,
    BitAnd,
    BitOr,
    BitXor,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    LogicalAnd,
    LogicalOr,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum UnaryOperator {
    LogicalNot,
    Negate,
    BitNot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Rvalue {
    Use(Operand),
    Unary {
        operator: UnaryOperator,
        operand: Operand,
        operand_type: Type,
        result_type: Type,
    },
    CheckedBinary {
        operator: BinaryOperator,
        left: Operand,
        right: Operand,
        operand_type: Type,
        result_type: Type,
    },
    CheckedIndex {
        collection: Place,
        index: Operand,
    },
    Aggregate {
        ty: Type,
        variant: Option<u32>,
        fields: Vec<Operand>,
        field_types: Vec<Type>,
    },
    Closure {
        id: HirClosureId,
        function: FunctionType,
        captures: Vec<Operand>,
        body: Box<Body>,
    },
    Template {
        id: HirTemplateId,
        parts: Vec<TemplatePart>,
        captures: Vec<Operand>,
        ty: Type,
    },
    Length(Place),
    VtableLookup {
        object: Place,
        implementation: DeclarationId,
        member: MemberId,
        slot: u32,
        receiver: ReceiverMode,
        ty: Type,
    },
    WitnessLookup {
        object: Place,
        interface: DeclarationId,
        slot: u32,
        receiver: ReceiverMode,
        ty: Type,
    },
    DirectMethod {
        object: Place,
        implementation: DeclarationId,
        member: MemberId,
        receiver: ReceiverMode,
        ty: Type,
    },
    TypeTest {
        operand: Operand,
        target: Type,
    },
    RawOperation {
        operation: String,
        operands: Vec<Operand>,
        ty: Type,
    },
    Cast {
        operand: Operand,
        ty: Type,
        kind: CastKind,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TemplatePart {
    Literal(String),
    Interpolation { capture: u32, value_type: Type },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum CastKind {
    ClassUpcast,
    InterfaceCoercion,
    Reborrow,
    CheckedDowncast,
    RawPointer,
    ErrorUnion,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StatementKind {
    Assign(Place, Box<Rvalue>),
    SetDiscriminant(Place, u32),
    StorageLive(LocalId),
    StorageDead(LocalId),
    Borrow {
        destination: LocalId,
        kind: BorrowKind,
        place: Place,
        region: RegionId,
    },
    Retag(Place),
    SetDropFlag(Place, bool),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub kind: StatementKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TerminatorKind {
    Goto(BasicBlockId),
    Switch {
        value: Operand,
        targets: Vec<(u128, BasicBlockId)>,
        otherwise: BasicBlockId,
    },
    Call {
        function: Operand,
        receiver: Option<Operand>,
        arguments: Vec<Operand>,
        destination: Option<Place>,
        error_destination: Option<Place>,
        success: BasicBlockId,
        error: Option<BasicBlockId>,
    },
    Return(Option<Operand>),
    Throw(Operand),
    TaggedReturn {
        completion: Completion,
        payload: Option<Operand>,
    },
    Suspend {
        value: Operand,
        destination: Option<Place>,
        error_destination: Option<Place>,
        resume: BasicBlockId,
        error: Option<BasicBlockId>,
        cancel: BasicBlockId,
    },
    Drop {
        place: Place,
        success: BasicBlockId,
    },
    Abort(String),
    Unreachable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Completion {
    Success,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Terminator {
    pub kind: TerminatorKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BasicBlock {
    pub statements: Vec<Statement>,
    pub terminator: Terminator,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Local {
    pub name: Option<String>,
    pub ty: Type,
    pub mutable: bool,
    pub argument: bool,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Body {
    pub declaration: DeclarationId,
    pub member: Option<MemberId>,
    pub locals: Vec<Local>,
    pub blocks: Vec<BasicBlock>,
    pub return_type: Type,
    pub effects: Vec<DeclarationId>,
}

impl std::fmt::Display for Body {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            formatter,
            "mir {:?}{} {{",
            self.declaration,
            self.member
                .map_or_else(String::new, |member| format!("::{member:?}"))
        )?;
        for (index, local) in self.locals.iter().enumerate() {
            writeln!(
                formatter,
                "  _{index}: {:?}{};",
                local.ty,
                if local.mutable { " mut" } else { "" }
            )?;
        }
        for (index, block) in self.blocks.iter().enumerate() {
            writeln!(formatter, "  bb{index}:")?;
            for statement in &block.statements {
                writeln!(formatter, "    {:?};", statement.kind)?;
            }
            writeln!(formatter, "    -> {:?};", block.terminator.kind)?;
        }
        formatter.write_str("}\n")
    }
}
