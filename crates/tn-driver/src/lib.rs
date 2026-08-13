//! Revision-oriented compiler driver and configuration boundary.

mod build;
mod docs;
mod lsp;
mod project;
mod test;

use std::path::Path;
use std::time::Instant;
use tn_diagnostics::{ConditionId, Diagnostic, Label, SourceSpan};

pub use build::{BuildError, BuildOutput, build_project, build_project_with_timings};
pub use docs::generate_docs;
pub use lsp::run_lsp;
pub use project::{
    Emit, LinkConfig, Profile, Project, ProjectConfig, Target, UnsupportedHost, load_project,
};
pub use test::{TestRun, run_tests};

#[derive(Clone, Debug)]
pub struct CheckOutput {
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckOutput {
    pub fn is_success(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

pub fn check_source(path: &Path, bytes: &[u8]) -> CheckOutput {
    let file = path.to_string_lossy();
    let parsed = tn_syntax::parse(&file, bytes);
    CheckOutput {
        diagnostics: parsed.diagnostics().to_vec(),
    }
}

pub fn check_project(project: &Project) -> CheckOutput {
    check_project_with_timings(project, false)
}

pub fn check_project_with_timings(project: &Project, timings_enabled: bool) -> CheckOutput {
    let started = Instant::now();
    let standard_library = standard_library_path();
    let graph = match tn_hir::load_module_graph(&project.root, &project.entry, &standard_library) {
        Ok(graph) => graph,
        Err(error) => {
            if !error.diagnostics().is_empty() {
                return CheckOutput {
                    diagnostics: error.diagnostics().to_vec(),
                };
            }
            return CheckOutput {
                diagnostics: vec![driver_diagnostic(
                    &project.entry,
                    format!("failed to load module graph: {error}"),
                )],
            };
        }
    };
    let program = match tn_hir::lower_program(graph) {
        Ok(program) => program,
        Err(diagnostics) => return CheckOutput { diagnostics },
    };
    if timings_enabled {
        eprintln!(
            "tn-timing phase=module-check micros={}",
            started.elapsed().as_micros()
        );
    }
    let started = Instant::now();
    let ownership_facts = tn_typecheck::derive_ownership_facts(&program);
    let checked = tn_typecheck::check_signatures_with_ownership(&program, &ownership_facts);
    let source_rules = tn_typecheck::check_source_rules(&program);
    let bodies = tn_typecheck::check_bodies_with_ownership(&program, &ownership_facts);
    let mir_ready = checked.diagnostics.is_empty() && bodies.diagnostics.is_empty();
    let static_requirements = tn_typecheck::check_static_requirements(&program, &ownership_facts);
    if timings_enabled {
        eprintln!(
            "tn-timing phase=ownership micros={}",
            started.elapsed().as_micros()
        );
    }
    let mut diagnostics = checked.diagnostics;
    diagnostics.extend(source_rules.diagnostics);
    diagnostics.extend(bodies.diagnostics);
    diagnostics.extend(static_requirements.diagnostics);
    if mir_ready {
        let started = Instant::now();
        for body in
            tn_typecheck::lower_mir_with_ownership(&program, &bodies.bodies, &ownership_facts)
        {
            diagnostics.extend(tn_typecheck::check_ownership(&body, &ownership_facts).diagnostics);
        }
        if timings_enabled {
            eprintln!(
                "tn-timing phase=mir-drop micros={}",
                started.elapsed().as_micros()
            );
        }
    }
    diagnostics.sort_by(|left, right| {
        left.primary
            .span
            .file
            .cmp(&right.primary.span.file)
            .then(
                left.primary
                    .span
                    .byte_start
                    .cmp(&right.primary.span.byte_start),
            )
            .then(left.condition.as_str().cmp(right.condition.as_str()))
    });
    diagnostics.dedup_by(|left, right| {
        left.condition == right.condition && left.primary.span == right.primary.span
    });
    CheckOutput { diagnostics }
}

fn standard_library_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("TYPENATIVE_STDLIB") {
        return path.into();
    }
    let installed = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .map(|directory| directory.join("../lib/typenative/std"));
    if let Some(path) = installed.filter(|path| path.is_dir()) {
        return path;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std")
}

fn driver_diagnostic(path: &Path, message: String) -> Diagnostic {
    let file = path.to_string_lossy();
    Diagnostic::error(
        ConditionId::new("DRIVER_MODULE_IO_ERROR").expect("static condition is valid"),
        message,
        Label {
            span: SourceSpan::new(&*file, 0..0, ""),
            message: "the compiler could not read a required module".into(),
        },
        "driver/module/io/error",
    )
}

#[derive(Clone, Debug)]
pub struct FormatOutput {
    pub formatted: String,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn format_source(path: &Path, bytes: &[u8]) -> FormatOutput {
    let file = path.to_string_lossy();
    let formatted = tn_syntax::format(&file, bytes);
    FormatOutput {
        formatted: formatted.output,
        diagnostics: formatted.diagnostics,
    }
}
