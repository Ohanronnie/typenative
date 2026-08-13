//! LLVM 22 adapter. LLVM types do not cross this crate boundary.

use inkwell::AddressSpace;
use inkwell::IntPredicate;
use inkwell::OptimizationLevel;
use inkwell::basic_block::BasicBlock as LlvmBlock;
use inkwell::builder::{Builder, BuilderError};
use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DIFlags, DIFlagsConstants, DWARFEmissionKind, DWARFSourceLanguage,
};
use inkwell::intrinsics::Intrinsic;
use inkwell::module::Linkage;
use inkwell::module::{FlagBehavior, Module};
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetData, TargetMachine,
    TargetTriple,
};
use inkwell::types::{
    BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType as LlvmFunctionType, StructType,
};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use tn_hir::{DeclarationId, FunctionType, PrimitiveType, Type};
use tn_mir::{
    BasicBlockId, BinaryOperator, Body, Callable, Completion, Constant, Instance,
    MonomorphizedBody, Operand, Place, Projection, Rvalue, StatementKind, TerminatorKind,
    UnaryOperator,
};

pub const REQUIRED_LLVM_VERSION: (u32, u32, u32) = (22, 1, 8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodegenProfile {
    Debug,
    Optimized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Emission {
    LlvmIr,
    Bitcode,
    Assembly,
    Object,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Layouts {
    pub nominals: BTreeMap<DeclarationId, NominalLayout>,
    pub witnesses: BTreeMap<(DeclarationId, DeclarationId), Vec<VtableEntry>>,
    pub interfaces: BTreeMap<DeclarationId, u32>,
    pub externs: BTreeMap<Callable, ExternLayout>,
    pub exports: BTreeMap<Callable, String>,
    pub drops: BTreeMap<DeclarationId, Callable>,
    pub async_functions: BTreeMap<Callable, FunctionType>,
    pub abi_wrappers: BTreeMap<Callable, AbiWrapperKind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbiWrapperKind {
    /// Adapt a monomorphized infallible body to a call-site effect signature.
    EffectLift,
    FallibleVoid,
    FallibleValue,
    FallibleIndirect,
    Indirect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalLayout {
    pub type_parameters: Vec<String>,
    pub kind: NominalKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VtableEntry {
    pub name: String,
    pub owner: DeclarationId,
    pub member: tn_hir::MemberId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConstructorLayout {
    pub member: tn_hir::MemberId,
    pub function: FunctionType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternLayout {
    pub name: String,
    pub function: FunctionType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NominalKind {
    Struct {
        fields: Vec<Type>,
    },
    Enum {
        variants: Vec<Vec<Type>>,
        c_repr: bool,
        discriminants: Vec<i128>,
    },
    Class {
        base: Option<DeclarationId>,
        fields: Vec<Type>,
        vtable: Vec<VtableEntry>,
        constructor: Option<ConstructorLayout>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CodegenError {
    #[error("TypeNative requires LLVM {required:?}, but the linked library is {actual:?}")]
    LlvmVersion {
        required: (u32, u32, u32),
        actual: (u32, u32, u32),
    },
    #[error("LLVM target error: {0}")]
    Target(String),
    #[error("LLVM builder error: {0}")]
    Builder(String),
    #[error("unsupported monomorphized MIR: {0}")]
    Unsupported(String),
    #[error("LLVM rejected generated module: {0}")]
    Verification(String),
    #[error("LLVM optimization pipeline failed: {0}")]
    Optimization(String),
    #[error("could not write compiler output: {0}")]
    Output(String),
}

impl From<BuilderError> for CodegenError {
    fn from(error: BuilderError) -> Self {
        Self::Builder(error.to_string())
    }
}

/// Lowers validated monomorphized MIR to verified LLVM IR for the requested target.
///
/// # Errors
///
/// Returns an error for a toolchain mismatch, unsupported residual generic MIR, target-machine
/// creation failure, builder invariant failure, or LLVM verification failure.
pub fn compile_to_llvm_ir(
    module_name: &str,
    bodies: &[Body],
    target_triple: &str,
    profile: CodegenProfile,
) -> Result<String, CodegenError> {
    let units = concrete_units(bodies);
    compile_monomorphized_to_llvm_ir(module_name, &units, target_triple, profile)
}

/// Lowers reachable specialized MIR instances to verified LLVM IR.
///
/// # Errors
///
/// Returns the same errors as [`compile_to_llvm_ir`].
pub fn compile_monomorphized_to_llvm_ir(
    module_name: &str,
    units: &[MonomorphizedBody],
    target_triple: &str,
    profile: CodegenProfile,
) -> Result<String, CodegenError> {
    compile_program_to_llvm_ir(
        module_name,
        units,
        &Layouts::default(),
        target_triple,
        profile,
    )
}

/// Lowers reachable MIR with resolved nominal layouts to verified LLVM IR.
///
/// # Errors
///
/// Returns the same errors as [`compile_to_llvm_ir`].
pub fn compile_program_to_llvm_ir(
    module_name: &str,
    units: &[MonomorphizedBody],
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
) -> Result<String, CodegenError> {
    let context = Context::create();
    let (generator, _) = generate(
        &context,
        module_name,
        units,
        layouts,
        target_triple,
        profile,
    )?;
    Ok(generator.module.print_to_string().to_string())
}

/// Emits a verified LLVM module to a backend product file.
///
/// # Errors
///
/// Returns the same lowering errors as [`compile_to_llvm_ir`] and reports filesystem or target
/// emission failures.
pub fn emit_to_file(
    module_name: &str,
    bodies: &[Body],
    target_triple: &str,
    profile: CodegenProfile,
    emission: Emission,
    path: &Path,
) -> Result<(), CodegenError> {
    let units = concrete_units(bodies);
    emit_monomorphized_to_file(module_name, &units, target_triple, profile, emission, path)
}

/// Emits reachable specialized MIR instances to a backend product file.
///
/// # Errors
///
/// Returns the same errors as [`emit_to_file`].
pub fn emit_monomorphized_to_file(
    module_name: &str,
    units: &[MonomorphizedBody],
    target_triple: &str,
    profile: CodegenProfile,
    emission: Emission,
    path: &Path,
) -> Result<(), CodegenError> {
    emit_program_to_file(
        module_name,
        units,
        &Layouts::default(),
        target_triple,
        profile,
        emission,
        path,
    )
}

/// Emits reachable MIR with resolved nominal layouts to a backend product file.
///
/// # Errors
///
/// Returns the same errors as [`emit_to_file`].
#[allow(clippy::too_many_arguments)]
pub fn emit_program_to_file(
    module_name: &str,
    units: &[MonomorphizedBody],
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
    emission: Emission,
    path: &Path,
) -> Result<(), CodegenError> {
    let context = Context::create();
    let (generator, machine) = generate(
        &context,
        module_name,
        units,
        layouts,
        target_triple,
        profile,
    )?;
    match emission {
        Emission::LlvmIr => generator
            .module
            .print_to_file(path)
            .map_err(|error| CodegenError::Output(error.to_string())),
        Emission::Bitcode => {
            if generator.module.write_bitcode_to_path(path) {
                Ok(())
            } else {
                Err(CodegenError::Output(format!(
                    "LLVM could not write {}",
                    path.display()
                )))
            }
        }
        Emission::Assembly | Emission::Object => machine
            .write_to_file(
                &generator.module,
                if emission == Emission::Assembly {
                    FileType::Assembly
                } else {
                    FileType::Object
                },
                path,
            )
            .map_err(|error| CodegenError::Output(error.to_string())),
    }
}

fn generate<'ctx>(
    context: &'ctx Context,
    module_name: &str,
    units: &[MonomorphizedBody],
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
) -> Result<(Generator<'ctx>, TargetMachine), CodegenError> {
    verify_llvm_version()?;
    Target::initialize_all(&InitializationConfig::default());
    let module = context.create_module(module_name);
    let triple = TargetTriple::create(target_triple);
    let machine = target_machine(&triple, profile)?;
    module.set_triple(&triple);
    let target_data = machine.get_target_data();
    module.set_data_layout(&target_data.get_data_layout());
    let mut generator = Generator::new(
        context,
        module,
        target_data,
        layouts.clone(),
        module_name,
        profile,
    );
    generator.declare_externs()?;
    generator.declare_bodies(units)?;
    generator.declare_descriptors(units)?;
    generator.declare_witnesses()?;
    generator.lower_constructor_wrappers()?;
    generator.lower_bodies(units)?;
    generator.lower_async_wrappers()?;
    generator.lower_abi_wrappers()?;
    generator.debug_info.finalize();
    generator
        .module
        .verify()
        .map_err(|error| CodegenError::Verification(error.to_string()))?;
    if profile == CodegenProfile::Optimized {
        generator
            .module
            .run_passes("default<O2>", &machine, PassBuilderOptions::create())
            .map_err(|error| CodegenError::Optimization(error.to_string()))?;
        generator
            .module
            .verify()
            .map_err(|error| CodegenError::Verification(error.to_string()))?;
    }
    Ok((generator, machine))
}

fn verify_llvm_version() -> Result<(), CodegenError> {
    let actual = inkwell::support::get_llvm_version();
    if actual == REQUIRED_LLVM_VERSION {
        Ok(())
    } else {
        Err(CodegenError::LlvmVersion {
            required: REQUIRED_LLVM_VERSION,
            actual,
        })
    }
}

fn target_machine(
    triple: &TargetTriple,
    profile: CodegenProfile,
) -> Result<TargetMachine, CodegenError> {
    let target =
        Target::from_triple(triple).map_err(|error| CodegenError::Target(error.to_string()))?;
    let optimization = match profile {
        CodegenProfile::Debug => OptimizationLevel::None,
        CodegenProfile::Optimized => OptimizationLevel::Aggressive,
    };
    target
        .create_target_machine(
            triple,
            "generic",
            "",
            optimization,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| CodegenError::Target("could not create target machine".into()))
}

struct Generator<'ctx> {
    context: &'ctx Context,
    module: Module<'ctx>,
    target_data: TargetData,
    functions: BTreeMap<Instance, FunctionValue<'ctx>>,
    body_functions: BTreeMap<Instance, FunctionValue<'ctx>>,
    signatures: BTreeMap<Instance, FunctionType>,
    layouts: Layouts,
    constructors: Vec<ConstructorTarget<'ctx>>,
    descriptors: BTreeMap<(DeclarationId, Vec<Type>), PointerValue<'ctx>>,
    witnesses: BTreeMap<(DeclarationId, DeclarationId), PointerValue<'ctx>>,
    debug_info: DebugInfoState<'ctx>,
    async_wrappers: Vec<AsyncWrapper<'ctx>>,
    abi_wrappers: Vec<AbiWrapper<'ctx>>,
}

fn collect_class_specializations(
    ty: &Type,
    layouts: &Layouts,
    specializations: &mut BTreeSet<(DeclarationId, Vec<Type>)>,
) {
    match ty {
        Type::Nominal(declaration, arguments) => {
            if layouts
                .nominals
                .get(declaration)
                .is_some_and(|layout| matches!(layout.kind, NominalKind::Class { .. }))
            {
                specializations.insert((*declaration, arguments.clone()));
            }
            for argument in arguments {
                collect_class_specializations(argument, layouts, specializations);
            }
        }
        Type::Optional(inner)
        | Type::Array(inner, _)
        | Type::Slice(inner)
        | Type::Reference {
            referent: inner, ..
        }
        | Type::RawPointer { pointee: inner, .. } => {
            collect_class_specializations(inner, layouts, specializations);
        }
        Type::Promise { result, .. } => {
            collect_class_specializations(result, layouts, specializations);
        }
        Type::Tuple(elements) | Type::Template(elements) => {
            for element in elements {
                collect_class_specializations(element, layouts, specializations);
            }
        }
        Type::Function(function) => {
            for parameter in &function.parameters {
                collect_class_specializations(parameter, layouts, specializations);
            }
            collect_class_specializations(&function.result, layouts, specializations);
        }
        Type::DynamicInterface(_, arguments) => {
            for argument in arguments {
                collect_class_specializations(argument, layouts, specializations);
            }
        }
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Generic(_)
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => {}
    }
}

struct DebugInfoState<'ctx> {
    builder: inkwell::debug_info::DebugInfoBuilder<'ctx>,
    compile_unit: inkwell::debug_info::DICompileUnit<'ctx>,
}

impl<'ctx> DebugInfoState<'ctx> {
    fn new(module: &Module<'ctx>, module_name: &str, profile: CodegenProfile) -> Self {
        let context = module.get_context();
        module.add_basic_value_flag(
            "Debug Info Version",
            FlagBehavior::Warning,
            context.i32_type().const_int(3, false),
        );
        let directory = Path::new(module_name)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or(".");
        let filename = Path::new(module_name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(module_name);
        let (builder, compile_unit) = module.create_debug_info_builder(
            true,
            DWARFSourceLanguage::C,
            filename,
            directory,
            "TypeNative compiler",
            profile == CodegenProfile::Optimized,
            "",
            0,
            "",
            DWARFEmissionKind::Full,
            0,
            false,
            false,
            "",
            "",
        );
        Self {
            builder,
            compile_unit,
        }
    }

    fn attach_function(&self, function: FunctionValue<'ctx>, name: &str) {
        let subroutine = self.builder.create_subroutine_type(
            self.compile_unit.get_file(),
            None,
            &[],
            DIFlags::PUBLIC,
        );
        let subprogram = self.builder.create_function(
            self.compile_unit.as_debug_info_scope(),
            name,
            None,
            self.compile_unit.get_file(),
            1,
            subroutine,
            false,
            true,
            1,
            DIFlags::PUBLIC,
            false,
        );
        function.set_subprogram(subprogram);
    }

    fn finalize(&self) {
        self.builder.finalize();
    }
}

struct ConstructorTarget<'ctx> {
    owner: DeclarationId,
    member: Option<tn_hir::MemberId>,
    signature: FunctionType,
    function: FunctionValue<'ctx>,
    initializer: Option<FunctionValue<'ctx>>,
    initializer_signature: Option<FunctionType>,
}

struct AsyncWrapper<'ctx> {
    wrapper: FunctionValue<'ctx>,
    body: FunctionValue<'ctx>,
    poll: FunctionValue<'ctx>,
    drop: FunctionValue<'ctx>,
    context_type: StructType<'ctx>,
    body_argument_types: Vec<Type>,
    has_receiver: bool,
    body_result: Type,
    body_effects: Vec<DeclarationId>,
}

struct AbiWrapper<'ctx> {
    wrapper: FunctionValue<'ctx>,
    body: FunctionValue<'ctx>,
    signature: FunctionType,
    kind: AbiWrapperKind,
}

impl<'ctx> Generator<'ctx> {
    fn new(
        context: &'ctx Context,
        module: Module<'ctx>,
        target_data: TargetData,
        layouts: Layouts,
        module_name: &str,
        profile: CodegenProfile,
    ) -> Self {
        let debug_info = DebugInfoState::new(&module, module_name, profile);
        Self {
            context,
            module,
            target_data,
            functions: BTreeMap::new(),
            body_functions: BTreeMap::new(),
            signatures: BTreeMap::new(),
            layouts,
            constructors: Vec::new(),
            descriptors: BTreeMap::new(),
            witnesses: BTreeMap::new(),
            debug_info,
            async_wrappers: Vec::new(),
            abi_wrappers: Vec::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn declare_bodies(&mut self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        for unit in units {
            let exported_name = self
                .layouts
                .exports
                .get(&unit.instance.callable)
                .cloned()
                .unwrap_or_else(|| symbol_for_instance(&unit.instance));
            let body_signature = body_signature(&unit.body);
            let body_type = self.body_function_type(&unit.body)?;
            let is_async = self
                .layouts
                .async_functions
                .contains_key(&unit.instance.callable);
            let abi_kind = self
                .layouts
                .abi_wrappers
                .get(&unit.instance.callable)
                .copied();
            let effect_lift = !is_async
                && body_signature.effects.is_empty()
                && !unit.instance.effects.is_empty()
                && abi_kind.is_none();
            let body_name = if is_async || abi_kind.is_some() || effect_lift {
                format!("{exported_name}_body")
            } else {
                exported_name.clone()
            };
            let body_function = self.module.add_function(&body_name, body_type, None);
            if !self.layouts.exports.is_empty()
                && !self.layouts.exports.contains_key(&unit.instance.callable)
            {
                body_function.set_linkage(Linkage::Internal);
            }
            self.debug_info.attach_function(body_function, &body_name);
            self.body_functions
                .insert(unit.instance.clone(), body_function);
            if is_async {
                let signature = self
                    .layouts
                    .async_functions
                    .get(&unit.instance.callable)
                    .cloned()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("async signature is missing".into())
                    })?;
                let body_parameters = self.body_parameter_types(&unit.body);
                let wrapper_type = self.llvm_function_type(
                    &body_parameters,
                    &signature.result,
                    &signature.effects,
                )?;
                let wrapper = self.module.add_function(&exported_name, wrapper_type, None);
                if !self.layouts.exports.is_empty()
                    && !self.layouts.exports.contains_key(&unit.instance.callable)
                {
                    wrapper.set_linkage(Linkage::Internal);
                }
                self.debug_info.attach_function(wrapper, &exported_name);
                self.functions.insert(unit.instance.clone(), wrapper);
                self.signatures
                    .insert(unit.instance.clone(), signature.clone());
                let context_fields = body_type
                    .get_param_types()
                    .into_iter()
                    .map(BasicTypeEnum::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|()| {
                        CodegenError::Unsupported(
                            "async argument metadata is not a basic LLVM type".into(),
                        )
                    })?;
                let context_type = if context_fields.is_empty() {
                    self.context
                        .struct_type(&[self.context.i8_type().as_basic_type_enum()], false)
                } else {
                    self.context.struct_type(&context_fields, false)
                };
                let poll_type = self.context.void_type().fn_type(
                    &[
                        self.context
                            .ptr_type(AddressSpace::default())
                            .as_basic_type_enum()
                            .into(),
                        self.context
                            .ptr_type(AddressSpace::default())
                            .as_basic_type_enum()
                            .into(),
                    ],
                    false,
                );
                let poll = self.module.add_function(
                    &format!("{exported_name}_async_poll"),
                    poll_type,
                    None,
                );
                self.debug_info
                    .attach_function(poll, &format!("{exported_name}_async_poll"));
                let drop_type = self.context.void_type().fn_type(
                    &[self
                        .context
                        .ptr_type(AddressSpace::default())
                        .as_basic_type_enum()
                        .into()],
                    false,
                );
                let drop = self.module.add_function(
                    &format!("{exported_name}_async_drop"),
                    drop_type,
                    None,
                );
                self.debug_info
                    .attach_function(drop, &format!("{exported_name}_async_drop"));
                self.async_wrappers.push(AsyncWrapper {
                    wrapper,
                    body: body_function,
                    poll,
                    drop,
                    context_type,
                    body_argument_types: body_signature.parameters.clone(),
                    has_receiver: unit.body.locals.iter().any(|local| {
                        local.argument && matches!(local.name.as_deref(), Some("self" | "this"))
                    }),
                    body_result: body_signature.result.as_ref().clone(),
                    body_effects: body_signature.effects.clone(),
                });
            } else if effect_lift {
                let parameters = body_signature
                    .parameters
                    .iter()
                    .map(|parameter| match parameter {
                        Type::Promise { result, effects } if effects.is_empty() => Type::Promise {
                            result: result.clone(),
                            effects: unit.instance.effects.clone(),
                        },
                        _ => parameter.clone(),
                    })
                    .collect();
                let signature = FunctionType {
                    parameters,
                    result: body_signature.result.clone(),
                    effects: unit.instance.effects.clone(),
                    generics: Vec::new(),
                    is_async: false,
                    is_unsafe: body_signature.is_unsafe,
                };
                let wrapper_type = self.llvm_function_type(
                    &signature.parameters,
                    &signature.result,
                    &signature.effects,
                )?;
                let wrapper = self.module.add_function(&exported_name, wrapper_type, None);
                self.debug_info.attach_function(wrapper, &exported_name);
                self.functions.insert(unit.instance.clone(), wrapper);
                self.signatures
                    .insert(unit.instance.clone(), signature.clone());
                self.abi_wrappers.push(AbiWrapper {
                    wrapper,
                    body: body_function,
                    signature,
                    kind: AbiWrapperKind::EffectLift,
                });
            } else {
                self.signatures
                    .insert(unit.instance.clone(), body_signature.clone());
                self.functions.insert(unit.instance.clone(), body_function);
            }
            if let Some(kind) = abi_kind {
                let abi_type = self.abi_wrapper_type(kind, &body_signature)?;
                let wrapper = self.module.add_function(&exported_name, abi_type, None);
                self.debug_info.attach_function(wrapper, &exported_name);
                self.abi_wrappers.push(AbiWrapper {
                    wrapper,
                    body: body_function,
                    signature: body_signature,
                    kind,
                });
            }
        }
        self.declare_constructor_wrappers(units)?;
        Ok(())
    }

    fn declare_externs(&mut self) -> Result<(), CodegenError> {
        let mut declared = BTreeMap::<String, (FunctionValue<'ctx>, FunctionType)>::new();
        for (callable, external) in &self.layouts.externs {
            let function_type = self.llvm_function_type(
                &external.function.parameters,
                &external.function.result,
                &external.function.effects,
            )?;
            let function = if let Some((function, signature)) = declared.get(&external.name) {
                if signature != &external.function {
                    return Err(CodegenError::Unsupported(format!(
                        "extern symbol `{}` has incompatible declarations",
                        external.name
                    )));
                }
                *function
            } else {
                let function = self
                    .module
                    .add_function(&external.name, function_type, None);
                declared.insert(external.name.clone(), (function, external.function.clone()));
                function
            };
            self.functions
                .insert(Instance::concrete(*callable), function);
            self.signatures
                .insert(Instance::concrete(*callable), external.function.clone());
        }
        Ok(())
    }

    fn declare_constructor_wrappers(
        &mut self,
        units: &[MonomorphizedBody],
    ) -> Result<(), CodegenError> {
        for unit in units {
            let Some(NominalLayout {
                kind:
                    NominalKind::Class {
                        constructor: Some(constructor),
                        ..
                    },
                ..
            }) = self
                .layouts
                .nominals
                .get(&unit.instance.callable.declaration)
            else {
                continue;
            };
            if unit.instance.callable.member != Some(constructor.member) {
                continue;
            }
            let initializer_signature =
                self.signatures
                    .get(&unit.instance)
                    .cloned()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("constructor body signature missing".into())
                    })?;
            let parameters = initializer_signature
                .parameters
                .get(1..)
                .unwrap_or_default()
                .to_vec();
            let signature = FunctionType {
                parameters,
                result: Box::new(Type::Nominal(
                    unit.instance.callable.declaration,
                    unit.instance.type_arguments.clone(),
                )),
                effects: initializer_signature.effects.clone(),
                generics: Vec::new(),
                is_async: false,
                is_unsafe: false,
            };
            let initializer = self.functions.get(&unit.instance).copied();
            self.add_constructor_wrapper(
                unit.instance.callable.declaration,
                Some(constructor.member),
                signature,
                initializer,
                Some(initializer_signature),
            )?;
        }

        for unit in units {
            for block in &unit.body.blocks {
                let TerminatorKind::Call { function, .. } = &block.terminator.kind else {
                    continue;
                };
                let Operand::Constant(Constant::Constructor {
                    owner,
                    member: None,
                    ty: Type::Function(signature),
                }) = function
                else {
                    continue;
                };
                self.add_constructor_wrapper(*owner, None, signature.clone(), None, None)?;
            }
        }

        // A class without an explicit constructor still has a public zero-argument constructor
        // in the source language.  Native clients (not just TypeNative `new` expressions) need a
        // stable wrapper as well, so register it even when no MIR call happened to reach it.
        let implicit_owners = self
            .layouts
            .nominals
            .iter()
            .filter_map(|(owner, layout)| {
                matches!(
                    layout.kind,
                    NominalKind::Class {
                        constructor: None,
                        ..
                    }
                )
                .then_some(*owner)
            })
            .collect::<Vec<_>>();
        for owner in implicit_owners {
            let signature = FunctionType {
                parameters: Vec::new(),
                result: Box::new(Type::Nominal(owner, Vec::new())),
                effects: Vec::new(),
                generics: Vec::new(),
                is_async: false,
                is_unsafe: false,
            };
            self.add_constructor_wrapper(owner, None, signature, None, None)?;
        }
        Ok(())
    }

    fn add_constructor_wrapper(
        &mut self,
        owner: DeclarationId,
        member: Option<tn_hir::MemberId>,
        signature: FunctionType,
        initializer: Option<FunctionValue<'ctx>>,
        initializer_signature: Option<FunctionType>,
    ) -> Result<(), CodegenError> {
        if self.constructors.iter().any(|target| {
            target.owner == owner && target.member == member && target.signature == signature
        }) {
            return Ok(());
        }
        let llvm_type =
            self.llvm_function_type(&signature.parameters, &signature.result, &signature.effects)?;
        let name = symbol_for_constructor(owner, member, &signature);
        let function = self.module.add_function(&name, llvm_type, None);
        self.constructors.push(ConstructorTarget {
            owner,
            member,
            signature,
            function,
            initializer,
            initializer_signature,
        });
        Ok(())
    }

    fn declare_descriptors(&mut self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        let mut specializations = BTreeSet::new();
        for (declaration, layout) in &self.layouts.nominals {
            if matches!(layout.kind, NominalKind::Class { .. }) && layout.type_parameters.is_empty()
            {
                specializations.insert((*declaration, Vec::new()));
            }
        }
        for unit in units {
            for local in &unit.body.locals {
                collect_class_specializations(&local.ty, &self.layouts, &mut specializations);
            }
            collect_class_specializations(
                &unit.body.return_type,
                &self.layouts,
                &mut specializations,
            );
        }
        for target in &self.constructors {
            collect_class_specializations(
                &target.signature.result,
                &self.layouts,
                &mut specializations,
            );
        }

        for (declaration, arguments) in specializations {
            let Some(NominalKind::Class { vtable, .. }) = self
                .layouts
                .nominals
                .get(&declaration)
                .map(|layout| &layout.kind)
            else {
                continue;
            };
            let slot_count = u32::try_from(vtable.len().saturating_add(1))
                .map_err(|_| CodegenError::Unsupported("class vtable exceeds LLVM limit".into()))?;
            let pointer = self.context.ptr_type(AddressSpace::default());
            let descriptor_type = pointer.array_type(slot_count);
            let name = if arguments.is_empty() {
                format!("tn_class_descriptor_{}", declaration.0)
            } else {
                format!(
                    "tn_class_descriptor_{}_{}",
                    declaration.0,
                    self.descriptors.len()
                )
            };
            let global = self.module.add_global(descriptor_type, None, &name);
            let null = pointer.const_null();
            let mut values = vec![null; usize::try_from(slot_count).unwrap_or_default()];
            for (index, entry) in vtable.iter().enumerate() {
                let callable = Callable {
                    declaration: entry.owner,
                    member: Some(entry.member),
                };
                let function = units.iter().find_map(|unit| {
                    if unit.instance.callable != callable {
                        return None;
                    }
                    let receiver = unit
                        .body
                        .locals
                        .iter()
                        .find(|local| {
                            local.argument && matches!(local.name.as_deref(), Some("this" | "self"))
                        })
                        .map(|local| &local.ty);
                    if receiver != Some(&Type::Nominal(declaration, arguments.clone())) {
                        return None;
                    }
                    self.functions
                        .get(&unit.instance)
                        .map(|function| function.as_global_value().as_pointer_value())
                });
                if let Some(function) = function {
                    values[index + 1] = function;
                }
            }
            global.set_initializer(&pointer.const_array(&values));
            self.descriptors
                .insert((declaration, arguments), global.as_pointer_value());
        }
        Ok(())
    }

    fn declare_witnesses(&mut self) -> Result<(), CodegenError> {
        let entries = self
            .layouts
            .witnesses
            .iter()
            .map(|(key, entries)| (*key, entries.clone()))
            .collect::<Vec<_>>();
        for ((interface, target), entries) in entries {
            let count = u32::try_from(entries.len()).map_err(|_| {
                CodegenError::Unsupported("witness table exceeds LLVM limit".into())
            })?;
            let pointer = self.context.ptr_type(AddressSpace::default());
            let table_name = format!("tn_witness_{}_{}", interface.0, target.0);
            let global = self
                .module
                .add_global(pointer.array_type(count), None, &table_name);
            let mut values = vec![pointer.const_null(); usize::try_from(count).unwrap_or_default()];
            for (index, entry) in entries.iter().enumerate() {
                if let Some(function) = self
                    .functions
                    .iter()
                    .find(|(instance, _)| {
                        instance.callable
                            == (Callable {
                                declaration: entry.owner,
                                member: Some(entry.member),
                            })
                    })
                    .map(|(_, function)| function.as_global_value().as_pointer_value())
                {
                    values[index] = function;
                }
            }
            global.set_initializer(&pointer.const_array(&values));
            self.witnesses
                .insert((interface, target), global.as_pointer_value());
        }
        Ok(())
    }

    fn lower_constructor_wrappers(&self) -> Result<(), CodegenError> {
        for target in &self.constructors {
            let entry = self.context.append_basic_block(target.function, "entry");
            let builder = self.context.create_builder();
            builder.position_at_end(entry);
            let object_type = self.class_object_type(&target.signature.result)?;
            let size = object_type.size_of().ok_or_else(|| {
                CodegenError::Unsupported("class object has no statically known size".into())
            })?;
            let object = builder
                .build_call(self.runtime_alloc(), &[size.into()], "class.object")?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                .into_pointer_value();
            let header = builder.build_struct_gep(object_type, object, 0, "class.descriptor")?;
            let descriptor = self
                .descriptor_for_type(&target.signature.result)
                .unwrap_or_else(|| self.context.ptr_type(AddressSpace::default()).const_null());
            builder.build_store(header, descriptor)?;

            let parameters = target
                .function
                .get_param_iter()
                .map(BasicMetadataValueEnum::from)
                .collect::<Vec<_>>();
            let mut initializer_arguments = vec![object.into()];
            initializer_arguments.extend(parameters);
            if let Some(initializer) = target.initializer {
                let call = builder.build_call(initializer, &initializer_arguments, "initialize")?;
                if let Some(initializer_signature) = &target.initializer_signature
                    && !initializer_signature.effects.is_empty()
                {
                    let result = call
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| CodegenError::Builder("constructor returned void".into()))?
                        .into_struct_value();
                    let failed = builder
                        .build_extract_value(result, 0, "constructor.failed")?
                        .into_int_value();
                    let failed = builder.build_int_compare(
                        IntPredicate::NE,
                        failed,
                        failed.get_type().const_zero(),
                        "constructor.failed.test",
                    )?;
                    let failed_block = self
                        .context
                        .append_basic_block(target.function, "constructor.error");
                    let success_block = self
                        .context
                        .append_basic_block(target.function, "constructor.success");
                    builder.build_conditional_branch(failed, failed_block, success_block)?;
                    builder.position_at_end(failed_block);
                    let error =
                        builder.build_extract_value(result, 1, "constructor.error.value")?;
                    let failed_result =
                        self.completion_type(&target.signature.result)?.const_zero();
                    let failed_result = builder
                        .build_insert_value(
                            failed_result,
                            self.context.i8_type().const_int(1, false),
                            0,
                            "constructor.error.tag",
                        )?
                        .into_struct_value();
                    let failed_result = builder
                        .build_insert_value(failed_result, error, 2, "constructor.error.payload")?
                        .into_struct_value();
                    builder.build_return(Some(&failed_result))?;
                    builder.position_at_end(success_block);
                    return_constructor_success(&builder, self, target, object)?;
                } else {
                    builder.build_return(Some(&object))?;
                }
            } else {
                return_constructor_success(&builder, self, target, object)?;
            }
        }
        Ok(())
    }

    fn runtime_alloc(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_alloc")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_alloc",
                    self.context
                        .ptr_type(AddressSpace::default())
                        .fn_type(&[self.pointer_int_type().into()], false),
                    None,
                )
            })
    }

    fn runtime_free(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_free")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_free",
                    self.context.void_type().fn_type(
                        &[self.context.ptr_type(AddressSpace::default()).into()],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_string_equals(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_string_equals")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_string_equals",
                    self.context.i32_type().fn_type(
                        &[
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                        ],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_string_from_bytes(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_string_from_bytes")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_string_from_bytes",
                    self.context.ptr_type(AddressSpace::default()).fn_type(
                        &[
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                            self.pointer_int_type().into(),
                        ],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_strlen(&self) -> FunctionValue<'ctx> {
        self.module.get_function("strlen").unwrap_or_else(|| {
            self.module.add_function(
                "strlen",
                self.pointer_int_type().fn_type(
                    &[self
                        .context
                        .ptr_type(AddressSpace::default())
                        .as_basic_type_enum()
                        .into()],
                    false,
                ),
                None,
            )
        })
    }

    fn runtime_ref_retain(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_ref_retain")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_ref_retain",
                    self.context.ptr_type(AddressSpace::default()).fn_type(
                        &[self
                            .context
                            .ptr_type(AddressSpace::default())
                            .as_basic_type_enum()
                            .into()],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_async_create(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_async_create")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_async_create",
                    self.context.ptr_type(AddressSpace::default()).fn_type(
                        &[
                            self.pointer_int_type().into(),
                            self.pointer_int_type().into(),
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                        ],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_async_wait(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_async_wait")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_async_wait",
                    self.context.i32_type().fn_type(
                        &[self
                            .context
                            .ptr_type(AddressSpace::default())
                            .as_basic_type_enum()
                            .into()],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_async_raw_result(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_async_raw_result")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_async_raw_result",
                    self.context.ptr_type(AddressSpace::default()).fn_type(
                        &[self
                            .context
                            .ptr_type(AddressSpace::default())
                            .as_basic_type_enum()
                            .into()],
                        false,
                    ),
                    None,
                )
            })
    }

    fn runtime_async_destroy(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_async_destroy")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_async_destroy",
                    self.context.i32_type().fn_type(
                        &[self
                            .context
                            .ptr_type(AddressSpace::default())
                            .as_basic_type_enum()
                            .into()],
                        false,
                    ),
                    None,
                )
            })
    }

    fn lower_bodies(&self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        for unit in units {
            let function = self
                .body_functions
                .get(&unit.instance)
                .copied()
                .ok_or_else(|| CodegenError::Unsupported("body function is missing".into()))?;
            FunctionGenerator::new(self, &unit.body, function)
                .and_then(|generator| generator.lower())
                .map_err(|error| {
                    CodegenError::Unsupported(format!(
                        "while lowering {} ({:?}): {error}",
                        function.get_name().to_string_lossy(),
                        unit.instance
                    ))
                })?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn lower_async_wrappers(&self) -> Result<(), CodegenError> {
        for wrapper in &self.async_wrappers {
            let parameter_types = wrapper
                .body
                .get_type()
                .get_param_types()
                .into_iter()
                .map(BasicTypeEnum::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|()| {
                    CodegenError::Unsupported(
                        "async argument metadata is not a basic LLVM type".into(),
                    )
                })?;
            let promise_element = if wrapper.body_effects.is_empty() {
                if wrapper.body_result == Type::Primitive(PrimitiveType::Void) {
                    self.context.i8_type().as_basic_type_enum()
                } else {
                    self.basic_type(&wrapper.body_result)?
                }
            } else {
                self.completion_type(&wrapper.body_result)?
                    .as_basic_type_enum()
            };

            let poll_entry = self.context.append_basic_block(wrapper.poll, "entry");
            let poll_builder = self.context.create_builder();
            poll_builder.position_at_end(poll_entry);
            if let Some(subprogram) = wrapper.poll.get_subprogram() {
                let location = self.debug_info.builder.create_debug_location(
                    self.context,
                    1,
                    1,
                    subprogram.as_debug_info_scope(),
                    None,
                );
                poll_builder.set_current_debug_location(location);
            }
            let mut poll_arguments = Vec::with_capacity(parameter_types.len());
            let context_argument = wrapper
                .poll
                .get_first_param()
                .ok_or_else(|| CodegenError::Builder("async poll context is missing".into()))?
                .into_pointer_value();
            let context_pointer = poll_builder.build_pointer_cast(
                context_argument,
                self.context.ptr_type(AddressSpace::default()),
                "async.context",
            )?;
            for (index, parameter_type) in parameter_types.iter().enumerate() {
                let field = poll_builder.build_struct_gep(
                    wrapper.context_type,
                    context_pointer,
                    u32::try_from(index).map_err(|_| {
                        CodegenError::Unsupported("async argument index overflow".into())
                    })?,
                    &format!("async.argument.{index}"),
                )?;
                let value = poll_builder.build_load(
                    *parameter_type,
                    field,
                    &format!("async.argument.load.{index}"),
                )?;
                poll_arguments.push(value.into());
            }
            let result = poll_builder
                .build_call(wrapper.body, &poll_arguments, "async.body")?
                .try_as_basic_value()
                .basic();
            if let Some(result) = result {
                let result_pointer = poll_builder.build_pointer_cast(
                    wrapper
                        .poll
                        .get_last_param()
                        .ok_or_else(|| {
                            CodegenError::Builder("async poll result is missing".into())
                        })?
                        .into_pointer_value(),
                    self.context.ptr_type(AddressSpace::default()),
                    "async.result",
                )?;
                poll_builder.build_store(result_pointer, result)?;
            }
            poll_builder.build_return(None)?;

            let drop_entry = self.context.append_basic_block(wrapper.drop, "entry");
            let drop_builder = self.context.create_builder();
            drop_builder.position_at_end(drop_entry);
            if let Some(subprogram) = wrapper.drop.get_subprogram() {
                let location = self.debug_info.builder.create_debug_location(
                    self.context,
                    1,
                    1,
                    subprogram.as_debug_info_scope(),
                    None,
                );
                drop_builder.set_current_debug_location(location);
            }
            let context_argument = wrapper
                .drop
                .get_first_param()
                .ok_or_else(|| CodegenError::Builder("async drop context is missing".into()))?
                .into_pointer_value();
            let context_pointer = drop_builder.build_pointer_cast(
                context_argument,
                self.context.ptr_type(AddressSpace::default()),
                "async.drop.context",
            )?;
            for (index, source_type) in wrapper.body_argument_types.iter().enumerate() {
                if wrapper.has_receiver && index == 0 {
                    continue;
                }
                let Type::Nominal(declaration, _) = source_type else {
                    continue;
                };
                let Some(callable) = self.layouts.drops.get(declaration).copied() else {
                    continue;
                };
                let Some((drop_function, _)) = self
                    .signatures
                    .iter()
                    .find(|(instance, signature)| {
                        instance.callable == callable
                            && signature.parameters.len() == 1
                            && signature.parameters[0] == *source_type
                    })
                    .map(|(instance, signature)| (self.functions[instance], signature))
                else {
                    continue;
                };
                let field = drop_builder.build_struct_gep(
                    wrapper.context_type,
                    context_pointer,
                    u32::try_from(index).map_err(|_| {
                        CodegenError::Unsupported("async argument index overflow".into())
                    })?,
                    &format!("async.drop.field.{index}"),
                )?;
                let receiver = if self.is_class_type(source_type) {
                    drop_builder.build_load(
                        self.context.ptr_type(AddressSpace::default()),
                        field,
                        &format!("async.drop.class.{index}"),
                    )?
                } else {
                    field.into()
                };
                drop_builder.build_call(
                    drop_function,
                    &[receiver.into()],
                    "async.argument.drop",
                )?;
            }
            drop_builder.build_return(None)?;

            let entry = self.context.append_basic_block(wrapper.wrapper, "entry");
            let builder = self.context.create_builder();
            builder.position_at_end(entry);
            if let Some(subprogram) = wrapper.wrapper.get_subprogram() {
                let location = self.debug_info.builder.create_debug_location(
                    self.context,
                    1,
                    1,
                    subprogram.as_debug_info_scope(),
                    None,
                );
                builder.set_current_debug_location(location);
            }
            let context_pointer = builder
                .build_call(
                    self.runtime_alloc(),
                    &[wrapper
                        .context_type
                        .size_of()
                        .ok_or_else(|| {
                            CodegenError::Unsupported("async context has no known size".into())
                        })?
                        .into()],
                    "async.context.alloc",
                )?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| {
                    CodegenError::Builder("async context allocation returned void".into())
                })?
                .into_pointer_value();
            let context_pointer = builder.build_pointer_cast(
                context_pointer,
                self.context.ptr_type(AddressSpace::default()),
                "async.context.cast",
            )?;
            for (index, argument) in wrapper.wrapper.get_param_iter().enumerate() {
                let field = builder.build_struct_gep(
                    wrapper.context_type,
                    context_pointer,
                    u32::try_from(index).map_err(|_| {
                        CodegenError::Unsupported("async argument index overflow".into())
                    })?,
                    &format!("async.context.field.{index}"),
                )?;
                builder.build_store(field, argument)?;
            }
            let result_size = promise_element.size_of().ok_or_else(|| {
                CodegenError::Unsupported("async result has no known size".into())
            })?;
            let payload_offset = if wrapper.body_effects.is_empty()
                || wrapper.body_result == Type::Primitive(PrimitiveType::Void)
            {
                0
            } else {
                let completion = self.completion_type(&wrapper.body_result)?;
                self.target_data
                    .offset_of_element(&completion, 1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "async completion payload has no known offset".into(),
                        )
                    })?
            };
            let promise_pointer = builder
                .build_call(
                    self.runtime_async_create(),
                    &[
                        result_size.into(),
                        self.pointer_int_type()
                            .const_int(payload_offset, false)
                            .into(),
                        wrapper.poll.as_global_value().as_pointer_value().into(),
                        context_pointer.into(),
                        wrapper.drop.as_global_value().as_pointer_value().into(),
                    ],
                    "async.promise",
                )?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| {
                    CodegenError::Builder("async promise creation returned void".into())
                })?;
            builder.build_return(Some(&promise_pointer))?;
        }
        Ok(())
    }

    fn abi_wrapper_type(
        &self,
        kind: AbiWrapperKind,
        signature: &FunctionType,
    ) -> Result<LlvmFunctionType<'ctx>, CodegenError> {
        if kind == AbiWrapperKind::EffectLift {
            return self.llvm_function_type(
                &signature.parameters,
                &signature.result,
                &signature.effects,
            );
        }
        if kind == AbiWrapperKind::Indirect || kind == AbiWrapperKind::FallibleIndirect {
            let parameters = signature
                .parameters
                .iter()
                .map(|parameter| {
                    if self.is_indirect_abi_type(parameter) {
                        Ok(self.context.ptr_type(AddressSpace::default()).into())
                    } else {
                        self.basic_type(parameter).map(BasicMetadataTypeEnum::from)
                    }
                })
                .collect::<Result<Vec<_>, CodegenError>>()?;
            if kind == AbiWrapperKind::FallibleIndirect {
                let packed = self.context.i64_type().array_type(2);
                return Ok(packed.fn_type(&parameters, false));
            }
            if signature.result.as_ref() == &Type::Primitive(PrimitiveType::Void) {
                return Ok(self.context.void_type().fn_type(&parameters, false));
            }
            let result = if self.is_indirect_abi_type(&signature.result) {
                self.context.ptr_type(AddressSpace::default()).into()
            } else {
                self.basic_type(&signature.result)?
            };
            return Ok(result.fn_type(&parameters, false));
        }
        let packed = self.context.i64_type().array_type(2);
        Ok(packed.fn_type(
            &signature
                .parameters
                .iter()
                .map(|parameter| self.basic_type(parameter).map(BasicMetadataTypeEnum::from))
                .collect::<Result<Vec<_>, _>>()?,
            false,
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn lower_abi_wrappers(&self) -> Result<(), CodegenError> {
        for wrapper in &self.abi_wrappers {
            let entry = self.context.append_basic_block(wrapper.wrapper, "entry");
            let builder = self.context.create_builder();
            builder.position_at_end(entry);
            if let Some(subprogram) = wrapper.wrapper.get_subprogram() {
                let location = self.debug_info.builder.create_debug_location(
                    self.context,
                    1,
                    1,
                    subprogram.as_debug_info_scope(),
                    None,
                );
                builder.set_current_debug_location(location);
            }
            let arguments = wrapper
                .wrapper
                .get_param_iter()
                .map(BasicMetadataValueEnum::from)
                .collect::<Vec<_>>();
            if wrapper.kind == AbiWrapperKind::EffectLift {
                let call = builder.build_call(wrapper.body, &arguments, "effect.body")?;
                let completion = self
                    .completion_type(&wrapper.signature.result)?
                    .const_zero();
                let completion = builder
                    .build_insert_value(
                        completion,
                        self.context.i8_type().const_zero(),
                        0,
                        "effect.success.tag",
                    )?
                    .into_struct_value();
                let completion =
                    if wrapper.signature.result.as_ref() == &Type::Primitive(PrimitiveType::Void) {
                        completion
                    } else {
                        let value = call.try_as_basic_value().basic().ok_or_else(|| {
                            CodegenError::Builder("effect-lift body returned void".into())
                        })?;
                        builder
                            .build_insert_value(completion, value, 1, "effect.success.value")?
                            .into_struct_value()
                    };
                builder.build_return(Some(&completion))?;
                continue;
            }
            if matches!(
                wrapper.kind,
                AbiWrapperKind::Indirect | AbiWrapperKind::FallibleIndirect
            ) {
                let arguments = wrapper
                    .wrapper
                    .get_param_iter()
                    .zip(&wrapper.signature.parameters)
                    .map(|(argument, ty)| {
                        if self.is_indirect_abi_type(ty) {
                            let loaded = builder.build_load(
                                self.basic_type(ty)?,
                                argument.into_pointer_value(),
                                "abi.indirect.load",
                            )?;
                            Ok(BasicMetadataValueEnum::from(loaded))
                        } else {
                            Ok(BasicMetadataValueEnum::from(argument))
                        }
                    })
                    .collect::<Result<Vec<_>, CodegenError>>()?;
                let call = builder.build_call(wrapper.body, &arguments, "abi.indirect.body")?;
                if wrapper.kind == AbiWrapperKind::FallibleIndirect {
                    let completion = call
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| {
                            CodegenError::Builder("fallible indirect body returned void".into())
                        })?
                        .into_struct_value();
                    let failed = builder
                        .build_extract_value(completion, 0, "abi.indirect.failed")?
                        .into_int_value();
                    let failed = builder.build_int_compare(
                        IntPredicate::NE,
                        failed,
                        failed.get_type().const_zero(),
                        "abi.indirect.failed.test",
                    )?;
                    let value = builder.build_extract_value(completion, 1, "abi.indirect.value")?;
                    let llvm_type = self.basic_type(&wrapper.signature.result)?;
                    let size = llvm_type.size_of().ok_or_else(|| {
                        CodegenError::Unsupported(
                            "fallible indirect ABI result has no known size".into(),
                        )
                    })?;
                    let value_pointer = builder
                        .build_call(
                            self.runtime_alloc(),
                            &[size.into()],
                            "abi.indirect.value.alloc",
                        )?
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                        .into_pointer_value();
                    builder.build_store(value_pointer, value)?;
                    let error_pointer = builder
                        .build_extract_value(
                            completion,
                            if wrapper.signature.result.as_ref()
                                == &Type::Primitive(PrimitiveType::Void)
                            {
                                1
                            } else {
                                2
                            },
                            "abi.indirect.error",
                        )?
                        .into_pointer_value();
                    let failed_block = self
                        .context
                        .append_basic_block(wrapper.wrapper, "abi.indirect.failed");
                    let success_block = self
                        .context
                        .append_basic_block(wrapper.wrapper, "abi.indirect.success");
                    builder.build_conditional_branch(failed, failed_block, success_block)?;
                    builder.position_at_end(failed_block);
                    builder.build_call(
                        self.runtime_free(),
                        &[value_pointer.into()],
                        "abi.indirect.value.free",
                    )?;
                    let error_payload = builder.build_ptr_to_int(
                        error_pointer,
                        self.context.i64_type(),
                        "abi.indirect.error.wide",
                    )?;
                    let failed_packed = self.context.i64_type().array_type(2).const_zero();
                    let failed_packed = builder
                        .build_insert_value(
                            failed_packed,
                            self.context.i64_type().const_int(1, false),
                            0,
                            "abi.indirect.failed.field",
                        )?
                        .into_array_value();
                    let failed_packed = builder
                        .build_insert_value(
                            failed_packed,
                            error_payload,
                            1,
                            "abi.indirect.error.field",
                        )?
                        .into_array_value();
                    builder.build_return(Some(&failed_packed))?;
                    builder.position_at_end(success_block);
                    let value_payload = builder.build_ptr_to_int(
                        value_pointer,
                        self.context.i64_type(),
                        "abi.indirect.value.wide",
                    )?;
                    let success_packed = self.context.i64_type().array_type(2).const_zero();
                    let success_packed = builder
                        .build_insert_value(
                            success_packed,
                            self.context.i64_type().const_zero(),
                            0,
                            "abi.indirect.success.field",
                        )?
                        .into_array_value();
                    let success_packed = builder
                        .build_insert_value(
                            success_packed,
                            value_payload,
                            1,
                            "abi.indirect.value.field",
                        )?
                        .into_array_value();
                    builder.build_return(Some(&success_packed))?;
                } else if wrapper.signature.result.as_ref() == &Type::Primitive(PrimitiveType::Void)
                {
                    builder.build_return(None)?;
                } else if self.is_indirect_abi_type(&wrapper.signature.result) {
                    let result = call.try_as_basic_value().basic().ok_or_else(|| {
                        CodegenError::Builder("indirect body returned void".into())
                    })?;
                    let llvm_type = self.basic_type(&wrapper.signature.result)?;
                    let size = llvm_type.size_of().ok_or_else(|| {
                        CodegenError::Unsupported("indirect ABI result has no known size".into())
                    })?;
                    let pointer = builder
                        .build_call(self.runtime_alloc(), &[size.into()], "abi.indirect.alloc")?
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                        .into_pointer_value();
                    builder.build_store(pointer, result)?;
                    builder.build_return(Some(&pointer))?;
                } else {
                    let result = call.try_as_basic_value().basic().ok_or_else(|| {
                        CodegenError::Builder("indirect body returned void".into())
                    })?;
                    builder.build_return(Some(&result))?;
                }
                continue;
            }
            let call = builder
                .build_call(wrapper.body, &arguments, "abi.body")?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("fallible body returned void".into()))?
                .into_struct_value();
            let failed = builder
                .build_extract_value(call, 0, "abi.failed")?
                .into_int_value();
            let failed =
                builder.build_int_z_extend(failed, self.context.i64_type(), "abi.failed.wide")?;
            let payload = match wrapper.kind {
                AbiWrapperKind::FallibleVoid => builder.build_ptr_to_int(
                    builder
                        .build_extract_value(call, 1, "abi.error")?
                        .into_pointer_value(),
                    self.context.i64_type(),
                    "abi.error.wide",
                )?,
                AbiWrapperKind::FallibleValue => {
                    let value = builder.build_extract_value(call, 1, "abi.value")?;
                    self.abi_payload_to_i64(&builder, value, &wrapper.signature.result)?
                }
                AbiWrapperKind::FallibleIndirect => {
                    unreachable!("fallible indirect ABI wrapper handled above")
                }
                AbiWrapperKind::EffectLift => unreachable!("effect-lift wrapper handled above"),
                AbiWrapperKind::Indirect => unreachable!("indirect ABI wrapper handled above"),
            };
            let value = self.context.i64_type().array_type(2).const_zero();
            let value = builder
                .build_insert_value(value, failed, 0, "abi.failed.field")?
                .into_array_value();
            let value = builder
                .build_insert_value(value, payload, 1, "abi.payload.field")?
                .into_array_value();
            builder.build_return(Some(&value))?;
        }
        Ok(())
    }

    fn is_indirect_abi_type(&self, ty: &Type) -> bool {
        matches!(ty, Type::Optional(_) | Type::Array(_, _) | Type::Slice(_))
            || matches!(ty, Type::Nominal(declaration, _) if !self.is_class_type(ty)
                && self.layouts.nominals.contains_key(declaration))
    }

    fn abi_payload_to_i64(
        &self,
        builder: &Builder<'ctx>,
        value: BasicValueEnum<'ctx>,
        ty: &Type,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        if value.is_pointer_value() {
            return Ok(builder.build_ptr_to_int(
                value.into_pointer_value(),
                self.context.i64_type(),
                "abi.payload.pointer",
            )?);
        }
        if value.is_int_value() {
            let value = value.into_int_value();
            return Ok(match value.get_type().get_bit_width().cmp(&64) {
                std::cmp::Ordering::Less => {
                    builder.build_int_z_extend(value, self.context.i64_type(), "abi.payload.int")?
                }
                std::cmp::Ordering::Greater => {
                    builder.build_int_truncate(value, self.context.i64_type(), "abi.payload.int")?
                }
                std::cmp::Ordering::Equal => value,
            });
        }
        Err(CodegenError::Unsupported(format!(
            "fallible ABI payload type cannot cross the packed boundary: {ty:?}"
        )))
    }

    fn body_function_type(&self, body: &Body) -> Result<LlvmFunctionType<'ctx>, CodegenError> {
        let parameters = self.body_parameter_types(body);
        self.llvm_function_type(&parameters, &body.return_type, &body.effects)
    }

    fn body_parameter_types(&self, body: &Body) -> Vec<Type> {
        let mut parameters = body
            .locals
            .iter()
            .filter(|local| local.argument)
            .map(|local| local.ty.clone())
            .collect::<Vec<_>>();
        if body.locals.first().is_some_and(|local| {
            local.argument && matches!(local.name.as_deref(), Some("self" | "this"))
        }) && body
            .locals
            .first()
            .is_some_and(|local| !self.is_class_type(&local.ty))
        {
            parameters[0] = Self::receiver_pointer_type();
        }
        parameters
    }

    fn receiver_pointer_type() -> Type {
        Type::RawPointer {
            mutable: true,
            pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
        }
    }

    fn llvm_function_type(
        &self,
        parameters: &[Type],
        result: &Type,
        effects: &[DeclarationId],
    ) -> Result<LlvmFunctionType<'ctx>, CodegenError> {
        let parameters = parameters
            .iter()
            .map(|parameter| self.basic_type(parameter).map(BasicMetadataTypeEnum::from))
            .collect::<Result<Vec<_>, _>>()?;
        if matches!(result, Type::Promise { .. }) {
            return Ok(self.basic_type(result)?.fn_type(&parameters, false));
        }
        if effects.is_empty() {
            if *result == Type::Primitive(PrimitiveType::Void) {
                Ok(self.context.void_type().fn_type(&parameters, false))
            } else {
                Ok(self.basic_type(result)?.fn_type(&parameters, false))
            }
        } else {
            Ok(self.completion_type(result)?.fn_type(&parameters, false))
        }
    }

    fn completion_type(
        &self,
        success: &Type,
    ) -> Result<inkwell::types::StructType<'ctx>, CodegenError> {
        // Completion records cross the C and Node-API boundaries by value. An ABI-sized byte
        // gives the tag the same layout as C `_Bool` on both supported targets; LLVM `i1` can
        // otherwise be packed differently in a returned aggregate.
        let mut fields = vec![self.context.i8_type().into()];
        if *success != Type::Primitive(PrimitiveType::Void) {
            fields.push(self.basic_type(success)?);
        }
        fields.push(self.context.ptr_type(AddressSpace::default()).into());
        Ok(self.context.struct_type(&fields, false))
    }

    fn basic_type(&self, ty: &Type) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        let pointer = || self.context.ptr_type(AddressSpace::default()).into();
        Ok(match ty {
            Type::Primitive(primitive) => match primitive {
                PrimitiveType::Bool => self.context.bool_type().into(),
                PrimitiveType::I8 | PrimitiveType::U8 => self.context.i8_type().into(),
                PrimitiveType::I16 | PrimitiveType::U16 => self.context.i16_type().into(),
                PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => {
                    self.context.i32_type().into()
                }
                PrimitiveType::I64 | PrimitiveType::U64 => self.context.i64_type().into(),
                PrimitiveType::I128 | PrimitiveType::U128 => self.context.i128_type().into(),
                PrimitiveType::Isize | PrimitiveType::Usize => self.pointer_int_type().into(),
                PrimitiveType::F32 => self.context.f32_type().into(),
                PrimitiveType::F64 => self.context.f64_type().into(),
                PrimitiveType::Void | PrimitiveType::Never => {
                    return Err(CodegenError::Unsupported(format!(
                        "{primitive:?} is not a first-class value type"
                    )));
                }
            },
            Type::Unknown => {
                return Err(CodegenError::Unsupported(
                    "unknown values must be narrowed before code generation".into(),
                ));
            }
            Type::Reference { .. }
            | Type::RawPointer { .. }
            | Type::String
            | Type::Str
            | Type::Promise { .. }
            | Type::Function(_)
            | Type::Template(_)
            | Type::ErrorUnion(_) => pointer(),
            Type::DynamicInterface(_, _) => self
                .context
                .struct_type(&[pointer(), pointer()], false)
                .into(),
            Type::Nominal(declaration, arguments) => self.nominal_type(*declaration, arguments)?,
            Type::Optional(inner) => self
                .context
                .struct_type(
                    &[self.context.bool_type().into(), self.basic_type(inner)?],
                    false,
                )
                .into(),
            Type::Array(element, length) => self
                .basic_type(element)?
                .array_type(u32::try_from(*length).map_err(|_| {
                    CodegenError::Unsupported("array length exceeds LLVM limit".into())
                })?)
                .into(),
            Type::Slice(_) => self
                .context
                .struct_type(&[pointer(), self.pointer_int_type().into()], false)
                .into(),
            Type::Tuple(elements) => self
                .context
                .struct_type(
                    &elements
                        .iter()
                        .map(|element| self.basic_type(element))
                        .collect::<Result<Vec<_>, _>>()?,
                    false,
                )
                .into(),
            Type::Generic(_) | Type::Lifetime(_) | Type::Error => {
                return Err(CodegenError::Unsupported(format!(
                    "unresolved type reached LLVM lowering: {ty:?}"
                )));
            }
        })
    }

    fn nominal_type(
        &self,
        declaration: DeclarationId,
        arguments: &[Type],
    ) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        let Some(layout) = self.layouts.nominals.get(&declaration) else {
            return Ok(self.context.ptr_type(AddressSpace::default()).into());
        };
        if layout.type_parameters.len() != arguments.len() {
            return Err(CodegenError::Unsupported(format!(
                "nominal layout {:?} expects {} type arguments, found {}",
                declaration,
                layout.type_parameters.len(),
                arguments.len()
            )));
        }
        let substitutions = layout
            .type_parameters
            .iter()
            .cloned()
            .zip(arguments.iter().cloned())
            .collect::<BTreeMap<_, _>>();
        match &layout.kind {
            NominalKind::Class { .. } => Ok(self.context.ptr_type(AddressSpace::default()).into()),
            NominalKind::Struct { fields } => Ok(self
                .context
                .struct_type(
                    &fields
                        .iter()
                        .map(|field| self.basic_type(&instantiate_type(field, &substitutions)))
                        .collect::<Result<Vec<_>, _>>()?,
                    false,
                )
                .into()),
            NominalKind::Enum {
                variants, c_repr, ..
            } => {
                if *c_repr {
                    return Ok(self.context.i32_type().into());
                }
                let mut fields = vec![self.context.i64_type().into()];
                for field in variants.iter().flatten() {
                    fields.push(self.basic_type(&instantiate_type(field, &substitutions))?);
                }
                Ok(self.context.struct_type(&fields, false).into())
            }
        }
    }

    fn pointer_int_type(&self) -> inkwell::types::IntType<'ctx> {
        let layout = self.module.get_data_layout();
        let bits = layout
            .as_str()
            .to_str()
            .ok()
            .and_then(parse_pointer_bits)
            .unwrap_or(64);
        if bits == 32 {
            self.context.i32_type()
        } else {
            self.context.i64_type()
        }
    }

    fn class_object_type(
        &self,
        ty: &Type,
    ) -> Result<inkwell::types::StructType<'ctx>, CodegenError> {
        let Type::Nominal(declaration, arguments) = ty else {
            return Err(CodegenError::Unsupported(format!(
                "class object layout requires a nominal class type, found {ty:?}"
            )));
        };
        let Some(layout) = self.layouts.nominals.get(declaration) else {
            return Err(CodegenError::Unsupported(format!(
                "class object layout is not registered: {ty:?}"
            )));
        };
        let NominalKind::Class { .. } = &layout.kind else {
            return Err(CodegenError::Unsupported(format!(
                "class object layout requested for non-class type: {ty:?}"
            )));
        };
        if layout.type_parameters.len() != arguments.len() {
            return Err(CodegenError::Unsupported(format!(
                "class layout {:?} expects {} type arguments, found {}",
                declaration,
                layout.type_parameters.len(),
                arguments.len()
            )));
        }
        let substitutions = layout
            .type_parameters
            .iter()
            .cloned()
            .zip(arguments.iter().cloned())
            .collect::<BTreeMap<_, _>>();
        let mut fields = vec![self.context.ptr_type(AddressSpace::default()).into()];
        for field in self.class_field_types(*declaration, &substitutions)? {
            fields.push(self.basic_type(&field)?);
        }
        Ok(self.context.struct_type(&fields, false))
    }

    fn class_field_types(
        &self,
        declaration: DeclarationId,
        substitutions: &BTreeMap<String, Type>,
    ) -> Result<Vec<Type>, CodegenError> {
        let Some(layout) = self.layouts.nominals.get(&declaration) else {
            return Err(CodegenError::Unsupported(format!(
                "class layout {declaration:?} is not registered"
            )));
        };
        let NominalKind::Class { base, fields, .. } = &layout.kind else {
            return Err(CodegenError::Unsupported(format!(
                "class field layout requested for non-class {declaration:?}"
            )));
        };
        let mut result = if let Some(base) = base {
            self.class_field_types(*base, &BTreeMap::new())?
        } else {
            Vec::new()
        };
        result.extend(
            fields
                .iter()
                .map(|field| instantiate_type(field, substitutions)),
        );
        Ok(result)
    }

    fn is_enum(&self, declaration: DeclarationId) -> bool {
        self.layouts
            .nominals
            .get(&declaration)
            .is_some_and(|layout| matches!(layout.kind, NominalKind::Enum { .. }))
    }

    fn is_class_type(&self, ty: &Type) -> bool {
        matches!(
            ty,
            Type::Nominal(declaration, _)
                if self
                    .layouts
                    .nominals
                    .get(declaration)
                    .is_some_and(|layout| matches!(layout.kind, NominalKind::Class { .. }))
        )
    }

    fn descriptor_for_type(&self, ty: &Type) -> Option<PointerValue<'ctx>> {
        let Type::Nominal(declaration, arguments) = ty else {
            return None;
        };
        self.descriptors
            .get(&(*declaration, arguments.clone()))
            .copied()
    }

    fn layout_field_index(
        &self,
        ty: &Type,
        variant: Option<u32>,
        field: u32,
    ) -> Result<u32, CodegenError> {
        let Type::Nominal(declaration, _) = ty else {
            return Ok(field);
        };
        let Some(layout) = self.layouts.nominals.get(declaration) else {
            return Ok(field);
        };
        match &layout.kind {
            NominalKind::Struct { .. } => Ok(field),
            NominalKind::Enum {
                variants, c_repr, ..
            } => {
                if *c_repr {
                    return Err(CodegenError::Unsupported(
                        "C-represented fieldless enum has no payload fields".into(),
                    ));
                }
                let variant = variant.ok_or_else(|| {
                    CodegenError::Unsupported("enum field projection lacks downcast".into())
                })?;
                let offset = 1_usize
                    + variants
                        .iter()
                        .take(variant as usize)
                        .map(Vec::len)
                        .sum::<usize>()
                    + field as usize;
                u32::try_from(offset)
                    .map_err(|_| CodegenError::Unsupported("enum field limit".into()))
            }
            NominalKind::Class { .. } => field
                .checked_add(1)
                .ok_or_else(|| CodegenError::Unsupported("class field limit".into())),
        }
    }
}

fn return_constructor_success<'ctx>(
    builder: &Builder<'ctx>,
    generator: &Generator<'ctx>,
    target: &ConstructorTarget<'ctx>,
    object: PointerValue<'ctx>,
) -> Result<(), CodegenError> {
    if target.signature.effects.is_empty() {
        builder.build_return(Some(&object))?;
        return Ok(());
    }
    let completion = generator.completion_type(&target.signature.result)?;
    let value = builder
        .build_insert_value(
            completion.const_zero(),
            generator.context.i8_type().const_zero(),
            0,
            "constructor.success.tag",
        )?
        .into_struct_value();
    let value = builder
        .build_insert_value(value, object, 1, "constructor.success.value")?
        .into_struct_value();
    let value = builder
        .build_insert_value(
            value,
            generator
                .context
                .ptr_type(AddressSpace::default())
                .const_null(),
            2,
            "constructor.success.error",
        )?
        .into_struct_value();
    builder.build_return(Some(&value))?;
    Ok(())
}

struct FunctionGenerator<'a, 'ctx> {
    generator: &'a Generator<'ctx>,
    body: &'a Body,
    function: FunctionValue<'ctx>,
    builder: Builder<'ctx>,
    blocks: Vec<LlvmBlock<'ctx>>,
    locals: Vec<PointerValue<'ctx>>,
    drop_flags: Vec<PointerValue<'ctx>>,
}

impl<'a, 'ctx> FunctionGenerator<'a, 'ctx> {
    fn new(
        generator: &'a Generator<'ctx>,
        body: &'a Body,
        function: FunctionValue<'ctx>,
    ) -> Result<Self, CodegenError> {
        let builder = generator.context.create_builder();
        let entry = generator.context.append_basic_block(function, "allocas");
        builder.position_at_end(entry);
        if let Some(subprogram) = function.get_subprogram() {
            let location = generator.debug_info.builder.create_debug_location(
                generator.context,
                1,
                1,
                subprogram.as_debug_info_scope(),
                None,
            );
            builder.set_current_debug_location(location);
        }
        let mut locals = Vec::new();
        let mut drop_flags = Vec::new();
        for (index, local) in body.locals.iter().enumerate() {
            locals.push(
                builder
                    .build_alloca(generator.basic_type(&local.ty)?, &format!("local.{index}"))?,
            );
            let flag = builder
                .build_alloca(generator.context.bool_type(), &format!("dropflag.{index}"))?;
            builder.build_store(flag, generator.context.bool_type().const_zero())?;
            drop_flags.push(flag);
        }
        for (argument_index, (parameter, (local_index, _))) in function
            .get_param_iter()
            .zip(
                body.locals
                    .iter()
                    .enumerate()
                    .filter(|(_, local)| local.argument),
            )
            .enumerate()
        {
            let local = body
                .locals
                .iter()
                .filter(|local| local.argument)
                .nth(argument_index)
                .ok_or_else(|| CodegenError::Unsupported("missing method argument".into()))?;
            if argument_index == 0
                && matches!(local.name.as_deref(), Some("self" | "this"))
                && !generator.is_class_type(&local.ty)
            {
                // Value receivers use an indirect ABI. Keep the receiver local
                // as an alias of the caller's storage so mutable methods update
                // the original value instead of a detached copy.
                locals[local_index] = parameter.into_pointer_value();
            } else {
                builder.build_store(locals[local_index], parameter)?;
            }
        }
        let blocks = body
            .blocks
            .iter()
            .enumerate()
            .map(|(index, _)| {
                generator
                    .context
                    .append_basic_block(function, &format!("bb{index}"))
            })
            .collect::<Vec<_>>();
        builder.build_unconditional_branch(blocks[0])?;
        Ok(Self {
            generator,
            body,
            function,
            builder,
            blocks,
            locals,
            drop_flags,
        })
    }

    fn lower(&self) -> Result<(), CodegenError> {
        for (index, block) in self.body.blocks.iter().enumerate() {
            self.builder.position_at_end(self.blocks[index]);
            if let Some(subprogram) = self.function.get_subprogram() {
                let location = self.generator.debug_info.builder.create_debug_location(
                    self.generator.context,
                    1,
                    1,
                    subprogram.as_debug_info_scope(),
                    None,
                );
                self.builder.set_current_debug_location(location);
            }
            for statement in &block.statements {
                self.lower_statement(&statement.kind)?;
            }
            self.lower_terminator(&block.terminator.kind)?;
        }
        Ok(())
    }

    fn lower_statement(&self, statement: &StatementKind) -> Result<(), CodegenError> {
        match statement {
            StatementKind::Assign(destination, value) => {
                let value = self.lower_rvalue(value)?;
                self.builder
                    .build_store(self.place_pointer(destination)?, value)?;
            }
            StatementKind::SetDropFlag(place, value) => {
                if self.is_borrowed_class_receiver(place) {
                    return Ok(());
                }
                let flag = self.drop_flags[usize::try_from(place.local.0).map_err(|_| {
                    CodegenError::Unsupported("drop flag local index overflow".into())
                })?];
                self.builder.build_store(
                    flag,
                    self.generator
                        .context
                        .bool_type()
                        .const_int(u64::from(*value), false),
                )?;
            }
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Retag(_) => {}
            StatementKind::Borrow {
                destination, place, ..
            } => {
                let destination = self.locals[usize::try_from(destination.0).map_err(|_| {
                    CodegenError::Unsupported("borrow destination index overflow".into())
                })?];
                self.builder
                    .build_store(destination, self.place_pointer(place)?)?;
            }
            StatementKind::SetDiscriminant(place, discriminant) => {
                let ty = self.place_type(place)?;
                let place = self.place_pointer(place)?;
                if let Type::Nominal(declaration, _) = &ty
                    && self.generator.is_enum(*declaration)
                    && self
                        .generator
                        .layouts
                        .nominals
                        .get(declaration)
                        .is_some_and(|layout| {
                            matches!(layout.kind, NominalKind::Enum { c_repr: true, .. })
                        })
                {
                    let tag = self.builder.build_store(
                        place,
                        self.generator
                            .context
                            .i32_type()
                            .const_int(u64::from(*discriminant), false),
                    )?;
                    let _ = tag;
                } else {
                    let structure = self.generator.basic_type(&ty)?.into_struct_type();
                    let tag = self.builder.build_struct_gep(
                        structure,
                        place,
                        0,
                        "optional.tag.address",
                    )?;
                    self.builder.build_store(
                        tag,
                        match &ty {
                            Type::Optional(_) => self
                                .generator
                                .context
                                .bool_type()
                                .const_int(u64::from(*discriminant != 0), false),
                            Type::Nominal(declaration, _)
                                if self.generator.is_enum(*declaration) =>
                            {
                                self.generator
                                    .context
                                    .i64_type()
                                    .const_int(u64::from(*discriminant), false)
                            }
                            _ => {
                                return Err(CodegenError::Unsupported(format!(
                                    "discriminant layout is not registered: {ty:?}"
                                )));
                            }
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn lower_rvalue(&self, value: &Rvalue) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match value {
            Rvalue::Use(operand) => self.lower_operand(operand),
            Rvalue::Unary {
                operator,
                operand,
                operand_type,
                ..
            } => {
                let operand = self.lower_operand(operand)?;
                match operator {
                    UnaryOperator::LogicalNot => Ok(self
                        .builder
                        .build_not(operand.into_int_value(), "not")?
                        .into()),
                    UnaryOperator::BitNot => Ok(self
                        .builder
                        .build_not(operand.into_int_value(), "bitnot")?
                        .into()),
                    UnaryOperator::Negate if operand.is_int_value() => {
                        let operand = operand.into_int_value();
                        Ok(self
                            .checked_integer_arithmetic(
                                BinaryOperator::Subtract,
                                operand.get_type().const_zero(),
                                operand,
                                is_signed(operand_type),
                            )?
                            .into())
                    }
                    UnaryOperator::Negate if operand.is_float_value() => Ok(self
                        .builder
                        .build_float_neg(operand.into_float_value(), "fneg")?
                        .into()),
                    UnaryOperator::Negate => Err(CodegenError::Unsupported(
                        "negation requires a numeric LLVM value".into(),
                    )),
                }
            }
            Rvalue::CheckedBinary {
                operator,
                left,
                right,
                operand_type,
                ..
            } => self.lower_binary(*operator, left, right, operand_type),
            Rvalue::CheckedIndex { collection, index } => {
                let index = self.lower_operand(index)?.into_int_value();
                let (pointer, element) = self.index_pointer(collection, index)?;
                Ok(self.builder.build_load(
                    self.generator.basic_type(&element)?,
                    pointer,
                    "indexed_value",
                )?)
            }
            Rvalue::Aggregate {
                ty,
                variant,
                fields,
                ..
            } => self.lower_aggregate(ty, *variant, fields),
            Rvalue::Length(place) => {
                let length = match self.place_type(place)? {
                    Type::Array(_, length) => {
                        self.generator.pointer_int_type().const_int(length, false)
                    }
                    Type::Slice(_) => {
                        let pointer = self.place_pointer(place)?;
                        let structure = self.generator.basic_type(&self.place_type(place)?)?;
                        let BasicTypeEnum::StructType(structure) = structure else {
                            return Err(CodegenError::Unsupported(
                                "slice length requires a slice layout".into(),
                            ));
                        };
                        let field = self.builder.build_struct_gep(
                            structure,
                            pointer,
                            1,
                            "slice.length.address",
                        )?;
                        self.builder
                            .build_load(self.generator.pointer_int_type(), field, "slice.length")?
                            .into_int_value()
                    }
                    ty => {
                        return Err(CodegenError::Unsupported(format!(
                            "length requires an array or slice, found {ty:?}"
                        )));
                    }
                };
                Ok(length.into())
            }
            Rvalue::Cast {
                operand, ty, kind, ..
            } => {
                if *kind == tn_mir::CastKind::InterfaceCoercion {
                    return self.lower_interface_cast(operand, ty);
                }
                if *kind == tn_mir::CastKind::CheckedDowncast
                    && matches!(self.operand_type(operand)?, Type::ErrorUnion(_))
                {
                    return self.lower_error_downcast(operand, ty);
                }
                if let Type::Optional(inner) = ty
                    && self.operand_type(operand)? != ty.clone()
                {
                    let value = self.lower_operand(operand)?;
                    let structure = self.generator.basic_type(ty)?.into_struct_type();
                    let payload = self.lower_cast(value, self.generator.basic_type(inner)?)?;
                    let value = self
                        .builder
                        .build_insert_value(
                            structure.const_zero(),
                            self.generator.context.bool_type().const_int(1, false),
                            0,
                            "optional.present",
                        )?
                        .into_struct_value();
                    return Ok(self
                        .builder
                        .build_insert_value(value, payload, 1, "optional.payload")?
                        .into_struct_value()
                        .into());
                }
                let value = self.lower_operand(operand)?;
                let target = self.generator.basic_type(ty)?;
                self.lower_cast(value, target)
            }
            Rvalue::DirectMethod {
                implementation,
                member,
                ty,
                ..
            } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "direct method lookup lacks a function type".into(),
                    ));
                };
                let (function, _) =
                    self.resolve_function(&Operand::Constant(Constant::Method {
                        owner: *implementation,
                        member: *member,
                        ty: ty.clone(),
                    }))?;
                let _ = function_type;
                Ok(function.as_global_value().as_pointer_value().into())
            }
            Rvalue::VtableLookup { object, slot, .. } => {
                Ok(self.lower_vtable_lookup(object, *slot)?.into())
            }
            Rvalue::WitnessLookup { object, slot, .. } => {
                Ok(self.lower_witness_lookup(object, *slot)?.into())
            }
            Rvalue::TypeTest { operand, target } => {
                let Type::Nominal(_, _) = target else {
                    return Err(CodegenError::Unsupported(format!(
                        "instanceof target is not a nominal class: {target:?}"
                    )));
                };
                if !self.generator.is_class_type(target) {
                    return Err(CodegenError::Unsupported(format!(
                        "instanceof target is not a class: {target:?}"
                    )));
                }
                let source = self.operand_type(operand)?;
                if !self.generator.is_class_type(&source) {
                    return Err(CodegenError::Unsupported(format!(
                        "instanceof source is not a class: {source:?}"
                    )));
                }
                let object = self.lower_operand(operand)?.into_pointer_value();
                let source_layout = self.generator.class_object_type(&source)?;
                let header = self.builder.build_struct_gep(
                    source_layout,
                    object,
                    0,
                    "type.test.descriptor.address",
                )?;
                let descriptor = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        header,
                        "type.test.descriptor",
                    )?
                    .into_pointer_value();
                let target_descriptor =
                    self.generator.descriptor_for_type(target).ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "class descriptor is not registered for {target:?}"
                        ))
                    })?;
                let descriptor = self.builder.build_ptr_to_int(
                    descriptor,
                    self.generator.pointer_int_type(),
                    "type.test.descriptor.int",
                )?;
                let target_descriptor = self.builder.build_ptr_to_int(
                    target_descriptor,
                    self.generator.pointer_int_type(),
                    "type.test.target.int",
                )?;
                Ok(self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        descriptor,
                        target_descriptor,
                        "type.test",
                    )?
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "size_of" => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("size_of operation lacks a type marker".into())
                })?;
                let ty = self.operand_type(operand)?;
                let size = self.generator.basic_type(&ty)?.size_of().ok_or_else(|| {
                    CodegenError::Unsupported(format!(
                        "could not determine size of monomorphized type {ty:?}"
                    ))
                })?;
                Ok(size.into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "is_string" => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("is_string operation lacks a type marker".into())
                })?;
                let ty = self.operand_type(operand)?;
                Ok(self
                    .generator
                    .context
                    .bool_type()
                    .const_int(u64::from(matches!(ty, Type::String)), false)
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "element_initialized" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "element_initialized operation lacks a pointer".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let index = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "element_initialized operation lacks an index".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let address = unsafe {
                    self.builder.build_gep(
                        self.generator.context.i8_type(),
                        pointer,
                        &[index],
                        "element.initialized.address",
                    )?
                };
                let value = self
                    .builder
                    .build_load(
                        self.generator.context.i8_type(),
                        address,
                        "element.initialized.value",
                    )?
                    .into_int_value();
                Ok(self
                    .builder
                    .build_int_compare(
                        IntPredicate::NE,
                        value,
                        value.get_type().const_zero(),
                        "element.initialized",
                    )?
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "move_element" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("move_element operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let index = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("move_element operation lacks an index".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let initialized = operands
                    .get(2)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "move_element operation lacks an initialized bitmap".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let Type::RawPointer { pointee, .. } =
                    self.operand_type(operands.first().ok_or_else(|| {
                        CodegenError::Unsupported("move_element operation lacks a pointer".into())
                    })?)?
                else {
                    return Err(CodegenError::Unsupported(
                        "move_element pointer is not a raw pointer".into(),
                    ));
                };
                let element_type = pointee.as_ref().clone();
                let element_pointer = unsafe {
                    self.builder.build_gep(
                        self.generator.basic_type(&element_type)?,
                        pointer,
                        &[index],
                        "move.element.address",
                    )?
                };
                let value = self.builder.build_load(
                    self.generator.basic_type(&element_type)?,
                    element_pointer,
                    "move.element.value",
                )?;
                let initialized_address = unsafe {
                    self.builder.build_gep(
                        self.generator.context.i8_type(),
                        initialized,
                        &[index],
                        "move.element.initialized.address",
                    )?
                };
                self.builder.build_store(
                    initialized_address,
                    self.generator.context.i8_type().const_zero(),
                )?;
                Ok(value)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "store_element" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("store_element operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let index = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("store_element operation lacks an index".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let initialized = operands
                    .get(2)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "store_element operation lacks an initialized bitmap".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let value = operands.get(3).ok_or_else(|| {
                    CodegenError::Unsupported("store_element operation lacks a value".into())
                })?;
                let value = self.lower_operand(value)?;
                let Type::RawPointer { pointee, .. } =
                    self.operand_type(operands.first().ok_or_else(|| {
                        CodegenError::Unsupported("store_element operation lacks a pointer".into())
                    })?)?
                else {
                    return Err(CodegenError::Unsupported(
                        "store_element pointer is not a raw pointer".into(),
                    ));
                };
                let element_type = pointee.as_ref().clone();
                let element_pointer = unsafe {
                    self.builder.build_gep(
                        self.generator.basic_type(&element_type)?,
                        pointer,
                        &[index],
                        "store.element.address",
                    )?
                };
                let initialized_address = unsafe {
                    self.builder.build_gep(
                        self.generator.context.i8_type(),
                        initialized,
                        &[index],
                        "store.element.initialized.address",
                    )?
                };
                let initialized_value = self
                    .builder
                    .build_load(
                        self.generator.context.i8_type(),
                        initialized_address,
                        "store.element.initialized.value",
                    )?
                    .into_int_value();
                let occupied = self.builder.build_int_compare(
                    IntPredicate::NE,
                    initialized_value,
                    initialized_value.get_type().const_zero(),
                    "store.element.occupied",
                )?;
                let drop_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "store.element.drop");
                let store_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "store.element.store");
                let done_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "store.element.done");
                self.builder
                    .build_conditional_branch(occupied, drop_block, store_block)?;
                self.builder.position_at_end(drop_block);
                self.lower_drop_value_at_pointer(element_pointer, &element_type)?;
                self.builder.build_unconditional_branch(store_block)?;
                self.builder.position_at_end(store_block);
                self.builder.build_store(element_pointer, value)?;
                self.builder.build_store(
                    initialized_address,
                    self.generator.context.i8_type().const_int(1, false),
                )?;
                self.builder.build_unconditional_branch(done_block)?;
                self.builder.position_at_end(done_block);
                Ok(self.generator.context.bool_type().const_all_ones().into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "dereference" => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("dereference operation lacks an operand".into())
                })?;
                let pointer = self.lower_operand(operand)?.into_pointer_value();
                Ok(self.builder.build_load(
                    self.generator.basic_type(ty)?,
                    pointer,
                    "raw.dereference",
                )?)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if matches!(operation.as_str(), "borrow_mut" | "borrow_shared") => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported(format!("{operation} operation lacks an operand"))
                })?;
                Ok(self.lower_operand(operand)?.into_pointer_value().into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "store_raw" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("store_raw operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let value = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("store_raw operation lacks a value".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?;
                self.builder.build_store(pointer, value)?;
                Ok(self.generator.context.bool_type().const_all_ones().into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "drop_initialized_elements" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "drop_initialized_elements operation lacks a pointer".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let length = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "drop_initialized_elements operation lacks a length".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let initialized = operands
                    .get(2)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "drop_initialized_elements operation lacks an initialized bitmap"
                                .into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let Type::RawPointer { pointee, .. } =
                    self.operand_type(operands.first().ok_or_else(|| {
                        CodegenError::Unsupported(
                            "drop_initialized_elements operation lacks a pointer".into(),
                        )
                    })?)?
                else {
                    return Err(CodegenError::Unsupported(
                        "drop_initialized_elements pointer is not a raw pointer".into(),
                    ));
                };
                let element_type = pointee.as_ref().clone();
                let loop_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.elements.loop");
                let body_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.elements.check");
                let drop_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.elements.body");
                let skip_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.elements.skip");
                let next_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.elements.next");
                let done_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.elements.done");
                let index = self
                    .builder
                    .build_alloca(self.generator.pointer_int_type(), "drop.elements.index")?;
                self.builder
                    .build_store(index, self.generator.pointer_int_type().const_zero())?;
                self.builder.build_unconditional_branch(loop_block)?;
                self.builder.position_at_end(loop_block);
                let current = self
                    .builder
                    .build_load(
                        self.generator.pointer_int_type(),
                        index,
                        "drop.elements.current",
                    )?
                    .into_int_value();
                let active = self.builder.build_int_compare(
                    IntPredicate::ULT,
                    current,
                    length,
                    "drop.elements.active",
                )?;
                self.builder
                    .build_conditional_branch(active, body_block, done_block)?;
                self.builder.position_at_end(body_block);
                let initialized_address = unsafe {
                    self.builder.build_gep(
                        self.generator.context.i8_type(),
                        initialized,
                        &[current],
                        "drop.elements.initialized.address",
                    )?
                };
                let initialized_value = self
                    .builder
                    .build_load(
                        self.generator.context.i8_type(),
                        initialized_address,
                        "drop.elements.initialized.value",
                    )?
                    .into_int_value();
                let occupied = self.builder.build_int_compare(
                    IntPredicate::NE,
                    initialized_value,
                    initialized_value.get_type().const_zero(),
                    "drop.elements.occupied",
                )?;
                self.builder
                    .build_conditional_branch(occupied, drop_block, skip_block)?;
                self.builder.position_at_end(drop_block);
                let element_llvm_type = self.generator.basic_type(&element_type)?;
                let element_pointer = unsafe {
                    self.builder.build_gep(
                        element_llvm_type,
                        pointer,
                        &[current],
                        "drop.elements.address",
                    )?
                };
                self.lower_drop_value_at_pointer(element_pointer, &element_type)?;
                self.builder.build_store(
                    initialized_address,
                    self.generator.context.i8_type().const_zero(),
                )?;
                self.builder.build_unconditional_branch(next_block)?;
                self.builder.position_at_end(skip_block);
                self.builder.build_unconditional_branch(next_block)?;
                self.builder.position_at_end(next_block);
                let next = self.builder.build_int_add(
                    current,
                    current.get_type().const_int(1, false),
                    "drop.elements.next",
                )?;
                self.builder.build_store(index, next)?;
                self.builder.build_unconditional_branch(loop_block)?;
                self.builder.position_at_end(done_block);
                Ok(self.generator.context.bool_type().const_all_ones().into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "string_from_static" => {
                let text = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "string_from_static operation lacks an operand".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let length = self
                    .builder
                    .build_call(
                        self.generator.runtime_strlen(),
                        &[text.into()],
                        "string.length",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError::Builder("strlen returned void".into()))?
                    .into_int_value();
                let value = self
                    .builder
                    .build_call(
                        self.generator.runtime_string_from_bytes(),
                        &[text.into(), length.into()],
                        "string.from",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| {
                        CodegenError::Builder("string conversion returned void".into())
                    })?;
                let _ = ty;
                Ok(value)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "arc_clone" => {
                let source_operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("arc_clone operation lacks a receiver".into())
                })?;
                let source_type = self.operand_type(source_operand)?;
                let source = self.lower_operand(source_operand)?.into_pointer_value();
                let source = if matches!(&source_type, Type::Reference { .. }) {
                    self.builder
                        .build_load(
                            self.generator.context.ptr_type(AddressSpace::default()),
                            source,
                            "arc.reference",
                        )?
                        .into_pointer_value()
                } else {
                    source
                };
                let source_type = match source_type {
                    Type::Reference { referent, .. } => referent.as_ref().clone(),
                    ty => ty,
                };
                let source_layout = self.generator.class_object_type(&source_type)?;
                let source_pointer = self.builder.build_struct_gep(
                    source_layout,
                    source,
                    1,
                    "arc.source.pointer.address",
                )?;
                let source_pointer = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        source_pointer,
                        "arc.source.pointer",
                    )?
                    .into_pointer_value();
                let object_layout = self.generator.class_object_type(ty)?;
                let size = object_layout.size_of().ok_or_else(|| {
                    CodegenError::Unsupported(
                        "arc class object has no statically known size".into(),
                    )
                })?;
                let object = self
                    .builder
                    .build_call(self.generator.runtime_alloc(), &[size.into()], "arc.object")?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                    .into_pointer_value();
                let descriptor = self.generator.descriptor_for_type(ty).unwrap_or_else(|| {
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null()
                });
                let descriptor_address = self.builder.build_struct_gep(
                    object_layout,
                    object,
                    0,
                    "arc.descriptor.address",
                )?;
                self.builder.build_store(descriptor_address, descriptor)?;
                let retained = self
                    .builder
                    .build_call(
                        self.generator.runtime_ref_retain(),
                        &[source_pointer.into()],
                        "arc.retain",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError::Builder("arc retain returned void".into()))?
                    .into_pointer_value();
                let pointer_address = self.builder.build_struct_gep(
                    object_layout,
                    object,
                    1,
                    "arc.pointer.address",
                )?;
                self.builder.build_store(pointer_address, retained)?;
                let alive_address =
                    self.builder
                        .build_struct_gep(object_layout, object, 2, "arc.alive.address")?;
                self.builder.build_store(
                    alive_address,
                    self.generator.context.bool_type().const_int(1, false),
                )?;
                Ok(object.into())
            }
            _ => Err(CodegenError::Unsupported(format!(
                "rvalue has not reached a codegen-ready form: {value:?}"
            ))),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_aggregate(
        &self,
        ty: &Type,
        variant: Option<u32>,
        fields: &[Operand],
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match ty {
            Type::Tuple(_) | Type::Optional(_) => {
                let structure = self.generator.basic_type(ty)?.into_struct_type();
                let mut value = structure.const_zero();
                for (index, field) in fields.iter().enumerate() {
                    value = self
                        .builder
                        .build_insert_value(
                            value,
                            self.lower_operand(field)?,
                            u32::try_from(index).map_err(|_| {
                                CodegenError::Unsupported("aggregate field limit".into())
                            })?,
                            "aggregate.field",
                        )?
                        .into_struct_value();
                }
                Ok(value.into())
            }
            Type::Array(_, _) => {
                let array = self.generator.basic_type(ty)?.into_array_type();
                let mut value = array.const_zero();
                for (index, field) in fields.iter().enumerate() {
                    value = self
                        .builder
                        .build_insert_value(
                            value,
                            self.lower_operand(field)?,
                            u32::try_from(index).map_err(|_| {
                                CodegenError::Unsupported("array element limit".into())
                            })?,
                            "array.element",
                        )?
                        .into_array_value();
                }
                Ok(value.into())
            }
            Type::Nominal(declaration, _) => {
                let layout = self
                    .generator
                    .layouts
                    .nominals
                    .get(declaration)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "nominal aggregate layout is not registered: {ty:?}"
                        ))
                    })?;
                if matches!(layout.kind, NominalKind::Class { .. }) {
                    return Err(CodegenError::Unsupported(
                        "class values must be created by a constructor".into(),
                    ));
                }
                if let NominalKind::Enum {
                    c_repr: true,
                    discriminants,
                    ..
                } = &layout.kind
                {
                    let variant = variant.ok_or_else(|| {
                        CodegenError::Unsupported(
                            "C-represented enum aggregate lacks a variant".into(),
                        )
                    })?;
                    let value = *discriminants.get(variant as usize).ok_or_else(|| {
                        CodegenError::Unsupported(
                            "C-represented enum variant is out of range".into(),
                        )
                    })?;
                    let value = i32::try_from(value).map_err(|_| {
                        CodegenError::Unsupported(
                            "C-represented enum discriminant does not fit i32".into(),
                        )
                    })?;
                    return Ok(self
                        .generator
                        .context
                        .i32_type()
                        .const_int(u64::from(value.cast_unsigned()), true)
                        .into());
                }
                let structure = self.generator.basic_type(ty)?.into_struct_type();
                let mut value = structure.const_zero();
                let offset = match (&layout.kind, variant) {
                    (NominalKind::Struct { .. }, None) => 0,
                    (NominalKind::Enum { variants, .. }, Some(variant)) => {
                        1 + variants
                            .iter()
                            .take(variant as usize)
                            .map(Vec::len)
                            .sum::<usize>()
                    }
                    _ => {
                        return Err(CodegenError::Unsupported(format!(
                            "aggregate variant does not match layout: {ty:?}"
                        )));
                    }
                };
                for (index, field) in fields.iter().enumerate() {
                    value = self
                        .builder
                        .build_insert_value(
                            value,
                            self.lower_operand(field)?,
                            u32::try_from(offset + index).map_err(|_| {
                                CodegenError::Unsupported("nominal field limit".into())
                            })?,
                            "nominal.field",
                        )?
                        .into_struct_value();
                }
                Ok(value.into())
            }
            _ => Err(CodegenError::Unsupported(format!(
                "invalid aggregate type: {ty:?}"
            ))),
        }
    }

    fn lower_binary(
        &self,
        operator: BinaryOperator,
        left: &Operand,
        right: &Operand,
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let left = self.lower_operand(left)?;
        let right = self.lower_operand(right)?;
        if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual) {
            if matches!(ty, Type::String | Type::Str) {
                return self.lower_string_equality(operator, left, right);
            }
            if matches!(ty, Type::Optional(_)) {
                return self.lower_optional_equality(operator, left, right);
            }
        }

        if left.is_float_value() {
            return self.lower_float_binary(operator, left, right);
        }
        let left = left.into_int_value();
        let right = right.into_int_value();
        let signed = is_signed(ty);
        Ok(match operator {
            BinaryOperator::Add | BinaryOperator::Subtract | BinaryOperator::Multiply => self
                .checked_integer_arithmetic(operator, left, right, signed)?
                .into(),
            BinaryOperator::Divide | BinaryOperator::Remainder => {
                self.guard(
                    self.builder.build_int_compare(
                        IntPredicate::NE,
                        right,
                        right.get_type().const_zero(),
                        "nonzero",
                    )?,
                    "division by zero",
                )?;
                if signed {
                    let bits = left.get_type().get_bit_width();
                    let minimum = left
                        .get_type()
                        .const_int_arbitrary_precision(&u128_words(1_u128 << (bits - 1)));
                    let minus_one = left.get_type().const_all_ones();
                    let minimum_left = self.builder.build_int_compare(
                        IntPredicate::EQ,
                        left,
                        minimum,
                        "minimum_left",
                    )?;
                    let minus_one_right = self.builder.build_int_compare(
                        IntPredicate::EQ,
                        right,
                        minus_one,
                        "minus_one_right",
                    )?;
                    let overflows = self.builder.build_and(
                        minimum_left,
                        minus_one_right,
                        "division_overflows",
                    )?;
                    self.guard(
                        self.builder.build_not(overflows, "division_valid")?,
                        "integer division overflow",
                    )?;
                }
                let value = match (operator, signed) {
                    (BinaryOperator::Divide, true) => {
                        self.builder.build_int_signed_div(left, right, "sdiv")?
                    }
                    (BinaryOperator::Divide, false) => {
                        self.builder.build_int_unsigned_div(left, right, "udiv")?
                    }
                    (BinaryOperator::Remainder, true) => {
                        self.builder.build_int_signed_rem(left, right, "srem")?
                    }
                    (BinaryOperator::Remainder, false) => {
                        self.builder.build_int_unsigned_rem(left, right, "urem")?
                    }
                    _ => unreachable!(),
                };
                value.into()
            }
            BinaryOperator::ShiftLeft => self.checked_left_shift(left, right, signed)?.into(),
            BinaryOperator::ShiftRight => {
                self.guard_shift_count(right)?;
                self.builder
                    .build_right_shift(left, right, signed, if signed { "ashr" } else { "lshr" })?
                    .into()
            }
            BinaryOperator::BitAnd | BinaryOperator::LogicalAnd => {
                self.builder.build_and(left, right, "and")?.into()
            }
            BinaryOperator::BitOr | BinaryOperator::LogicalOr => {
                self.builder.build_or(left, right, "or")?.into()
            }
            BinaryOperator::BitXor => self.builder.build_xor(left, right, "xor")?.into(),
            comparison => self
                .builder
                .build_int_compare(
                    integer_predicate(comparison, signed)?,
                    left,
                    right,
                    "compare",
                )?
                .into(),
        })
    }

    fn lower_string_equality(
        &self,
        operator: BinaryOperator,
        left: BasicValueEnum<'ctx>,
        right: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let equal = self
            .builder
            .build_call(
                self.generator.runtime_string_equals(),
                &[
                    left.into_pointer_value().into(),
                    right.into_pointer_value().into(),
                ],
                "string.equals",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("string equality returned void".into()))?
            .into_int_value();
        let equal = self.builder.build_int_compare(
            IntPredicate::NE,
            equal,
            equal.get_type().const_zero(),
            "string.equal",
        )?;
        Ok(if operator == BinaryOperator::Equal {
            equal.into()
        } else {
            self.builder.build_not(equal, "string.not_equal")?.into()
        })
    }

    fn lower_optional_equality(
        &self,
        operator: BinaryOperator,
        left: BasicValueEnum<'ctx>,
        right: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let left = left.into_struct_value();
        let right = right.into_struct_value();
        let left_tag = self
            .builder
            .build_extract_value(left, 0, "optional.left.present")?
            .into_int_value();
        let right_tag = self
            .builder
            .build_extract_value(right, 0, "optional.right.present")?
            .into_int_value();
        let equal = self.builder.build_int_compare(
            IntPredicate::EQ,
            left_tag,
            right_tag,
            "optional.equal",
        )?;
        Ok(if operator == BinaryOperator::Equal {
            equal.into()
        } else {
            self.builder.build_not(equal, "optional.not_equal")?.into()
        })
    }

    fn lower_float_binary(
        &self,
        operator: BinaryOperator,
        left: BasicValueEnum<'ctx>,
        right: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        use inkwell::FloatPredicate;
        let left = left.into_float_value();
        let right = right.into_float_value();
        Ok(match operator {
            BinaryOperator::Add => self.builder.build_float_add(left, right, "fadd")?.into(),
            BinaryOperator::Subtract => self.builder.build_float_sub(left, right, "fsub")?.into(),
            BinaryOperator::Multiply => self.builder.build_float_mul(left, right, "fmul")?.into(),
            BinaryOperator::Divide => self.builder.build_float_div(left, right, "fdiv")?.into(),
            BinaryOperator::Remainder => self.builder.build_float_rem(left, right, "frem")?.into(),
            BinaryOperator::Equal => self
                .builder
                .build_float_compare(FloatPredicate::OEQ, left, right, "feq")?
                .into(),
            BinaryOperator::NotEqual => self
                .builder
                .build_float_compare(FloatPredicate::UNE, left, right, "fne")?
                .into(),
            BinaryOperator::Less => self
                .builder
                .build_float_compare(FloatPredicate::OLT, left, right, "flt")?
                .into(),
            BinaryOperator::LessEqual => self
                .builder
                .build_float_compare(FloatPredicate::OLE, left, right, "fle")?
                .into(),
            BinaryOperator::Greater => self
                .builder
                .build_float_compare(FloatPredicate::OGT, left, right, "fgt")?
                .into(),
            BinaryOperator::GreaterEqual => self
                .builder
                .build_float_compare(FloatPredicate::OGE, left, right, "fge")?
                .into(),
            _ => {
                return Err(CodegenError::Unsupported(
                    "bitwise or logical float operation".into(),
                ));
            }
        })
    }

    fn checked_integer_arithmetic(
        &self,
        operator: BinaryOperator,
        left: IntValue<'ctx>,
        right: IntValue<'ctx>,
        signed: bool,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        let stem = match operator {
            BinaryOperator::Add => "add",
            BinaryOperator::Subtract => "sub",
            BinaryOperator::Multiply => "mul",
            _ => {
                return Err(CodegenError::Unsupported(
                    "non-arithmetic overflow check".into(),
                ));
            }
        };
        let intrinsic = Intrinsic::find(&format!(
            "llvm.{}{}.with.overflow",
            if signed { "s" } else { "u" },
            stem
        ))
        .ok_or_else(|| CodegenError::Unsupported("LLVM overflow intrinsic unavailable".into()))?;
        let declaration = intrinsic
            .get_declaration(&self.generator.module, &[left.get_type().into()])
            .ok_or_else(|| {
                CodegenError::Unsupported("LLVM overflow overload unavailable".into())
            })?;
        let result = self
            .builder
            .build_call(declaration, &[left.into(), right.into()], "checked")?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("overflow intrinsic returned void".into()))?
            .into_struct_value();
        let value = self
            .builder
            .build_extract_value(result, 0, "value")?
            .into_int_value();
        let overflow = self
            .builder
            .build_extract_value(result, 1, "overflow")?
            .into_int_value();
        self.guard(
            self.builder.build_not(overflow, "no_overflow")?,
            "integer overflow",
        )?;
        Ok(value)
    }

    fn checked_left_shift(
        &self,
        left: IntValue<'ctx>,
        right: IntValue<'ctx>,
        signed: bool,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        self.guard_shift_count(right)?;
        let shifted = self.builder.build_left_shift(left, right, "shl")?;
        let restored = self.builder.build_right_shift(
            shifted,
            right,
            signed,
            if signed {
                "shift_restore_signed"
            } else {
                "shift_restore_unsigned"
            },
        )?;
        let preserved =
            self.builder
                .build_int_compare(IntPredicate::EQ, restored, left, "shift_preserved")?;
        self.guard(preserved, "integer shift overflow")?;
        Ok(shifted)
    }

    fn guard_shift_count(&self, count: IntValue<'ctx>) -> Result<(), CodegenError> {
        let width = count.get_type().get_bit_width();
        let valid = self.builder.build_int_compare(
            IntPredicate::ULT,
            count,
            count.get_type().const_int(u64::from(width), false),
            "shift_count_valid",
        )?;
        self.guard(valid, "invalid shift count")
    }

    fn guard(&self, valid: IntValue<'ctx>, message: &str) -> Result<(), CodegenError> {
        let ok = self
            .generator
            .context
            .append_basic_block(self.function, "check.ok");
        let panic = self
            .generator
            .context
            .append_basic_block(self.function, "check.panic");
        self.builder.build_conditional_branch(valid, ok, panic)?;
        self.builder.position_at_end(panic);
        let abort = self.runtime_abort();
        let code = stable_panic_code(message);
        self.builder.build_call(
            abort,
            &[self
                .generator
                .context
                .i32_type()
                .const_int(u64::from(code), false)
                .into()],
            "abort",
        )?;
        self.builder.build_unreachable()?;
        self.builder.position_at_end(ok);
        Ok(())
    }

    fn runtime_abort(&self) -> FunctionValue<'ctx> {
        self.generator
            .module
            .get_function("tn_runtime_abort")
            .unwrap_or_else(|| {
                self.generator.module.add_function(
                    "tn_runtime_abort",
                    self.generator
                        .context
                        .void_type()
                        .fn_type(&[self.generator.context.i32_type().into()], false),
                    None,
                )
            })
    }

    fn lower_cast(
        &self,
        value: BasicValueEnum<'ctx>,
        target: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if value.get_type() == target {
            return Ok(value);
        }
        match (value, target) {
            (BasicValueEnum::IntValue(value), BasicTypeEnum::IntType(target)) => Ok(self
                .builder
                .build_int_cast(value, target, "intcast")?
                .into()),
            (BasicValueEnum::FloatValue(value), BasicTypeEnum::FloatType(target)) => Ok(self
                .builder
                .build_float_cast(value, target, "floatcast")?
                .into()),
            (BasicValueEnum::PointerValue(value), BasicTypeEnum::PointerType(target)) => Ok(self
                .builder
                .build_pointer_cast(value, target, "ptrcast")?
                .into()),
            _ => Err(CodegenError::Unsupported(format!(
                "invalid residual cast: {} -> {}",
                value.get_type().print_to_string(),
                target.print_to_string()
            ))),
        }
    }

    fn lower_error_downcast(
        &self,
        operand: &Operand,
        target: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let envelope = self.lower_operand(operand)?.into_pointer_value();
        let envelope_type = self.error_union_type();
        let payload_address =
            self.builder
                .build_struct_gep(envelope_type, envelope, 1, "error.payload.address")?;
        let payload = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                payload_address,
                "error.payload",
            )?
            .into_pointer_value();
        if self.is_pointer_representation(target) {
            return Ok(payload.into());
        }
        Ok(self
            .builder
            .build_load(self.generator.basic_type(target)?, payload, "error.value")?)
    }

    fn is_pointer_representation(&self, ty: &Type) -> bool {
        matches!(
            ty,
            Type::Reference { .. }
                | Type::RawPointer { .. }
                | Type::String
                | Type::Str
                | Type::Promise { .. }
                | Type::Function(_)
                | Type::Template(_)
                | Type::ErrorUnion(_)
        ) || matches!(
            ty,
            Type::Nominal(_, _) if self.generator.is_class_type(ty)
        )
    }

    fn error_union_type(&self) -> inkwell::types::StructType<'ctx> {
        self.generator.context.struct_type(
            &[
                self.generator.context.i64_type().into(),
                self.generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .into(),
            ],
            false,
        )
    }

    fn lower_terminator(&self, terminator: &TerminatorKind) -> Result<(), CodegenError> {
        match terminator {
            TerminatorKind::Goto(target) => {
                self.builder
                    .build_unconditional_branch(self.block(*target))?;
            }
            TerminatorKind::Switch {
                value,
                targets,
                otherwise,
            } => {
                let value = self.lower_switch_value(value)?;
                let cases = targets
                    .iter()
                    .map(|(case, target)| {
                        (
                            value
                                .get_type()
                                .const_int_arbitrary_precision(&u128_words(*case)),
                            self.block(*target),
                        )
                    })
                    .collect::<Vec<_>>();
                self.builder
                    .build_switch(value, self.block(*otherwise), &cases)?;
            }
            TerminatorKind::Return(payload) => self.return_success(payload.as_ref())?,
            TerminatorKind::Throw(payload) => self.return_error(payload)?,
            TerminatorKind::TaggedReturn {
                completion,
                payload,
            } => match completion {
                Completion::Success => self.return_success(payload.as_ref())?,
                Completion::Error => self.return_error(payload.as_ref().ok_or_else(|| {
                    CodegenError::Unsupported("error completion lacks payload".into())
                })?)?,
            },
            TerminatorKind::Call {
                function,
                receiver,
                arguments,
                destination,
                error_destination,
                success,
                error,
            } => self.lower_call(
                function,
                receiver.as_ref(),
                arguments,
                destination.as_ref(),
                error_destination.as_ref(),
                *success,
                *error,
            )?,
            TerminatorKind::Drop { place, success } => self.lower_drop(place, *success)?,
            TerminatorKind::Abort(message) => {
                let abort = self.runtime_abort();
                self.builder.build_call(
                    abort,
                    &[self
                        .generator
                        .context
                        .i32_type()
                        .const_int(u64::from(stable_panic_code(message)), false)
                        .into()],
                    "abort",
                )?;
                self.builder.build_unreachable()?;
            }
            TerminatorKind::Unreachable => {
                self.builder.build_unreachable()?;
            }
            TerminatorKind::Suspend {
                value,
                destination,
                error_destination,
                resume,
                error,
                cancel,
            } => self.lower_suspend(
                value,
                destination.as_ref(),
                error_destination.as_ref(),
                *resume,
                *error,
                *cancel,
            )?,
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn lower_suspend(
        &self,
        value: &Operand,
        destination: Option<&Place>,
        error_destination: Option<&Place>,
        resume: BasicBlockId,
        error: Option<BasicBlockId>,
        cancel: BasicBlockId,
    ) -> Result<(), CodegenError> {
        let promise = self.lower_operand(value)?.into_pointer_value();
        let Type::Promise { result, effects } = self.operand_type(value)? else {
            return Err(CodegenError::Unsupported(
                "suspend operand is not a promise".into(),
            ));
        };
        let wait_status = self.builder.build_call(
            self.generator.runtime_async_wait(),
            &[promise.into()],
            "promise.wait",
        )?;
        let wait_status = wait_status
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("promise wait returned void".into()))?
            .into_int_value();
        let wait_failed = self.builder.build_int_compare(
            IntPredicate::NE,
            wait_status,
            wait_status.get_type().const_zero(),
            "promise.wait.failed",
        )?;
        let wait_error = self
            .generator
            .context
            .append_basic_block(self.function, "promise.wait.error");
        let wait_ready = self
            .generator
            .context
            .append_basic_block(self.function, "promise.wait.ready");
        self.builder
            .build_conditional_branch(wait_failed, wait_error, wait_ready)?;
        self.builder.position_at_end(wait_error);
        self.builder.build_call(
            self.generator.runtime_async_destroy(),
            &[promise.into()],
            "promise.cancel",
        )?;
        self.builder
            .build_unconditional_branch(self.block(cancel))?;
        self.builder.position_at_end(wait_ready);
        let result_pointer = self
            .builder
            .build_call(
                self.generator.runtime_async_raw_result(),
                &[promise.into()],
                "promise.result",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("promise result returned void".into()))?
            .into_pointer_value();
        if effects.is_empty() {
            if let Some(destination) = destination
                && *result != Type::Primitive(PrimitiveType::Void)
            {
                let value = self.builder.build_load(
                    self.generator.basic_type(&result)?,
                    result_pointer,
                    "promise.result",
                )?;
                self.builder
                    .build_store(self.place_pointer(destination)?, value)?;
            }
            self.builder.build_call(
                self.generator.runtime_async_destroy(),
                &[promise.into()],
                "promise.free",
            )?;
            self.builder
                .build_unconditional_branch(self.block(resume))?;
            return Ok(());
        }
        let completion = self.generator.completion_type(&result)?;
        let loaded = self
            .builder
            .build_load(completion, result_pointer, "promise.completion")?
            .into_struct_value();
        let failed = self
            .builder
            .build_extract_value(loaded, 0, "promise.failed")?
            .into_int_value();
        let failed = self.builder.build_int_compare(
            IntPredicate::NE,
            failed,
            failed.get_type().const_zero(),
            "promise.failed.test",
        )?;
        self.builder.build_call(
            self.generator.runtime_async_destroy(),
            &[promise.into()],
            "promise.free",
        )?;
        let failed_block = self
            .generator
            .context
            .append_basic_block(self.function, "promise.error");
        let success_block = self
            .generator
            .context
            .append_basic_block(self.function, "promise.resume");
        self.builder
            .build_conditional_branch(failed, failed_block, success_block)?;
        self.builder.position_at_end(success_block);
        if let Some(destination) = destination {
            let success_value = self
                .builder
                .build_extract_value(loaded, 1, "promise.value")?;
            self.builder
                .build_store(self.place_pointer(destination)?, success_value)?;
        }
        self.builder
            .build_unconditional_branch(self.block(resume))?;
        self.builder.position_at_end(failed_block);
        if let Some(error_destination) = error_destination {
            let index = usize::from(*result != Type::Primitive(PrimitiveType::Void)) + 1;
            let error_value = self.builder.build_extract_value(
                loaded,
                u32::try_from(index).unwrap_or(u32::MAX),
                "promise.error.value",
            )?;
            self.builder
                .build_store(self.place_pointer(error_destination)?, error_value)?;
        }
        if let Some(error) = error {
            self.builder.build_unconditional_branch(self.block(error))?;
        } else {
            self.builder
                .build_unconditional_branch(self.block(cancel))?;
        }
        Ok(())
    }

    fn lower_drop(&self, place: &Place, success: BasicBlockId) -> Result<(), CodegenError> {
        if self.is_borrowed_class_receiver(place) {
            self.builder
                .build_unconditional_branch(self.block(success))?;
            return Ok(());
        }
        let local = usize::try_from(place.local.0)
            .map_err(|_| CodegenError::Unsupported("drop local index overflow".into()))?;
        let flag = *self
            .drop_flags
            .get(local)
            .ok_or_else(|| CodegenError::Unsupported("drop flag local is missing".into()))?;
        let initialized = self
            .builder
            .build_load(self.generator.context.bool_type(), flag, "drop.initialized")?
            .into_int_value();
        let drop_block = self
            .generator
            .context
            .append_basic_block(self.function, "drop.value");
        let skip_block = self
            .generator
            .context
            .append_basic_block(self.function, "drop.skip");
        self.builder
            .build_conditional_branch(initialized, drop_block, skip_block)?;
        self.builder.position_at_end(drop_block);
        self.lower_drop_value(place)?;
        self.builder
            .build_store(flag, self.generator.context.bool_type().const_zero())?;
        self.builder
            .build_unconditional_branch(self.block(success))?;
        self.builder.position_at_end(skip_block);
        self.builder
            .build_unconditional_branch(self.block(success))?;
        Ok(())
    }

    fn lower_drop_value(&self, place: &Place) -> Result<(), CodegenError> {
        let ty = self.place_type(place)?;
        let pointer = self.place_pointer(place)?;
        self.lower_drop_value_at_pointer(pointer, &ty)
    }

    #[allow(clippy::too_many_lines)]
    fn lower_drop_value_at_pointer(
        &self,
        pointer: PointerValue<'ctx>,
        ty: &Type,
    ) -> Result<(), CodegenError> {
        match ty {
            Type::String => {
                let value = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        pointer,
                        "drop.string.value",
                    )?
                    .into_pointer_value();
                self.builder.build_call(
                    self.generator.runtime_free(),
                    &[value.into()],
                    "drop.string",
                )?;
            }
            Type::Promise { .. } => {
                let promise = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        pointer,
                        "drop.promise.value",
                    )?
                    .into_pointer_value();
                self.builder.build_call(
                    self.generator.runtime_async_destroy(),
                    &[promise.into()],
                    "drop.promise",
                )?;
            }
            Type::Optional(inner) => {
                let structure = self.generator.basic_type(ty)?.into_struct_type();
                let tag_address = self.builder.build_struct_gep(
                    structure,
                    pointer,
                    0,
                    "drop.optional.tag.address",
                )?;
                let tag = self
                    .builder
                    .build_load(
                        self.generator.context.bool_type(),
                        tag_address,
                        "drop.optional.tag",
                    )?
                    .into_int_value();
                let payload_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.optional.payload");
                let merge_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.optional.merge");
                self.builder
                    .build_conditional_branch(tag, payload_block, merge_block)?;
                self.builder.position_at_end(payload_block);
                let payload = self.builder.build_struct_gep(
                    structure,
                    pointer,
                    1,
                    "drop.optional.payload.address",
                )?;
                self.lower_drop_value_at_pointer(payload, inner)?;
                self.builder.build_unconditional_branch(merge_block)?;
                self.builder.position_at_end(merge_block);
            }
            Type::Tuple(elements) => {
                let structure = self.generator.basic_type(ty)?.into_struct_type();
                for (index, element) in elements.iter().enumerate() {
                    let field = self.builder.build_struct_gep(
                        structure,
                        pointer,
                        u32::try_from(index)
                            .map_err(|_| CodegenError::Unsupported("tuple field limit".into()))?,
                        "drop.tuple.field",
                    )?;
                    self.lower_drop_value_at_pointer(field, element)?;
                }
            }
            Type::Array(element, length) => {
                let array = self.generator.basic_type(ty)?.into_array_type();
                let zero = self.generator.context.i32_type().const_zero();
                for index in 0..*length {
                    let index = self.generator.context.i32_type().const_int(index, false);
                    // SAFETY: `pointer` points to the fixed-size array represented by `array`,
                    // and every constant index is within the statically known length.
                    let field = unsafe {
                        self.builder.build_gep(
                            array,
                            pointer,
                            &[zero, index],
                            "drop.array.element",
                        )?
                    };
                    self.lower_drop_value_at_pointer(field, element)?;
                }
            }
            Type::Nominal(declaration, arguments) => {
                let layout = self
                    .generator
                    .layouts
                    .nominals
                    .get(declaration)
                    .cloned()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "nominal drop layout is not registered: {ty:?}"
                        ))
                    })?;
                if layout.type_parameters.len() != arguments.len() {
                    return Err(CodegenError::Unsupported(format!(
                        "nominal drop layout {:?} expects {} type arguments, found {}",
                        declaration,
                        layout.type_parameters.len(),
                        arguments.len()
                    )));
                }
                let substitutions = layout
                    .type_parameters
                    .iter()
                    .cloned()
                    .zip(arguments.iter().cloned())
                    .collect::<BTreeMap<_, _>>();
                self.lower_explicit_drop(ty, pointer)?;
                match layout.kind {
                    NominalKind::Struct { fields } => {
                        let structure = self.generator.basic_type(ty)?.into_struct_type();
                        for (index, field) in fields.iter().enumerate() {
                            let field = instantiate_type(field, &substitutions);
                            let field_pointer = self.builder.build_struct_gep(
                                structure,
                                pointer,
                                u32::try_from(index).map_err(|_| {
                                    CodegenError::Unsupported("struct field limit".into())
                                })?,
                                "drop.struct.field",
                            )?;
                            self.lower_drop_value_at_pointer(field_pointer, &field)?;
                        }
                    }
                    NominalKind::Enum {
                        variants, c_repr, ..
                    } => {
                        if c_repr {
                            return Ok(());
                        }
                        let structure = self.generator.basic_type(ty)?.into_struct_type();
                        let tag_address = self.builder.build_struct_gep(
                            structure,
                            pointer,
                            0,
                            "drop.enum.tag.address",
                        )?;
                        let tag = self
                            .builder
                            .build_load(
                                self.generator.context.i64_type(),
                                tag_address,
                                "drop.enum.tag",
                            )?
                            .into_int_value();
                        let merge_block = self
                            .generator
                            .context
                            .append_basic_block(self.function, "drop.enum.merge");
                        let mut cases = Vec::with_capacity(variants.len());
                        let mut variant_blocks = Vec::with_capacity(variants.len());
                        for (variant, _) in variants.iter().enumerate() {
                            let block = self
                                .generator
                                .context
                                .append_basic_block(self.function, "drop.enum.variant");
                            cases.push((
                                self.generator
                                    .context
                                    .i64_type()
                                    .const_int(u64::try_from(variant).unwrap_or(u64::MAX), false),
                                block,
                            ));
                            variant_blocks.push(block);
                        }
                        self.builder.build_switch(tag, merge_block, &cases)?;
                        for (variant, (fields, block)) in
                            variants.iter().zip(variant_blocks).enumerate()
                        {
                            self.builder.position_at_end(block);
                            let offset = 1_usize
                                + variants.iter().take(variant).map(Vec::len).sum::<usize>();
                            for (field_index, field) in fields.iter().enumerate() {
                                let field = instantiate_type(field, &substitutions);
                                let field_pointer = self.builder.build_struct_gep(
                                    structure,
                                    pointer,
                                    u32::try_from(offset + field_index).map_err(|_| {
                                        CodegenError::Unsupported("enum field limit".into())
                                    })?,
                                    "drop.enum.field",
                                )?;
                                self.lower_drop_value_at_pointer(field_pointer, &field)?;
                            }
                            self.builder.build_unconditional_branch(merge_block)?;
                        }
                        self.builder.position_at_end(merge_block);
                    }
                    NominalKind::Class { .. } => {
                        let object = self
                            .builder
                            .build_load(
                                self.generator.context.ptr_type(AddressSpace::default()),
                                pointer,
                                "drop.class.object",
                            )?
                            .into_pointer_value();
                        let object_layout = self.generator.class_object_type(ty)?;
                        let fields = self
                            .generator
                            .class_field_types(*declaration, &substitutions)?;
                        for (index, field) in fields.iter().enumerate() {
                            let field_pointer = self.builder.build_struct_gep(
                                object_layout,
                                object,
                                u32::try_from(index + 1).map_err(|_| {
                                    CodegenError::Unsupported("class field limit".into())
                                })?,
                                "drop.class.field",
                            )?;
                            self.lower_drop_value_at_pointer(field_pointer, field)?;
                        }
                        self.builder.build_call(
                            self.generator.runtime_free(),
                            &[object.into()],
                            "free.class",
                        )?;
                    }
                }
            }
            Type::Template(_)
            | Type::DynamicInterface(_, _)
            | Type::ErrorUnion(_)
            | Type::Primitive(_)
            | Type::Str
            | Type::Slice(_)
            | Type::Reference { .. }
            | Type::RawPointer { .. }
            | Type::Function(_)
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => {}
        }
        Ok(())
    }

    fn lower_explicit_drop(
        &self,
        ty: &Type,
        pointer: PointerValue<'ctx>,
    ) -> Result<(), CodegenError> {
        let Type::Nominal(declaration, _) = ty else {
            return Ok(());
        };
        let Some(callable) = self.generator.layouts.drops.get(declaration).copied() else {
            return Ok(());
        };
        let signature = self
            .generator
            .signatures
            .iter()
            .find(|(instance, signature)| {
                instance.callable == callable
                    && signature.parameters.len() == 1
                    && signature.parameters[0] == *ty
            })
            .map(|(_, signature)| signature.clone())
            .ok_or_else(|| CodegenError::Unsupported("drop method signature is missing".into()))?;
        let function = self
            .resolve_emitted_callable(callable, &signature)
            .map_err(|error| CodegenError::Unsupported(error.to_string()))?;
        let receiver = if self.generator.is_class_type(ty) {
            self.builder
                .build_load(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    pointer,
                    "drop.class.receiver",
                )?
                .into()
        } else {
            pointer.into()
        };
        self.builder
            .build_call(function, &[receiver], "drop.explicit")?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn lower_call(
        &self,
        function: &Operand,
        receiver: Option<&Operand>,
        arguments: &[Operand],
        destination: Option<&Place>,
        error_destination: Option<&Place>,
        success: BasicBlockId,
        error: Option<BasicBlockId>,
    ) -> Result<(), CodegenError> {
        let arguments = receiver
            .map(|receiver| self.lower_receiver_operand(receiver))
            .transpose()?
            .into_iter()
            .chain(
                arguments
                    .iter()
                    .map(|argument| self.lower_operand(argument))
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .map(BasicMetadataValueEnum::from)
            .collect::<Vec<_>>();
        let (call, function_type) = match function {
            Operand::Constant(_) => {
                let (callee, function_type) = self.resolve_function(function)?;
                (
                    self.builder.build_call(callee, &arguments, "call")?,
                    function_type,
                )
            }
            Operand::Copy(_) | Operand::Move(_) => {
                let function_place = operand_place(function).ok_or_else(|| {
                    CodegenError::Unsupported("indirect call target lacks a place".into())
                })?;
                let Type::Function(function_type) = self.place_type(function_place)? else {
                    return Err(CodegenError::Unsupported(
                        "indirect call target is not a function value".into(),
                    ));
                };
                let mut parameters = Vec::with_capacity(
                    function_type.parameters.len() + usize::from(receiver.is_some()),
                );
                if let Some(receiver) = receiver {
                    parameters.push(match self.operand_type(receiver)? {
                        Type::DynamicInterface(_, _) => Type::RawPointer {
                            mutable: true,
                            pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
                        },
                        Type::Nominal(declaration, arguments)
                            if !self
                                .generator
                                .is_class_type(&Type::Nominal(declaration, arguments.clone())) =>
                        {
                            Generator::receiver_pointer_type()
                        }
                        ty => ty,
                    });
                }
                parameters.extend(function_type.parameters.iter().cloned());
                let llvm_type = self.generator.llvm_function_type(
                    &parameters,
                    &function_type.result,
                    &function_type.effects,
                )?;
                let pointer = self.lower_operand(function)?.into_pointer_value();
                (
                    self.builder
                        .build_indirect_call(llvm_type, pointer, &arguments, "call")?,
                    function_type.clone(),
                )
            }
        };
        if function_type.is_async {
            if let Some(destination) = destination {
                let value = call.try_as_basic_value().basic().ok_or_else(|| {
                    CodegenError::Builder("async call did not return a promise".into())
                })?;
                self.builder
                    .build_store(self.place_pointer(destination)?, value)?;
            }
            self.builder
                .build_unconditional_branch(self.block(success))?;
            return Ok(());
        }
        if function_type.effects.is_empty() {
            if let Some(destination) = destination {
                let value = call.try_as_basic_value().basic().ok_or_else(|| {
                    CodegenError::Builder("non-void call did not return a value".into())
                })?;
                self.builder
                    .build_store(self.place_pointer(destination)?, value)?;
            }
            self.builder
                .build_unconditional_branch(self.block(success))?;
            return Ok(());
        }
        let result = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("fallible call returned void".into()))?
            .into_struct_value();
        let failed = self
            .builder
            .build_extract_value(result, 0, "failed")?
            .into_int_value();
        let failed = self.builder.build_int_compare(
            IntPredicate::NE,
            failed,
            failed.get_type().const_zero(),
            "failed.test",
        )?;
        let mut field = 1;
        if let Some(destination) = destination {
            let value = self
                .builder
                .build_extract_value(result, field, "success.value")?;
            self.builder
                .build_store(self.place_pointer(destination)?, value)?;
            field += 1;
        }
        if let Some(error_destination) = error_destination {
            let value = self
                .builder
                .build_extract_value(result, field, "error.value")?;
            self.builder
                .build_store(self.place_pointer(error_destination)?, value)?;
        }
        self.builder.build_conditional_branch(
            failed,
            self.block(error.ok_or_else(|| {
                CodegenError::Unsupported("fallible call lacks error successor".into())
            })?),
            self.block(success),
        )?;
        Ok(())
    }

    fn lower_receiver_operand(
        &self,
        receiver: &Operand,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let receiver_type = self.operand_type(receiver)?;
        if matches!(receiver_type, Type::DynamicInterface(_, _)) {
            let place = operand_place(receiver).ok_or_else(|| {
                CodegenError::Unsupported("dynamic interface receiver must be addressable".into())
            })?;
            let pair = self
                .generator
                .basic_type(&self.place_type(place)?)?
                .into_struct_type();
            let data = self.builder.build_struct_gep(
                pair,
                self.place_pointer(place)?,
                0,
                "interface.data.address",
            )?;
            return Ok(self.builder.build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                data,
                "interface.data",
            )?);
        }
        if let Type::Nominal(declaration, arguments) = receiver_type
            && !self
                .generator
                .is_class_type(&Type::Nominal(declaration, arguments))
        {
            let place = operand_place(receiver).ok_or_else(|| {
                CodegenError::Unsupported("value receiver must be addressable".into())
            })?;
            return Ok(self.place_pointer(place)?.into());
        }
        self.lower_operand(receiver)
    }

    fn resolve_function(
        &self,
        operand: &Operand,
    ) -> Result<(FunctionValue<'ctx>, FunctionType), CodegenError> {
        let Operand::Constant(constant) = operand else {
            return Err(CodegenError::Unsupported(
                "indirect calls require lowered callable metadata".into(),
            ));
        };
        match constant {
            Constant::Function(declaration, ty) => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "call target lacks function type".into(),
                    ));
                };
                let callable = Callable::function(*declaration);
                let function = self.resolve_emitted_callable(callable, function_type)?;
                Ok((function, function_type.clone()))
            }
            Constant::Method {
                owner, member, ty, ..
            } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "call target lacks function type".into(),
                    ));
                };
                let function = self.resolve_emitted_callable(
                    Callable {
                        declaration: *owner,
                        member: Some(*member),
                    },
                    function_type,
                )?;
                Ok((function, function_type.clone()))
            }
            Constant::Constructor { owner, member, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "constructor target lacks function type".into(),
                    ));
                };
                let target = self
                    .generator
                    .constructors
                    .iter()
                    .find(|target| {
                        target.owner == *owner
                            && target.member == *member
                            && target.signature == *function_type
                    })
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "constructor {:?} was not emitted",
                            (*owner, *member)
                        ))
                    })?;
                Ok((target.function, function_type.clone()))
            }
            _ => Err(CodegenError::Unsupported(
                "call target is not a direct callable constant".into(),
            )),
        }
    }

    fn resolve_emitted_callable(
        &self,
        callable: Callable,
        function_type: &FunctionType,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let mut matches = self
            .generator
            .signatures
            .iter()
            .filter(|(instance, signature)| {
                instance.callable == callable && signature_matches(signature, function_type)
            })
            .map(|(instance, _)| self.generator.functions[instance]);
        let function = matches.next().ok_or_else(|| {
            CodegenError::Unsupported(format!("call target {callable:?} was not emitted"))
        })?;
        if matches.next().is_some() {
            return Err(CodegenError::Unsupported(format!(
                "call target {callable:?} is ambiguous after specialization"
            )));
        }
        Ok(function)
    }

    fn return_success(&self, payload: Option<&Operand>) -> Result<(), CodegenError> {
        if self.body.effects.is_empty() {
            if let Some(payload) = payload {
                let value = self.lower_operand(payload)?;
                self.builder.build_return(Some(&value))?;
            } else {
                self.builder.build_return(None)?;
            }
            return Ok(());
        }
        let completion = self.generator.completion_type(&self.body.return_type)?;
        let mut value = completion.const_zero();
        let mut field = 1;
        if let Some(payload) = payload {
            value = self
                .builder
                .build_insert_value(
                    value,
                    self.lower_operand(payload)?,
                    field,
                    "success.payload",
                )?
                .into_struct_value();
            field += 1;
        }
        value = self
            .builder
            .build_insert_value(
                value,
                self.generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null(),
                field,
                "no.error",
            )?
            .into_struct_value();
        self.builder.build_return(Some(&value))?;
        Ok(())
    }

    fn return_error(&self, payload: &Operand) -> Result<(), CodegenError> {
        if self.body.effects.is_empty() {
            return Err(CodegenError::Unsupported(
                "error completion in an infallible function".into(),
            ));
        }
        let completion = self.generator.completion_type(&self.body.return_type)?;
        let mut value = completion.const_zero();
        value = self
            .builder
            .build_insert_value(
                value,
                self.generator.context.i8_type().const_int(1, false),
                0,
                "error.tag",
            )?
            .into_struct_value();
        let field = if self.body.return_type == Type::Primitive(PrimitiveType::Void) {
            1
        } else {
            2
        };
        let error_pointer = self.lower_error_payload(payload)?;
        value = self
            .builder
            .build_insert_value(value, error_pointer, field, "error.payload")?
            .into_struct_value();
        self.builder.build_return(Some(&value))?;
        Ok(())
    }

    fn lower_error_payload(&self, payload: &Operand) -> Result<PointerValue<'ctx>, CodegenError> {
        if matches!(self.operand_type(payload)?, Type::ErrorUnion(_)) {
            return Ok(self.lower_operand(payload)?.into_pointer_value());
        }
        let value = self.lower_operand(payload)?;
        let payload_pointer = if value.is_pointer_value() {
            value.into_pointer_value()
        } else {
            let ty = self.operand_type(payload)?;
            let llvm_type = self.generator.basic_type(&ty)?;
            let size = llvm_type.size_of().ok_or_else(|| {
                CodegenError::Unsupported("error payload has no statically known size".into())
            })?;
            let pointer = self
                .builder
                .build_call(self.generator.runtime_alloc(), &[size.into()], "error.box")?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                .into_pointer_value();
            self.builder.build_store(pointer, value)?;
            pointer
        };
        let envelope_type = self.error_union_type();
        let envelope_size = envelope_type
            .size_of()
            .ok_or_else(|| CodegenError::Unsupported("error union has no known size".into()))?;
        let envelope = self
            .builder
            .build_call(
                self.generator.runtime_alloc(),
                &[envelope_size.into()],
                "error.envelope",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
            .into_pointer_value();
        let tag = self
            .operand_type(payload)
            .ok()
            .and_then(|ty| match ty {
                Type::Nominal(declaration, _) => self
                    .body
                    .effects
                    .iter()
                    .position(|effect| *effect == declaration),
                _ => None,
            })
            .unwrap_or(0);
        let tag_address =
            self.builder
                .build_struct_gep(envelope_type, envelope, 0, "error.tag.address")?;
        self.builder.build_store(
            tag_address,
            self.generator
                .context
                .i64_type()
                .const_int(u64::try_from(tag).unwrap_or(u64::MAX), false),
        )?;
        let payload_address =
            self.builder
                .build_struct_gep(envelope_type, envelope, 1, "error.payload.address")?;
        self.builder.build_store(payload_address, payload_pointer)?;
        Ok(envelope)
    }

    fn lower_operand(&self, operand: &Operand) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => {
                let ty = self.generator.basic_type(&self.place_type(place)?)?;
                Ok(self
                    .builder
                    .build_load(ty, self.place_pointer(place)?, "load")?)
            }
            Operand::Constant(constant) => self.lower_constant(constant),
        }
    }

    fn operand_type(&self, operand: &Operand) -> Result<Type, CodegenError> {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => self.place_type(place),
            Operand::Constant(constant) => Ok(constant.ty()),
        }
    }

    fn lower_vtable_lookup(
        &self,
        object: &Place,
        slot: u32,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let object_type = self.place_type(object)?;
        let Type::Nominal(declaration, _) = object_type.clone() else {
            return Err(CodegenError::Unsupported(format!(
                "virtual lookup requires a class object, found {object_type:?}"
            )));
        };
        let NominalKind::Class { vtable, .. } = self
            .generator
            .layouts
            .nominals
            .get(&declaration)
            .map(|layout| &layout.kind)
            .ok_or_else(|| {
                CodegenError::Unsupported(format!(
                    "virtual lookup class layout is not registered: {object_type:?}"
                ))
            })?
        else {
            return Err(CodegenError::Unsupported(format!(
                "virtual lookup requires a class object, found {object_type:?}"
            )));
        };
        if usize::try_from(slot).map_or(true, |slot| slot > vtable.len()) {
            return Err(CodegenError::Unsupported(
                "virtual slot is out of range".into(),
            ));
        }
        let object_pointer = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                self.place_pointer(object)?,
                "virtual.object",
            )?
            .into_pointer_value();
        let object_layout = self.generator.class_object_type(&object_type)?;
        let header = self.builder.build_struct_gep(
            object_layout,
            object_pointer,
            0,
            "virtual.descriptor.address",
        )?;
        let descriptor = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                header,
                "virtual.descriptor",
            )?
            .into_pointer_value();
        let pointer = self.generator.context.ptr_type(AddressSpace::default());
        let descriptor_type = pointer
            .array_type(u32::try_from(vtable.len().saturating_add(1)).map_err(|_| {
                CodegenError::Unsupported("class vtable exceeds LLVM limit".into())
            })?);
        // SAFETY: the constructor stores a pointer to the descriptor array in the object header;
        // `slot` is checked against the statically registered vtable length above.
        let entry = unsafe {
            self.builder.build_gep(
                descriptor_type,
                descriptor,
                &[
                    self.generator.context.i32_type().const_zero(),
                    self.generator
                        .context
                        .i32_type()
                        .const_int(u64::from(slot), false),
                ],
                "virtual.entry.address",
            )?
        };
        Ok(self
            .builder
            .build_load(pointer, entry, "virtual.entry")?
            .into_pointer_value())
    }

    fn lower_witness_lookup(
        &self,
        object: &Place,
        slot: u32,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let object_type = self.place_type(object)?;
        let Type::DynamicInterface(interface, _) = object_type.clone() else {
            return Err(CodegenError::Unsupported(format!(
                "witness lookup requires a dynamic interface, found {object_type:?}"
            )));
        };
        let slot_count = *self
            .generator
            .layouts
            .interfaces
            .get(&interface)
            .ok_or_else(|| {
                CodegenError::Unsupported("interface layout is not registered".into())
            })?;
        if slot >= slot_count {
            return Err(CodegenError::Unsupported(
                "witness slot is out of range".into(),
            ));
        }
        let pair = self.generator.basic_type(&object_type)?.into_struct_type();
        let witness_address = self.builder.build_struct_gep(
            pair,
            self.place_pointer(object)?,
            1,
            "witness.table.address",
        )?;
        let witness = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                witness_address,
                "witness.table",
            )?
            .into_pointer_value();
        let pointer = self.generator.context.ptr_type(AddressSpace::default());
        let table_type = pointer.array_type(slot_count);
        // SAFETY: the dynamic interface coercion stores a pointer to the immutable witness table;
        // `slot` is checked against the declaration-defined interface width above.
        let entry = unsafe {
            self.builder.build_gep(
                table_type,
                witness,
                &[
                    self.generator.context.i32_type().const_zero(),
                    self.generator
                        .context
                        .i32_type()
                        .const_int(u64::from(slot), false),
                ],
                "witness.entry.address",
            )?
        };
        Ok(self
            .builder
            .build_load(pointer, entry, "witness.entry")?
            .into_pointer_value())
    }

    fn lower_interface_cast(
        &self,
        operand: &Operand,
        target: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let Type::DynamicInterface(interface, _) = target else {
            return Err(CodegenError::Unsupported(
                "interface coercion target is not dynamic".into(),
            ));
        };
        let source = self.operand_type(operand)?;
        let Type::Nominal(target_declaration, _) = source.clone() else {
            return Err(CodegenError::Unsupported(format!(
                "interface coercion source is not nominal: {source:?}"
            )));
        };
        let data = if self.generator.is_class_type(&source) {
            self.lower_operand(operand)?.into_pointer_value()
        } else if let Some(place) = operand_place(operand) {
            self.place_pointer(place)?
        } else {
            let value = self.lower_operand(operand)?;
            let llvm_type = self.generator.basic_type(&source)?;
            let size = llvm_type.size_of().ok_or_else(|| {
                CodegenError::Unsupported("interface source has no statically known size".into())
            })?;
            let data = self
                .builder
                .build_call(
                    self.generator.runtime_alloc(),
                    &[size.into()],
                    "interface.box",
                )?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                .into_pointer_value();
            self.builder.build_store(data, value)?;
            data
        };
        let witness = self
            .generator
            .witnesses
            .get(&(*interface, target_declaration))
            .copied()
            .ok_or_else(|| {
                CodegenError::Unsupported(format!(
                    "no witness table for interface {interface:?} and target {target_declaration:?}"
                ))
            })?;
        let pair = self.generator.basic_type(target)?.into_struct_type();
        let pair = self
            .builder
            .build_insert_value(pair.const_zero(), data, 0, "interface.data")?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(pair, witness, 1, "interface.witness")?
            .into_struct_value()
            .into())
    }

    fn lower_switch_value(&self, operand: &Operand) -> Result<IntValue<'ctx>, CodegenError> {
        let place = match operand {
            Operand::Copy(place) | Operand::Move(place) => place,
            Operand::Constant(_) => return Ok(self.lower_operand(operand)?.into_int_value()),
        };
        let ty = self.place_type(place)?;
        let tag_type = match &ty {
            Type::Optional(_) => Some(self.generator.context.bool_type()),
            Type::Nominal(declaration, _) if self.generator.is_enum(*declaration) => {
                let c_repr = self
                    .generator
                    .layouts
                    .nominals
                    .get(declaration)
                    .is_some_and(|layout| {
                        matches!(layout.kind, NominalKind::Enum { c_repr: true, .. })
                    });
                Some(if c_repr {
                    self.generator.context.i32_type()
                } else {
                    self.generator.context.i64_type()
                })
            }
            _ => None,
        };
        let Some(tag_type) = tag_type else {
            if matches!(ty, Type::ErrorUnion(_)) {
                let envelope = self.lower_operand(operand)?.into_pointer_value();
                let envelope_type = self.error_union_type();
                let tag_address = self.builder.build_struct_gep(
                    envelope_type,
                    envelope,
                    0,
                    "error.tag.address",
                )?;
                return Ok(self
                    .builder
                    .build_load(self.generator.context.i64_type(), tag_address, "error.tag")?
                    .into_int_value());
            }
            return Ok(self.lower_operand(operand)?.into_int_value());
        };
        if matches!(
            &ty,
            Type::Nominal(declaration, _)
                if self
                    .generator
                    .layouts
                    .nominals
                    .get(declaration)
                    .is_some_and(|layout| {
                        matches!(layout.kind, NominalKind::Enum { c_repr: true, .. })
                    })
        ) {
            return Ok(self
                .builder
                .build_load(
                    self.generator.basic_type(&ty)?,
                    self.place_pointer(place)?,
                    "c.enum.discriminant",
                )?
                .into_int_value());
        }
        let structure = self.generator.basic_type(&ty)?.into_struct_type();
        let address = self.builder.build_struct_gep(
            structure,
            self.place_pointer(place)?,
            0,
            "discriminant.address",
        )?;
        Ok(self
            .builder
            .build_load(tag_type, address, "discriminant")?
            .into_int_value())
    }

    fn lower_constant(&self, constant: &Constant) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        Ok(match constant {
            Constant::Bool(value) => self
                .generator
                .context
                .bool_type()
                .const_int(u64::from(*value), false)
                .into(),
            Constant::Integer { value, ty } => self
                .generator
                .basic_type(ty)?
                .into_int_type()
                .const_int_arbitrary_precision(&u128_words(value.cast_unsigned()))
                .into(),
            Constant::Float { bits, ty } => match ty {
                Type::Primitive(PrimitiveType::F32) => self
                    .generator
                    .context
                    .f32_type()
                    .const_float(f64::from(f32::from_bits(u32::try_from(*bits).map_err(
                        |_| CodegenError::Unsupported("f32 constant has excess bits".into()),
                    )?)))
                    .into(),
                Type::Primitive(PrimitiveType::F64) => self
                    .generator
                    .context
                    .f64_type()
                    .const_float(f64::from_bits(*bits))
                    .into(),
                _ => {
                    return Err(CodegenError::Unsupported(
                        "float constant has non-float type".into(),
                    ));
                }
            },
            Constant::Character(value) => self
                .generator
                .context
                .i32_type()
                .const_int(u64::from(u32::from(*value)), false)
                .into(),
            Constant::Undefined(ty) => self.generator.basic_type(ty)?.const_zero(),
            Constant::Function(declaration, ty) => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "function constant lacks a function type".into(),
                    ));
                };
                self.resolve_emitted_callable(Callable::function(*declaration), function_type)?
                    .as_global_value()
                    .as_pointer_value()
                    .into()
            }
            Constant::Method { owner, member, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "method constant lacks a function type".into(),
                    ));
                };
                self.resolve_emitted_callable(
                    Callable {
                        declaration: *owner,
                        member: Some(*member),
                    },
                    function_type,
                )?
                .as_global_value()
                .as_pointer_value()
                .into()
            }
            Constant::Constructor { owner, member, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "constructor constant lacks a function type".into(),
                    ));
                };
                self.generator
                    .constructors
                    .iter()
                    .find(|target| {
                        target.owner == *owner
                            && target.member == *member
                            && target.signature == *function_type
                    })
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "constructor {:?} was not emitted",
                            (*owner, *member)
                        ))
                    })?
                    .function
                    .as_global_value()
                    .as_pointer_value()
                    .into()
            }
            Constant::String(value) => self
                .builder
                .build_global_string_ptr(value, "tn.string")?
                .as_pointer_value()
                .into(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn place_pointer(&self, place: &Place) -> Result<PointerValue<'ctx>, CodegenError> {
        let mut pointer = self
            .locals
            .get(
                usize::try_from(place.local.0)
                    .map_err(|_| CodegenError::Unsupported("local index overflow".into()))?,
            )
            .copied()
            .ok_or_else(|| CodegenError::Unsupported("missing local".into()))?;
        let mut ty = self.local_type(place.local.0)?;
        let mut variant = None;
        for projection in &place.projection {
            match projection {
                Projection::Field { index, ty: field } => {
                    let structure = if self.generator.is_class_type(&ty) {
                        let object = self
                            .builder
                            .build_load(
                                self.generator.context.ptr_type(AddressSpace::default()),
                                pointer,
                                "class.object",
                            )?
                            .into_pointer_value();
                        pointer = object;
                        self.generator.class_object_type(&ty)?
                    } else {
                        let BasicTypeEnum::StructType(structure) =
                            self.generator.basic_type(&ty)?
                        else {
                            return Err(CodegenError::Unsupported(format!(
                                "field projection requires registered aggregate layout: {ty:?}"
                            )));
                        };
                        structure
                    };
                    pointer = self.builder.build_struct_gep(
                        structure,
                        pointer,
                        self.generator.layout_field_index(&ty, variant, *index)?,
                        "field.address",
                    )?;
                    ty = field.clone();
                    variant = None;
                }
                Projection::Dereference => {
                    let referent_type = match &ty {
                        Type::Reference { referent, .. } => referent.as_ref().clone(),
                        Type::RawPointer { pointee, .. } => pointee.as_ref().clone(),
                        _ => {
                            return Err(CodegenError::Unsupported(format!(
                                "dereference projection on {ty:?}"
                            )));
                        }
                    };
                    pointer = self
                        .builder
                        .build_load(
                            self.generator.context.ptr_type(AddressSpace::default()),
                            pointer,
                            "dereference.address",
                        )?
                        .into_pointer_value();
                    ty = referent_type;
                }
                Projection::Index(index) => {
                    let index = self
                        .builder
                        .build_load(
                            self.generator.pointer_int_type(),
                            self.locals[index.0 as usize],
                            "projection.index",
                        )?
                        .into_int_value();
                    (pointer, ty) = self.index_pointer_from(pointer, &ty, index)?;
                }
                Projection::Downcast(selected) => {
                    if let Type::Optional(inner) = &ty {
                        if *selected != 1 {
                            return Err(CodegenError::Unsupported(
                                "absent optional has no payload place".into(),
                            ));
                        }
                        let structure = self.generator.basic_type(&ty)?.into_struct_type();
                        pointer = self.builder.build_struct_gep(
                            structure,
                            pointer,
                            1,
                            "optional.payload.address",
                        )?;
                        ty = inner.as_ref().clone();
                    } else if matches!(ty, Type::Nominal(declaration, _) if self.generator.is_enum(declaration))
                    {
                        variant = Some(*selected);
                    } else {
                        return Err(CodegenError::Unsupported(
                            "nominal downcast layout is not registered".into(),
                        ));
                    }
                }
                Projection::BaseClass(base) => {
                    if !self.generator.is_class_type(&ty) {
                        return Err(CodegenError::Unsupported(format!(
                            "base projection requires a class value, found {ty:?}"
                        )));
                    }
                    ty = Type::Nominal(*base, Vec::new());
                }
            }
        }
        Ok(pointer)
    }

    fn place_type(&self, place: &Place) -> Result<Type, CodegenError> {
        let mut ty = self.local_type(place.local.0)?;
        for projection in &place.projection {
            ty = match projection {
                Projection::Field { ty, .. } => ty.clone(),
                Projection::Dereference => match ty {
                    Type::Reference { referent, .. } => *referent,
                    Type::RawPointer { pointee, .. } => *pointee,
                    _ => return Err(CodegenError::Unsupported("invalid dereference type".into())),
                },
                Projection::Index(_) => match ty {
                    Type::Array(element, _) | Type::Slice(element) => *element,
                    _ => return Err(CodegenError::Unsupported("invalid index type".into())),
                },
                Projection::Downcast(1) => match ty {
                    Type::Optional(inner) => *inner,
                    Type::Nominal(_, _) => ty,
                    _ => return Err(CodegenError::Unsupported("invalid downcast type".into())),
                },
                Projection::BaseClass(base) => Type::Nominal(*base, Vec::new()),
                Projection::Downcast(_) => ty,
            };
        }
        Ok(ty)
    }

    fn local_type(&self, local: u32) -> Result<Type, CodegenError> {
        self.body
            .locals
            .get(local as usize)
            .map(|local| local.ty.clone())
            .ok_or_else(|| CodegenError::Unsupported("missing local type".into()))
    }

    fn is_borrowed_class_receiver(&self, place: &Place) -> bool {
        place.local.0 == 0
            && place.projection.is_empty()
            && self
                .body
                .locals
                .first()
                .is_some_and(|local| local.argument && local.name.as_deref() == Some("self"))
            && self
                .body
                .locals
                .first()
                .is_some_and(|local| self.generator.is_class_type(&local.ty))
    }

    fn index_pointer(
        &self,
        collection: &Place,
        index: IntValue<'ctx>,
    ) -> Result<(PointerValue<'ctx>, Type), CodegenError> {
        self.index_pointer_from(
            self.place_pointer(collection)?,
            &self.place_type(collection)?,
            index,
        )
    }

    fn index_pointer_from(
        &self,
        pointer: PointerValue<'ctx>,
        collection: &Type,
        index: IntValue<'ctx>,
    ) -> Result<(PointerValue<'ctx>, Type), CodegenError> {
        match collection {
            Type::Array(element, length) => {
                let valid = self.builder.build_int_compare(
                    IntPredicate::ULT,
                    index,
                    index.get_type().const_int(*length, false),
                    "index.in_bounds",
                )?;
                self.guard(valid, "index out of range")?;
                let array = self.generator.basic_type(collection)?.into_array_type();
                let zero = self.generator.context.i32_type().const_zero();
                // SAFETY: `pointer` addresses an alloca with `array` layout, and the preceding
                // check proves `index < length`. A non-inbounds GEP deliberately adds no stronger
                // provenance or overflow promise to LLVM.
                let element_pointer = unsafe {
                    self.builder.build_gep(
                        array,
                        pointer,
                        &[zero, index],
                        "array.element.address",
                    )?
                };
                Ok((element_pointer, element.as_ref().clone()))
            }
            Type::Slice(element) => {
                let slice = self.generator.basic_type(collection)?.into_struct_type();
                let data_field =
                    self.builder
                        .build_struct_gep(slice, pointer, 0, "slice.data.address")?;
                let length_field =
                    self.builder
                        .build_struct_gep(slice, pointer, 1, "slice.length.address")?;
                let data = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        data_field,
                        "slice.data",
                    )?
                    .into_pointer_value();
                let length = self
                    .builder
                    .build_load(
                        self.generator.pointer_int_type(),
                        length_field,
                        "slice.length",
                    )?
                    .into_int_value();
                let valid = self.builder.build_int_compare(
                    IntPredicate::ULT,
                    index,
                    length,
                    "index.in_bounds",
                )?;
                self.guard(valid, "index out of range")?;
                let element_type = self.generator.basic_type(element)?;
                // SAFETY: the slice invariant provides storage for `length` consecutive elements,
                // and the preceding check proves this element address lies within that storage.
                let element_pointer = unsafe {
                    self.builder
                        .build_gep(element_type, data, &[index], "slice.element.address")?
                };
                Ok((element_pointer, element.as_ref().clone()))
            }
            _ => Err(CodegenError::Unsupported(format!(
                "indexing requires array or slice layout, found {collection:?}"
            ))),
        }
    }

    fn block(&self, block: BasicBlockId) -> LlvmBlock<'ctx> {
        self.blocks[block.0 as usize]
    }
}

fn concrete_units(bodies: &[Body]) -> Vec<MonomorphizedBody> {
    bodies
        .iter()
        .map(|body| MonomorphizedBody {
            instance: Instance::concrete(Callable {
                declaration: body.declaration,
                member: body.member,
            }),
            body: body.clone(),
        })
        .collect()
}

fn operand_place(operand: &Operand) -> Option<&Place> {
    match operand {
        Operand::Copy(place) | Operand::Move(place) => Some(place),
        Operand::Constant(_) => None,
    }
}

/// Returns the deterministic native symbol assigned to a specialized callable instance.
pub fn symbol_for_instance(instance: &Instance) -> String {
    let mut name = instance.callable.member.map_or_else(
        || format!("tn_{}", instance.callable.declaration.0),
        |member| format!("tn_{}_{}", instance.callable.declaration.0, member.0),
    );
    if !instance.type_arguments.is_empty() || !instance.effects.is_empty() {
        name.push('_');
        let _ = write!(name, "{:016x}", stable_hash(&format!("{instance:?}")));
    }
    name
}

/// Returns the deterministic symbol assigned to a generated class constructor wrapper.
pub fn symbol_for_constructor(
    owner: DeclarationId,
    member: Option<tn_hir::MemberId>,
    signature: &FunctionType,
) -> String {
    format!(
        "tn_ctor_{}_{}_{:016x}",
        owner.0,
        member.map_or(0, |member| member.0),
        stable_hash(&format!("{owner:?}:{member:?}:{signature:?}"))
    )
}

fn body_signature(body: &Body) -> FunctionType {
    FunctionType {
        parameters: body
            .locals
            .iter()
            .filter(|local| local.argument)
            .map(|local| local.ty.clone())
            .collect(),
        result: Box::new(body.return_type.clone()),
        effects: body.effects.clone(),
        generics: Vec::new(),
        is_async: false,
        is_unsafe: false,
    }
}

fn signature_matches(emitted: &FunctionType, requested: &FunctionType) -> bool {
    emitted.parameters.ends_with(&requested.parameters)
        && emitted.result == requested.result
        && emitted.effects == requested.effects
}

fn stable_hash(value: &str) -> u64 {
    value
        .bytes()
        .fold(14_695_981_039_346_656_037, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
}

fn parse_pointer_bits(layout: &str) -> Option<u32> {
    layout
        .split('-')
        .find_map(|part| part.strip_prefix("p:")?.split(':').next()?.parse().ok())
}

fn is_signed(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Primitive(
            PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::I64
                | PrimitiveType::I128
                | PrimitiveType::Isize
        )
    )
}

fn integer_predicate(operator: BinaryOperator, signed: bool) -> Result<IntPredicate, CodegenError> {
    Ok(match (operator, signed) {
        (BinaryOperator::Equal, _) => IntPredicate::EQ,
        (BinaryOperator::NotEqual, _) => IntPredicate::NE,
        (BinaryOperator::Less, true) => IntPredicate::SLT,
        (BinaryOperator::LessEqual, true) => IntPredicate::SLE,
        (BinaryOperator::Greater, true) => IntPredicate::SGT,
        (BinaryOperator::GreaterEqual, true) => IntPredicate::SGE,
        (BinaryOperator::Less, false) => IntPredicate::ULT,
        (BinaryOperator::LessEqual, false) => IntPredicate::ULE,
        (BinaryOperator::Greater, false) => IntPredicate::UGT,
        (BinaryOperator::GreaterEqual, false) => IntPredicate::UGE,
        _ => {
            return Err(CodegenError::Unsupported(
                "operator is not a comparison".into(),
            ));
        }
    })
}

fn stable_panic_code(message: &str) -> u32 {
    message.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}

fn u128_words(value: u128) -> [u64; 2] {
    let bytes = value.to_le_bytes();
    let low = u64::from_le_bytes(bytes[..8].try_into().expect("fixed low word"));
    let high = u64::from_le_bytes(bytes[8..].try_into().expect("fixed high word"));
    [low, high]
}

fn instantiate_type(ty: &Type, substitutions: &BTreeMap<String, Type>) -> Type {
    match ty {
        Type::Generic(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::Promise { result, effects } => Type::Promise {
            result: Box::new(instantiate_type(result, substitutions)),
            effects: effects.clone(),
        },
        Type::Nominal(declaration, arguments) => Type::Nominal(
            *declaration,
            arguments
                .iter()
                .map(|argument| instantiate_type(argument, substitutions))
                .collect(),
        ),
        Type::Optional(inner) => Type::Optional(Box::new(instantiate_type(inner, substitutions))),
        Type::Array(inner, length) => {
            Type::Array(Box::new(instantiate_type(inner, substitutions)), *length)
        }
        Type::Slice(inner) => Type::Slice(Box::new(instantiate_type(inner, substitutions))),
        Type::Tuple(elements) => Type::Tuple(
            elements
                .iter()
                .map(|element| instantiate_type(element, substitutions))
                .collect(),
        ),
        Type::Reference {
            mutable,
            lifetime,
            referent,
        } => Type::Reference {
            mutable: *mutable,
            lifetime: lifetime.clone(),
            referent: Box::new(instantiate_type(referent, substitutions)),
        },
        Type::RawPointer { mutable, pointee } => Type::RawPointer {
            mutable: *mutable,
            pointee: Box::new(instantiate_type(pointee, substitutions)),
        },
        Type::Function(function) => Type::Function(FunctionType {
            parameters: function
                .parameters
                .iter()
                .map(|parameter| instantiate_type(parameter, substitutions))
                .collect(),
            result: Box::new(instantiate_type(&function.result, substitutions)),
            effects: function.effects.clone(),
            generics: Vec::new(),
            is_async: function.is_async,
            is_unsafe: function.is_unsafe,
        }),
        Type::Template(elements) => Type::Template(
            elements
                .iter()
                .map(|element| instantiate_type(element, substitutions))
                .collect(),
        ),
        Type::DynamicInterface(declaration, arguments) => Type::DynamicInterface(
            *declaration,
            arguments
                .iter()
                .map(|argument| instantiate_type(argument, substitutions))
                .collect(),
        ),
        Type::Primitive(_)
        | Type::String
        | Type::Str
        | Type::Lifetime(_)
        | Type::ErrorUnion(_)
        | Type::Error
        | Type::Unknown => ty.clone(),
    }
}
