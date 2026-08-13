use crate::{Body, Completion, Rvalue, StatementKind, TerminatorKind};

/// Rewrites language-level returns and typed throws to the internal tagged completion ABI.
///
/// Fallible calls already carry distinct success and error successors in generic MIR. The LLVM
/// backend lowers those successors to a tag test without native exception handling.
pub fn lower_typed_errors(body: &Body) -> Body {
    let mut lowered = body.clone();
    for block in &mut lowered.blocks {
        block.terminator.kind = match block.terminator.kind.clone() {
            TerminatorKind::Return(payload) => TerminatorKind::TaggedReturn {
                completion: Completion::Success,
                payload,
            },
            TerminatorKind::Throw(payload) => TerminatorKind::TaggedReturn {
                completion: Completion::Error,
                payload: Some(payload),
            },
            other => other,
        };
        for statement in &mut block.statements {
            if let StatementKind::Assign(_, value) = &mut statement.kind
                && let Rvalue::Closure { body, .. } = value.as_mut()
            {
                **body = lower_typed_errors(body);
            }
        }
    }
    lowered
}
