//! Type, effect, conformance, and ownership semantic analysis.

mod bodies;
mod mir_lower;
mod ownership;
mod signatures;
mod source_rules;

use tn_diagnostics::Diagnostic;
use tn_hir::{DefinitionData, Program, Type};
use tn_mir::{Body, DropSemantics};

pub use bodies::{
    BodyCheckResult, CallableIdentity, ClosureAnalysis, MonomorphizationInstance, check_bodies,
    check_bodies_with_ownership,
};
pub use mir_lower::{lower_mir, lower_mir_with_ownership};
pub use ownership::{
    Capture, CaptureKind, OwnershipFacts, check_capture_requirements, check_ownership,
    check_static_requirements, derive_ownership_facts,
};
pub use signatures::{check_signatures, check_signatures_with_ownership};
pub use source_rules::{check_source_rules, is_c_abi_type};

/// Computes explicit and structural destruction requirements for nominal types.
pub fn derive_drop_semantics(program: &Program) -> DropSemantics {
    let ownership = derive_ownership_facts(program);
    derive_drop_semantics_with_ownership(program, &ownership)
}

/// Computes destruction requirements while reusing ownership facts from the same compilation.
pub fn derive_drop_semantics_with_ownership(
    program: &Program,
    ownership: &OwnershipFacts,
) -> DropSemantics {
    let mut semantics = DropSemantics {
        nominal: ownership.drop.clone(),
    };
    semantics.nominal.extend(
        program
            .definitions
            .iter()
            .filter(|definition| matches!(definition.data, DefinitionData::Class { .. }))
            .map(|definition| definition.declaration),
    );
    loop {
        let before = semantics.nominal.len();
        for definition in &program.definitions {
            let fields = match &definition.data {
                DefinitionData::Struct { fields, .. } => {
                    fields.iter().map(|field| &field.ty).collect()
                }
                DefinitionData::Enum { variants, .. } => variants
                    .iter()
                    .flat_map(|variant| variant.fields.iter().map(|field| &field.ty))
                    .collect(),
                _ => Vec::new(),
            };
            if fields
                .iter()
                .any(|field| type_may_need_drop(field, &semantics))
            {
                semantics.nominal.insert(definition.declaration);
            }
        }
        if semantics.nominal.len() == before {
            break;
        }
    }
    semantics
}

fn type_may_need_drop(ty: &Type, semantics: &DropSemantics) -> bool {
    semantics.needs_drop(ty)
        || match ty {
            Type::Generic(_) => true,
            Type::Optional(inner) | Type::Array(inner, _) => type_may_need_drop(inner, semantics),
            Type::Union(alternatives) => alternatives
                .iter()
                .any(|alternative| type_may_need_drop(alternative, semantics)),
            Type::Tuple(elements) | Type::Template(elements) => elements
                .iter()
                .any(|element| type_may_need_drop(element, semantics)),
            _ => false,
        }
}

/// Elaborates conditional destruction for an ownership-checked generic MIR body.
pub fn elaborate_drops(program: &Program, body: &Body) -> Body {
    let semantics = derive_drop_semantics(program);
    elaborate_drops_with_semantics(program, body, &semantics)
}

/// Elaborates destruction using precomputed whole-program drop semantics.
pub fn elaborate_drops_with_semantics(
    program: &Program,
    body: &Body,
    semantics: &DropSemantics,
) -> Body {
    let mut semantics = semantics.clone();
    if let Some(definition) = program.definition(body.declaration)
        && let DefinitionData::Implementation {
            interface: Some(Type::Nominal(interface, _)),
            target: Type::Nominal(target, _),
            ..
        } = &definition.data
        && program
            .graph
            .declaration(*interface)
            .and_then(|declaration| declaration.name.as_deref())
            == Some("Disposable")
    {
        // A destructor owns the receiver for the duration of its body. Automatically invoking the
        // same destructor on `self` at the method's lexical exit would recurse forever.
        semantics.nominal.remove(target);
    }
    tn_mir::elaborate_drops(body, &semantics)
}

#[derive(Clone, Debug, Default)]
pub struct CheckResult {
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckResult {
    pub fn is_success(&self) -> bool {
        self.diagnostics.is_empty()
    }
}
