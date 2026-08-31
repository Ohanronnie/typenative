//! LLVM 22 adapter. LLVM types do not cross this crate boundary.

use inkwell::AddressSpace;
use inkwell::AtomicOrdering;
use inkwell::AtomicRMWBinOp;
use inkwell::OptimizationLevel;
use inkwell::attributes::AttributeLoc;
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
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue,
    StructValue,
};
use inkwell::{FloatPredicate, IntPredicate};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use tn_hir::{DeclarationId, FunctionType, HirClosureId, PrimitiveType, Type};
use tn_mir::{
    BasicBlockId, BinaryOperator, Body, Callable, Completion, Constant, Instance,
    MonomorphizedBody, Operand, Place, Projection, Rvalue, StatementKind, TerminatorKind,
    UnaryOperator,
};

pub const REQUIRED_LLVM_VERSION: (u32, u32, u32) = (22, 1, 8);
const STRING_HEADER_MAGIC: u64 = 6_076_299_263_593_804_116;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodegenProfile {
    Debug,
    Optimized,
}

/// Compiler-owned instrumentation requested for a native product.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Sanitizer {
    Address,
    Undefined,
    Thread,
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
    pub globals: BTreeMap<DeclarationId, GlobalLayout>,
    pub aliases: BTreeMap<DeclarationId, AliasLayout>,
    pub nominals: BTreeMap<DeclarationId, NominalLayout>,
    pub witnesses: BTreeMap<(DeclarationId, DeclarationId), Vec<VtableEntry>>,
    pub interfaces: BTreeMap<DeclarationId, u32>,
    pub interface_names: BTreeMap<DeclarationId, String>,
    pub externs: BTreeMap<Callable, ExternLayout>,
    pub exports: BTreeMap<Callable, String>,
    pub export_instances: BTreeMap<Instance, String>,
    pub drops: BTreeMap<DeclarationId, Callable>,
    pub copies: BTreeSet<DeclarationId>,
    pub inlines: BTreeSet<Callable>,
    pub async_functions: BTreeMap<Callable, FunctionType>,
    pub abi_wrappers: BTreeMap<Callable, AbiWrapperKind>,
    /// User decorators attached to callable declarations.  The code generator keeps the
    /// original callable available and builds a wrapper that applies these decorators at
    /// runtime before invoking the resulting callable.
    pub decorators: BTreeMap<Callable, Vec<DecoratorLayout>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecoratorLayout {
    pub decorator: Callable,
    pub signature: FunctionType,
    pub name: String,
    pub is_static: bool,
    pub is_private: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AliasLayout {
    pub parameters: Vec<String>,
    pub body: Type,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalLayout {
    pub name: String,
    pub ty: Type,
    pub mutable_static: bool,
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
        &[],
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
    emit_program_to_file_with_sanitizers(
        module_name,
        units,
        layouts,
        target_triple,
        profile,
        emission,
        &[],
        path,
    )
}

/// Emits reachable MIR with compiler-owned sanitizer instrumentation.
///
/// Address and thread instrumentation is inserted by the LLVM pass pipeline. Undefined behavior
/// checks are emitted during lowering at every guarded operation because LLVM 22 has no general
/// UBSan module pass. The resulting product still requires the matching sanitizer runtime at link
/// time, which the active driver supplies for executable and library products.
#[allow(clippy::too_many_arguments)]
pub fn emit_program_to_file_with_sanitizers(
    module_name: &str,
    units: &[MonomorphizedBody],
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
    emission: Emission,
    sanitizers: &[Sanitizer],
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
        sanitizers,
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

/// Emits the Node-API bridge plan as a verified LLVM object.
///
/// The bridge is emitted in a separate module so the native `TypeNative` program and its bridge
/// retain independent ownership of symbol declarations while sharing the final linker image.
/// Node-API itself is intentionally represented by its stable C ABI, using opaque pointers; no
/// Node headers or generated C source participate in this path.
///
/// # Errors
///
/// Returns an error when the LLVM version, target machine, bridge lowering, verification,
/// optimization, or object emission fails.
#[allow(clippy::too_many_arguments)]
pub fn emit_node_bridge_to_file(
    plan: &tn_node_api::BridgePlan,
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
    path: &Path,
) -> Result<(), CodegenError> {
    emit_node_bridge_to_file_with_sanitizers(
        "typenative.node.bridge",
        plan,
        layouts,
        target_triple,
        profile,
        &[],
        path,
    )
}

/// Emits a Node-API bridge with the same compiler-owned sanitizer instrumentation as the program
/// module.
#[allow(clippy::too_many_arguments)]
pub fn emit_node_bridge_to_file_with_sanitizers(
    module_name: &str,
    plan: &tn_node_api::BridgePlan,
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
    sanitizers: &[Sanitizer],
    path: &Path,
) -> Result<(), CodegenError> {
    verify_llvm_version()?;
    let sanitizer_set = sanitizer_set(sanitizers)?;
    Target::initialize_all(&InitializationConfig::default());
    let context = Context::create();
    let module = context.create_module("typenative.node.bridge");
    let triple = TargetTriple::create(target_triple);
    let machine = target_machine(&triple, profile)?;
    module.set_triple(&triple);
    let target_data = machine.get_target_data();
    module.set_data_layout(&target_data.get_data_layout());
    let bridge = NodeBridgeGenerator::new(
        &context,
        module,
        target_data,
        layouts.clone(),
        module_name,
        target_triple,
        profile,
        &sanitizer_set,
    );
    bridge.emit(plan)?;
    bridge.generator.debug_info.finalize();
    bridge
        .generator
        .module
        .verify()
        .map_err(|error| CodegenError::Verification(error.to_string()))?;
    run_backend_pipeline(&bridge.generator.module, &machine, profile, &sanitizer_set)?;
    if let Some(path) = std::env::var_os("TN_NODE_BRIDGE_IR") {
        bridge
            .generator
            .module
            .print_to_file(Path::new(&path))
            .map_err(|error| CodegenError::Output(error.to_string()))?;
    }
    machine
        .write_to_file(&bridge.generator.module, FileType::Object, path)
        .map_err(|error| CodegenError::Output(error.to_string()))
}

fn generate<'ctx>(
    context: &'ctx Context,
    module_name: &str,
    units: &[MonomorphizedBody],
    layouts: &Layouts,
    target_triple: &str,
    profile: CodegenProfile,
    sanitizers: &[Sanitizer],
) -> Result<(Generator<'ctx>, TargetMachine), CodegenError> {
    verify_llvm_version()?;
    let sanitizer_set = sanitizer_set(sanitizers)?;
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
        target_triple,
        profile,
        &sanitizer_set,
    );
    generator.declare_externs(units)?;
    generator.declare_globals()?;
    generator.declare_bodies(units)?;
    generator.declare_closures(units)?;
    generator.declare_descriptors(units)?;
    generator.declare_witnesses()?;
    generator.lower_constructor_wrappers()?;
    generator.lower_bodies(units)?;
    generator.lower_closures(units)?;
    generator.lower_async_wrappers()?;
    generator.lower_abi_wrappers()?;
    generator.debug_info.finalize();
    generator
        .module
        .verify()
        .map_err(|error| CodegenError::Verification(error.to_string()))?;
    run_backend_pipeline(&generator.module, &machine, profile, &sanitizer_set)?;
    Ok((generator, machine))
}

fn sanitizer_set(sanitizers: &[Sanitizer]) -> Result<BTreeSet<Sanitizer>, CodegenError> {
    let set = sanitizers.iter().copied().collect::<BTreeSet<_>>();
    if set.contains(&Sanitizer::Address) && set.contains(&Sanitizer::Thread) {
        return Err(CodegenError::Unsupported(
            "AddressSanitizer and ThreadSanitizer cannot be enabled together".into(),
        ));
    }
    Ok(set)
}

fn run_backend_pipeline(
    module: &Module<'_>,
    machine: &TargetMachine,
    profile: CodegenProfile,
    sanitizers: &BTreeSet<Sanitizer>,
) -> Result<(), CodegenError> {
    if profile == CodegenProfile::Optimized {
        let options = PassBuilderOptions::create();
        options.set_verify_each(true);
        module
            .run_passes("default<O2>", machine, options)
            .map_err(|error| CodegenError::Optimization(error.to_string()))?;
    }
    let mut instrumentation = Vec::new();
    if sanitizers.contains(&Sanitizer::Address) {
        instrumentation.push("asan");
    }
    if sanitizers.contains(&Sanitizer::Thread) {
        instrumentation.push("tsan");
    }
    if !instrumentation.is_empty() {
        let options = PassBuilderOptions::create();
        options.set_verify_each(true);
        module
            .run_passes(&instrumentation.join(","), machine, options)
            .map_err(|error| CodegenError::Optimization(error.to_string()))?;
    }
    module
        .verify()
        .map_err(|error| CodegenError::Verification(error.to_string()))
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
    globals: BTreeMap<DeclarationId, PointerValue<'ctx>>,
    global_initialized: BTreeMap<DeclarationId, PointerValue<'ctx>>,
    constructors: Vec<ConstructorTarget<'ctx>>,
    descriptors: BTreeMap<(DeclarationId, Vec<Type>), PointerValue<'ctx>>,
    witnesses: BTreeMap<(DeclarationId, DeclarationId), PointerValue<'ctx>>,
    builtin_witnesses: BTreeMap<(DeclarationId, Type), PointerValue<'ctx>>,
    debug_info: DebugInfoState<'ctx>,
    async_wrappers: Vec<AsyncWrapper<'ctx>>,
    abi_wrappers: Vec<AbiWrapper<'ctx>>,
    closures: BTreeMap<ClosureKey, ClosureTarget<'ctx>>,
    is_macos: bool,
    sanitizers: BTreeSet<Sanitizer>,
}

struct NodeBridgeGenerator<'ctx> {
    generator: Generator<'ctx>,
    _profile: CodegenProfile,
}

struct NodeClassCallbacks<'ctx> {
    constructor: FunctionValue<'ctx>,
    methods: Vec<(String, FunctionValue<'ctx>, tn_hir::ReceiverMode)>,
}

impl<'ctx> NodeBridgeGenerator<'ctx> {
    fn new(
        context: &'ctx Context,
        module: Module<'ctx>,
        target_data: TargetData,
        layouts: Layouts,
        module_name: &str,
        target_triple: &str,
        profile: CodegenProfile,
        sanitizers: &BTreeSet<Sanitizer>,
    ) -> Self {
        Self {
            generator: Generator::new(
                context,
                module,
                target_data,
                layouts,
                module_name,
                target_triple,
                profile,
                sanitizers,
            ),
            _profile: profile,
        }
    }

    fn attach_debug(&self, function: FunctionValue<'ctx>, name: &str) {
        self.generator.debug_info.attach_function(function, name);
    }

    fn set_debug_location(&self, builder: &Builder<'ctx>, function: FunctionValue<'ctx>) {
        if let Some(subprogram) = function.get_subprogram() {
            let location = self.generator.debug_info.builder.create_debug_location(
                self.generator.context,
                1,
                1,
                subprogram.as_debug_info_scope(),
                None,
            );
            builder.set_current_debug_location(location);
        }
    }

    fn emit(&self, plan: &tn_node_api::BridgePlan) -> Result<(), CodegenError> {
        if plan.functions.is_empty() && plan.classes.is_empty() {
            return Err(CodegenError::Unsupported(
                "Node bridge plan contains no exports".into(),
            ));
        }
        let mut callbacks = Vec::with_capacity(plan.functions.len());
        for (index, function) in plan.functions.iter().enumerate() {
            callbacks.push(self.emit_function_callback(index, function)?);
        }
        let mut class_callbacks = Vec::with_capacity(plan.classes.len());
        for (index, class) in plan.classes.iter().enumerate() {
            class_callbacks.push(self.emit_class(index, class)?);
        }
        self.emit_module_initializer(plan, &callbacks, &class_callbacks)?;
        Ok(())
    }

    fn pointer_type(&self) -> inkwell::types::PointerType<'ctx> {
        self.generator.context.ptr_type(AddressSpace::default())
    }

    fn size_type(&self) -> inkwell::types::IntType<'ctx> {
        self.generator.pointer_int_type()
    }

    fn status_type(&self) -> inkwell::types::IntType<'ctx> {
        self.generator.context.i32_type()
    }

    fn napi_function(
        &self,
        name: &str,
        result: inkwell::types::BasicTypeEnum<'ctx>,
        parameters: &[inkwell::types::BasicMetadataTypeEnum<'ctx>],
    ) -> FunctionValue<'ctx> {
        self.generator.module.get_function(name).unwrap_or_else(|| {
            self.generator
                .module
                .add_function(name, result.fn_type(parameters, false), None)
        })
    }

    fn napi_status_function(
        &self,
        name: &str,
        parameters: &[inkwell::types::BasicMetadataTypeEnum<'ctx>],
    ) -> FunctionValue<'ctx> {
        self.napi_function(name, self.status_type().into(), parameters)
    }

    fn callback_type(&self) -> LlvmFunctionType<'ctx> {
        self.pointer_type().fn_type(
            &[self.pointer_type().into(), self.pointer_type().into()],
            false,
        )
    }

    fn finalize_type(&self) -> LlvmFunctionType<'ctx> {
        self.generator.context.void_type().fn_type(
            &[
                self.pointer_type().into(),
                self.pointer_type().into(),
                self.pointer_type().into(),
            ],
            false,
        )
    }

    fn c_string(
        &self,
        builder: &Builder<'ctx>,
        value: &str,
        name: &str,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        builder
            .build_global_string_ptr(value, name)
            .map(|global| global.as_pointer_value())
            .map_err(CodegenError::from)
    }

    fn call_value(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        arguments: &[BasicMetadataValueEnum<'ctx>],
        name: &str,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        builder
            .build_call(function, arguments, name)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder(format!("{name} returned void")))
    }

    fn call_status(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        arguments: &[BasicMetadataValueEnum<'ctx>],
        name: &str,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        Ok(self
            .call_value(builder, function, arguments, name)?
            .into_int_value())
    }

    fn append_error_block(
        &self,
        function: FunctionValue<'ctx>,
        message: &str,
    ) -> Result<LlvmBlock<'ctx>, CodegenError> {
        let block = self
            .generator
            .context
            .append_basic_block(function, "node.error");
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, function);
        builder.position_at_end(block);
        let env = function
            .get_nth_param(0)
            .ok_or_else(|| CodegenError::Builder("Node callback environment is missing".into()))?;
        let message = self.c_string(&builder, message, "node.error.message")?;
        let null = self.pointer_type().const_null();
        builder.build_call(
            self.napi_status_function(
                "napi_throw_type_error",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[env.into(), null.into(), message.into()],
            "node.throw",
        )?;
        builder.build_return(Some(&null))?;
        Ok(block)
    }

    fn continue_if_status_ok(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        status: IntValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        let ok = builder.build_int_compare(
            IntPredicate::EQ,
            status,
            self.status_type().const_zero(),
            &format!("{name}.ok"),
        )?;
        let next = self.generator.context.append_basic_block(function, name);
        builder.build_conditional_branch(ok, next, error)?;
        builder.position_at_end(next);
        Ok(())
    }

    fn continue_if(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        condition: IntValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        let next = self.generator.context.append_basic_block(function, name);
        builder.build_conditional_branch(condition, next, error)?;
        builder.position_at_end(next);
        Ok(())
    }

    fn native_signature(
        &self,
        signature: &FunctionType,
        receiver: Option<&Type>,
        result: &Type,
        async_function: bool,
    ) -> Result<LlvmFunctionType<'ctx>, CodegenError> {
        let mut parameters = receiver
            .map(|receiver| {
                self.generator
                    .basic_type(receiver)
                    .map(BasicMetadataTypeEnum::from)
            })
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        parameters.extend(
            signature
                .parameters
                .iter()
                .map(|parameter| {
                    if !async_function && self.node_requires_indirect(parameter) {
                        Ok(self.pointer_type().into())
                    } else {
                        self.generator
                            .basic_type(parameter)
                            .map(BasicMetadataTypeEnum::from)
                    }
                })
                .collect::<Result<Vec<_>, CodegenError>>()?,
        );
        if async_function {
            return Ok(self
                .generator
                .basic_type(result)?
                .fn_type(&parameters, false));
        }
        if !signature.effects.is_empty() {
            return Ok(self
                .generator
                .context
                .i64_type()
                .array_type(2)
                .fn_type(&parameters, false));
        }
        if *result == Type::Primitive(PrimitiveType::Void) {
            Ok(self
                .generator
                .context
                .void_type()
                .fn_type(&parameters, false))
        } else if self.generator.is_indirect_abi_type(result) {
            Ok(self.pointer_type().fn_type(&parameters, false))
        } else {
            Ok(self
                .generator
                .basic_type(result)?
                .fn_type(&parameters, false))
        }
    }

    fn node_requires_indirect(&self, ty: &Type) -> bool {
        self.generator.is_indirect_abi_type(ty)
    }

    fn convert_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match &ty.kind {
            tn_node_api::NodeTypeKind::Void => Err(CodegenError::Unsupported(
                "Node arguments cannot have void type".into(),
            )),
            tn_node_api::NodeTypeKind::Promise { .. } => Err(CodegenError::Unsupported(
                "Node Promise values are produced asynchronously".into(),
            )),
            tn_node_api::NodeTypeKind::Scalar(primitive) => {
                self.convert_scalar_argument(builder, function, env, value, primitive, error, index)
            }
            tn_node_api::NodeTypeKind::String => {
                self.convert_string_argument(builder, function, env, value, error, index)
            }
            tn_node_api::NodeTypeKind::Bytes => {
                self.convert_bytes_argument(builder, function, env, value, ty, error, index)
            }
            tn_node_api::NodeTypeKind::Optional(inner) => self
                .convert_optional_argument(builder, function, env, value, ty, inner, error, index),
            tn_node_api::NodeTypeKind::Array {
                fixed_length: Some(length),
                element,
                ..
            } => self.convert_fixed_array_argument(
                builder, function, env, value, ty, element, *length, error, index,
            ),
            tn_node_api::NodeTypeKind::Array { element, .. } => self
                .convert_array_argument(builder, function, env, value, ty, element, error, index),
            tn_node_api::NodeTypeKind::Class(_) => Err(CodegenError::Unsupported(
                "Node class handles are not accepted as arguments".into(),
            )),
        }
    }

    fn convert_scalar_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        primitive: &PrimitiveType,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let name = format!("node.scalar.{index}");
        let pointer = self.pointer_type();
        let status_args = &[pointer.into(), pointer.into(), pointer.into()];
        match primitive {
            PrimitiveType::Bool => {
                let slot = builder.build_alloca(self.generator.context.i8_type(), &name)?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function("napi_get_value_bool", status_args),
                    &[env.into(), value.into(), slot.into()],
                    &format!("{name}.get"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                let loaded = builder
                    .build_load(
                        self.generator.context.i8_type(),
                        slot,
                        &format!("{name}.value"),
                    )?
                    .into_int_value();
                Ok(builder
                    .build_int_compare(
                        IntPredicate::NE,
                        loaded,
                        self.generator.context.i8_type().const_zero(),
                        &format!("{name}.bool"),
                    )?
                    .into())
            }
            PrimitiveType::I64 | PrimitiveType::Isize => {
                let slot = builder.build_alloca(self.generator.context.i64_type(), &name)?;
                let lossless = builder.build_alloca(
                    self.generator.context.i8_type(),
                    &format!("{name}.lossless"),
                )?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_get_value_bigint_int64",
                        &[
                            pointer.into(),
                            pointer.into(),
                            pointer.into(),
                            pointer.into(),
                        ],
                    ),
                    &[env.into(), value.into(), slot.into(), lossless.into()],
                    &format!("{name}.get"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                let lossless_value = builder
                    .build_load(
                        self.generator.context.i8_type(),
                        lossless,
                        &format!("{name}.lossless.value"),
                    )?
                    .into_int_value();
                self.continue_if(
                    builder,
                    function,
                    builder.build_int_compare(
                        IntPredicate::NE,
                        lossless_value,
                        self.generator.context.i8_type().const_zero(),
                        &format!("{name}.lossless.ok"),
                    )?,
                    error,
                    &format!("{name}.lossless.status"),
                )?;
                let loaded = builder
                    .build_load(
                        self.generator.context.i64_type(),
                        slot,
                        &format!("{name}.value"),
                    )?
                    .into_int_value();
                Ok(loaded.into())
            }
            PrimitiveType::U64 | PrimitiveType::Usize => {
                let slot = builder.build_alloca(self.generator.context.i64_type(), &name)?;
                let lossless = builder.build_alloca(
                    self.generator.context.i8_type(),
                    &format!("{name}.lossless"),
                )?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_get_value_bigint_uint64",
                        &[
                            pointer.into(),
                            pointer.into(),
                            pointer.into(),
                            pointer.into(),
                        ],
                    ),
                    &[env.into(), value.into(), slot.into(), lossless.into()],
                    &format!("{name}.get"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                let lossless_value = builder
                    .build_load(
                        self.generator.context.i8_type(),
                        lossless,
                        &format!("{name}.lossless.value"),
                    )?
                    .into_int_value();
                self.continue_if(
                    builder,
                    function,
                    builder.build_int_compare(
                        IntPredicate::NE,
                        lossless_value,
                        self.generator.context.i8_type().const_zero(),
                        &format!("{name}.lossless.ok"),
                    )?,
                    error,
                    &format!("{name}.lossless.status"),
                )?;
                Ok(builder.build_load(
                    self.generator.context.i64_type(),
                    slot,
                    &format!("{name}.value"),
                )?)
            }
            PrimitiveType::I128 | PrimitiveType::U128 => {
                self.convert_i128_argument(builder, function, env, value, primitive, error, index)
            }
            PrimitiveType::F32 | PrimitiveType::F64 => {
                let slot = builder.build_alloca(self.generator.context.f64_type(), &name)?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function("napi_get_value_double", status_args),
                    &[env.into(), value.into(), slot.into()],
                    &format!("{name}.get"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                let loaded = builder
                    .build_load(
                        self.generator.context.f64_type(),
                        slot,
                        &format!("{name}.value"),
                    )?
                    .into_float_value();
                if matches!(primitive, PrimitiveType::F32) {
                    Ok(builder
                        .build_float_trunc(
                            loaded,
                            self.generator.context.f32_type(),
                            &format!("{name}.f32"),
                        )?
                        .into())
                } else {
                    Ok(loaded.into())
                }
            }
            PrimitiveType::Char => {
                let slot = builder.build_alloca(self.generator.context.i32_type(), &name)?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function("napi_get_value_uint32", status_args),
                    &[env.into(), value.into(), slot.into()],
                    &format!("{name}.get"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                let loaded = builder
                    .build_load(
                        self.generator.context.i32_type(),
                        slot,
                        &format!("{name}.value"),
                    )?
                    .into_int_value();
                let scalar = builder.build_int_compare(
                    IntPredicate::ULE,
                    loaded,
                    self.generator
                        .context
                        .i32_type()
                        .const_int(0x0010_FFFF, false),
                    "node.char.range",
                )?;
                let lower = builder.build_int_compare(
                    IntPredicate::ULT,
                    loaded,
                    self.generator.context.i32_type().const_int(0xD800, false),
                    "node.char.lower",
                )?;
                let upper = builder.build_int_compare(
                    IntPredicate::UGE,
                    loaded,
                    self.generator.context.i32_type().const_int(0xE000, false),
                    "node.char.upper",
                )?;
                self.continue_if(
                    builder,
                    function,
                    builder.build_and(
                        scalar,
                        builder.build_or(lower, upper, "node.char.surrogate")?,
                        "node.char.valid",
                    )?,
                    error,
                    "node.char.status",
                )?;
                Ok(loaded.into())
            }
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32 => {
                let slot = builder.build_alloca(self.generator.context.i32_type(), &name)?;
                let getter = if matches!(primitive, PrimitiveType::U32) {
                    "napi_get_value_uint32"
                } else {
                    "napi_get_value_int32"
                };
                let status = self.call_status(
                    builder,
                    self.napi_status_function(getter, status_args),
                    &[env.into(), value.into(), slot.into()],
                    &format!("{name}.get"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                let loaded = builder
                    .build_load(
                        self.generator.context.i32_type(),
                        slot,
                        &format!("{name}.value"),
                    )?
                    .into_int_value();
                let valid = match primitive {
                    PrimitiveType::I8 | PrimitiveType::I16 => {
                        let (minimum, maximum, label) = if matches!(primitive, PrimitiveType::I8) {
                            (i32::from(i8::MIN), i32::from(i8::MAX), "i8")
                        } else {
                            (i32::from(i16::MIN), i32::from(i16::MAX), "i16")
                        };
                        let minimum = builder.build_int_compare(
                            IntPredicate::SGE,
                            loaded,
                            self.generator
                                .context
                                .i32_type()
                                .const_int(u64::from(minimum.cast_unsigned()), true),
                            &format!("node.{label}.min"),
                        )?;
                        let maximum = builder.build_int_compare(
                            IntPredicate::SLE,
                            loaded,
                            self.generator
                                .context
                                .i32_type()
                                .const_int(u64::from(maximum.cast_unsigned()), true),
                            &format!("node.{label}.max"),
                        )?;
                        builder.build_and(minimum, maximum, &format!("node.{label}.range"))?
                    }
                    PrimitiveType::U8 => builder.build_int_compare(
                        IntPredicate::ULE,
                        loaded,
                        self.generator
                            .context
                            .i32_type()
                            .const_int(u64::from(u8::MAX), false),
                        "node.u8.max",
                    )?,
                    PrimitiveType::U16 => builder.build_int_compare(
                        IntPredicate::ULE,
                        loaded,
                        self.generator
                            .context
                            .i32_type()
                            .const_int(u64::from(u16::MAX), false),
                        "node.u16.max",
                    )?,
                    _ => self.generator.context.bool_type().const_int(1, false),
                };
                self.continue_if(builder, function, valid, error, &format!("{name}.range"))?;
                let native_type = self
                    .generator
                    .basic_type(&Type::Primitive(primitive.clone()))?;
                Ok(match native_type {
                    BasicTypeEnum::IntType(int_type) if int_type.get_bit_width() < 32 => builder
                        .build_int_truncate(loaded, int_type, &format!("{name}.narrow"))?
                        .into(),
                    _ => loaded.into(),
                })
            }
            PrimitiveType::Void | PrimitiveType::Never => Err(CodegenError::Unsupported(
                "void/never Node scalar is invalid".into(),
            )),
        }
    }

    fn convert_i128_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        primitive: &PrimitiveType,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let pointer = self.pointer_type();
        let name = format!("node.i128.{index}");
        let sign =
            builder.build_alloca(self.generator.context.i32_type(), &format!("{name}.sign"))?;
        let count = builder.build_alloca(self.size_type(), &format!("{name}.count"))?;
        let words = builder.build_alloca(
            self.generator.context.i64_type().array_type(2),
            &format!("{name}.words"),
        )?;
        builder.build_store(sign, self.generator.context.i32_type().const_zero())?;
        builder.build_store(count, self.size_type().const_int(2, false))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_value_bigint_words",
                &[
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                ],
            ),
            &[
                env.into(),
                value.into(),
                sign.into(),
                count.into(),
                words.into(),
            ],
            &format!("{name}.get"),
        )?;
        self.continue_if_status_ok(builder, function, status, error, &format!("{name}.status"))?;
        let actual_count = builder
            .build_load(self.size_type(), count, &format!("{name}.count.value"))?
            .into_int_value();
        self.continue_if(
            builder,
            function,
            builder.build_int_compare(
                IntPredicate::ULE,
                actual_count,
                self.size_type().const_int(2, false),
                &format!("{name}.count.ok"),
            )?,
            error,
            &format!("{name}.count.status"),
        )?;
        if matches!(primitive, PrimitiveType::U128) {
            let sign_value = builder
                .build_load(
                    self.generator.context.i32_type(),
                    sign,
                    &format!("{name}.sign.value"),
                )?
                .into_int_value();
            self.continue_if(
                builder,
                function,
                builder.build_int_compare(
                    IntPredicate::EQ,
                    sign_value,
                    self.generator.context.i32_type().const_zero(),
                    &format!("{name}.unsigned"),
                )?,
                error,
                &format!("{name}.unsigned.status"),
            )?;
        }
        let word_type = self.generator.context.i64_type();
        let low = builder
            .build_extract_value(
                builder
                    .build_load(
                        word_type.array_type(2),
                        words,
                        &format!("{name}.words.value"),
                    )?
                    .into_array_value(),
                0,
                &format!("{name}.low"),
            )?
            .into_int_value();
        let high = builder
            .build_extract_value(
                builder
                    .build_load(
                        word_type.array_type(2),
                        words,
                        &format!("{name}.words.high.value"),
                    )?
                    .into_array_value(),
                1,
                &format!("{name}.high"),
            )?
            .into_int_value();
        let wide = self.generator.context.i128_type();
        let low = builder.build_int_z_extend(low, wide, &format!("{name}.low.wide"))?;
        let high = builder.build_int_z_extend(high, wide, &format!("{name}.high.wide"))?;
        let high = builder.build_left_shift(
            high,
            wide.const_int(64, false),
            &format!("{name}.high.shift"),
        )?;
        let magnitude = builder.build_or(low, high, &format!("{name}.magnitude"))?;
        let result = if matches!(primitive, PrimitiveType::I128) {
            let sign_value = builder
                .build_load(
                    self.generator.context.i32_type(),
                    sign,
                    &format!("{name}.signed"),
                )?
                .into_int_value();
            let negative = builder.build_int_compare(
                IntPredicate::NE,
                sign_value,
                self.generator.context.i32_type().const_zero(),
                &format!("{name}.negative"),
            )?;
            let negated =
                builder.build_int_sub(wide.const_zero(), magnitude, &format!("{name}.negated"))?;
            let selected = builder.build_select(
                negative,
                negated,
                magnitude,
                &format!("{name}.signed.magnitude"),
            )?;
            selected.into_int_value()
        } else {
            magnitude
        };
        Ok(result.into())
    }

    fn convert_string_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let pointer = self.pointer_type();
        let name = format!("node.string.{index}");
        let length = builder.build_alloca(self.size_type(), &format!("{name}.length"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_value_string_utf8",
                &[
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    self.size_type().into(),
                    pointer.into(),
                ],
            ),
            &[
                env.into(),
                value.into(),
                pointer.const_null().into(),
                self.size_type().const_zero().into(),
                length.into(),
            ],
            &format!("{name}.length.get"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.length.status"),
        )?;
        let length_value = builder
            .build_load(self.size_type(), length, &format!("{name}.length.value"))?
            .into_int_value();
        let capacity = builder.build_int_add(
            length_value,
            self.size_type().const_int(1, false),
            &format!("{name}.capacity"),
        )?;
        let raw = self
            .call_value(
                builder,
                self.generator.runtime_alloc(),
                &[capacity.into()],
                &format!("{name}.alloc"),
            )?
            .into_pointer_value();
        self.continue_if(
            builder,
            function,
            builder.build_is_not_null(raw, &format!("{name}.alloc.ok"))?,
            error,
            &format!("{name}.alloc.status"),
        )?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_value_string_utf8",
                &[
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    self.size_type().into(),
                    pointer.into(),
                ],
            ),
            &[
                env.into(),
                value.into(),
                raw.into(),
                capacity.into(),
                length.into(),
            ],
            &format!("{name}.get"),
        )?;
        self.continue_if_status_ok(builder, function, status, error, &format!("{name}.status"))?;
        let string = self.call_value(
            builder,
            self.generator.runtime_string_from_bytes(),
            &[raw.into(), length_value.into()],
            &format!("{name}.own"),
        )?;
        builder.build_call(
            self.generator.runtime_free(),
            &[raw.into()],
            &format!("{name}.raw.free"),
        )?;
        self.continue_if(
            builder,
            function,
            builder.build_is_not_null(string.into_pointer_value(), &format!("{name}.own.ok"))?,
            error,
            &format!("{name}.own.status"),
        )?;
        Ok(string)
    }

    fn convert_bytes_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let pointer = self.pointer_type();
        let name = format!("node.bytes.{index}");
        let typed_array_type =
            builder.build_alloca(self.generator.context.i32_type(), &format!("{name}.type"))?;
        let length = builder.build_alloca(self.size_type(), &format!("{name}.length"))?;
        let data = builder.build_alloca(pointer, &format!("{name}.data"))?;
        let array_buffer = builder.build_alloca(pointer, &format!("{name}.buffer"))?;
        let offset = builder.build_alloca(self.size_type(), &format!("{name}.offset"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_typedarray_info",
                &[
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                ],
            ),
            &[
                env.into(),
                value.into(),
                typed_array_type.into(),
                length.into(),
                data.into(),
                array_buffer.into(),
                offset.into(),
            ],
            &format!("{name}.get"),
        )?;
        self.continue_if_status_ok(builder, function, status, error, &format!("{name}.status"))?;
        let typed_array_value = builder
            .build_load(
                self.generator.context.i32_type(),
                typed_array_type,
                &format!("{name}.type.value"),
            )?
            .into_int_value();
        self.continue_if(
            builder,
            function,
            builder.build_int_compare(
                IntPredicate::EQ,
                typed_array_value,
                self.generator.context.i32_type().const_int(1, false),
                &format!("{name}.uint8"),
            )?,
            error,
            &format!("{name}.type.status"),
        )?;
        let length_value = builder
            .build_load(self.size_type(), length, &format!("{name}.length.value"))?
            .into_int_value();
        let owned = self
            .call_value(
                builder,
                self.generator.runtime_alloc(),
                &[length_value.into()],
                &format!("{name}.alloc"),
            )?
            .into_pointer_value();
        self.continue_if(
            builder,
            function,
            builder.build_or(
                builder.build_int_compare(
                    IntPredicate::EQ,
                    length_value,
                    self.size_type().const_zero(),
                    &format!("{name}.empty"),
                )?,
                builder.build_is_not_null(owned, &format!("{name}.alloc.ok"))?,
                &format!("{name}.alloc.valid"),
            )?,
            error,
            &format!("{name}.alloc.status"),
        )?;
        let copied = self
            .call_value(
                builder,
                self.runtime_bytes_copy(),
                &[
                    builder
                        .build_load(pointer, data, &format!("{name}.data.value"))?
                        .into_pointer_value()
                        .into(),
                    length_value.into(),
                    owned.into(),
                ],
                &format!("{name}.copy"),
            )?
            .into_int_value();
        self.continue_if(
            builder,
            function,
            builder.build_int_compare(
                IntPredicate::EQ,
                copied,
                length_value,
                &format!("{name}.copy.ok"),
            )?,
            error,
            &format!("{name}.copy.status"),
        )?;
        let native_type = self.generator.basic_type(&ty.native)?.into_struct_type();
        let native = native_type.const_zero();
        let native = builder
            .build_insert_value(native, owned, 0, &format!("{name}.pointer"))?
            .into_struct_value();
        let native = builder
            .build_insert_value(native, length_value, 1, &format!("{name}.length.field"))?
            .into_struct_value();
        Ok(native.into())
    }

    fn runtime_bytes_copy(&self) -> FunctionValue<'ctx> {
        self.generator
            .module
            .get_function("tn_bytes_copy")
            .unwrap_or_else(|| {
                self.generator.module.add_function(
                    "tn_bytes_copy",
                    self.size_type().fn_type(
                        &[
                            self.pointer_type().into(),
                            self.size_type().into(),
                            self.pointer_type().into(),
                        ],
                        false,
                    ),
                    None,
                )
            })
    }

    fn convert_optional_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        inner: &tn_node_api::NodeType,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let pointer = self.pointer_type();
        let name = format!("node.optional.{index}");
        let value_type =
            builder.build_alloca(self.generator.context.i32_type(), &format!("{name}.type"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_typeof",
                &[pointer.into(), pointer.into(), pointer.into()],
            ),
            &[env.into(), value.into(), value_type.into()],
            &format!("{name}.typeof"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.typeof.status"),
        )?;
        let type_value = builder
            .build_load(
                self.generator.context.i32_type(),
                value_type,
                &format!("{name}.type.value"),
            )?
            .into_int_value();
        let is_undefined = builder.build_int_compare(
            IntPredicate::EQ,
            type_value,
            self.generator.context.i32_type().const_zero(),
            &format!("{name}.undefined"),
        )?;
        let present = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.present"));
        let absent = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.absent"));
        let merge = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.merge"));
        let output = builder.build_alloca(
            self.generator.basic_type(&ty.native)?,
            &format!("{name}.output"),
        )?;
        builder.build_conditional_branch(is_undefined, absent, present)?;
        builder.position_at_end(present);
        let payload = self.convert_argument(builder, function, env, value, inner, error, index)?;
        let structure = self.generator.basic_type(&ty.native)?.into_struct_type();
        let present_value = builder
            .build_insert_value(
                structure.const_zero(),
                self.generator.context.bool_type().const_int(1, false),
                0,
                &format!("{name}.present.tag"),
            )?
            .into_struct_value();
        let present_value = builder
            .build_insert_value(present_value, payload, 1, &format!("{name}.present.value"))?
            .into_struct_value();
        builder.build_store(output, present_value)?;
        builder.build_unconditional_branch(merge)?;
        builder.position_at_end(absent);
        builder.build_store(output, structure.const_zero())?;
        builder.build_unconditional_branch(merge)?;
        builder.position_at_end(merge);
        Ok(builder
            .build_load(structure, output, &format!("{name}.value"))?
            .into())
    }

    fn require_js_array(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        let is_array = builder.build_alloca(
            self.generator.context.i8_type(),
            &format!("{name}.is_array"),
        )?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_is_array",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[env.into(), value.into(), is_array.into()],
            &format!("{name}.is_array.get"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.is_array.status"),
        )?;
        let is_array = builder
            .build_load(
                self.generator.context.i8_type(),
                is_array,
                &format!("{name}.is_array.value"),
            )?
            .into_int_value();
        self.continue_if(
            builder,
            function,
            builder.build_int_compare(
                IntPredicate::NE,
                is_array,
                self.generator.context.i8_type().const_zero(),
                &format!("{name}.is_array.ok"),
            )?,
            error,
            &format!("{name}.is_array.valid"),
        )?;
        let length =
            builder.build_alloca(self.generator.context.i32_type(), &format!("{name}.length"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_array_length",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[env.into(), value.into(), length.into()],
            &format!("{name}.length.get"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.length.status"),
        )?;
        Ok(builder
            .build_load(
                self.generator.context.i32_type(),
                length,
                &format!("{name}.length.value"),
            )?
            .into_int_value())
    }

    fn convert_fixed_array_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        element: &tn_node_api::NodeType,
        length: usize,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let name = format!("node.fixed_array.{index}");
        let js_length = self.require_js_array(builder, function, env, value, error, &name)?;
        self.continue_if(
            builder,
            function,
            builder.build_int_compare(
                IntPredicate::EQ,
                js_length,
                self.generator
                    .context
                    .i32_type()
                    .const_int(u64::try_from(length).unwrap_or(u64::MAX), false),
                &format!("{name}.length.ok"),
            )?,
            error,
            &format!("{name}.length.valid"),
        )?;
        let array_type = self.generator.basic_type(&ty.native)?.into_array_type();
        let mut result = array_type.const_zero();
        for element_index in 0..length {
            let js_element = builder.build_alloca(
                self.pointer_type(),
                &format!("{name}.element.{element_index}"),
            )?;
            let status = self.call_status(
                builder,
                self.napi_status_function(
                    "napi_get_element",
                    &[
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                        self.size_type().into(),
                        self.pointer_type().into(),
                    ],
                ),
                &[
                    env.into(),
                    value.into(),
                    self.size_type()
                        .const_int(u64::try_from(element_index).unwrap_or(u64::MAX), false)
                        .into(),
                    js_element.into(),
                ],
                &format!("{name}.element.{element_index}.get"),
            )?;
            self.continue_if_status_ok(
                builder,
                function,
                status,
                error,
                &format!("{name}.element.{element_index}.status"),
            )?;
            let js_value = builder
                .build_load(
                    self.pointer_type(),
                    js_element,
                    &format!("{name}.element.{element_index}.value"),
                )?
                .into_pointer_value();
            let native = self.convert_argument(
                builder,
                function,
                env,
                js_value,
                element,
                error,
                element_index,
            )?;
            result = builder
                .build_insert_value(
                    result,
                    native,
                    u32::try_from(element_index).unwrap_or(u32::MAX),
                    &format!("{name}.element.{element_index}.store"),
                )?
                .into_array_value();
        }
        Ok(result.into())
    }

    fn convert_array_argument(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        value: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        element: &tn_node_api::NodeType,
        error: LlvmBlock<'ctx>,
        index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let name = format!("node.array.{index}");
        let length_i32 = self.require_js_array(builder, function, env, value, error, &name)?;
        let length = builder.build_int_z_extend(
            length_i32,
            self.size_type(),
            &format!("{name}.length.wide"),
        )?;
        let class_type = match &ty.native {
            Type::Reference { referent, .. } => referent.as_ref().clone(),
            native => native.clone(),
        };
        let object_type = self.generator.class_object_type(&class_type)?;
        let object_size = object_type
            .size_of()
            .ok_or_else(|| CodegenError::Unsupported("Node Array object has no size".into()))?;
        let object = self
            .call_value(
                builder,
                self.generator.runtime_alloc(),
                &[object_size.into()],
                &format!("{name}.object.alloc"),
            )?
            .into_pointer_value();
        self.continue_if(
            builder,
            function,
            builder.build_is_not_null(object, &format!("{name}.object.ok"))?,
            error,
            &format!("{name}.object.status"),
        )?;
        let element_type = self.generator.basic_type(&element.native)?;
        let element_size = element_type
            .size_of()
            .ok_or_else(|| CodegenError::Unsupported("Node Array element has no size".into()))?;
        let bytes = builder.build_int_mul(length, element_size, &format!("{name}.bytes"))?;
        let data = self
            .call_value(
                builder,
                self.generator.runtime_alloc(),
                &[bytes.into()],
                &format!("{name}.data.alloc"),
            )?
            .into_pointer_value();
        let initialized = self
            .call_value(
                builder,
                self.generator.runtime_alloc(),
                &[length.into()],
                &format!("{name}.initialized.alloc"),
            )?
            .into_pointer_value();
        let descriptor =
            builder.build_struct_gep(object_type, object, 0, &format!("{name}.descriptor"))?;
        builder.build_store(descriptor, self.pointer_type().const_null())?;
        let data_field =
            builder.build_struct_gep(object_type, object, 1, &format!("{name}.data"))?;
        builder.build_store(data_field, data)?;
        let initialized_field =
            builder.build_struct_gep(object_type, object, 2, &format!("{name}.initialized"))?;
        builder.build_store(initialized_field, initialized)?;
        let length_field =
            builder.build_struct_gep(object_type, object, 3, &format!("{name}.length"))?;
        builder.build_store(length_field, length)?;
        let capacity_field =
            builder.build_struct_gep(object_type, object, 4, &format!("{name}.capacity"))?;
        builder.build_store(capacity_field, length)?;
        let element_size_field =
            builder.build_struct_gep(object_type, object, 5, &format!("{name}.element_size"))?;
        builder.build_store(element_size_field, element_size)?;

        let loop_block = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.loop"));
        let body_block = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.body"));
        let done_block = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.done"));
        let pre_loop = builder
            .get_insert_block()
            .ok_or_else(|| CodegenError::Builder("Node Array pre-loop block is missing".into()))?;
        builder.build_unconditional_branch(loop_block)?;
        builder.position_at_end(loop_block);
        let phi = builder.build_phi(self.size_type(), &format!("{name}.index"))?;
        phi.add_incoming(&[(&self.size_type().const_zero(), pre_loop)]);
        let current = phi.as_basic_value().into_int_value();
        let condition = builder.build_int_compare(
            IntPredicate::ULT,
            current,
            length,
            &format!("{name}.condition"),
        )?;
        builder.build_conditional_branch(condition, body_block, done_block)?;
        builder.position_at_end(body_block);
        let js_element = builder.build_alloca(self.pointer_type(), &format!("{name}.element"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_element",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.size_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[env.into(), value.into(), current.into(), js_element.into()],
            &format!("{name}.element.get"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.element.status"),
        )?;
        let js_value = builder
            .build_load(
                self.pointer_type(),
                js_element,
                &format!("{name}.element.value"),
            )?
            .into_pointer_value();
        let native =
            self.convert_argument(builder, function, env, js_value, element, error, index)?;
        let element_address = unsafe {
            builder.build_gep(
                element_type,
                data,
                &[current],
                &format!("{name}.element.address"),
            )?
        };
        builder.build_store(element_address, native)?;
        let initialized_address = unsafe {
            builder.build_gep(
                self.generator.context.i8_type(),
                initialized,
                &[current],
                &format!("{name}.initialized.address"),
            )?
        };
        builder.build_store(
            initialized_address,
            self.generator.context.i8_type().const_int(1, false),
        )?;
        let next = builder.build_int_add(
            current,
            self.size_type().const_int(1, false),
            &format!("{name}.next"),
        )?;
        let body_end = builder
            .get_insert_block()
            .ok_or_else(|| CodegenError::Builder("Node Array body block is missing".into()))?;
        builder.build_unconditional_branch(loop_block)?;
        phi.add_incoming(&[(&next, body_end)]);
        builder.position_at_end(done_block);
        Ok(object.into())
    }

    fn emit_function_callback(
        &self,
        index: usize,
        function: &tn_node_api::BridgeFunction,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let callback = self.generator.module.add_function(
            &format!("tn_node_callback_{index}"),
            self.callback_type(),
            None,
        );
        self.attach_debug(callback, &format!("tn_node_callback_{index}"));
        let entry = self.generator.context.append_basic_block(callback, "entry");
        let error =
            self.append_error_block(callback, "TypeNative Node argument conversion failed")?;
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, callback);
        builder.position_at_end(entry);
        let env = callback
            .get_nth_param(0)
            .ok_or_else(|| CodegenError::Builder("Node callback environment is missing".into()))?
            .into_pointer_value();
        let info = callback
            .get_nth_param(1)
            .ok_or_else(|| CodegenError::Builder("Node callback info is missing".into()))?;
        let argc = builder.build_alloca(self.size_type(), "node.argc")?;
        builder.build_store(
            argc,
            self.size_type().const_int(
                u64::try_from(function.parameters.len()).unwrap_or(u64::MAX),
                false,
            ),
        )?;
        let argv = if function.parameters.is_empty() {
            self.pointer_type().const_null()
        } else {
            builder.build_array_alloca(
                self.pointer_type(),
                self.size_type().const_int(
                    u64::try_from(function.parameters.len()).unwrap_or(u64::MAX),
                    false,
                ),
                "node.argv",
            )?
        };
        let status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_get_cb_info",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                info.into(),
                argc.into(),
                argv.into(),
                self.pointer_type().const_null().into(),
                self.pointer_type().const_null().into(),
            ],
            "node.get_cb_info",
        )?;
        self.continue_if_status_ok(&builder, callback, status, error, "node.cb_info")?;
        let actual_argc = builder
            .build_load(self.size_type(), argc, "node.argc.value")?
            .into_int_value();
        let count_ok = builder.build_int_compare(
            IntPredicate::EQ,
            actual_argc,
            self.size_type().const_int(
                u64::try_from(function.parameters.len()).unwrap_or(u64::MAX),
                false,
            ),
            "node.argc.valid",
        )?;
        let count_next = self
            .generator
            .context
            .append_basic_block(callback, "node.argc.ok");
        builder.build_conditional_branch(count_ok, count_next, error)?;
        builder.position_at_end(count_next);

        let mut native_arguments = Vec::with_capacity(function.parameters.len());
        for (index, parameter) in function.parameters.iter().enumerate() {
            let argument = unsafe {
                builder.build_gep(
                    self.pointer_type(),
                    argv,
                    &[self
                        .size_type()
                        .const_int(u64::try_from(index).unwrap_or(u64::MAX), false)],
                    &format!("node.argv.{index}"),
                )?
            };
            let js_value = builder
                .build_load(self.pointer_type(), argument, &format!("node.arg.{index}"))?
                .into_pointer_value();
            let native =
                self.convert_argument(&builder, callback, env, js_value, parameter, error, index)?;
            if !function.signature.is_async && self.node_requires_indirect(&parameter.native) {
                let pointer = builder.build_alloca(
                    self.generator.basic_type(&parameter.native)?,
                    &format!("node.arg.indirect.{index}"),
                )?;
                builder.build_store(pointer, native)?;
                native_arguments.push(pointer.into());
            } else {
                native_arguments.push(native.into());
            }
        }
        let native_signature = self.native_signature(
            &function.signature,
            None,
            &function.result.native,
            function.signature.is_async,
        )?;
        let symbol = self.emitted_symbol(function.callable, &function.signature.effects);
        let native_function = self
            .generator
            .module
            .get_function(&symbol)
            .unwrap_or_else(|| {
                self.generator
                    .module
                    .add_function(&symbol, native_signature, None)
            });
        if function.signature.is_async {
            let native_result = self.call_value(
                &builder,
                native_function,
                &native_arguments,
                "node.async.native.call",
            )?;
            let native_promise = native_result.into_pointer_value();
            self.continue_if(
                &builder,
                callback,
                builder.build_is_not_null(native_promise, "node.async.promise.ok")?,
                error,
                "node.async.promise.valid",
            )?;
            let promise = self.emit_async_function_bridge(
                index,
                callback,
                &builder,
                env,
                function,
                native_promise,
            )?;
            builder.build_return(Some(&promise))?;
            return Ok(callback);
        }
        let mut native_result_pointer = None;
        let native_result = if function.signature.effects.is_empty()
            && function.result.kind == tn_node_api::NodeTypeKind::Void
        {
            builder.build_call(native_function, &native_arguments, "node.native.call")?;
            None
        } else {
            let result = self.call_value(
                &builder,
                native_function,
                &native_arguments,
                "node.native.call",
            )?;
            if function.signature.effects.is_empty()
                && self.node_requires_indirect(&function.result.native)
            {
                let pointer = result.into_pointer_value();
                let loaded = builder.build_load(
                    self.generator.basic_type(&function.result.native)?,
                    pointer,
                    "node.native.indirect.result",
                )?;
                native_result_pointer = Some(pointer);
                Some(loaded)
            } else {
                Some(result)
            }
        };
        let result =
            self.convert_result(&builder, callback, env, function, native_result, error)?;
        if function.signature.effects.is_empty()
            && let Some(native_result) = native_result
        {
            self.drop_node_value(
                &builder,
                callback,
                &function.result,
                native_result,
                "node.native.result.drop",
            )?;
        }
        if let Some(pointer) = native_result_pointer {
            builder.build_call(
                self.generator.runtime_free(),
                &[pointer.into()],
                "node.native.indirect.free",
            )?;
        }
        builder.build_return(Some(&result))?;
        Ok(callback)
    }

    fn async_context_type(&self) -> StructType<'ctx> {
        self.generator.context.struct_type(
            &[
                self.pointer_type().into(),
                self.pointer_type().into(),
                self.pointer_type().into(),
                self.pointer_type().into(),
                self.status_type().into(),
            ],
            false,
        )
    }

    fn async_execute_type(&self) -> LlvmFunctionType<'ctx> {
        self.generator.context.void_type().fn_type(
            &[self.pointer_type().into(), self.pointer_type().into()],
            false,
        )
    }

    fn async_complete_type(&self) -> LlvmFunctionType<'ctx> {
        self.generator.context.void_type().fn_type(
            &[
                self.pointer_type().into(),
                self.status_type().into(),
                self.pointer_type().into(),
            ],
            false,
        )
    }

    fn emit_async_function_bridge(
        &self,
        index: usize,
        callback: FunctionValue<'ctx>,
        builder: &Builder<'ctx>,
        env: PointerValue<'ctx>,
        function: &tn_node_api::BridgeFunction,
        native_promise: PointerValue<'ctx>,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let context_type = self.async_context_type();
        let execute = self.generator.module.add_function(
            &format!("tn_node_async_execute_{index}"),
            self.async_execute_type(),
            None,
        );
        let complete = self.generator.module.add_function(
            &format!("tn_node_async_complete_{index}"),
            self.async_complete_type(),
            None,
        );
        self.attach_debug(execute, &format!("tn_node_async_execute_{index}"));
        self.attach_debug(complete, &format!("tn_node_async_complete_{index}"));
        self.emit_async_execute(execute, context_type)?;

        self.emit_async_complete(complete, context_type, function)?;

        let promise_slot = builder.build_alloca(self.pointer_type(), "node.async.promise")?;
        let deferred_slot = builder.build_alloca(self.pointer_type(), "node.async.deferred")?;
        let create_status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_create_promise",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[env.into(), deferred_slot.into(), promise_slot.into()],
            "node.async.promise.create",
        )?;
        let promise_ok = self
            .generator
            .context
            .append_basic_block(callback, "node.async.promise.created");
        let promise_failure = self
            .generator
            .context
            .append_basic_block(callback, "node.async.promise.failure");
        builder.build_conditional_branch(
            builder.build_int_compare(
                IntPredicate::EQ,
                create_status,
                self.status_type().const_zero(),
                "node.async.promise.status",
            )?,
            promise_ok,
            promise_failure,
        )?;
        builder.position_at_end(promise_failure);
        self.emit_async_start_failure(
            &builder,
            env,
            native_promise,
            None,
            "TypeNative async promise creation failed",
        )?;

        builder.position_at_end(promise_ok);
        let context_size = context_type
            .size_of()
            .ok_or_else(|| CodegenError::Unsupported("Node async context has no size".into()))?;
        let context = self
            .call_value(
                &builder,
                self.generator.runtime_alloc(),
                &[context_size.into()],
                "node.async.context.alloc",
            )?
            .into_pointer_value();
        let context_ok = self
            .generator
            .context
            .append_basic_block(callback, "node.async.context.created");
        let context_failure = self
            .generator
            .context
            .append_basic_block(callback, "node.async.context.failure");
        builder.build_conditional_branch(
            builder.build_is_not_null(context, "node.async.context.status")?,
            context_ok,
            context_failure,
        )?;
        builder.position_at_end(context_failure);
        self.emit_async_start_failure(
            &builder,
            env,
            native_promise,
            None,
            "TypeNative async context allocation failed",
        )?;

        builder.position_at_end(context_ok);
        let env_field = builder.build_struct_gep(context_type, context, 0, "node.async.env")?;
        builder.build_store(env_field, env)?;
        let deferred = builder
            .build_load(
                self.pointer_type(),
                deferred_slot,
                "node.async.deferred.value",
            )?
            .into_pointer_value();
        let deferred_field =
            builder.build_struct_gep(context_type, context, 1, "node.async.deferred.field")?;
        builder.build_store(deferred_field, deferred)?;
        let work_field = builder.build_struct_gep(context_type, context, 2, "node.async.work")?;
        let native_field =
            builder.build_struct_gep(context_type, context, 3, "node.async.native")?;
        builder.build_store(native_field, native_promise)?;
        let status_field =
            builder.build_struct_gep(context_type, context, 4, "node.async.wait.status")?;
        builder.build_store(
            status_field,
            self.status_type().const_int((-1_i64).cast_unsigned(), true),
        )?;

        let resource_name = self.c_string(&builder, "TypeNative async", "node.async.resource")?;
        let resource_slot =
            builder.build_alloca(self.pointer_type(), "node.async.resource.value")?;
        let resource_status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_create_string_utf8",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.size_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                resource_name.into(),
                self.size_type().const_int(16, false).into(),
                resource_slot.into(),
            ],
            "node.async.resource.create",
        )?;
        let resource_ok = self
            .generator
            .context
            .append_basic_block(callback, "node.async.resource.created");
        let resource_failure = self
            .generator
            .context
            .append_basic_block(callback, "node.async.resource.failure");
        builder.build_conditional_branch(
            builder.build_int_compare(
                IntPredicate::EQ,
                resource_status,
                self.status_type().const_zero(),
                "node.async.resource.status",
            )?,
            resource_ok,
            resource_failure,
        )?;
        builder.position_at_end(resource_failure);
        self.emit_async_start_failure(
            &builder,
            env,
            native_promise,
            Some(context),
            "TypeNative async resource creation failed",
        )?;

        builder.position_at_end(resource_ok);
        let work_slot = builder.build_alloca(self.pointer_type(), "node.async.work.slot")?;
        let create_work_status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_create_async_work",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                self.pointer_type().const_null().into(),
                builder
                    .build_load(
                        self.pointer_type(),
                        resource_slot,
                        "node.async.resource.value",
                    )?
                    .into(),
                execute.as_global_value().as_pointer_value().into(),
                complete.as_global_value().as_pointer_value().into(),
                context.into(),
                work_slot.into(),
            ],
            "node.async.work.create",
        )?;
        let work_ok = self
            .generator
            .context
            .append_basic_block(callback, "node.async.work.created");
        let work_failure = self
            .generator
            .context
            .append_basic_block(callback, "node.async.work.failure");
        builder.build_conditional_branch(
            builder.build_int_compare(
                IntPredicate::EQ,
                create_work_status,
                self.status_type().const_zero(),
                "node.async.work.status",
            )?,
            work_ok,
            work_failure,
        )?;
        builder.position_at_end(work_failure);
        self.emit_async_start_failure(
            &builder,
            env,
            native_promise,
            Some(context),
            "TypeNative async work creation failed",
        )?;

        builder.position_at_end(work_ok);
        let work = builder
            .build_load(self.pointer_type(), work_slot, "node.async.work.value")?
            .into_pointer_value();
        builder.build_store(work_field, work)?;
        let queue_status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_queue_async_work",
                &[self.pointer_type().into(), self.pointer_type().into()],
            ),
            &[env.into(), work.into()],
            "node.async.work.queue",
        )?;
        let queue_ok = self
            .generator
            .context
            .append_basic_block(callback, "node.async.work.queued");
        let queue_failure = self
            .generator
            .context
            .append_basic_block(callback, "node.async.work.queue.failure");
        builder.build_conditional_branch(
            builder.build_int_compare(
                IntPredicate::EQ,
                queue_status,
                self.status_type().const_zero(),
                "node.async.work.queue.status",
            )?,
            queue_ok,
            queue_failure,
        )?;
        builder.position_at_end(queue_failure);
        builder.build_call(
            self.napi_status_function(
                "napi_delete_async_work",
                &[self.pointer_type().into(), self.pointer_type().into()],
            ),
            &[env.into(), work.into()],
            "node.async.work.delete",
        )?;
        self.emit_async_start_failure(
            &builder,
            env,
            native_promise,
            Some(context),
            "TypeNative async work queue failed",
        )?;

        builder.position_at_end(queue_ok);
        Ok(builder
            .build_load(
                self.pointer_type(),
                promise_slot,
                "node.async.promise.value",
            )?
            .into_pointer_value())
    }

    fn emit_async_start_failure(
        &self,
        builder: &Builder<'ctx>,
        env: PointerValue<'ctx>,
        native_promise: PointerValue<'ctx>,
        context: Option<PointerValue<'ctx>>,
        message: &str,
    ) -> Result<(), CodegenError> {
        if let Some(context) = context {
            builder.build_call(
                self.generator.runtime_free(),
                &[context.into()],
                "node.async.start.context.free",
            )?;
        }
        builder.build_call(
            self.generator.runtime_async_destroy(),
            &[native_promise.into()],
            "node.async.start.promise.destroy",
        )?;
        let message = self.c_string(builder, message, "node.async.start.error")?;
        builder.build_call(
            self.napi_status_function(
                "napi_throw_error",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                self.pointer_type().const_null().into(),
                message.into(),
            ],
            "node.async.start.throw",
        )?;
        builder.build_return(Some(&self.pointer_type().const_null()))?;
        Ok(())
    }

    fn emit_async_execute(
        &self,
        function: FunctionValue<'ctx>,
        context_type: StructType<'ctx>,
    ) -> Result<(), CodegenError> {
        let entry = self.generator.context.append_basic_block(function, "entry");
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, function);
        builder.position_at_end(entry);
        let context = function
            .get_nth_param(1)
            .ok_or_else(|| CodegenError::Builder("Node async context is missing".into()))?
            .into_pointer_value();
        let native_field = builder.build_struct_gep(context_type, context, 3, "async.native")?;
        let native = builder
            .build_load(self.pointer_type(), native_field, "async.native.value")?
            .into_pointer_value();
        let status = self.call_value(
            &builder,
            self.generator.runtime_async_wait(),
            &[native.into()],
            "async.wait",
        )?;
        let status = status.into_int_value();
        let status_field = builder.build_struct_gep(context_type, context, 4, "async.status")?;
        builder.build_store(status_field, status)?;
        builder.build_return(None)?;
        Ok(())
    }

    fn emit_async_complete(
        &self,
        function: FunctionValue<'ctx>,
        context_type: StructType<'ctx>,
        export: &tn_node_api::BridgeFunction,
    ) -> Result<(), CodegenError> {
        let entry = self.generator.context.append_basic_block(function, "entry");
        let ready = self
            .generator
            .context
            .append_basic_block(function, "node.async.ready");
        let rejected = self
            .generator
            .context
            .append_basic_block(function, "node.async.rejected");
        let cleanup = self
            .generator
            .context
            .append_basic_block(function, "node.async.cleanup");
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, function);
        builder.position_at_end(entry);
        let env = function
            .get_nth_param(0)
            .ok_or_else(|| CodegenError::Builder("Node async environment is missing".into()))?
            .into_pointer_value();
        let napi_status = function
            .get_nth_param(1)
            .ok_or_else(|| CodegenError::Builder("Node async completion status is missing".into()))?
            .into_int_value();
        let context = function
            .get_nth_param(2)
            .ok_or_else(|| {
                CodegenError::Builder("Node async completion context is missing".into())
            })?
            .into_pointer_value();
        let wait_status = builder
            .build_load(
                self.status_type(),
                builder.build_struct_gep(context_type, context, 4, "async.status")?,
                "async.status.value",
            )?
            .into_int_value();
        let status_ok = builder.build_and(
            builder.build_int_compare(
                IntPredicate::EQ,
                napi_status,
                self.status_type().const_zero(),
                "async.napi.status.ok",
            )?,
            builder.build_int_compare(
                IntPredicate::EQ,
                wait_status,
                self.status_type().const_zero(),
                "async.wait.status.ok",
            )?,
            "async.status.ok",
        )?;
        builder.build_conditional_branch(status_ok, ready, rejected)?;

        builder.position_at_end(rejected);
        self.emit_async_rejection(
            &builder,
            env,
            context_type,
            context,
            "TypeNative async work failed",
        )?;
        builder.build_unconditional_branch(cleanup)?;

        builder.position_at_end(ready);
        let deferred = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(context_type, context, 1, "async.deferred")?,
                "async.deferred.value",
            )?
            .into_pointer_value();
        let native = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(context_type, context, 3, "async.native")?,
                "async.native.value",
            )?
            .into_pointer_value();
        let conversion_error = self
            .generator
            .context
            .append_basic_block(function, "node.async.conversion.error");
        let result = match &export.result.kind {
            tn_node_api::NodeTypeKind::Promise { result, errors } => {
                if errors.is_empty() {
                    let result_pointer = self
                        .call_value(
                            &builder,
                            self.generator.runtime_async_result(),
                            &[native.into()],
                            "async.result",
                        )?
                        .into_pointer_value();
                    self.continue_if(
                        &builder,
                        function,
                        builder.build_is_not_null(result_pointer, "async.result.valid")?,
                        conversion_error,
                        "async.result.status",
                    )?;
                    let value = builder.build_load(
                        self.generator.basic_type(&result.native)?,
                        result_pointer,
                        "async.result.value",
                    )?;
                    let output = builder.build_alloca(self.pointer_type(), "async.js.result")?;
                    self.convert_result_value(
                        &builder,
                        function,
                        env,
                        result,
                        value,
                        output,
                        conversion_error,
                        "async.result.convert",
                    )?;
                    Some(
                        builder
                            .build_load(self.pointer_type(), output, "async.js.result.value")?
                            .into_pointer_value(),
                    )
                } else {
                    let result_pointer = self
                        .call_value(
                            &builder,
                            self.generator.runtime_async_raw_result(),
                            &[native.into()],
                            "async.raw.result",
                        )?
                        .into_pointer_value();
                    self.continue_if(
                        &builder,
                        function,
                        builder.build_is_not_null(result_pointer, "async.raw.result.valid")?,
                        conversion_error,
                        "async.raw.result.status",
                    )?;
                    let completion = self.generator.completion_type(&result.native)?;
                    let completion = builder
                        .build_load(completion, result_pointer, "async.completion")?
                        .into_struct_value();
                    let failed = builder
                        .build_extract_value(completion, 0, "async.failed")?
                        .into_int_value();
                    let success = self
                        .generator
                        .context
                        .append_basic_block(function, "async.success");
                    let failure = self
                        .generator
                        .context
                        .append_basic_block(function, "async.failure");
                    builder.build_conditional_branch(
                        builder.build_int_compare(
                            IntPredicate::EQ,
                            failed,
                            self.generator.context.i8_type().const_zero(),
                            "async.failed.test",
                        )?,
                        success,
                        failure,
                    )?;
                    builder.position_at_end(failure);
                    let error_index = if result.native == Type::Primitive(PrimitiveType::Void) {
                        1
                    } else {
                        2
                    };
                    let error_pointer = builder
                        .build_extract_value(completion, error_index, "async.error.pointer")?
                        .into_pointer_value();
                    let error_conversion_failure = self
                        .generator
                        .context
                        .append_basic_block(function, "async.error.conversion.failure");
                    let error = self.emit_node_error_object(
                        &builder,
                        function,
                        env,
                        error_pointer,
                        errors,
                        error_conversion_failure,
                        "async.error",
                    )?;
                    self.emit_async_rejection_object(&builder, env, context_type, context, error)?;
                    self.release_node_error(
                        &builder,
                        function,
                        error_pointer,
                        errors,
                        "async.error.release",
                    )?;
                    builder.build_unconditional_branch(cleanup)?;

                    builder.position_at_end(error_conversion_failure);
                    self.emit_async_rejection(
                        &builder,
                        env,
                        context_type,
                        context,
                        "TypeNative asynchronous error conversion failed",
                    )?;
                    self.release_node_error(
                        &builder,
                        function,
                        error_pointer,
                        errors,
                        "async.error.conversion.release",
                    )?;
                    builder.build_unconditional_branch(cleanup)?;

                    builder.position_at_end(success);
                    let output = builder.build_alloca(self.pointer_type(), "async.js.result")?;
                    if result.native == Type::Primitive(PrimitiveType::Void) {
                        self.convert_result_value(
                            &builder,
                            function,
                            env,
                            result,
                            self.generator.context.i8_type().const_zero().into(),
                            output,
                            conversion_error,
                            "async.result.convert",
                        )?;
                    } else {
                        let value = builder.build_extract_value(completion, 1, "async.value")?;
                        self.convert_result_value(
                            &builder,
                            function,
                            env,
                            result,
                            value,
                            output,
                            conversion_error,
                            "async.result.convert",
                        )?;
                    }
                    Some(
                        builder
                            .build_load(self.pointer_type(), output, "async.js.result.value")?
                            .into_pointer_value(),
                    )
                }
            }
            _ => {
                return Err(CodegenError::Unsupported(
                    "async Node export result is not a Promise".into(),
                ));
            }
        };
        if let Some(result) = result {
            builder.build_call(
                self.napi_status_function(
                    "napi_resolve_deferred",
                    &[
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                    ],
                ),
                &[env.into(), deferred.into(), result.into()],
                "async.resolve",
            )?;
        }
        builder.build_unconditional_branch(cleanup)?;

        builder.position_at_end(conversion_error);
        self.emit_async_rejection(
            &builder,
            env,
            context_type,
            context,
            "TypeNative asynchronous result conversion failed",
        )?;
        builder.build_unconditional_branch(cleanup)?;

        builder.position_at_end(cleanup);
        let work = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(context_type, context, 2, "async.work")?,
                "async.work.value",
            )?
            .into_pointer_value();
        builder.build_call(
            self.napi_status_function(
                "napi_delete_async_work",
                &[self.pointer_type().into(), self.pointer_type().into()],
            ),
            &[env.into(), work.into()],
            "async.work.delete",
        )?;
        let cleanup_native = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(context_type, context, 3, "async.cleanup.native")?,
                "async.cleanup.native.value",
            )?
            .into_pointer_value();
        builder.build_call(
            self.generator.runtime_async_destroy(),
            &[cleanup_native.into()],
            "async.promise.destroy",
        )?;
        builder.build_call(
            self.generator.runtime_free(),
            &[context.into()],
            "async.context.free",
        )?;
        builder.build_return(None)?;
        Ok(())
    }

    fn emit_async_rejection(
        &self,
        builder: &Builder<'ctx>,
        env: PointerValue<'ctx>,
        context_type: StructType<'ctx>,
        context: PointerValue<'ctx>,
        message: &str,
    ) -> Result<(), CodegenError> {
        let message_value = self.c_string(builder, message, "async.reject.message")?;
        let message_slot = builder.build_alloca(self.pointer_type(), "async.reject.message")?;
        self.call_status(
            builder,
            self.napi_status_function(
                "napi_create_string_utf8",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.size_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                message_value.into(),
                self.size_type()
                    .const_int(u64::try_from(message.len()).unwrap_or(u64::MAX), false)
                    .into(),
                message_slot.into(),
            ],
            "async.reject.message.create",
        )?;
        let error = builder.build_alloca(self.pointer_type(), "async.reject.error")?;
        builder.build_call(
            self.napi_status_function(
                "napi_create_error",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                self.pointer_type().const_null().into(),
                builder
                    .build_load(
                        self.pointer_type(),
                        message_slot,
                        "async.reject.message.value",
                    )?
                    .into(),
                error.into(),
            ],
            "async.reject.error.create",
        )?;
        let error = builder
            .build_load(self.pointer_type(), error, "async.reject.error.value")?
            .into_pointer_value();
        self.emit_async_rejection_object(builder, env, context_type, context, error)?;
        Ok(())
    }

    fn emit_async_rejection_object(
        &self,
        builder: &Builder<'ctx>,
        env: PointerValue<'ctx>,
        context_type: StructType<'ctx>,
        context: PointerValue<'ctx>,
        error: PointerValue<'ctx>,
    ) -> Result<(), CodegenError> {
        let deferred = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(context_type, context, 1, "async.reject.deferred")?,
                "async.reject.deferred.value",
            )?
            .into_pointer_value();
        builder.build_call(
            self.napi_status_function(
                "napi_reject_deferred",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[env.into(), deferred.into(), error.into()],
            "async.reject",
        )?;
        Ok(())
    }

    fn emitted_symbol(&self, callable: Callable, effects: &[DeclarationId]) -> String {
        self.emitted_instance_symbol(&Instance {
            callable,
            type_arguments: Vec::new(),
            effects: effects.to_vec(),
        })
    }

    fn emitted_instance_symbol(&self, instance: &Instance) -> String {
        self.generator
            .layouts
            .export_instances
            .get(instance)
            .cloned()
            .or_else(|| {
                if instance.type_arguments.is_empty() {
                    self.generator
                        .layouts
                        .exports
                        .get(&instance.callable)
                        .cloned()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| symbol_for_instance(instance))
    }

    fn emit_class(
        &self,
        index: usize,
        class: &tn_node_api::BridgeClass,
    ) -> Result<NodeClassCallbacks<'ctx>, CodegenError> {
        let finalizer = self.generator.module.add_function(
            &format!("tn_node_class_finalizer_{index}"),
            self.finalize_type(),
            None,
        );
        self.attach_debug(finalizer, &format!("tn_node_class_finalizer_{index}"));
        self.emit_class_finalizer(finalizer, class)?;
        let constructor = self.generator.module.add_function(
            &format!("tn_node_class_constructor_{index}"),
            self.callback_type(),
            None,
        );
        self.attach_debug(constructor, &format!("tn_node_class_constructor_{index}"));
        self.emit_class_constructor(constructor, class, finalizer)?;
        let mut methods = Vec::new();
        for (method_index, method) in class.methods.iter().enumerate() {
            if method.name == "[Symbol.dispose]" {
                continue;
            }
            let callback = self.emit_class_method(index, method_index, class, method)?;
            methods.push((method.name.clone(), callback, method.receiver));
        }
        Ok(NodeClassCallbacks {
            constructor,
            methods,
        })
    }

    fn emit_class_finalizer(
        &self,
        function: FunctionValue<'ctx>,
        class: &tn_node_api::BridgeClass,
    ) -> Result<(), CodegenError> {
        let entry = self.generator.context.append_basic_block(function, "entry");
        let has_data = self
            .generator
            .context
            .append_basic_block(function, "has_data");
        let done = self.generator.context.append_basic_block(function, "done");
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, function);
        builder.position_at_end(entry);
        let data = function
            .get_nth_param(1)
            .ok_or_else(|| CodegenError::Builder("Node finalizer data is missing".into()))?
            .into_pointer_value();
        let present = builder.build_is_not_null(data, "node.finalizer.data")?;
        builder.build_conditional_branch(present, has_data, done)?;
        builder.position_at_end(has_data);
        if let Some(drop) = class.drop {
            let symbol = self.emitted_symbol(drop, &[]);
            let drop_type = self
                .generator
                .context
                .void_type()
                .fn_type(&[self.pointer_type().into()], false);
            let function_value = self
                .generator
                .module
                .get_function(&symbol)
                .unwrap_or_else(|| self.generator.module.add_function(&symbol, drop_type, None));
            builder.build_call(function_value, &[data.into()], "node.finalizer.drop")?;
        }
        builder.build_call(
            self.generator.runtime_free(),
            &[data.into()],
            "node.finalizer.free",
        )?;
        builder.build_unconditional_branch(done)?;
        builder.position_at_end(done);
        builder.build_return(None)?;
        Ok(())
    }

    fn emit_class_constructor(
        &self,
        function: FunctionValue<'ctx>,
        class: &tn_node_api::BridgeClass,
        finalizer: FunctionValue<'ctx>,
    ) -> Result<(), CodegenError> {
        let method = class.constructor.as_ref();
        let parameters = method.map_or(&[][..], |method| method.parameters.as_slice());
        let mut signature = method.map_or_else(
            || FunctionType {
                parameters: Vec::new(),
                result: Box::new(Type::Nominal(class.declaration, Vec::new())),
                effects: Vec::new(),
                generics: Vec::new(),
                is_async: false,
                is_unsafe: false,
            },
            |method| method.signature.clone(),
        );
        signature.result = Box::new(Type::Nominal(class.declaration, Vec::new()));
        let entry = self.generator.context.append_basic_block(function, "entry");
        let error = self.append_error_block(function, "TypeNative Node constructor failed")?;
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, function);
        builder.position_at_end(entry);
        let env = function
            .get_nth_param(0)
            .ok_or_else(|| CodegenError::Builder("Node constructor environment is missing".into()))?
            .into_pointer_value();
        let info = function.get_nth_param(1).ok_or_else(|| {
            CodegenError::Builder("Node constructor callback info is missing".into())
        })?;
        let argc = builder.build_alloca(self.size_type(), "node.constructor.argc")?;
        builder.build_store(
            argc,
            self.size_type()
                .const_int(u64::try_from(parameters.len()).unwrap_or(u64::MAX), false),
        )?;
        let argv = if parameters.is_empty() {
            self.pointer_type().const_null()
        } else {
            builder.build_array_alloca(
                self.pointer_type(),
                self.size_type()
                    .const_int(u64::try_from(parameters.len()).unwrap_or(u64::MAX), false),
                "node.constructor.argv",
            )?
        };
        let this_arg = builder.build_alloca(self.pointer_type(), "node.constructor.this")?;
        let status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_get_cb_info",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                info.into(),
                argc.into(),
                argv.into(),
                this_arg.into(),
                self.pointer_type().const_null().into(),
            ],
            "node.constructor.cb_info",
        )?;
        self.continue_if_status_ok(
            &builder,
            function,
            status,
            error,
            "node.constructor.cb_info.status",
        )?;
        let actual_argc = builder
            .build_load(self.size_type(), argc, "node.constructor.argc.value")?
            .into_int_value();
        self.continue_if(
            &builder,
            function,
            builder.build_int_compare(
                IntPredicate::EQ,
                actual_argc,
                self.size_type()
                    .const_int(u64::try_from(parameters.len()).unwrap_or(u64::MAX), false),
                "node.constructor.argc.ok",
            )?,
            error,
            "node.constructor.argc.valid",
        )?;
        let mut native_arguments = Vec::with_capacity(parameters.len());
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = unsafe {
                builder.build_gep(
                    self.pointer_type(),
                    argv,
                    &[self
                        .size_type()
                        .const_int(u64::try_from(index).unwrap_or(u64::MAX), false)],
                    &format!("node.constructor.argv.{index}"),
                )?
            };
            let js_value = builder
                .build_load(
                    self.pointer_type(),
                    argument,
                    &format!("node.constructor.arg.{index}"),
                )?
                .into_pointer_value();
            let native =
                self.convert_argument(&builder, function, env, js_value, parameter, error, index)?;
            if self.node_requires_indirect(&parameter.native) {
                let pointer = builder.build_alloca(
                    self.generator.basic_type(&parameter.native)?,
                    &format!("node.constructor.indirect.{index}"),
                )?;
                builder.build_store(pointer, native)?;
                native_arguments.push(pointer.into());
            } else {
                native_arguments.push(native.into());
            }
        }
        let signature_type = self.native_signature(&signature, None, &signature.result, false)?;
        let symbol = method.map_or_else(
            || format!("tn_ctor_{}_0_0", class.declaration.0),
            |method| symbol_for_constructor(class.declaration, method.callable.member, &signature),
        );
        let native_function = self
            .generator
            .module
            .get_function(&symbol)
            .unwrap_or_else(|| {
                self.generator
                    .module
                    .add_function(&symbol, signature_type, None)
            });
        let native = self.call_value(
            &builder,
            native_function,
            &native_arguments,
            "node.constructor.call",
        )?;
        let native = if signature.effects.is_empty() {
            native.into_pointer_value()
        } else {
            let packed = native.into_array_value();
            let failed = builder
                .build_extract_value(packed, 0, "node.constructor.failed")?
                .into_int_value();
            let payload = builder
                .build_extract_value(packed, 1, "node.constructor.payload")?
                .into_int_value();
            let success = self
                .generator
                .context
                .append_basic_block(function, "node.constructor.success");
            let failure = self
                .generator
                .context
                .append_basic_block(function, "node.constructor.failure");
            builder.build_conditional_branch(
                builder.build_int_compare(
                    IntPredicate::EQ,
                    failed,
                    self.generator.context.i64_type().const_zero(),
                    "node.constructor.ok",
                )?,
                success,
                failure,
            )?;
            builder.position_at_end(failure);
            let error_pointer =
                builder.build_int_to_ptr(payload, self.pointer_type(), "node.constructor.error")?;
            builder.build_call(
                self.generator.runtime_free(),
                &[error_pointer.into()],
                "node.constructor.error.free",
            )?;
            builder.build_unconditional_branch(error)?;
            builder.position_at_end(success);
            builder.build_int_to_ptr(payload, self.pointer_type(), "node.constructor.value")?
        };
        let this_value = builder
            .build_load(self.pointer_type(), this_arg, "node.constructor.this.value")?
            .into_pointer_value();
        let status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_wrap",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                this_value.into(),
                native.into(),
                finalizer.as_global_value().as_pointer_value().into(),
                self.pointer_type().const_null().into(),
                self.pointer_type().const_null().into(),
            ],
            "node.constructor.wrap",
        )?;
        self.continue_if_status_ok(
            &builder,
            function,
            status,
            error,
            "node.constructor.wrap.status",
        )?;
        builder.build_return(Some(&this_value))?;
        Ok(())
    }

    fn emit_class_method(
        &self,
        class_index: usize,
        method_index: usize,
        class: &tn_node_api::BridgeClass,
        method: &tn_node_api::BridgeMethod,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let callback = self.generator.module.add_function(
            &format!("tn_node_class_method_{class_index}_{method_index}"),
            self.callback_type(),
            None,
        );
        self.attach_debug(
            callback,
            &format!("tn_node_class_method_{class_index}_{method_index}"),
        );
        if method.signature.is_async {
            return Err(CodegenError::Unsupported(format!(
                "async Node class method `{}` is not yet lowered",
                method.name
            )));
        }
        let entry = self.generator.context.append_basic_block(callback, "entry");
        let error = self.append_error_block(callback, "TypeNative Node method failed")?;
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, callback);
        builder.position_at_end(entry);
        let env = callback
            .get_nth_param(0)
            .ok_or_else(|| CodegenError::Builder("Node method environment is missing".into()))?
            .into_pointer_value();
        let info = callback
            .get_nth_param(1)
            .ok_or_else(|| CodegenError::Builder("Node method callback info is missing".into()))?;
        let argc = builder.build_alloca(self.size_type(), "node.method.argc")?;
        builder.build_store(
            argc,
            self.size_type().const_int(
                u64::try_from(method.parameters.len()).unwrap_or(u64::MAX),
                false,
            ),
        )?;
        let argv = if method.parameters.is_empty() {
            self.pointer_type().const_null()
        } else {
            builder.build_array_alloca(
                self.pointer_type(),
                self.size_type().const_int(
                    u64::try_from(method.parameters.len()).unwrap_or(u64::MAX),
                    false,
                ),
                "node.method.argv",
            )?
        };
        let this_arg = builder.build_alloca(self.pointer_type(), "node.method.this")?;
        let status = self.call_status(
            &builder,
            self.napi_status_function(
                "napi_get_cb_info",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                info.into(),
                argc.into(),
                argv.into(),
                this_arg.into(),
                self.pointer_type().const_null().into(),
            ],
            "node.method.cb_info",
        )?;
        self.continue_if_status_ok(
            &builder,
            callback,
            status,
            error,
            "node.method.cb_info.status",
        )?;
        let actual_argc = builder
            .build_load(self.size_type(), argc, "node.method.argc.value")?
            .into_int_value();
        self.continue_if(
            &builder,
            callback,
            builder.build_int_compare(
                IntPredicate::EQ,
                actual_argc,
                self.size_type().const_int(
                    u64::try_from(method.parameters.len()).unwrap_or(u64::MAX),
                    false,
                ),
                "node.method.argc.ok",
            )?,
            error,
            "node.method.argc.valid",
        )?;
        let mut native_arguments = Vec::with_capacity(method.parameters.len() + 1);
        if method.receiver != tn_hir::ReceiverMode::Static {
            let receiver = builder.build_alloca(self.pointer_type(), "node.method.receiver")?;
            let status = self.call_status(
                &builder,
                self.napi_status_function(
                    "napi_unwrap",
                    &[
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                    ],
                ),
                &[
                    env.into(),
                    builder
                        .build_load(self.pointer_type(), this_arg, "node.method.this.value")?
                        .into(),
                    receiver.into(),
                ],
                "node.method.unwrap",
            )?;
            self.continue_if_status_ok(
                &builder,
                callback,
                status,
                error,
                "node.method.unwrap.status",
            )?;
            let receiver = builder
                .build_load(self.pointer_type(), receiver, "node.method.receiver.value")?
                .into_pointer_value();
            self.continue_if(
                &builder,
                callback,
                builder.build_is_not_null(receiver, "node.method.receiver.valid")?,
                error,
                "node.method.receiver.status",
            )?;
            native_arguments.push(receiver.into());
        }
        for (index, parameter) in method.parameters.iter().enumerate() {
            let argument = unsafe {
                builder.build_gep(
                    self.pointer_type(),
                    argv,
                    &[self
                        .size_type()
                        .const_int(u64::try_from(index).unwrap_or(u64::MAX), false)],
                    &format!("node.method.argv.{index}"),
                )?
            };
            let js_value = builder
                .build_load(
                    self.pointer_type(),
                    argument,
                    &format!("node.method.arg.{index}"),
                )?
                .into_pointer_value();
            let native =
                self.convert_argument(&builder, callback, env, js_value, parameter, error, index)?;
            if self.node_requires_indirect(&parameter.native) {
                let pointer = builder.build_alloca(
                    self.generator.basic_type(&parameter.native)?,
                    &format!("node.method.indirect.{index}"),
                )?;
                builder.build_store(pointer, native)?;
                native_arguments.push(pointer.into());
            } else {
                native_arguments.push(native.into());
            }
        }
        let receiver_type = if method.receiver == tn_hir::ReceiverMode::Static {
            None
        } else {
            Some(Type::Nominal(class.declaration, Vec::new()))
        };
        let native_signature = self.native_signature(
            &method.signature,
            receiver_type.as_ref(),
            &method.result.native,
            false,
        )?;
        let symbol = self.emitted_symbol(method.callable, &method.signature.effects);
        let native_function = self
            .generator
            .module
            .get_function(&symbol)
            .unwrap_or_else(|| {
                self.generator
                    .module
                    .add_function(&symbol, native_signature, None)
            });
        let mut native_result_pointer = None;
        let native_result = if method.signature.effects.is_empty()
            && method.result.kind == tn_node_api::NodeTypeKind::Void
        {
            builder.build_call(native_function, &native_arguments, "node.method.call")?;
            None
        } else {
            let result = self.call_value(
                &builder,
                native_function,
                &native_arguments,
                "node.method.call",
            )?;
            if method.signature.effects.is_empty()
                && self.node_requires_indirect(&method.result.native)
            {
                let pointer = result.into_pointer_value();
                let loaded = builder.build_load(
                    self.generator.basic_type(&method.result.native)?,
                    pointer,
                    "node.method.indirect.result",
                )?;
                native_result_pointer = Some(pointer);
                Some(loaded)
            } else {
                Some(result)
            }
        };
        let bridge_function = tn_node_api::BridgeFunction {
            export_name: method.name.clone(),
            callable: method.callable,
            signature: method.signature.clone(),
            parameters: method.parameters.clone(),
            result: method.result.clone(),
            errors: method.errors.clone(),
        };
        let result = self.convert_result(
            &builder,
            callback,
            env,
            &bridge_function,
            native_result,
            error,
        )?;
        if let Some(pointer) = native_result_pointer {
            builder.build_call(
                self.generator.runtime_free(),
                &[pointer.into()],
                "node.method.indirect.free",
            )?;
        }
        builder.build_return(Some(&result))?;
        Ok(callback)
    }

    fn emit_module_initializer(
        &self,
        plan: &tn_node_api::BridgePlan,
        callbacks: &[FunctionValue<'ctx>],
        classes: &[NodeClassCallbacks<'ctx>],
    ) -> Result<(), CodegenError> {
        let pointer = self.pointer_type();
        let function = self.generator.module.add_function(
            "napi_register_module_v1",
            pointer.fn_type(&[pointer.into(), pointer.into()], false),
            None,
        );
        self.attach_debug(function, "napi_register_module_v1");
        let entry = self.generator.context.append_basic_block(function, "entry");
        let builder = self.generator.context.create_builder();
        self.set_debug_location(&builder, function);
        builder.position_at_end(entry);
        let env = function
            .get_nth_param(0)
            .ok_or_else(|| CodegenError::Builder("Node module environment is missing".into()))?
            .into_pointer_value();
        let exports = function
            .get_nth_param(1)
            .ok_or_else(|| {
                CodegenError::Builder("Node module exports parameter is missing".into())
            })?
            .into_pointer_value();
        for (index, export) in plan.functions.iter().enumerate() {
            let name = self.c_string(
                &builder,
                &export.export_name,
                &format!("node.export.{index}"),
            )?;
            let created = builder.build_alloca(pointer, &format!("node.export.value.{index}"))?;
            let status = self.call_status(
                &builder,
                self.napi_status_function(
                    "napi_create_function",
                    &[
                        pointer.into(),
                        pointer.into(),
                        self.size_type().into(),
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    name.into(),
                    self.size_type()
                        .const_int(
                            u64::try_from(export.export_name.len()).unwrap_or(u64::MAX),
                            false,
                        )
                        .into(),
                    callbacks[index].as_global_value().as_pointer_value().into(),
                    pointer.const_null().into(),
                    created.into(),
                ],
                &format!("node.export.create.{index}"),
            )?;
            let ok = builder.build_int_compare(
                IntPredicate::EQ,
                status,
                self.status_type().const_zero(),
                &format!("node.export.status.{index}"),
            )?;
            let next = self
                .generator
                .context
                .append_basic_block(function, &format!("node.export.next.{index}"));
            let failure = self
                .generator
                .context
                .append_basic_block(function, &format!("node.export.failure.{index}"));
            builder.build_conditional_branch(ok, next, failure)?;
            builder.position_at_end(failure);
            builder.build_return(Some(&pointer.const_null()))?;
            builder.position_at_end(next);
            let status = self.call_status(
                &builder,
                self.napi_status_function(
                    "napi_set_named_property",
                    &[
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    exports.into(),
                    name.into(),
                    builder
                        .build_load(pointer, created, &format!("node.export.load.{index}"))?
                        .into(),
                ],
                &format!("node.export.set.{index}"),
            )?;
            let ok = builder.build_int_compare(
                IntPredicate::EQ,
                status,
                self.status_type().const_zero(),
                &format!("node.export.set.status.{index}"),
            )?;
            let next = self
                .generator
                .context
                .append_basic_block(function, &format!("node.export.set.next.{index}"));
            let failure = self
                .generator
                .context
                .append_basic_block(function, &format!("node.export.set.failure.{index}"));
            builder.build_conditional_branch(ok, next, failure)?;
            builder.position_at_end(failure);
            builder.build_return(Some(&pointer.const_null()))?;
            builder.position_at_end(next);
        }
        for (index, class) in plan.classes.iter().enumerate() {
            let callback = classes[index]
                .constructor
                .as_global_value()
                .as_pointer_value();
            let name =
                self.c_string(&builder, &class.export_name, &format!("node.class.{index}"))?;
            let class_value =
                builder.build_alloca(pointer, &format!("node.class.value.{index}"))?;
            let status = self.call_status(
                &builder,
                self.napi_status_function(
                    "napi_define_class",
                    &[
                        pointer.into(),
                        pointer.into(),
                        self.size_type().into(),
                        pointer.into(),
                        pointer.into(),
                        self.size_type().into(),
                        pointer.into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    name.into(),
                    self.size_type()
                        .const_int(
                            u64::try_from(class.export_name.len()).unwrap_or(u64::MAX),
                            false,
                        )
                        .into(),
                    callback.into(),
                    pointer.const_null().into(),
                    self.size_type().const_zero().into(),
                    pointer.const_null().into(),
                    class_value.into(),
                ],
                &format!("node.class.define.{index}"),
            )?;
            let ok = builder.build_int_compare(
                IntPredicate::EQ,
                status,
                self.status_type().const_zero(),
                &format!("node.class.status.{index}"),
            )?;
            let next = self
                .generator
                .context
                .append_basic_block(function, &format!("node.class.next.{index}"));
            let failure = self
                .generator
                .context
                .append_basic_block(function, &format!("node.class.failure.{index}"));
            builder.build_conditional_branch(ok, next, failure)?;
            builder.position_at_end(failure);
            builder.build_return(Some(&pointer.const_null()))?;
            builder.position_at_end(next);
            let status = self.call_status(
                &builder,
                self.napi_status_function(
                    "napi_set_named_property",
                    &[
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    exports.into(),
                    name.into(),
                    builder
                        .build_load(pointer, class_value, &format!("node.class.load.{index}"))?
                        .into(),
                ],
                &format!("node.class.set.{index}"),
            )?;
            let ok = builder.build_int_compare(
                IntPredicate::EQ,
                status,
                self.status_type().const_zero(),
                &format!("node.class.set.status.{index}"),
            )?;
            let next = self
                .generator
                .context
                .append_basic_block(function, &format!("node.class.set.next.{index}"));
            let failure = self
                .generator
                .context
                .append_basic_block(function, &format!("node.class.set.failure.{index}"));
            builder.build_conditional_branch(ok, next, failure)?;
            builder.position_at_end(failure);
            builder.build_return(Some(&pointer.const_null()))?;
            builder.position_at_end(next);
            let class_object = builder
                .build_load(pointer, class_value, &format!("node.class.object.{index}"))?
                .into_pointer_value();
            self.emit_class_methods_in_initializer(
                &builder,
                function,
                env,
                index,
                class_object,
                &classes[index].methods,
            )?;
        }
        let exports = function.get_nth_param(1).ok_or_else(|| {
            CodegenError::Builder("Node module exports parameter is missing".into())
        })?;
        builder.build_return(Some(&exports))?;
        Ok(())
    }

    fn emit_class_methods_in_initializer(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        class_index: usize,
        class_object: PointerValue<'ctx>,
        methods: &[(String, FunctionValue<'ctx>, tn_hir::ReceiverMode)],
    ) -> Result<(), CodegenError> {
        if methods.is_empty() {
            return Ok(());
        }
        let pointer = self.pointer_type();
        let prototype_name = self.c_string(
            builder,
            "prototype",
            &format!("node.class.prototype.name.{class_index}"),
        )?;
        let prototype =
            builder.build_alloca(pointer, &format!("node.class.prototype.{class_index}"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_get_named_property",
                &[
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                    pointer.into(),
                ],
            ),
            &[
                env.into(),
                class_object.into(),
                prototype_name.into(),
                prototype.into(),
            ],
            &format!("node.class.prototype.get.{class_index}"),
        )?;
        let ok = builder.build_int_compare(
            IntPredicate::EQ,
            status,
            self.status_type().const_zero(),
            &format!("node.class.prototype.status.{class_index}"),
        )?;
        let next = self.generator.context.append_basic_block(
            function,
            &format!("node.class.prototype.next.{class_index}"),
        );
        let failure = self.generator.context.append_basic_block(
            function,
            &format!("node.class.prototype.failure.{class_index}"),
        );
        builder.build_conditional_branch(ok, next, failure)?;
        builder.position_at_end(failure);
        builder.build_return(Some(&pointer.const_null()))?;
        builder.position_at_end(next);
        let prototype_object = builder
            .build_load(
                pointer,
                prototype,
                &format!("node.class.prototype.value.{class_index}"),
            )?
            .into_pointer_value();
        for (method_index, (method_name, method_callback, receiver)) in methods.iter().enumerate() {
            let method_name_value = self.c_string(
                builder,
                method_name,
                &format!("node.class.method.name.{class_index}.{method_index}"),
            )?;
            let method_value = builder.build_alloca(
                pointer,
                &format!("node.class.method.value.{class_index}.{method_index}"),
            )?;
            let status = self.call_status(
                builder,
                self.napi_status_function(
                    "napi_create_function",
                    &[
                        pointer.into(),
                        pointer.into(),
                        self.size_type().into(),
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    method_name_value.into(),
                    self.size_type()
                        .const_int(u64::try_from(method_name.len()).unwrap_or(u64::MAX), false)
                        .into(),
                    method_callback.as_global_value().as_pointer_value().into(),
                    pointer.const_null().into(),
                    method_value.into(),
                ],
                &format!("node.class.method.create.{class_index}.{method_index}"),
            )?;
            let ok = builder.build_int_compare(
                IntPredicate::EQ,
                status,
                self.status_type().const_zero(),
                &format!("node.class.method.status.{class_index}.{method_index}"),
            )?;
            let next = self.generator.context.append_basic_block(
                function,
                &format!("node.class.method.next.{class_index}.{method_index}"),
            );
            let failure = self.generator.context.append_basic_block(
                function,
                &format!("node.class.method.failure.{class_index}.{method_index}"),
            );
            builder.build_conditional_branch(ok, next, failure)?;
            builder.position_at_end(failure);
            builder.build_return(Some(&pointer.const_null()))?;
            builder.position_at_end(next);
            let target = if *receiver == tn_hir::ReceiverMode::Static {
                class_object
            } else {
                prototype_object
            };
            let status = self.call_status(
                builder,
                self.napi_status_function(
                    "napi_set_named_property",
                    &[
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    target.into(),
                    method_name_value.into(),
                    builder
                        .build_load(
                            pointer,
                            method_value,
                            &format!("node.class.method.load.{class_index}.{method_index}"),
                        )?
                        .into(),
                ],
                &format!("node.class.method.set.{class_index}.{method_index}"),
            )?;
            let ok = builder.build_int_compare(
                IntPredicate::EQ,
                status,
                self.status_type().const_zero(),
                &format!("node.class.method.set.status.{class_index}.{method_index}"),
            )?;
            let next = self.generator.context.append_basic_block(
                function,
                &format!("node.class.method.set.next.{class_index}.{method_index}"),
            );
            let failure = self.generator.context.append_basic_block(
                function,
                &format!("node.class.method.set.failure.{class_index}.{method_index}"),
            );
            builder.build_conditional_branch(ok, next, failure)?;
            builder.position_at_end(failure);
            builder.build_return(Some(&pointer.const_null()))?;
            builder.position_at_end(next);
        }
        Ok(())
    }

    fn convert_result(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        export: &tn_node_api::BridgeFunction,
        native_result: Option<BasicValueEnum<'ctx>>,
        error: LlvmBlock<'ctx>,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let result = builder.build_alloca(self.pointer_type(), "node.result")?;
        if export.signature.effects.is_empty() {
            if export.result.kind == tn_node_api::NodeTypeKind::Void {
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_get_undefined",
                        &[self.pointer_type().into(), self.pointer_type().into()],
                    ),
                    &[env.into(), result.into()],
                    "node.result.undefined",
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    "node.result.undefined.status",
                )?;
            } else {
                let value = native_result
                    .ok_or_else(|| CodegenError::Builder("Node native result is missing".into()))?;
                self.convert_result_value(
                    builder,
                    function,
                    env,
                    &export.result,
                    value,
                    result,
                    error,
                    "node.result",
                )?;
            }
        } else {
            let packed = native_result
                .ok_or_else(|| {
                    CodegenError::Builder("fallible Node native result is missing".into())
                })?
                .into_array_value();
            let failed = builder
                .build_extract_value(packed, 0, "node.result.failed")?
                .into_int_value();
            let payload = builder
                .build_extract_value(packed, 1, "node.result.payload")?
                .into_int_value();
            let success = self
                .generator
                .context
                .append_basic_block(function, "node.result.success");
            let failure = self
                .generator
                .context
                .append_basic_block(function, "node.result.failure");
            let is_success = builder.build_int_compare(
                IntPredicate::EQ,
                failed,
                self.generator.context.i64_type().const_zero(),
                "node.result.success.test",
            )?;
            builder.build_conditional_branch(is_success, success, failure)?;
            builder.position_at_end(failure);
            let error_pointer =
                builder.build_int_to_ptr(payload, self.pointer_type(), "node.result.error")?;
            let error_value = self.emit_node_error_object(
                builder,
                function,
                env,
                error_pointer,
                &export.errors,
                error,
                "node.result.error",
            )?;
            builder.build_call(
                self.napi_status_function(
                    "napi_throw",
                    &[self.pointer_type().into(), self.pointer_type().into()],
                ),
                &[env.into(), error_value.into()],
                "node.result.error.throw",
            )?;
            self.release_node_error(
                builder,
                function,
                error_pointer,
                &export.errors,
                "node.result.error",
            )?;
            builder.build_return(Some(&self.pointer_type().const_null()))?;
            builder.position_at_end(success);
            if export.result.kind == tn_node_api::NodeTypeKind::Void {
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_get_undefined",
                        &[self.pointer_type().into(), self.pointer_type().into()],
                    ),
                    &[env.into(), result.into()],
                    "node.result.success.undefined",
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    "node.result.success.undefined.status",
                )?;
            } else if self.generator.is_indirect_abi_type(&export.result.native) {
                let native_pointer = builder.build_int_to_ptr(
                    payload,
                    self.pointer_type(),
                    "node.result.value.pointer",
                )?;
                let native = builder.build_load(
                    self.generator.basic_type(&export.result.native)?,
                    native_pointer,
                    "node.result.value",
                )?;
                self.convert_result_value(
                    builder,
                    function,
                    env,
                    &export.result,
                    native,
                    result,
                    error,
                    "node.result.value",
                )?;
                builder.build_call(
                    self.generator.runtime_free(),
                    &[native_pointer.into()],
                    "node.result.value.free",
                )?;
            } else {
                let native_type = self.generator.basic_type(&export.result.native)?;
                let native = builder.build_int_cast(
                    payload,
                    native_type.into_int_type(),
                    "node.result.scalar",
                )?;
                self.convert_result_value(
                    builder,
                    function,
                    env,
                    &export.result,
                    native.into(),
                    result,
                    error,
                    "node.result.scalar",
                )?;
            }
        }
        Ok(builder
            .build_load(self.pointer_type(), result, "node.result.value")?
            .into_pointer_value())
    }

    fn drop_node_value(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        ty: &tn_node_api::NodeType,
        value: BasicValueEnum<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        match &ty.kind {
            tn_node_api::NodeTypeKind::Optional(inner) => {
                let structure = value.into_struct_value();
                let tag = builder
                    .build_extract_value(structure, 0, &format!("{name}.tag"))?
                    .into_int_value();
                let payload =
                    builder.build_extract_value(structure, 1, &format!("{name}.payload"))?;
                let present = self
                    .generator
                    .context
                    .append_basic_block(function, &format!("{name}.present"));
                let absent = self
                    .generator
                    .context
                    .append_basic_block(function, &format!("{name}.absent"));
                let merge = self
                    .generator
                    .context
                    .append_basic_block(function, &format!("{name}.merge"));
                builder.build_conditional_branch(tag, present, absent)?;
                builder.position_at_end(present);
                self.drop_node_value(
                    builder,
                    function,
                    inner,
                    payload,
                    &format!("{name}.present"),
                )?;
                builder.build_unconditional_branch(merge)?;
                builder.position_at_end(absent);
                builder.build_unconditional_branch(merge)?;
                builder.position_at_end(merge);
            }
            tn_node_api::NodeTypeKind::Array { borrowed: true, .. } => {}
            tn_node_api::NodeTypeKind::Array {
                element,
                fixed_length: Some(length),
                ..
            } => {
                let array = value.into_array_value();
                for index in 0..*length {
                    let element_value = builder.build_extract_value(
                        array,
                        u32::try_from(index).unwrap_or(u32::MAX),
                        &format!("{name}.element.{index}"),
                    )?;
                    self.drop_node_value(
                        builder,
                        function,
                        element,
                        element_value,
                        &format!("{name}.element.{index}"),
                    )?;
                }
            }
            tn_node_api::NodeTypeKind::Array {
                borrowed: false, ..
            } => {
                let object = value.into_pointer_value();
                let Type::Nominal(declaration, arguments) = &ty.native else {
                    return Err(CodegenError::Unsupported(
                        "owned Node Array result has no nominal class layout".into(),
                    ));
                };
                let callable = self
                    .generator
                    .layouts
                    .drops
                    .get(declaration)
                    .copied()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "owned Node Array result {declaration:?} has no typed drop"
                        ))
                    })?;
                let instance = Instance {
                    callable,
                    type_arguments: arguments.clone(),
                    effects: Vec::new(),
                };
                let symbol = self.emitted_instance_symbol(&instance);
                let drop_type = self
                    .generator
                    .context
                    .void_type()
                    .fn_type(&[self.pointer_type().into()], false);
                let drop_function =
                    self.generator
                        .module
                        .get_function(&symbol)
                        .unwrap_or_else(|| {
                            self.generator.module.add_function(&symbol, drop_type, None)
                        });
                builder.build_call(drop_function, &[object.into()], &format!("{name}.drop"))?;
                builder.build_call(
                    self.generator.runtime_free(),
                    &[object.into()],
                    &format!("{name}.free"),
                )?;
            }
            tn_node_api::NodeTypeKind::String
            | tn_node_api::NodeTypeKind::Bytes
            | tn_node_api::NodeTypeKind::Void
            | tn_node_api::NodeTypeKind::Scalar(_)
            | tn_node_api::NodeTypeKind::Promise { .. }
            | tn_node_api::NodeTypeKind::Class(_) => {}
        }
        Ok(())
    }

    fn node_error_union_type(&self) -> StructType<'ctx> {
        self.generator.context.struct_type(
            &[
                self.generator.context.i64_type().into(),
                self.pointer_type().into(),
            ],
            false,
        )
    }

    fn create_node_error(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        output: PointerValue<'ctx>,
        name: &str,
        failure: LlvmBlock<'ctx>,
        label: &str,
    ) -> Result<(), CodegenError> {
        let message = self.c_string(builder, name, &format!("{label}.message"))?;
        let message_slot =
            builder.build_alloca(self.pointer_type(), &format!("{label}.message.slot"))?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_create_string_utf8",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.size_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                message.into(),
                self.size_type()
                    .const_int(u64::try_from(name.len()).unwrap_or(u64::MAX), false)
                    .into(),
                message_slot.into(),
            ],
            &format!("{label}.message.create"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            failure,
            &format!("{label}.message.status"),
        )?;
        let message_value = builder
            .build_load(
                self.pointer_type(),
                message_slot,
                &format!("{label}.message.value"),
            )?
            .into_pointer_value();
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_create_error",
                &[
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                    self.pointer_type().into(),
                ],
            ),
            &[
                env.into(),
                self.pointer_type().const_null().into(),
                message_value.into(),
                output.into(),
            ],
            &format!("{label}.create"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            failure,
            &format!("{label}.create.status"),
        )?;
        for property_name in ["name", "typeNative"] {
            let property =
                self.c_string(builder, property_name, &format!("{label}.{property_name}"))?;
            let status = self.call_status(
                builder,
                self.napi_status_function(
                    "napi_set_named_property",
                    &[
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                        self.pointer_type().into(),
                    ],
                ),
                &[
                    env.into(),
                    builder
                        .build_load(self.pointer_type(), output, &format!("{label}.object"))?
                        .into(),
                    property.into(),
                    message_value.into(),
                ],
                &format!("{label}.{property_name}.set"),
            )?;
            self.continue_if_status_ok(
                builder,
                function,
                status,
                failure,
                &format!("{label}.{property_name}.status"),
            )?;
        }
        Ok(())
    }

    fn emit_node_error_object(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        error_pointer: PointerValue<'ctx>,
        errors: &[tn_node_api::BridgeError],
        failure: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let envelope_type = self.node_error_union_type();
        let tag = builder
            .build_load(
                self.generator.context.i64_type(),
                builder.build_struct_gep(
                    envelope_type,
                    error_pointer,
                    0,
                    &format!("{name}.tag"),
                )?,
                &format!("{name}.tag.value"),
            )?
            .into_int_value();
        let payload = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(
                    envelope_type,
                    error_pointer,
                    1,
                    &format!("{name}.payload"),
                )?,
                &format!("{name}.payload.value"),
            )?
            .into_pointer_value();
        let output = builder.build_alloca(self.pointer_type(), &format!("{name}.output"))?;
        let merge = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.merge"));
        let default = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.default"));
        let blocks = errors
            .iter()
            .enumerate()
            .map(|(index, _)| {
                self.generator
                    .context
                    .append_basic_block(function, &format!("{name}.error.{index}"))
            })
            .collect::<Vec<_>>();
        let cases = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                (
                    self.generator
                        .context
                        .i64_type()
                        .const_int(u64::try_from(index).unwrap_or(u64::MAX), false),
                    *block,
                )
            })
            .collect::<Vec<_>>();
        builder.build_switch(tag, default, &cases)?;
        for (error, block) in errors.iter().zip(blocks) {
            builder.position_at_end(block);
            self.create_node_error(
                builder,
                function,
                env,
                output,
                &error.name,
                failure,
                &format!("{name}.{}", error.name),
            )?;
            let payload_type = if self.generator.is_class_type(&error.native) {
                self.generator.class_object_type(&error.native)?
            } else {
                self.generator.basic_type(&error.native)?.into_struct_type()
            };
            for field in &error.fields {
                let field_address = builder.build_struct_gep(
                    payload_type,
                    payload,
                    field.index,
                    &format!("{name}.{}.{}", error.name, field.name),
                )?;
                let field_value = builder.build_load(
                    self.generator.basic_type(&field.ty.native)?,
                    field_address,
                    &format!("{name}.{}.{}.value", error.name, field.name),
                )?;
                let field_output = builder.build_alloca(
                    self.pointer_type(),
                    &format!("{name}.{}.{}.output", error.name, field.name),
                )?;
                self.convert_result_value(
                    builder,
                    function,
                    env,
                    &field.ty,
                    field_value,
                    field_output,
                    failure,
                    &format!("{name}.{}.{}.convert", error.name, field.name),
                )?;
                // Error payloads use `rawCode` internally so the source model does not collide
                // with the language's diagnostic code terminology.  Node consumers receive the
                // conventional `code` property while all other public payload fields retain
                // their declared names.
                let property_name = if field.name == "rawCode" {
                    "code"
                } else {
                    field.name.as_str()
                };
                let property = self.c_string(
                    builder,
                    property_name,
                    &format!("{name}.{}.{}.property", error.name, field.name),
                )?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_set_named_property",
                        &[
                            self.pointer_type().into(),
                            self.pointer_type().into(),
                            self.pointer_type().into(),
                            self.pointer_type().into(),
                        ],
                    ),
                    &[
                        env.into(),
                        builder
                            .build_load(
                                self.pointer_type(),
                                output,
                                &format!("{name}.{}.object", error.name),
                            )?
                            .into(),
                        property.into(),
                        builder
                            .build_load(
                                self.pointer_type(),
                                field_output,
                                &format!("{name}.{}.{}.js", error.name, field.name),
                            )?
                            .into(),
                    ],
                    &format!("{name}.{}.{}.set", error.name, field.name),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    failure,
                    &format!("{name}.{}.{}.set.status", error.name, field.name),
                )?;
            }
            builder.build_unconditional_branch(merge)?;
        }
        builder.position_at_end(default);
        self.create_node_error(
            builder,
            function,
            env,
            output,
            "TypeNativeError",
            failure,
            &format!("{name}.default"),
        )?;
        builder.build_unconditional_branch(merge)?;
        builder.position_at_end(merge);
        Ok(builder
            .build_load(self.pointer_type(), output, &format!("{name}.value"))?
            .into_pointer_value())
    }

    fn release_node_error(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        error_pointer: PointerValue<'ctx>,
        errors: &[tn_node_api::BridgeError],
        name: &str,
    ) -> Result<(), CodegenError> {
        let envelope_type = self.node_error_union_type();
        let tag = builder
            .build_load(
                self.generator.context.i64_type(),
                builder.build_struct_gep(
                    envelope_type,
                    error_pointer,
                    0,
                    &format!("{name}.tag"),
                )?,
                &format!("{name}.tag.value"),
            )?
            .into_int_value();
        let payload = builder
            .build_load(
                self.pointer_type(),
                builder.build_struct_gep(
                    envelope_type,
                    error_pointer,
                    1,
                    &format!("{name}.payload"),
                )?,
                &format!("{name}.payload.value"),
            )?
            .into_pointer_value();
        let merge = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.merge"));
        let default = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.default"));
        let blocks = errors
            .iter()
            .enumerate()
            .map(|(index, _)| {
                self.generator
                    .context
                    .append_basic_block(function, &format!("{name}.error.{index}"))
            })
            .collect::<Vec<_>>();
        let cases = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                (
                    self.generator
                        .context
                        .i64_type()
                        .const_int(u64::try_from(index).unwrap_or(u64::MAX), false),
                    *block,
                )
            })
            .collect::<Vec<_>>();
        builder.build_switch(tag, default, &cases)?;
        for (error, block) in errors.iter().zip(blocks) {
            builder.position_at_end(block);
            if let Some(callable) = self.generator.layouts.drops.get(&error.declaration) {
                let instance = Instance {
                    callable: *callable,
                    type_arguments: Vec::new(),
                    effects: Vec::new(),
                };
                let symbol = self.emitted_instance_symbol(&instance);
                let drop_type = self
                    .generator
                    .context
                    .void_type()
                    .fn_type(&[self.pointer_type().into()], false);
                let drop_function =
                    self.generator
                        .module
                        .get_function(&symbol)
                        .unwrap_or_else(|| {
                            self.generator.module.add_function(&symbol, drop_type, None)
                        });
                builder.build_call(drop_function, &[payload.into()], &format!("{name}.drop"))?;
            }
            builder.build_call(
                self.generator.runtime_free(),
                &[payload.into()],
                &format!("{name}.payload.free"),
            )?;
            builder.build_unconditional_branch(merge)?;
        }
        builder.position_at_end(default);
        builder.build_call(
            self.generator.runtime_free(),
            &[payload.into()],
            &format!("{name}.default.payload.free"),
        )?;
        builder.build_unconditional_branch(merge)?;
        builder.position_at_end(merge);
        builder.build_call(
            self.generator.runtime_free(),
            &[error_pointer.into()],
            &format!("{name}.envelope.free"),
        )?;
        Ok(())
    }

    fn convert_result_value(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        value: BasicValueEnum<'ctx>,
        output: PointerValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        match &ty.kind {
            tn_node_api::NodeTypeKind::Void => {
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_get_undefined",
                        &[self.pointer_type().into(), self.pointer_type().into()],
                    ),
                    &[env.into(), output.into()],
                    &format!("{name}.undefined"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.undefined.status"),
                )?;
            }
            tn_node_api::NodeTypeKind::Promise { .. } => {
                return Err(CodegenError::Unsupported(
                    "Node Promise result requires the async bridge".into(),
                ));
            }
            tn_node_api::NodeTypeKind::Scalar(primitive) => {
                self.convert_scalar_result(
                    builder, function, env, primitive, value, output, error, name,
                )?;
            }
            tn_node_api::NodeTypeKind::String => {
                let native = value.into_pointer_value();
                let length = self
                    .call_value(
                        builder,
                        self.generator.runtime_string_length(),
                        &[native.into()],
                        &format!("{name}.length"),
                    )?
                    .into_int_value();
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_create_string_utf8",
                        &[
                            self.pointer_type().into(),
                            self.pointer_type().into(),
                            self.size_type().into(),
                            self.pointer_type().into(),
                        ],
                    ),
                    &[env.into(), native.into(), length.into(), output.into()],
                    &format!("{name}.create"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
            }
            tn_node_api::NodeTypeKind::Bytes => {
                let structure = value.into_struct_value();
                let pointer = builder
                    .build_extract_value(structure, 0, &format!("{name}.pointer"))?
                    .into_pointer_value();
                let length = builder
                    .build_extract_value(structure, 1, &format!("{name}.length"))?
                    .into_int_value();
                let buffer =
                    builder.build_alloca(self.pointer_type(), &format!("{name}.buffer"))?;
                let data = builder.build_alloca(self.pointer_type(), &format!("{name}.data"))?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_create_arraybuffer",
                        &[
                            self.pointer_type().into(),
                            self.size_type().into(),
                            self.pointer_type().into(),
                            self.pointer_type().into(),
                        ],
                    ),
                    &[env.into(), length.into(), data.into(), buffer.into()],
                    &format!("{name}.arraybuffer"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.arraybuffer.status"),
                )?;
                let copied = self
                    .call_value(
                        builder,
                        self.runtime_bytes_copy(),
                        &[
                            pointer.into(),
                            length.into(),
                            builder
                                .build_load(
                                    self.pointer_type(),
                                    data,
                                    &format!("{name}.data.value"),
                                )?
                                .into(),
                        ],
                        &format!("{name}.copy"),
                    )?
                    .into_int_value();
                self.continue_if(
                    builder,
                    function,
                    builder.build_int_compare(
                        IntPredicate::EQ,
                        copied,
                        length,
                        &format!("{name}.copy.ok"),
                    )?,
                    error,
                    &format!("{name}.copy.status"),
                )?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_create_typedarray",
                        &[
                            self.pointer_type().into(),
                            self.generator.context.i32_type().into(),
                            self.size_type().into(),
                            self.pointer_type().into(),
                            self.size_type().into(),
                            self.pointer_type().into(),
                        ],
                    ),
                    &[
                        env.into(),
                        self.generator.context.i32_type().const_int(1, false).into(),
                        length.into(),
                        builder
                            .build_load(
                                self.pointer_type(),
                                buffer,
                                &format!("{name}.buffer.value"),
                            )?
                            .into(),
                        self.size_type().const_zero().into(),
                        output.into(),
                    ],
                    &format!("{name}.typedarray"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.typedarray.status"),
                )?;
            }
            tn_node_api::NodeTypeKind::Optional(inner) => {
                let structure = value.into_struct_value();
                let tag = builder
                    .build_extract_value(structure, 0, &format!("{name}.tag"))?
                    .into_int_value();
                let payload =
                    builder.build_extract_value(structure, 1, &format!("{name}.payload"))?;
                let present = self
                    .generator
                    .context
                    .append_basic_block(function, &format!("{name}.present"));
                let absent = self
                    .generator
                    .context
                    .append_basic_block(function, &format!("{name}.absent"));
                let merge = self
                    .generator
                    .context
                    .append_basic_block(function, &format!("{name}.merge"));
                builder.build_conditional_branch(tag, present, absent)?;
                builder.position_at_end(present);
                self.convert_result_value(
                    builder,
                    function,
                    env,
                    inner,
                    payload,
                    output,
                    error,
                    &format!("{name}.present"),
                )?;
                builder.build_unconditional_branch(merge)?;
                builder.position_at_end(absent);
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_get_undefined",
                        &[self.pointer_type().into(), self.pointer_type().into()],
                    ),
                    &[env.into(), output.into()],
                    &format!("{name}.absent"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.absent.status"),
                )?;
                builder.build_unconditional_branch(merge)?;
                builder.position_at_end(merge);
            }
            tn_node_api::NodeTypeKind::Array {
                element,
                fixed_length: Some(length),
                ..
            } => {
                self.convert_fixed_array_result(
                    builder, function, env, element, *length, value, output, error, name,
                )?;
            }
            tn_node_api::NodeTypeKind::Array { element, .. } => {
                self.convert_dynamic_array_result(
                    builder, function, env, ty, element, value, output, error, name,
                )?;
            }
            tn_node_api::NodeTypeKind::Class(_) => {
                return Err(CodegenError::Unsupported(format!(
                    "Node class result `{name}` requires a class wrapper"
                )));
            }
        }
        Ok(())
    }

    fn convert_scalar_result(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        primitive: &PrimitiveType,
        value: BasicValueEnum<'ctx>,
        output: PointerValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        let pointer = self.pointer_type();
        let (creator, argument) = match primitive {
            PrimitiveType::Bool => {
                let value = value.into_int_value();
                let value = builder.build_int_z_extend(
                    value,
                    self.generator.context.i8_type(),
                    &format!("{name}.bool"),
                )?;
                ("napi_create_bool", value.into())
            }
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Char => {
                let value = value.into_int_value();
                let value = if value.get_type().get_bit_width() < 32 {
                    if matches!(primitive, PrimitiveType::I8 | PrimitiveType::I16) {
                        builder.build_int_s_extend(
                            value,
                            self.generator.context.i32_type(),
                            &format!("{name}.wide"),
                        )?
                    } else {
                        builder.build_int_z_extend(
                            value,
                            self.generator.context.i32_type(),
                            &format!("{name}.wide"),
                        )?
                    }
                } else {
                    value
                };
                let creator = if matches!(primitive, PrimitiveType::U32 | PrimitiveType::Char) {
                    "napi_create_uint32"
                } else {
                    "napi_create_int32"
                };
                (creator, value.into())
            }
            PrimitiveType::I64 | PrimitiveType::Isize => {
                ("napi_create_bigint_int64", value.into_int_value().into())
            }
            PrimitiveType::U64 | PrimitiveType::Usize => {
                ("napi_create_bigint_uint64", value.into_int_value().into())
            }
            PrimitiveType::F32 | PrimitiveType::F64 => {
                let value = value.into_float_value();
                let value = if matches!(primitive, PrimitiveType::F32) {
                    builder.build_float_ext(
                        value,
                        self.generator.context.f64_type(),
                        &format!("{name}.wide"),
                    )?
                } else {
                    value
                };
                ("napi_create_double", value.into())
            }
            PrimitiveType::I128 | PrimitiveType::U128 => {
                let value = value.into_int_value();
                let bits = builder
                    .build_bit_cast(
                        value,
                        self.generator.context.i128_type(),
                        &format!("{name}.bits"),
                    )?
                    .into_int_value();
                let low = builder.build_int_truncate(
                    bits,
                    self.generator.context.i64_type(),
                    &format!("{name}.low"),
                )?;
                let shifted = builder.build_right_shift(
                    bits,
                    self.generator.context.i128_type().const_int(64, false),
                    false,
                    &format!("{name}.high.bits"),
                )?;
                let high = builder.build_int_truncate(
                    shifted,
                    self.generator.context.i64_type(),
                    &format!("{name}.high"),
                )?;
                let words = builder.build_array_alloca(
                    self.generator.context.i64_type(),
                    self.generator.context.i32_type().const_int(2, false),
                    &format!("{name}.words"),
                )?;
                let low_address = unsafe {
                    builder.build_gep(
                        self.generator.context.i64_type(),
                        words,
                        &[self.size_type().const_zero()],
                        &format!("{name}.low.address"),
                    )?
                };
                let high_address = unsafe {
                    builder.build_gep(
                        self.generator.context.i64_type(),
                        words,
                        &[self.size_type().const_int(1, false)],
                        &format!("{name}.high.address"),
                    )?
                };
                builder.build_store(low_address, low)?;
                builder.build_store(high_address, high)?;
                let sign = if matches!(primitive, PrimitiveType::I128) {
                    builder.build_int_compare(
                        IntPredicate::SLT,
                        value,
                        self.generator.context.i128_type().const_zero(),
                        &format!("{name}.sign"),
                    )?
                } else {
                    self.generator.context.bool_type().const_zero()
                };
                let sign = builder.build_int_z_extend(
                    sign,
                    self.generator.context.i32_type(),
                    &format!("{name}.sign.wide"),
                )?;
                let status = self.call_status(
                    builder,
                    self.napi_status_function(
                        "napi_create_bigint_words",
                        &[
                            pointer.into(),
                            self.generator.context.i32_type().into(),
                            self.size_type().into(),
                            pointer.into(),
                            pointer.into(),
                        ],
                    ),
                    &[
                        env.into(),
                        sign.into(),
                        self.size_type().const_int(2, false).into(),
                        words.into(),
                        output.into(),
                    ],
                    &format!("{name}.create"),
                )?;
                self.continue_if_status_ok(
                    builder,
                    function,
                    status,
                    error,
                    &format!("{name}.status"),
                )?;
                return Ok(());
            }
            PrimitiveType::Void | PrimitiveType::Never => {
                return Err(CodegenError::Unsupported("void/never result".into()));
            }
        };
        let status = self.call_status(
            builder,
            self.napi_status_function(
                creator,
                &[
                    pointer.into(),
                    match creator {
                        "napi_create_bool" => self.generator.context.i8_type().into(),
                        "napi_create_int32" | "napi_create_uint32" => {
                            self.generator.context.i32_type().into()
                        }
                        "napi_create_bigint_int64" | "napi_create_bigint_uint64" => {
                            self.generator.context.i64_type().into()
                        }
                        "napi_create_double" => self.generator.context.f64_type().into(),
                        _ => {
                            return Err(CodegenError::Unsupported(
                                "unknown Node scalar creator".into(),
                            ));
                        }
                    },
                    pointer.into(),
                ],
            ),
            &[env.into(), argument, output.into()],
            &format!("{name}.create"),
        )?;
        self.continue_if_status_ok(builder, function, status, error, &format!("{name}.status"))?;
        Ok(())
    }

    fn convert_fixed_array_result(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        element: &tn_node_api::NodeType,
        length: usize,
        value: BasicValueEnum<'ctx>,
        output: PointerValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        let pointer = self.pointer_type();
        let status = self.call_status(
            builder,
            self.napi_status_function("napi_create_array", &[pointer.into(), pointer.into()]),
            &[env.into(), output.into()],
            &format!("{name}.create"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.create.status"),
        )?;
        let array = value.into_array_value();
        for index in 0..length {
            let element_value = builder.build_extract_value(
                array,
                u32::try_from(index).unwrap_or(u32::MAX),
                &format!("{name}.element.{index}"),
            )?;
            let js_element =
                builder.build_alloca(pointer, &format!("{name}.js_element.{index}"))?;
            self.convert_result_value(
                builder,
                function,
                env,
                element,
                element_value,
                js_element,
                error,
                &format!("{name}.convert.{index}"),
            )?;
            let status = self.call_status(
                builder,
                self.napi_status_function(
                    "napi_set_element",
                    &[
                        pointer.into(),
                        pointer.into(),
                        self.size_type().into(),
                        pointer.into(),
                    ],
                ),
                &[
                    env.into(),
                    builder
                        .build_load(pointer, output, &format!("{name}.array.{index}"))?
                        .into(),
                    self.size_type()
                        .const_int(u64::try_from(index).unwrap_or(u64::MAX), false)
                        .into(),
                    builder
                        .build_load(pointer, js_element, &format!("{name}.js.{index}"))?
                        .into(),
                ],
                &format!("{name}.set.{index}"),
            )?;
            self.continue_if_status_ok(
                builder,
                function,
                status,
                error,
                &format!("{name}.set.{index}.status"),
            )?;
        }
        Ok(())
    }

    fn convert_dynamic_array_result(
        &self,
        builder: &Builder<'ctx>,
        function: FunctionValue<'ctx>,
        env: PointerValue<'ctx>,
        ty: &tn_node_api::NodeType,
        element: &tn_node_api::NodeType,
        value: BasicValueEnum<'ctx>,
        output: PointerValue<'ctx>,
        error: LlvmBlock<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        let pointer = self.pointer_type();
        let status = self.call_status(
            builder,
            self.napi_status_function("napi_create_array", &[pointer.into(), pointer.into()]),
            &[env.into(), output.into()],
            &format!("{name}.create"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.create.status"),
        )?;
        let object = value.into_pointer_value();
        let array_type = match &ty.native {
            Type::Nominal(declaration, arguments) => Type::Nominal(*declaration, arguments.clone()),
            _ => {
                return Err(CodegenError::Unsupported(
                    "Node dynamic Array result has no class layout".into(),
                ));
            }
        };
        let object_type = self.generator.class_object_type(&array_type)?;
        let length = builder
            .build_load(
                self.size_type(),
                builder.build_struct_gep(
                    object_type,
                    object,
                    3,
                    &format!("{name}.length.address"),
                )?,
                &format!("{name}.length"),
            )?
            .into_int_value();
        let data = builder
            .build_load(
                pointer,
                builder.build_struct_gep(
                    object_type,
                    object,
                    1,
                    &format!("{name}.data.address"),
                )?,
                &format!("{name}.data"),
            )?
            .into_pointer_value();
        let element_type = self.generator.basic_type(&element.native)?;
        let loop_block = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.loop"));
        let body_block = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.body"));
        let done_block = self
            .generator
            .context
            .append_basic_block(function, &format!("{name}.done"));
        let pre_loop = builder
            .get_insert_block()
            .ok_or_else(|| CodegenError::Builder("Node result pre-loop block is missing".into()))?;
        builder.build_unconditional_branch(loop_block)?;
        builder.position_at_end(loop_block);
        let phi = builder.build_phi(self.size_type(), &format!("{name}.index"))?;
        phi.add_incoming(&[(&self.size_type().const_zero(), pre_loop)]);
        let current = phi.as_basic_value().into_int_value();
        let condition = builder.build_int_compare(
            IntPredicate::ULT,
            current,
            length,
            &format!("{name}.condition"),
        )?;
        builder.build_conditional_branch(condition, body_block, done_block)?;
        builder.position_at_end(body_block);
        let native_address = unsafe {
            builder.build_gep(
                element_type,
                data,
                &[current],
                &format!("{name}.element.address"),
            )?
        };
        let native =
            builder.build_load(element_type, native_address, &format!("{name}.element"))?;
        let js_element = builder.build_alloca(pointer, &format!("{name}.js_element"))?;
        self.convert_result_value(
            builder,
            function,
            env,
            element,
            native,
            js_element,
            error,
            &format!("{name}.convert"),
        )?;
        let status = self.call_status(
            builder,
            self.napi_status_function(
                "napi_set_element",
                &[
                    pointer.into(),
                    pointer.into(),
                    self.size_type().into(),
                    pointer.into(),
                ],
            ),
            &[
                env.into(),
                builder
                    .build_load(pointer, output, &format!("{name}.array"))?
                    .into(),
                current.into(),
                builder
                    .build_load(pointer, js_element, &format!("{name}.js"))?
                    .into(),
            ],
            &format!("{name}.set"),
        )?;
        self.continue_if_status_ok(
            builder,
            function,
            status,
            error,
            &format!("{name}.set.status"),
        )?;
        let next = builder.build_int_add(
            current,
            self.size_type().const_int(1, false),
            &format!("{name}.next"),
        )?;
        let body_end = builder
            .get_insert_block()
            .ok_or_else(|| CodegenError::Builder("Node result body block is missing".into()))?;
        builder.build_unconditional_branch(loop_block)?;
        phi.add_incoming(&[(&next, body_end)]);
        builder.position_at_end(done_block);
        Ok(())
    }
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

struct ClosureTarget<'ctx> {
    body: FunctionValue<'ctx>,
    trampoline: FunctionValue<'ctx>,
    drop: Option<FunctionValue<'ctx>>,
    environment: Option<StructType<'ctx>>,
    captures: Vec<Type>,
    function: FunctionType,
    consumes_environment: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ClosureKey {
    instance: Instance,
    id: HirClosureId,
}

impl<'ctx> Generator<'ctx> {
    fn add_inline_hint(&self, function: FunctionValue<'ctx>) {
        let kind = inkwell::attributes::Attribute::get_named_enum_kind_id("inlinehint");
        function.add_attribute(
            AttributeLoc::Function,
            self.context.create_enum_attribute(kind, 0),
        );
    }

    fn new(
        context: &'ctx Context,
        module: Module<'ctx>,
        target_data: TargetData,
        layouts: Layouts,
        module_name: &str,
        target_triple: &str,
        profile: CodegenProfile,
        sanitizers: &BTreeSet<Sanitizer>,
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
            globals: BTreeMap::new(),
            global_initialized: BTreeMap::new(),
            constructors: Vec::new(),
            descriptors: BTreeMap::new(),
            witnesses: BTreeMap::new(),
            builtin_witnesses: BTreeMap::new(),
            debug_info,
            async_wrappers: Vec::new(),
            abi_wrappers: Vec::new(),
            closures: BTreeMap::new(),
            is_macos: target_triple.contains("apple-darwin"),
            sanitizers: sanitizers.clone(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn declare_bodies(&mut self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        for unit in units {
            let explicitly_exported = self.layouts.export_instances.contains_key(&unit.instance)
                || self.layouts.exports.contains_key(&unit.instance.callable);
            let exported_name = self
                .layouts
                .export_instances
                .get(&unit.instance)
                .cloned()
                .or_else(|| self.layouts.exports.get(&unit.instance.callable).cloned())
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
            if self.layouts.inlines.contains(&unit.instance.callable) {
                self.add_inline_hint(body_function);
            }
            if !explicitly_exported {
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
                if self.layouts.inlines.contains(&unit.instance.callable) {
                    self.add_inline_hint(wrapper);
                }
                if !explicitly_exported {
                    wrapper.set_linkage(Linkage::Internal);
                }
                self.debug_info.attach_function(wrapper, &exported_name);
                self.functions.insert(unit.instance.clone(), wrapper);
                let mut emitted_signature = self.normalize_function_type(&signature);
                emitted_signature.effects.clone_from(&unit.instance.effects);
                self.signatures
                    .insert(unit.instance.clone(), emitted_signature);
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
                let poll_type = self.context.bool_type().fn_type(
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
                let drop_type = self.context.bool_type().fn_type(
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
                        Type::Promise {
                            result,
                            error,
                            effects,
                        } if effects.is_empty() => Type::Promise {
                            result: result.clone(),
                            error: error.clone(),
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
                if self.layouts.inlines.contains(&unit.instance.callable) {
                    self.add_inline_hint(wrapper);
                }
                self.debug_info.attach_function(wrapper, &exported_name);
                self.functions.insert(unit.instance.clone(), wrapper);
                self.signatures.insert(
                    unit.instance.clone(),
                    self.normalize_function_type(&signature),
                );
                self.abi_wrappers.push(AbiWrapper {
                    wrapper,
                    body: body_function,
                    signature,
                    kind: AbiWrapperKind::EffectLift,
                });
            } else {
                let signature = body_signature.clone();
                let function = body_function;
                self.functions.insert(unit.instance.clone(), function);
                self.signatures.insert(
                    unit.instance.clone(),
                    self.normalize_function_type(&signature),
                );
            }
            if let Some(kind) = abi_kind {
                let abi_type = self.abi_wrapper_type(kind, &body_signature)?;
                let wrapper = self.module.add_function(&exported_name, abi_type, None);
                if self.layouts.inlines.contains(&unit.instance.callable) {
                    self.add_inline_hint(wrapper);
                }
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

    fn declare_closures(&mut self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        for unit in units {
            self.declare_closures_in_body(&unit.instance, &unit.body)?;
        }
        Ok(())
    }

    fn declare_closures_in_body(
        &mut self,
        instance: &Instance,
        body: &Body,
    ) -> Result<(), CodegenError> {
        for block in &body.blocks {
            for statement in &block.statements {
                let StatementKind::Assign(_, value) = &statement.kind else {
                    continue;
                };
                let Rvalue::Closure {
                    id,
                    function,
                    captures,
                    body: closure_body,
                } = value.as_ref()
                else {
                    continue;
                };
                let key = ClosureKey {
                    instance: instance.clone(),
                    id: *id,
                };
                if !self.closures.contains_key(&key) {
                    let capture_types = captures
                        .iter()
                        .map(|capture| closure_operand_type(body, capture))
                        .collect::<Result<Vec<_>, _>>()?;
                    let body_type = self.body_function_type(closure_body)?;
                    let body_function = self.module.add_function(
                        &format!("tn_closure_{}_body", id.0),
                        body_type,
                        None,
                    );
                    body_function.set_linkage(Linkage::Internal);
                    let environment = if capture_types.is_empty() {
                        None
                    } else {
                        let mut fields = vec![self.context.bool_type().into()];
                        fields.extend(
                            capture_types
                                .iter()
                                .map(|capture| self.basic_type(capture))
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                        Some(self.context.struct_type(&fields, false))
                    };
                    let mut trampoline_parameters = vec![Type::RawPointer {
                        mutable: true,
                        pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
                    }];
                    trampoline_parameters.extend(function.parameters.clone());
                    let trampoline_type = self.llvm_function_type(
                        &trampoline_parameters,
                        &function.result,
                        &function.effects,
                    )?;
                    let trampoline = self.module.add_function(
                        &format!("tn_closure_{}_invoke", id.0),
                        trampoline_type,
                        None,
                    );
                    trampoline.set_linkage(Linkage::Internal);
                    let drop = environment.map(|_| {
                        let pointer = self.context.ptr_type(AddressSpace::default());
                        let function = self.module.add_function(
                            &format!("tn_closure_{}_drop", id.0),
                            self.context.void_type().fn_type(&[pointer.into()], false),
                            None,
                        );
                        function.set_linkage(Linkage::Internal);
                        function
                    });
                    self.closures.insert(
                        key,
                        ClosureTarget {
                            body: body_function,
                            trampoline,
                            drop,
                            environment,
                            captures: capture_types.clone(),
                            function: function.clone(),
                            consumes_environment: capture_types
                                .iter()
                                .any(|capture| !self.is_copy_type(capture)),
                        },
                    );
                    self.declare_closures_in_body(instance, closure_body)?;
                }
            }
        }
        Ok(())
    }

    fn lower_closures(&self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        for unit in units {
            self.lower_closures_in_body(&unit.instance, &unit.body)?;
        }
        Ok(())
    }

    fn lower_closures_in_body(&self, instance: &Instance, body: &Body) -> Result<(), CodegenError> {
        for block in &body.blocks {
            for statement in &block.statements {
                let StatementKind::Assign(_, value) = &statement.kind else {
                    continue;
                };
                let Rvalue::Closure {
                    id,
                    captures,
                    body: closure_body,
                    ..
                } = value.as_ref()
                else {
                    continue;
                };
                let target = self
                    .closures
                    .get(&ClosureKey {
                        instance: instance.clone(),
                        id: *id,
                    })
                    .ok_or_else(|| CodegenError::Unsupported("closure target is missing".into()))?;
                FunctionGenerator::new(self, instance, closure_body, target.body)
                    .and_then(|generator| generator.lower())?;
                self.lower_closure_trampoline(target, closure_body)?;
                if let Some(drop) = target.drop {
                    self.lower_closure_drop(target, drop, closure_body, instance)?;
                }
                let _ = captures;
                self.lower_closures_in_body(instance, closure_body)?;
            }
        }
        Ok(())
    }

    fn lower_closure_trampoline(
        &self,
        target: &ClosureTarget<'ctx>,
        body: &Body,
    ) -> Result<(), CodegenError> {
        let entry = self.context.append_basic_block(target.trampoline, "entry");
        let builder = self.context.create_builder();
        builder.position_at_end(entry);
        let mut parameters = target.trampoline.get_param_iter();
        let environment = parameters
            .next()
            .ok_or_else(|| {
                CodegenError::Unsupported("closure environment parameter missing".into())
            })?
            .into_pointer_value();
        let env_fields = target.environment;
        let mut arguments = Vec::new();
        if let Some(environment_type) = env_fields {
            for index in 0..target.captures.len() {
                let address = builder.build_struct_gep(
                    environment_type,
                    environment,
                    u32::try_from(index + 1)
                        .map_err(|_| CodegenError::Unsupported("closure capture limit".into()))?,
                    "closure.capture.address",
                )?;
                arguments.push(
                    builder
                        .build_load(
                            self.basic_type(&target.captures[index])?,
                            address,
                            "closure.capture",
                        )?
                        .into(),
                );
            }
            if target.consumes_environment {
                let consumed = builder.build_struct_gep(
                    environment_type,
                    environment,
                    0,
                    "closure.consumed",
                )?;
                builder.build_store(consumed, self.context.bool_type().const_int(1, false))?;
            }
        }
        arguments.extend(parameters.map(|parameter| parameter.as_basic_value_enum()));
        let call_arguments = arguments
            .iter()
            .copied()
            .map(BasicMetadataValueEnum::from)
            .collect::<Vec<_>>();
        let call = builder.build_call(target.body, &call_arguments, "closure.body")?;
        if target.function.result.as_ref() == &Type::Primitive(PrimitiveType::Void)
            && target.function.effects.is_empty()
        {
            builder.build_return(None)?;
        } else if target.function.effects.is_empty() {
            let value = call
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("closure body returned void".into()))?;
            builder.build_return(Some(&value))?;
        } else {
            let value = call
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("closure body returned void".into()))?;
            builder.build_return(Some(&value))?;
        }
        let _ = body;
        Ok(())
    }

    fn lower_closure_drop(
        &self,
        target: &ClosureTarget<'ctx>,
        function: FunctionValue<'ctx>,
        body: &Body,
        instance: &Instance,
    ) -> Result<(), CodegenError> {
        let entry = self.context.append_basic_block(function, "entry");
        let drop_block = self.context.append_basic_block(function, "drop");
        let free_block = self.context.append_basic_block(function, "free");
        let builder = self.context.create_builder();
        builder.position_at_end(entry);
        let environment = function
            .get_first_param()
            .ok_or_else(|| CodegenError::Unsupported("closure drop environment missing".into()))?
            .into_pointer_value();
        let environment_type = target
            .environment
            .ok_or_else(|| CodegenError::Unsupported("closure drop layout is missing".into()))?;
        let consumed = builder
            .build_load(
                self.context.bool_type(),
                builder.build_struct_gep(environment_type, environment, 0, "closure.consumed")?,
                "closure.consumed.value",
            )?
            .into_int_value();
        builder.build_conditional_branch(consumed, free_block, drop_block)?;
        builder.position_at_end(drop_block);
        let lightweight = FunctionGenerator {
            generator: self,
            instance,
            body,
            function,
            builder,
            blocks: Vec::new(),
            locals: Vec::new(),
            drop_flags: BTreeMap::new(),
            constructor_initializer: false,
        };
        for (index, capture) in target.captures.iter().enumerate() {
            let address = lightweight.builder.build_struct_gep(
                environment_type,
                environment,
                u32::try_from(index + 1)
                    .map_err(|_| CodegenError::Unsupported("closure capture limit".into()))?,
                "closure.drop.capture",
            )?;
            lightweight.lower_drop_value_at_pointer(address, capture, None)?;
        }
        lightweight.builder.build_unconditional_branch(free_block)?;
        lightweight.builder.position_at_end(free_block);
        lightweight.builder.build_call(
            self.runtime_free(),
            &[environment.into()],
            "closure.env.free",
        )?;
        lightweight.builder.build_return(None)?;
        Ok(())
    }

    fn declare_externs(&mut self, units: &[MonomorphizedBody]) -> Result<(), CodegenError> {
        let mut declared = BTreeMap::<String, (FunctionValue<'ctx>, FunctionType)>::new();
        let emitted_callables = units
            .iter()
            .map(|unit| unit.instance.callable)
            .collect::<BTreeSet<_>>();
        for (callable, external) in &self.layouts.externs {
            let shadowed_by_emitted_export =
                self.layouts
                    .exports
                    .iter()
                    .any(|(exported_callable, exported)| {
                        exported == &external.name && emitted_callables.contains(exported_callable)
                    });
            if shadowed_by_emitted_export {
                continue;
            }
            let signature = self.normalize_function_type(&external.function);
            let function_type = self.llvm_function_type(
                &signature.parameters,
                &signature.result,
                &signature.effects,
            )?;
            let function =
                if let Some((function, existing_signature)) = declared.get(&external.name) {
                    if existing_signature != &signature {
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
                    declared.insert(external.name.clone(), (function, signature.clone()));
                    function
                };
            self.functions
                .insert(Instance::concrete(*callable), function);
            self.signatures
                .insert(Instance::concrete(*callable), signature);
        }
        Ok(())
    }

    fn declare_globals(&mut self) -> Result<(), CodegenError> {
        for (declaration, layout) in &self.layouts.globals {
            let value_type = self.basic_type(&layout.ty)?;
            let global = self.module.add_global(value_type, None, &layout.name);
            global.set_linkage(Linkage::Private);
            global.set_initializer(&value_type.const_zero());
            self.globals.insert(*declaration, global.as_pointer_value());

            let initialized = self.module.add_global(
                self.context.bool_type(),
                None,
                &format!("{}_initialized", layout.name),
            );
            initialized.set_linkage(Linkage::Private);
            initialized.set_initializer(&self.context.bool_type().const_zero());
            self.global_initialized
                .insert(*declaration, initialized.as_pointer_value());
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
            global.set_linkage(Linkage::Private);
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
                    if !class_receiver_matches(receiver, declaration, arguments.as_slice()) {
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
        self.declare_builtin_ord_witnesses()?;
        self.declare_builtin_hash_witnesses()?;
        self.declare_builtin_equal_witnesses()?;
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
            global.set_linkage(Linkage::Private);
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

    fn declare_builtin_ord_witnesses(&mut self) -> Result<(), CodegenError> {
        let interfaces = self
            .layouts
            .interface_names
            .iter()
            .filter_map(|(declaration, name)| (name == "Ord").then_some(*declaration))
            .collect::<Vec<_>>();
        let types = [
            Type::Primitive(PrimitiveType::Bool),
            Type::Primitive(PrimitiveType::I8),
            Type::Primitive(PrimitiveType::I16),
            Type::Primitive(PrimitiveType::I32),
            Type::Primitive(PrimitiveType::I64),
            Type::Primitive(PrimitiveType::I128),
            Type::Primitive(PrimitiveType::Isize),
            Type::Primitive(PrimitiveType::U8),
            Type::Primitive(PrimitiveType::U16),
            Type::Primitive(PrimitiveType::U32),
            Type::Primitive(PrimitiveType::U64),
            Type::Primitive(PrimitiveType::U128),
            Type::Primitive(PrimitiveType::Usize),
            Type::Primitive(PrimitiveType::F32),
            Type::Primitive(PrimitiveType::F64),
            Type::Primitive(PrimitiveType::Char),
            Type::String,
            Type::Str,
        ];
        for interface in interfaces {
            for ty in &types {
                let function_name =
                    format!("tn_builtin_ord_{}_{}", interface.0, builtin_type_name(ty));
                let pointer = self.context.ptr_type(AddressSpace::default());
                let string_type = matches!(ty, Type::String | Type::Str);
                let mut parameters = vec![pointer.into()];
                parameters.push(if string_type {
                    self.borrowed_string_type().into()
                } else {
                    pointer.into()
                });
                let function = self.module.add_function(
                    &function_name,
                    self.context.i32_type().fn_type(&parameters, false),
                    None,
                );
                function.set_linkage(Linkage::Private);
                let entry = self.context.append_basic_block(function, "entry");
                let builder = self.context.create_builder();
                builder.position_at_end(entry);
                let receiver = function
                    .get_nth_param(0)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("builtin Ord receiver is missing".into())
                    })?
                    .into_pointer_value();
                let argument = function.get_nth_param(1).ok_or_else(|| {
                    CodegenError::Unsupported("builtin Ord argument is missing".into())
                })?;
                let (less, greater) = if string_type {
                    let receiver = builder
                        .build_load(
                            self.borrowed_string_type(),
                            receiver,
                            "builtin.string.receiver",
                        )?
                        .into_struct_value();
                    let left_pointer = builder
                        .build_extract_value(receiver, 0, "builtin.string.left.pointer")?
                        .into_pointer_value();
                    let left_length = builder
                        .build_extract_value(receiver, 1, "builtin.string.left.length")?
                        .into_int_value();
                    let argument = argument.into_struct_value();
                    let right_pointer = builder
                        .build_extract_value(argument, 0, "builtin.string.right.pointer")?
                        .into_pointer_value();
                    let right_length = builder
                        .build_extract_value(argument, 1, "builtin.string.right.length")?
                        .into_int_value();
                    let comparison = builder
                        .build_call(
                            self.runtime_string_compare(),
                            &[
                                left_pointer.into(),
                                left_length.into(),
                                right_pointer.into(),
                                right_length.into(),
                            ],
                            "builtin.string.compare",
                        )?
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| {
                            CodegenError::Builder("string comparison returned no value".into())
                        })?
                        .into_int_value();
                    (
                        builder.build_int_compare(
                            IntPredicate::SLT,
                            comparison,
                            comparison.get_type().const_zero(),
                            "builtin.string.less",
                        )?,
                        builder.build_int_compare(
                            IntPredicate::SGT,
                            comparison,
                            comparison.get_type().const_zero(),
                            "builtin.string.greater",
                        )?,
                    )
                } else {
                    let argument = argument.into_pointer_value();
                    let value_type = self.basic_type(ty)?;
                    let left = builder.build_load(value_type, receiver, "builtin.ord.left")?;
                    let right = builder.build_load(value_type, argument, "builtin.ord.right")?;
                    match (left, right) {
                        (BasicValueEnum::IntValue(left), BasicValueEnum::IntValue(right)) => {
                            let (less, greater) = if builtin_type_is_signed(ty) {
                                (IntPredicate::SLT, IntPredicate::SGT)
                            } else {
                                (IntPredicate::ULT, IntPredicate::UGT)
                            };
                            (
                                builder.build_int_compare(less, left, right, "builtin.ord.less")?,
                                builder.build_int_compare(
                                    greater,
                                    left,
                                    right,
                                    "builtin.ord.greater",
                                )?,
                            )
                        }
                        (BasicValueEnum::FloatValue(left), BasicValueEnum::FloatValue(right)) => (
                            builder.build_float_compare(
                                FloatPredicate::OLT,
                                left,
                                right,
                                "builtin.ord.less",
                            )?,
                            builder.build_float_compare(
                                FloatPredicate::OGT,
                                left,
                                right,
                                "builtin.ord.greater",
                            )?,
                        ),
                        _ => {
                            return Err(CodegenError::Unsupported(format!(
                                "builtin Ord does not support {ty:?}"
                            )));
                        }
                    }
                };
                let ordering_type = self.context.i32_type();
                let equal = ordering_type.const_int(1, false);
                let less_value = ordering_type.const_zero();
                let greater_value = ordering_type.const_int(2, false);
                let result = builder
                    .build_select(less, less_value, equal, "builtin.ord.less.value")?
                    .into_int_value();
                let result = builder
                    .build_select(greater, greater_value, result, "builtin.ord.greater.value")?
                    .into_int_value();
                builder.build_return(Some(&result))?;

                let table_name = format!(
                    "tn_builtin_witness_{}_{}",
                    interface.0,
                    builtin_type_name(ty)
                );
                let table = self
                    .module
                    .add_global(pointer.array_type(1), None, &table_name);
                table.set_linkage(Linkage::Private);
                let function_pointer = function.as_global_value().as_pointer_value();
                table.set_initializer(&pointer.const_array(&[function_pointer]));
                self.builtin_witnesses
                    .insert((interface, ty.clone()), table.as_pointer_value());
            }
        }
        Ok(())
    }

    fn declare_builtin_hash_witnesses(&mut self) -> Result<(), CodegenError> {
        let interfaces = self
            .layouts
            .interface_names
            .iter()
            .filter_map(|(declaration, name)| (name == "Hash").then_some(*declaration))
            .collect::<Vec<_>>();
        let types = [
            Type::Primitive(PrimitiveType::Bool),
            Type::Primitive(PrimitiveType::I8),
            Type::Primitive(PrimitiveType::I16),
            Type::Primitive(PrimitiveType::I32),
            Type::Primitive(PrimitiveType::I64),
            Type::Primitive(PrimitiveType::I128),
            Type::Primitive(PrimitiveType::Isize),
            Type::Primitive(PrimitiveType::U8),
            Type::Primitive(PrimitiveType::U16),
            Type::Primitive(PrimitiveType::U32),
            Type::Primitive(PrimitiveType::U64),
            Type::Primitive(PrimitiveType::U128),
            Type::Primitive(PrimitiveType::Usize),
            Type::Primitive(PrimitiveType::F32),
            Type::Primitive(PrimitiveType::F64),
            Type::Primitive(PrimitiveType::Char),
            Type::String,
            Type::Str,
        ];
        for interface in interfaces {
            for ty in &types {
                let pointer = self.context.ptr_type(AddressSpace::default());
                let function_name =
                    format!("tn_builtin_hash_{}_{}", interface.0, builtin_type_name(ty));
                let function = self.module.add_function(
                    &function_name,
                    self.context.i64_type().fn_type(&[pointer.into()], false),
                    None,
                );
                function.set_linkage(Linkage::Private);
                let entry = self.context.append_basic_block(function, "entry");
                let builder = self.context.create_builder();
                builder.position_at_end(entry);
                let receiver = function
                    .get_nth_param(0)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("builtin Hash receiver is missing".into())
                    })?
                    .into_pointer_value();
                let hash = if matches!(ty, Type::String | Type::Str) {
                    let receiver = builder
                        .build_load(
                            self.borrowed_string_type(),
                            receiver,
                            "builtin.string.receiver",
                        )?
                        .into_struct_value();
                    let pointer = builder
                        .build_extract_value(receiver, 0, "builtin.string.pointer")?
                        .into_pointer_value();
                    let length = builder
                        .build_extract_value(receiver, 1, "builtin.string.length")?
                        .into_int_value();
                    builder
                        .build_call(
                            self.runtime_bytes_hash(),
                            &[pointer.into(), length.into()],
                            "builtin.string.hash",
                        )?
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| {
                            CodegenError::Builder("string hash returned no value".into())
                        })?
                        .into_int_value()
                } else {
                    let value =
                        builder.build_load(self.basic_type(ty)?, receiver, "builtin.hash.value")?;
                    match value {
                        BasicValueEnum::IntValue(value) => {
                            let width = value.get_type().get_bit_width();
                            if width < 64 {
                                if builtin_type_is_signed(ty) {
                                    builder.build_int_s_extend(
                                        value,
                                        self.context.i64_type(),
                                        "builtin.hash.sign_extend",
                                    )?
                                } else {
                                    builder.build_int_z_extend(
                                        value,
                                        self.context.i64_type(),
                                        "builtin.hash.zero_extend",
                                    )?
                                }
                            } else if width == 64 {
                                value
                            } else {
                                let low = builder.build_int_truncate(
                                    value,
                                    self.context.i64_type(),
                                    "builtin.hash.low",
                                )?;
                                let shifted = builder.build_right_shift(
                                    value,
                                    self.context.i128_type().const_int(64, false),
                                    false,
                                    "builtin.hash.high_shift",
                                )?;
                                let high = builder.build_int_truncate(
                                    shifted,
                                    self.context.i64_type(),
                                    "builtin.hash.high",
                                )?;
                                builder.build_xor(low, high, "builtin.hash.combine")?
                            }
                        }
                        BasicValueEnum::FloatValue(value) => {
                            if value.get_type() == self.context.f32_type() {
                                let bits = builder
                                    .build_bit_cast(
                                        value,
                                        self.context.i32_type(),
                                        "builtin.hash.f32_bits",
                                    )?
                                    .into_int_value();
                                builder.build_int_z_extend(
                                    bits,
                                    self.context.i64_type(),
                                    "builtin.hash.f32_extend",
                                )?
                            } else {
                                builder
                                    .build_bit_cast(
                                        value,
                                        self.context.i64_type(),
                                        "builtin.hash.f64_bits",
                                    )?
                                    .into_int_value()
                            }
                        }
                        _ => {
                            return Err(CodegenError::Unsupported(format!(
                                "builtin Hash does not support {ty:?}"
                            )));
                        }
                    }
                };
                builder.build_return(Some(&hash))?;

                let table_name = format!(
                    "tn_builtin_witness_{}_{}",
                    interface.0,
                    builtin_type_name(ty)
                );
                let table = self
                    .module
                    .add_global(pointer.array_type(1), None, &table_name);
                table.set_linkage(Linkage::Private);
                table.set_initializer(
                    &pointer.const_array(&[function.as_global_value().as_pointer_value()]),
                );
                self.builtin_witnesses
                    .insert((interface, ty.clone()), table.as_pointer_value());
            }
        }
        Ok(())
    }

    fn declare_builtin_equal_witnesses(&mut self) -> Result<(), CodegenError> {
        let interfaces = self
            .layouts
            .interface_names
            .iter()
            .filter_map(|(declaration, name)| (name == "Equal").then_some(*declaration))
            .collect::<Vec<_>>();
        let types = [
            Type::Primitive(PrimitiveType::Bool),
            Type::Primitive(PrimitiveType::I8),
            Type::Primitive(PrimitiveType::I16),
            Type::Primitive(PrimitiveType::I32),
            Type::Primitive(PrimitiveType::I64),
            Type::Primitive(PrimitiveType::I128),
            Type::Primitive(PrimitiveType::Isize),
            Type::Primitive(PrimitiveType::U8),
            Type::Primitive(PrimitiveType::U16),
            Type::Primitive(PrimitiveType::U32),
            Type::Primitive(PrimitiveType::U64),
            Type::Primitive(PrimitiveType::U128),
            Type::Primitive(PrimitiveType::Usize),
            Type::Primitive(PrimitiveType::F32),
            Type::Primitive(PrimitiveType::F64),
            Type::Primitive(PrimitiveType::Char),
            Type::String,
            Type::Str,
        ];
        for interface in interfaces {
            for ty in &types {
                let pointer = self.context.ptr_type(AddressSpace::default());
                let function_name =
                    format!("tn_builtin_equal_{}_{}", interface.0, builtin_type_name(ty));
                let string_type = matches!(ty, Type::String | Type::Str);
                let mut parameters = vec![pointer.into()];
                parameters.push(if string_type {
                    self.borrowed_string_type().into()
                } else {
                    pointer.into()
                });
                let function = self.module.add_function(
                    &function_name,
                    self.context.bool_type().fn_type(&parameters, false),
                    None,
                );
                function.set_linkage(Linkage::Private);
                let entry = self.context.append_basic_block(function, "entry");
                let builder = self.context.create_builder();
                builder.position_at_end(entry);
                let receiver = function
                    .get_nth_param(0)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("builtin Equal receiver is missing".into())
                    })?
                    .into_pointer_value();
                let argument = function.get_nth_param(1).ok_or_else(|| {
                    CodegenError::Unsupported("builtin Equal argument is missing".into())
                })?;
                let equal = if string_type {
                    let receiver = builder
                        .build_load(
                            self.borrowed_string_type(),
                            receiver,
                            "builtin.string.receiver",
                        )?
                        .into_struct_value();
                    let left_pointer = builder
                        .build_extract_value(receiver, 0, "builtin.string.left.pointer")?
                        .into_pointer_value();
                    let left_length = builder
                        .build_extract_value(receiver, 1, "builtin.string.left.length")?
                        .into_int_value();
                    let argument = argument.into_struct_value();
                    let right_pointer = builder
                        .build_extract_value(argument, 0, "builtin.string.right.pointer")?
                        .into_pointer_value();
                    let right_length = builder
                        .build_extract_value(argument, 1, "builtin.string.right.length")?
                        .into_int_value();
                    let result = builder
                        .build_call(
                            self.runtime_string_equals(),
                            &[
                                left_pointer.into(),
                                left_length.into(),
                                right_pointer.into(),
                                right_length.into(),
                            ],
                            "builtin.string.equals",
                        )?
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| {
                            CodegenError::Builder("string equality returned no value".into())
                        })?
                        .into_int_value();
                    builder.build_int_compare(
                        IntPredicate::NE,
                        result,
                        result.get_type().const_zero(),
                        "builtin.string.equals.test",
                    )?
                } else {
                    let argument = argument.into_pointer_value();
                    let left =
                        builder.build_load(self.basic_type(ty)?, receiver, "builtin.equal.left")?;
                    let right = builder.build_load(
                        self.basic_type(ty)?,
                        argument,
                        "builtin.equal.right",
                    )?;
                    match (left, right) {
                        (BasicValueEnum::IntValue(left), BasicValueEnum::IntValue(right)) => {
                            builder.build_int_compare(
                                IntPredicate::EQ,
                                left,
                                right,
                                "builtin.equal.int",
                            )?
                        }
                        (BasicValueEnum::FloatValue(left), BasicValueEnum::FloatValue(right)) => {
                            builder.build_float_compare(
                                FloatPredicate::OEQ,
                                left,
                                right,
                                "builtin.equal.float",
                            )?
                        }
                        _ => {
                            return Err(CodegenError::Unsupported(format!(
                                "builtin Equal does not support {ty:?}"
                            )));
                        }
                    }
                };
                builder.build_return(Some(&equal))?;

                let table_name = format!(
                    "tn_builtin_witness_{}_{}",
                    interface.0,
                    builtin_type_name(ty)
                );
                let table = self
                    .module
                    .add_global(pointer.array_type(1), None, &table_name);
                table.set_linkage(Linkage::Private);
                table.set_initializer(
                    &pointer.const_array(&[function.as_global_value().as_pointer_value()]),
                );
                self.builtin_witnesses
                    .insert((interface, ty.clone()), table.as_pointer_value());
            }
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

    fn runtime_thread_spawn_task(&self) -> FunctionValue<'ctx> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        self.module
            .get_function("tn_thread_spawn_task")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_thread_spawn_task",
                    pointer.fn_type(
                        &[
                            pointer.into(),
                            pointer.into(),
                            pointer.into(),
                            pointer.into(),
                            self.pointer_int_type().into(),
                        ],
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
                            self.pointer_int_type().into(),
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

    fn runtime_string_compare(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_string_compare")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_string_compare",
                    self.context.i32_type().fn_type(
                        &[
                            self.context
                                .ptr_type(AddressSpace::default())
                                .as_basic_type_enum()
                                .into(),
                            self.pointer_int_type().into(),
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

    fn runtime_bytes_hash(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_bytes_hash")
            .unwrap_or_else(|| {
                let pointer = self.context.ptr_type(AddressSpace::default());
                self.module.add_function(
                    "tn_bytes_hash",
                    self.context
                        .i64_type()
                        .fn_type(&[pointer.into(), self.pointer_int_type().into()], false),
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

    fn runtime_string_length(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_string_length")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_string_length",
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

    fn runtime_string_scalar_length_bytes(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_string_scalar_length_bytes")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_string_scalar_length_bytes",
                    self.pointer_int_type().fn_type(
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

    fn runtime_string_free(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_string_free")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_string_free",
                    self.context.void_type().fn_type(
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

    fn runtime_arc_retain(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_arc_retain")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_arc_retain",
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

    fn runtime_arc_upgrade(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_arc_upgrade")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_arc_upgrade",
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

    fn runtime_async_result(&self) -> FunctionValue<'ctx> {
        self.module
            .get_function("tn_runtime_async_result")
            .unwrap_or_else(|| {
                self.module.add_function(
                    "tn_runtime_async_result",
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
            FunctionGenerator::new(self, &unit.instance, &unit.body, function)
                .and_then(|generator| generator.lower())
                .map_err(|error| {
                    let unresolved_locals = unit
                        .body
                        .locals
                        .iter()
                        .enumerate()
                        .filter(|(_, local)| matches!(local.ty, Type::Error))
                        .map(|(index, local)| format!("{index}:{:?}@{:?}", local.name, local.span))
                        .collect::<Vec<_>>();
                    CodegenError::Unsupported(format!(
                        "while lowering {} ({:?}, unresolved locals {:?}): {error}",
                        function.get_name().to_string_lossy(),
                        unit.instance,
                        unresolved_locals,
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
            poll_builder.build_return(Some(&self.context.bool_type().const_all_ones()))?;

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
            drop_builder.build_return(Some(&self.context.bool_type().const_all_ones()))?;

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
            if wrapper.kind == AbiWrapperKind::FallibleValue {
                let failed_test = builder.build_int_compare(
                    IntPredicate::NE,
                    failed,
                    failed.get_type().const_zero(),
                    "abi.failed.test",
                )?;
                let failed_block = self
                    .context
                    .append_basic_block(wrapper.wrapper, "abi.failed");
                let success_block = self
                    .context
                    .append_basic_block(wrapper.wrapper, "abi.success");
                builder.build_conditional_branch(failed_test, failed_block, success_block)?;

                builder.position_at_end(failed_block);
                let error = builder
                    .build_extract_value(call, 2, "abi.error")?
                    .into_pointer_value();
                let error =
                    builder.build_ptr_to_int(error, self.context.i64_type(), "abi.error.wide")?;
                let failed_wide = builder.build_int_z_extend(
                    failed,
                    self.context.i64_type(),
                    "abi.failed.wide",
                )?;
                let failed_value = self.context.i64_type().array_type(2).const_zero();
                let failed_value = builder
                    .build_insert_value(failed_value, failed_wide, 0, "abi.failed.field")?
                    .into_array_value();
                let failed_value = builder
                    .build_insert_value(failed_value, error, 1, "abi.error.field")?
                    .into_array_value();
                builder.build_return(Some(&failed_value))?;

                builder.position_at_end(success_block);
                let value = builder.build_extract_value(call, 1, "abi.value")?;
                let payload =
                    self.abi_payload_to_i64(&builder, value, &wrapper.signature.result)?;
                let success_value = self.context.i64_type().array_type(2).const_zero();
                let success_value = builder
                    .build_insert_value(
                        success_value,
                        self.context.i64_type().const_zero(),
                        0,
                        "abi.success.field",
                    )?
                    .into_array_value();
                let success_value = builder
                    .build_insert_value(success_value, payload, 1, "abi.value.field")?
                    .into_array_value();
                builder.build_return(Some(&success_value))?;
                continue;
            }
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
                    unreachable!("fallible value wrapper handled above")
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
            || matches!(
                ty,
                Type::Reference { referent, .. }
                    if matches!(referent.as_ref(), Type::Slice(_) | Type::String | Type::Str)
            )
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

    fn callable_type(&self) -> StructType<'ctx> {
        let pointer = self.context.ptr_type(AddressSpace::default());
        self.context.struct_type(
            &[
                pointer.into(),
                pointer.into(),
                pointer.into(),
                self.context.i64_type().into(),
            ],
            false,
        )
    }

    fn basic_type(&self, ty: &Type) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        let resolved = self.resolve_alias(ty);
        if resolved != *ty {
            return self.basic_type(&resolved);
        }
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
            Type::Reference { referent, .. } if matches!(referent.as_ref(), Type::Slice(_)) => {
                self.basic_type(referent)?
            }
            Type::Reference { referent, .. } if matches!(referent.as_ref(), Type::Str) => {
                self.borrowed_string_type().into()
            }
            Type::Reference {
                mutable: true,
                referent,
                ..
            } if matches!(referent.as_ref(), Type::String) => pointer(),
            Type::Reference { referent, .. } if matches!(referent.as_ref(), Type::String) => {
                self.borrowed_string_type().into()
            }
            Type::Reference { .. }
            | Type::RawPointer { .. }
            | Type::String
            | Type::Str
            | Type::Promise { .. }
            | Type::Template(_)
            | Type::ErrorUnion(_) => pointer(),
            Type::Function(_) => self.callable_type().into(),
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

    fn is_copy_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Primitive(_) | Type::RawPointer { .. } => true,
            Type::Reference { mutable, .. } => !mutable,
            Type::Optional(inner) | Type::Array(inner, _) => self.is_copy_type(inner),
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().all(|element| self.is_copy_type(element))
            }
            Type::Nominal(declaration, _) => self.layouts.copies.contains(declaration),
            Type::ErrorUnion(effects) => effects
                .iter()
                .all(|effect| self.layouts.copies.contains(effect)),
            Type::Function(_)
            | Type::Promise { .. }
            | Type::String
            | Type::Str
            | Type::Slice(_)
            | Type::DynamicInterface(_, _)
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
        }
    }

    fn nominal_type(
        &self,
        declaration: DeclarationId,
        arguments: &[Type],
    ) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        let Some(layout) = self.layouts.nominals.get(&declaration) else {
            return Ok(self.context.ptr_type(AddressSpace::default()).into());
        };
        let arguments = arguments
            .iter()
            .filter(|argument| !matches!(argument, Type::Lifetime(_)))
            .cloned()
            .collect::<Vec<_>>();
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
            .zip(arguments)
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
                if *c_repr || variants.iter().all(Vec::is_empty) {
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

    fn resolve_alias(&self, ty: &Type) -> Type {
        let mut current = ty.clone();
        let mut visited = BTreeSet::new();
        loop {
            let Type::Nominal(declaration, arguments) = &current else {
                return current;
            };
            if !visited.insert(*declaration) {
                return current;
            }
            let Some(alias) = self.layouts.aliases.get(declaration) else {
                return current;
            };
            if alias.parameters.len() != arguments.len() {
                return current;
            }
            let substitutions = alias
                .parameters
                .iter()
                .cloned()
                .zip(arguments.iter().cloned())
                .collect::<BTreeMap<_, _>>();
            current = instantiate_type(&alias.body, &substitutions);
        }
    }

    fn normalize_alias_deep(&self, ty: &Type) -> Type {
        let ty = self.resolve_alias(ty);
        match ty {
            Type::Nominal(declaration, arguments) => Type::Nominal(
                declaration,
                arguments
                    .iter()
                    .map(|argument| self.normalize_alias_deep(argument))
                    .collect(),
            ),
            Type::DynamicInterface(declaration, arguments) => Type::DynamicInterface(
                declaration,
                arguments
                    .iter()
                    .map(|argument| self.normalize_alias_deep(argument))
                    .collect(),
            ),
            Type::Promise {
                result,
                error,
                effects,
            } => Type::Promise {
                result: Box::new(self.normalize_alias_deep(&result)),
                error: Box::new(self.normalize_alias_deep(&error)),
                effects,
            },
            Type::Optional(inner) => Type::Optional(Box::new(self.normalize_alias_deep(&inner))),
            Type::Array(inner, length) => {
                Type::Array(Box::new(self.normalize_alias_deep(&inner)), length)
            }
            Type::Slice(inner) => Type::Slice(Box::new(self.normalize_alias_deep(&inner))),
            Type::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|element| self.normalize_alias_deep(element))
                    .collect(),
            ),
            Type::Reference {
                mutable,
                lifetime,
                referent,
            } => Type::Reference {
                mutable,
                lifetime,
                referent: Box::new(self.normalize_alias_deep(&referent)),
            },
            Type::RawPointer { mutable, pointee } => Type::RawPointer {
                mutable,
                pointee: Box::new(self.normalize_alias_deep(&pointee)),
            },
            Type::Function(function) => Type::Function(FunctionType {
                parameters: function
                    .parameters
                    .iter()
                    .map(|parameter| self.normalize_alias_deep(parameter))
                    .collect(),
                result: Box::new(self.normalize_alias_deep(&function.result)),
                effects: function.effects,
                generics: function.generics,
                is_async: function.is_async,
                is_unsafe: function.is_unsafe,
            }),
            Type::Template(elements) => Type::Template(
                elements
                    .iter()
                    .map(|element| self.normalize_alias_deep(element))
                    .collect(),
            ),
            Type::Primitive(_)
            | Type::String
            | Type::Str
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::ErrorUnion(_)
            | Type::Error
            | Type::Unknown => ty,
        }
    }

    fn normalize_function_type(&self, function: &FunctionType) -> FunctionType {
        FunctionType {
            parameters: function
                .parameters
                .iter()
                .map(|parameter| self.normalize_alias_deep(parameter))
                .collect(),
            result: Box::new(self.normalize_alias_deep(&function.result)),
            effects: function.effects.clone(),
            generics: function.generics.clone(),
            is_async: function.is_async,
            is_unsafe: function.is_unsafe,
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

    fn borrowed_string_type(&self) -> StructType<'ctx> {
        self.context.struct_type(
            &[
                self.context.ptr_type(AddressSpace::default()).into(),
                self.pointer_int_type().into(),
            ],
            false,
        )
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
        let arguments = arguments
            .iter()
            .filter(|argument| !matches!(argument, Type::Lifetime(_)))
            .cloned()
            .collect::<Vec<_>>();
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
            .zip(arguments)
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

    fn is_constructor_initializer(&self, instance: &Instance) -> bool {
        let Some(layout) = self.layouts.nominals.get(&instance.callable.declaration) else {
            return false;
        };
        let NominalKind::Class {
            constructor: Some(constructor),
            ..
        } = &layout.kind
        else {
            return false;
        };
        instance.callable.member == Some(constructor.member)
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
    instance: &'a Instance,
    body: &'a Body,
    function: FunctionValue<'ctx>,
    builder: Builder<'ctx>,
    blocks: Vec<LlvmBlock<'ctx>>,
    locals: Vec<PointerValue<'ctx>>,
    /// Initialization state is tracked per ownership path, not just per local. A destructured
    /// field may be moved while its containing aggregate remains live, so a root-only flag would
    /// call a destructor on storage that no longer contains a valid field value.
    drop_flags: BTreeMap<Place, PointerValue<'ctx>>,
    /// Constructor wrappers allocate zeroed class storage and then invoke the initializer.  The
    /// initializer must not release those zeroed slots, while ordinary methods may replace live
    /// class fields even though their receiver-owned paths do not have local drop flags.
    constructor_initializer: bool,
}

impl<'a, 'ctx> FunctionGenerator<'a, 'ctx> {
    fn new(
        generator: &'a Generator<'ctx>,
        instance: &'a Instance,
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
        let mut drop_flags = BTreeMap::new();
        for (index, local) in body.locals.iter().enumerate() {
            locals.push(
                builder
                    .build_alloca(generator.basic_type(&local.ty)?, &format!("local.{index}"))?,
            );
            let flag = builder
                .build_alloca(generator.context.bool_type(), &format!("dropflag.{index}"))?;
            builder.build_store(flag, generator.context.bool_type().const_zero())?;
            drop_flags.insert(
                Place::local(tn_mir::LocalId(u32::try_from(index).map_err(|_| {
                    CodegenError::Unsupported("local index overflow".into())
                })?)),
                flag,
            );
        }
        let projection_flags = body
            .blocks
            .iter()
            .flat_map(|block| &block.statements)
            .filter_map(|statement| match &statement.kind {
                StatementKind::SetDropFlag(place, _) if !place.projection.is_empty() => {
                    Some(place.clone())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        for place in projection_flags {
            let flag = builder.build_alloca(
                generator.context.bool_type(),
                &format!("dropflag.path.{}", drop_flags.len()),
            )?;
            builder.build_store(flag, generator.context.bool_type().const_zero())?;
            drop_flags.insert(place, flag);
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
        let constructor_initializer = generator.is_constructor_initializer(instance);
        Ok(Self {
            generator,
            instance,
            body,
            function,
            builder,
            blocks,
            locals,
            drop_flags,
            constructor_initializer,
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
                self.lower_assignment(destination, value)?;
            }
            StatementKind::SetDropFlag(place, value) => {
                if self.is_borrowed_class_receiver(place) {
                    return Ok(());
                }
                self.set_drop_flag_value(place, *value)?;
            }
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Retag(_) => {}
            StatementKind::Borrow {
                destination, place, ..
            } => self.lower_borrow_statement(*destination, place)?,
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
                                || matches!(
                                    layout.kind,
                                    NominalKind::Enum { ref variants, .. }
                                        if variants.iter().all(Vec::is_empty)
                                )
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

    /// Store a value into a place after releasing the value currently occupying that place.
    ///
    /// MIR assignments are allowed to target already-initialized locals and projections (for
    /// example `this.render = undefined`).  A plain LLVM store would overwrite the owner without
    /// running its destructor, leaking strings, closures, arrays, or class objects.  The drop
    /// flag is initialized to false for fresh storage and is set by drop elaboration after each
    /// successful assignment, so this conditional release is both idempotent and valid for
    /// constructor fields.
    fn lower_assignment(&self, destination: &Place, rvalue: &Rvalue) -> Result<(), CodegenError> {
        let value = self.lower_rvalue(rvalue)?;
        let pointer = self.place_pointer(destination)?;
        let destination_type = self.place_type(destination)?;
        if !self.type_needs_drop(&destination_type) {
            self.builder.build_store(pointer, value)?;
            return Ok(());
        }
        if self.is_class_field_place(destination) {
            if !self.constructor_initializer {
                self.lower_drop_value_at_pointer(pointer, &destination_type, Some(destination))?;
            }
            self.builder.build_store(pointer, value)?;
            return Ok(());
        }
        let Some(flag) = self.drop_flags.get(destination).copied() else {
            self.builder.build_store(pointer, value)?;
            return Ok(());
        };
        if self.is_borrowed_class_receiver(destination) {
            self.builder.build_store(pointer, value)?;
            return Ok(());
        }

        let initialized = self
            .builder
            .build_load(
                self.generator.context.bool_type(),
                flag,
                "assign.destination.initialized",
            )?
            .into_int_value();
        let drop_block = self
            .generator
            .context
            .append_basic_block(self.function, "assign.destination.drop");
        let store_block = self
            .generator
            .context
            .append_basic_block(self.function, "assign.destination.store");
        self.builder
            .build_conditional_branch(initialized, drop_block, store_block)?;

        self.builder.position_at_end(drop_block);
        self.lower_drop_value_at_pointer(pointer, &destination_type, Some(destination))?;
        self.set_drop_flag_value(destination, false)?;
        self.builder.build_unconditional_branch(store_block)?;

        self.builder.position_at_end(store_block);
        self.builder.build_store(pointer, value)?;
        Ok(())
    }

    fn is_class_field_place(&self, place: &Place) -> bool {
        if place.projection.is_empty() {
            return false;
        }
        let Ok(mut ty) = self.local_type(place.local.0) else {
            return false;
        };
        for projection in &place.projection {
            match projection {
                Projection::Field { ty: field, .. } => {
                    if self
                        .generator
                        .is_class_type(&self.generator.resolve_alias(&ty))
                    {
                        return true;
                    }
                    ty = field.clone();
                }
                Projection::Dereference => {
                    ty = match ty {
                        Type::Reference { referent, .. } => *referent,
                        Type::RawPointer { pointee, .. } => *pointee,
                        _ => return false,
                    };
                }
                Projection::Index(_) => {
                    ty = match ty {
                        Type::Array(element, _) | Type::Slice(element) => *element,
                        _ => return false,
                    };
                }
                Projection::Downcast(1) => {
                    ty = match ty {
                        Type::Optional(inner) => *inner,
                        Type::Nominal(_, _) => ty,
                        _ => return false,
                    };
                }
                Projection::BaseClass(base) => {
                    ty = Type::Nominal(*base, Vec::new());
                }
                Projection::Downcast(_) => {}
            }
        }
        false
    }

    fn type_needs_drop(&self, ty: &Type) -> bool {
        let resolved = self.generator.resolve_alias(ty);
        if resolved != *ty {
            return self.type_needs_drop(&resolved);
        }
        match ty {
            Type::String
            | Type::Function(_)
            | Type::Promise { .. }
            | Type::DynamicInterface(_, _) => true,
            Type::Optional(inner) | Type::Array(inner, _) => self.type_needs_drop(inner),
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().any(|element| self.type_needs_drop(element))
            }
            Type::Nominal(declaration, _) => {
                let Some(layout) = self.generator.layouts.nominals.get(declaration) else {
                    return false;
                };
                match &layout.kind {
                    NominalKind::Struct { fields } => {
                        fields.iter().any(|field| self.type_needs_drop(field))
                    }
                    NominalKind::Enum { variants, .. } => variants
                        .iter()
                        .flatten()
                        .any(|field| self.type_needs_drop(field)),
                    NominalKind::Class { .. } => true,
                }
            }
            Type::ErrorUnion(effects) => effects.iter().any(|effect| {
                self.generator
                    .layouts
                    .nominals
                    .get(effect)
                    .is_some_and(|layout| match &layout.kind {
                        NominalKind::Struct { fields } => {
                            fields.iter().any(|field| self.type_needs_drop(field))
                        }
                        NominalKind::Enum { variants, .. } => variants
                            .iter()
                            .flatten()
                            .any(|field| self.type_needs_drop(field)),
                        NominalKind::Class { .. } => true,
                    })
            }),
            Type::Primitive(_)
            | Type::Str
            | Type::Slice(_)
            | Type::Reference { .. }
            | Type::RawPointer { .. }
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
        }
    }

    fn lower_borrow_statement(
        &self,
        destination: tn_mir::LocalId,
        place: &Place,
    ) -> Result<(), CodegenError> {
        let destination_type = self.local_type(destination.0)?;
        let destination = self.locals[usize::try_from(destination.0)
            .map_err(|_| CodegenError::Unsupported("borrow destination index overflow".into()))?];
        if matches!(
            &destination_type,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::Slice(_))
        ) {
            let source_type = self.place_type(place)?;
            let value = self.builder.build_load(
                self.generator.basic_type(&source_type)?,
                self.place_pointer(place)?,
                "slice.borrow",
            )?;
            self.builder.build_store(destination, value)?;
        } else if matches!(
            &destination_type,
            Type::Reference {
                mutable: true,
                referent,
                ..
            } if matches!(referent.as_ref(), Type::String)
        ) {
            self.builder
                .build_store(destination, self.place_pointer(place)?)?;
        } else if matches!(
            &destination_type,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::String | Type::Str)
        ) {
            let value = self.lower_borrowed_string_from_place(place)?;
            self.builder.build_store(destination, value)?;
        } else if matches!(
            &destination_type,
            Type::Reference { referent, .. }
                if self.generator.is_class_type(referent)
        ) {
            // A dereference place already resolves to the class object's address.  Loading
            // from it would read the descriptor header and turn the reference into a bogus
            // object pointer (notably for `&mut *(raw as *mut Class)`).  Named/field places,
            // on the other hand, store the class pointer in their slot and still need a load.
            let value = if matches!(place.projection.last(), Some(Projection::Dereference)) {
                self.place_pointer(place)?.into()
            } else {
                self.builder.build_load(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    self.place_pointer(place)?,
                    "class.borrow",
                )?
            };
            self.builder.build_store(destination, value)?;
        } else {
            self.builder
                .build_store(destination, self.place_pointer(place)?)?;
        }
        Ok(())
    }

    fn lower_borrowed_string_from_place(
        &self,
        place: &Place,
    ) -> Result<StructValue<'ctx>, CodegenError> {
        let source_type = self.place_type(place)?;
        let source_pointer = self.place_pointer(place)?;
        if matches!(
            &source_type,
            Type::Reference {
                mutable: true,
                referent,
                ..
            } if matches!(referent.as_ref(), Type::String)
        ) {
            let slot = self
                .builder
                .build_load(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    source_pointer,
                    "borrowed.mutable.string.slot",
                )?
                .into_pointer_value();
            let pointer = self
                .builder
                .build_load(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    slot,
                    "borrowed.mutable.string.pointer",
                )?
                .into_pointer_value();
            let length = self
                .builder
                .build_call(
                    self.generator.runtime_string_length(),
                    &[pointer.into()],
                    "borrowed.mutable.string.length",
                )?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("string length returned void".into()))?
                .into_int_value();
            return self.make_borrowed_string(pointer, length);
        }
        if matches!(
            &source_type,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::String | Type::Str)
        ) {
            return Ok(self
                .builder
                .build_load(
                    self.generator.borrowed_string_type(),
                    source_pointer,
                    "borrowed.string.reborrow",
                )?
                .into_struct_value());
        }
        if self.place_is_fat_string_dereference(place)? {
            return Ok(self
                .builder
                .build_load(
                    self.generator.borrowed_string_type(),
                    source_pointer,
                    "borrowed.string.dereference",
                )?
                .into_struct_value());
        }
        if !matches!(&source_type, Type::String | Type::Str) {
            return Err(CodegenError::Unsupported(format!(
                "borrowed string source has unsupported type: {source_type:?}"
            )));
        }
        let pointer = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                source_pointer,
                "borrowed.string.pointer",
            )?
            .into_pointer_value();
        let length = self
            .builder
            .build_call(
                self.generator.runtime_string_length(),
                &[pointer.into()],
                "borrowed.string.length",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("string length returned void".into()))?
            .into_int_value();
        self.make_borrowed_string(pointer, length)
    }

    fn make_borrowed_string(
        &self,
        pointer: PointerValue<'ctx>,
        length: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>, CodegenError> {
        let structure = self.generator.borrowed_string_type();
        let value = self
            .builder
            .build_insert_value(
                structure.const_zero(),
                pointer,
                0,
                "borrowed.string.pointer",
            )?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(value, length, 1, "borrowed.string.length")?
            .into_struct_value())
    }

    fn lower_borrowed_string_from_address(
        &self,
        address: PointerValue<'ctx>,
        pointee: &Type,
    ) -> Result<StructValue<'ctx>, CodegenError> {
        if matches!(
            pointee,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::String | Type::Str)
        ) {
            return Ok(self
                .builder
                .build_load(
                    self.generator.borrowed_string_type(),
                    address,
                    "borrowed.element.string.reborrow",
                )?
                .into_struct_value());
        }
        if !matches!(pointee, Type::String | Type::Str) {
            return Err(CodegenError::Unsupported(format!(
                "borrowed string element has unsupported type: {pointee:?}"
            )));
        }
        let pointer = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                address,
                "borrowed.element.string.pointer",
            )?
            .into_pointer_value();
        let length = self
            .builder
            .build_call(
                self.generator.runtime_string_length(),
                &[pointer.into()],
                "borrowed.element.string.length",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("string length returned void".into()))?
            .into_int_value();
        self.make_borrowed_string(pointer, length)
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
            Rvalue::Closure { id, captures, .. } => {
                let target = self
                    .generator
                    .closures
                    .get(&ClosureKey {
                        instance: self.instance.clone(),
                        id: *id,
                    })
                    .ok_or_else(|| CodegenError::Unsupported("closure target is missing".into()))?;
                let pointer = self.generator.context.ptr_type(AddressSpace::default());
                let (environment, drop) = if let Some(environment_type) = target.environment {
                    let size = environment_type.size_of().ok_or_else(|| {
                        CodegenError::Unsupported("closure environment size is unavailable".into())
                    })?;
                    let environment = self
                        .builder
                        .build_call(
                            self.generator.runtime_alloc(),
                            &[size.into()],
                            "closure.env.alloc",
                        )?
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| {
                            CodegenError::Builder(
                                "closure environment allocation returned void".into(),
                            )
                        })?
                        .into_pointer_value();
                    let consumed = self.builder.build_struct_gep(
                        environment_type,
                        environment,
                        0,
                        "closure.consumed",
                    )?;
                    self.builder
                        .build_store(consumed, self.generator.context.bool_type().const_zero())?;
                    for (index, capture) in captures.iter().enumerate() {
                        let address = self.builder.build_struct_gep(
                            environment_type,
                            environment,
                            u32::try_from(index + 1).map_err(|_| {
                                CodegenError::Unsupported("closure capture limit".into())
                            })?,
                            "closure.capture.store",
                        )?;
                        self.builder
                            .build_store(address, self.lower_operand(capture)?)?;
                    }
                    (
                        environment,
                        target.drop.map_or(pointer.const_null(), |drop| {
                            drop.as_global_value().as_pointer_value()
                        }),
                    )
                } else {
                    (pointer.const_null(), pointer.const_null())
                };
                let callable_type = self.generator.callable_type();
                let mut callable = callable_type.const_zero();
                callable = self
                    .builder
                    .build_insert_value(
                        callable,
                        target.trampoline.as_global_value().as_pointer_value(),
                        0,
                        "closure.code",
                    )?
                    .into_struct_value();
                callable = self
                    .builder
                    .build_insert_value(callable, environment, 1, "closure.environment")?
                    .into_struct_value();
                callable = self
                    .builder
                    .build_insert_value(callable, drop, 2, "closure.drop")?
                    .into_struct_value();
                Ok(self
                    .builder
                    .build_insert_value(
                        callable,
                        self.generator.context.i64_type().const_int(
                            stable_hash(&format!("closure:{:?}:{:?}", self.instance, id)),
                            false,
                        ),
                        3,
                        "closure.identity",
                    )?
                    .into_struct_value()
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "thread_spawn" => self.lower_thread_spawn(operands, ty),
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
                let source_type = self.operand_type(operand)?;
                if matches!(source_type, Type::Function(_)) && matches!(ty, Type::RawPointer { .. })
                {
                    // A plain function cast to a raw pointer is used by the C ABI (for
                    // example, pthread entry points).  The callable representation adds an
                    // environment parameter for indirect language calls, but a C callback
                    // receives only its declared arguments.  Preserve the emitted function
                    // pointer for a direct function constant instead of exposing the language
                    // adapter, whose hidden environment argument would shift the callback's
                    // first argument.
                    if let Operand::Constant(Constant::Function(declaration, function_ty)) = operand
                    {
                        let Type::Function(function_type) = function_ty else {
                            return Err(CodegenError::Unsupported(
                                "function cast lacks a function type".into(),
                            ));
                        };
                        let function = self.resolve_emitted_callable(
                            Callable::function(*declaration),
                            function_type,
                        )?;
                        return Ok(function.as_global_value().as_pointer_value().into());
                    }
                    let callable = self.lower_operand(operand)?.into_struct_value();
                    return Ok(self.builder.build_extract_value(
                        callable,
                        0,
                        "callable.code.cast",
                    )?);
                }
                if matches!(
                    source_type,
                    Type::Reference {
                        mutable,
                        referent,
                        ..
                    }
                        if matches!(referent.as_ref(), Type::String | Type::Str)
                            && (!mutable || matches!(referent.as_ref(), Type::Str))
                ) && matches!(ty, Type::RawPointer { .. })
                {
                    let value = self.builder.build_extract_value(
                        self.lower_operand(operand)?.into_struct_value(),
                        0,
                        "borrowed.string.pointer",
                    )?;
                    return self.lower_cast(value, self.generator.basic_type(ty)?);
                }
                let value = self.lower_operand(operand)?;
                let target = self.generator.basic_type(ty)?;
                self.lower_cast(value, target).map_err(|error| {
                    CodegenError::Unsupported(format!(
                        "{error}; residual cast source {:?}, target {:?}, kind {:?}",
                        self.operand_type(operand).unwrap_or(Type::Error),
                        ty,
                        kind,
                    ))
                })
            }
            Rvalue::DirectMethod {
                implementation,
                member,
                ty,
                object,
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
                let receiver = self.lower_receiver_operand(&Operand::Copy(object.clone()))?;
                let receiver = receiver.into_pointer_value();
                let pointer = self
                    .generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null();
                let _ = function_type;
                self.callable_value(
                    function.as_global_value().as_pointer_value(),
                    receiver,
                    pointer,
                    self.generator.context.i64_type().const_int(
                        stable_hash(&format!("method:{implementation:?}:{member:?}")),
                        false,
                    ),
                )
                .map(Into::into)
            }
            Rvalue::VtableLookup { object, slot, .. } => {
                let code = self.lower_vtable_lookup(object, *slot)?;
                let receiver = self.lower_class_object_pointer(object)?;
                let null = self
                    .generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null();
                self.callable_value(
                    code,
                    receiver,
                    null,
                    self.generator.context.i64_type().const_zero(),
                )
                .map(Into::into)
            }
            Rvalue::WitnessLookup { object, slot, .. } => {
                let code = self.lower_witness_lookup(object, *slot)?;
                let receiver = self
                    .lower_receiver_operand(&Operand::Copy(object.clone()))?
                    .into_pointer_value();
                let null = self
                    .generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null();
                self.callable_value(
                    code,
                    receiver,
                    null,
                    self.generator.context.i64_type().const_zero(),
                )
                .map(Into::into)
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
                ty,
            } if operation == "binding_rest" => self.lower_binding_rest(operands, ty),
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if matches!(
                operation.as_str(),
                "atomic_i32_load"
                    | "atomic_i32_store"
                    | "atomic_i32_fetch_add"
                    | "atomic_i32_compare_exchange"
                    | "atomic_u64_load"
                    | "atomic_u64_store"
                    | "atomic_u64_fetch_add"
                    | "atomic_u64_compare_exchange"
                    | "atomic_usize_load"
                    | "atomic_usize_store"
                    | "atomic_usize_fetch_add"
                    | "atomic_usize_compare_exchange"
                    | "atomic_fence"
            ) =>
            {
                self.lower_atomic_operation(operation, operands, ty)
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
            Rvalue::RawOperation { operation, ty, .. }
                if operation == "platform_sockaddr_family" =>
            {
                let result = self.generator.basic_type(ty)?.into_int_type();
                let value = if self.generator.is_macos { 528 } else { 2 };
                Ok(result.const_int(value, false).into())
            }
            Rvalue::RawOperation { operation, ty, .. }
                if operation == "platform_socket_reuse_address_option" =>
            {
                let result = self.generator.basic_type(ty)?.into_int_type();
                let value = if self.generator.is_macos { 4 } else { 2 };
                Ok(result.const_int(value, false).into())
            }
            Rvalue::RawOperation { operation, ty, .. } if operation == "platform_socket_level" => {
                let result = self.generator.basic_type(ty)?.into_int_type();
                let value = if self.generator.is_macos { 65_535 } else { 1 };
                Ok(result.const_int(value, false).into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "checked_u16" => {
                let value = operands
                    .first()
                    .ok_or_else(|| CodegenError::Unsupported("checked_u16 lacks a value".into()))
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let target = self.generator.basic_type(ty)?.into_int_type();
                let max = value.get_type().const_int(u64::from(u16::MAX), false);
                let valid =
                    self.builder
                        .build_int_compare(IntPredicate::ULE, value, max, "u16.range")?;
                self.guard(valid, "u16 conversion overflow")?;
                Ok(self
                    .builder
                    .build_int_cast(value, target, "u16.cast")?
                    .into())
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
            } if operation == "is_null" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("is_null operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                Ok(self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        self.builder.build_ptr_to_int(
                            pointer,
                            self.generator.pointer_int_type(),
                            "is.null.pointer",
                        )?,
                        self.generator.pointer_int_type().const_zero(),
                        "is.null",
                    )?
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "component_identity" => {
                let callable = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "component_identity operation lacks a callable".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_struct_value();
                let code = self
                    .builder
                    .build_extract_value(callable, 3, "callable.identity")?
                    .into_int_value();
                let result = self.generator.basic_type(ty)?.into_int_type();
                Ok(self
                    .builder
                    .build_int_cast(code, result, "callable.identity.cast")?
                    .into())
            }
            Rvalue::RawOperation { operation, ty, .. } if operation == "null_pointer" => {
                if !matches!(ty, Type::RawPointer { .. }) {
                    return Err(CodegenError::Unsupported(
                        "null_pointer operation requires a raw pointer result".into(),
                    ));
                }
                Ok(self
                    .generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null()
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if matches!(
                operation.as_str(),
                "call_raw" | "call_raw_void" | "call_raw_pointer"
            ) =>
            {
                let callback = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("call_raw operation lacks a callback".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let pointer_type = self.generator.context.ptr_type(AddressSpace::default());
                let arguments = operands
                    .iter()
                    .skip(1)
                    .map(|operand| {
                        self.lower_operand(operand)
                            .map(BasicMetadataValueEnum::from)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let parameter_types = (0..arguments.len())
                    .map(|_| BasicMetadataTypeEnum::from(pointer_type.as_basic_type_enum()))
                    .collect::<Vec<_>>();
                let function_type = if *ty == Type::Primitive(PrimitiveType::Void) {
                    self.generator
                        .context
                        .void_type()
                        .fn_type(&parameter_types, false)
                } else {
                    self.generator
                        .basic_type(ty)?
                        .fn_type(&parameter_types, false)
                };
                let call = self.builder.build_indirect_call(
                    function_type,
                    callback,
                    &arguments,
                    "call.raw",
                )?;
                call.try_as_basic_value().basic().ok_or_else(|| {
                    CodegenError::Unsupported(
                        "call_raw operation requires a non-void callback result".into(),
                    )
                })
            }
            Rvalue::RawOperation { operation, .. } if operation.starts_with("global_address:") => {
                Ok(self.global_pointer(operation)?.into())
            }
            Rvalue::RawOperation { operation, ty, .. } if operation.starts_with("global_load:") => {
                let pointer = self.global_pointer(operation)?;
                let value = self.builder.build_load(
                    self.generator.basic_type(ty)?,
                    pointer,
                    "global.load",
                )?;
                self.lower_borrowed_global_value(ty, value)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation.starts_with("global_store:") => {
                let declaration = operation
                    .split_once(':')
                    .and_then(|(_, value)| value.parse::<u64>().ok())
                    .map(DeclarationId)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "invalid global store operation `{operation}`"
                        ))
                    })?;
                let pointer = self.global_pointer(operation)?;
                let initialized = self
                    .generator
                    .global_initialized
                    .get(&declaration)
                    .copied()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "global declaration {declaration:?} has no initialization flag"
                        ))
                    })?;
                let value = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("global_store operation lacks a value".into())
                })?;

                let initialized_value = self
                    .builder
                    .build_load(
                        self.generator.context.bool_type(),
                        initialized,
                        "global.initialized",
                    )?
                    .into_int_value();
                let drop_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "global.drop");
                let store_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "global.store");
                self.builder.build_conditional_branch(
                    initialized_value,
                    drop_block,
                    store_block,
                )?;
                self.builder.position_at_end(drop_block);
                let layout = self
                    .generator
                    .layouts
                    .globals
                    .get(&declaration)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!(
                            "global declaration {declaration:?} has no layout"
                        ))
                    })?;
                self.lower_drop_value_at_pointer(pointer, &layout.ty, None)?;
                self.builder.build_unconditional_branch(store_block)?;
                self.builder.position_at_end(store_block);
                self.builder
                    .build_store(pointer, self.lower_operand(value)?)?;
                self.builder.build_store(
                    initialized,
                    self.generator.context.bool_type().const_all_ones(),
                )?;
                Ok(self.generator.context.bool_type().const_all_ones().into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "is_copy" => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("is_copy operation lacks a type marker".into())
                })?;
                let ty = self.operand_type(operand)?;
                Ok(self
                    .generator
                    .context
                    .bool_type()
                    .const_int(u64::from(self.is_copy_type(&ty)), false)
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
                self.lower_drop_value_at_pointer(element_pointer, &element_type, None)?;
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
                ..
            } if operation == "drop_element" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("drop_element operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let index = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("drop_element operation lacks an index".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let initialized = operands
                    .get(2)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "drop_element operation lacks an initialized bitmap".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let Type::RawPointer { pointee, .. } =
                    self.operand_type(operands.first().ok_or_else(|| {
                        CodegenError::Unsupported("drop_element operation lacks a pointer".into())
                    })?)?
                else {
                    return Err(CodegenError::Unsupported(
                        "drop_element pointer is not a raw pointer".into(),
                    ));
                };
                let element_type = pointee.as_ref().clone();
                let element_pointer = unsafe {
                    self.builder.build_gep(
                        self.generator.basic_type(&element_type)?,
                        pointer,
                        &[index],
                        "drop.element.address",
                    )?
                };
                let initialized_address = unsafe {
                    self.builder.build_gep(
                        self.generator.context.i8_type(),
                        initialized,
                        &[index],
                        "drop.element.initialized.address",
                    )?
                };
                let initialized_value = self
                    .builder
                    .build_load(
                        self.generator.context.i8_type(),
                        initialized_address,
                        "drop.element.initialized.value",
                    )?
                    .into_int_value();
                let occupied = self.builder.build_int_compare(
                    IntPredicate::NE,
                    initialized_value,
                    initialized_value.get_type().const_zero(),
                    "drop.element.occupied",
                )?;
                let drop_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.element.drop");
                let skip_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.element.skip");
                let done_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.element.done");
                self.builder
                    .build_conditional_branch(occupied, drop_block, skip_block)?;
                self.builder.position_at_end(drop_block);
                self.lower_drop_value_at_pointer(element_pointer, &element_type, None)?;
                self.builder.build_store(
                    initialized_address,
                    self.generator.context.i8_type().const_zero(),
                )?;
                self.builder.build_unconditional_branch(done_block)?;
                let dropped_predecessor = self.builder.get_insert_block().ok_or_else(|| {
                    CodegenError::Builder("drop element block disappeared".into())
                })?;
                self.builder.position_at_end(skip_block);
                self.builder.build_unconditional_branch(done_block)?;
                let skipped_predecessor = self.builder.get_insert_block().ok_or_else(|| {
                    CodegenError::Builder("drop element block disappeared".into())
                })?;
                self.builder.position_at_end(done_block);
                let dropped = self.generator.context.bool_type().const_all_ones();
                let skipped = self.generator.context.bool_type().const_zero();
                let phi = self
                    .builder
                    .build_phi(self.generator.context.bool_type(), "drop.element.result")?;
                phi.add_incoming(&[
                    (&dropped as &dyn BasicValue, dropped_predecessor),
                    (&skipped as &dyn BasicValue, skipped_predecessor),
                ]);
                Ok(phi.as_basic_value())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "dereference" => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("dereference operation lacks an operand".into())
                })?;
                let pointer = match self.lower_operand(operand)? {
                    BasicValueEnum::PointerValue(pointer) => pointer,
                    BasicValueEnum::StructValue(view) if matches!(ty, Type::String | Type::Str) => {
                        return Ok(self.builder.build_extract_value(
                            view,
                            0,
                            "raw.dereference.pointer",
                        )?);
                    }
                    BasicValueEnum::StructValue(view) => self
                        .builder
                        .build_extract_value(view, 0, "raw.dereference.pointer")?
                        .into_pointer_value(),
                    other => {
                        return Err(CodegenError::Unsupported(format!(
                            "dereference operation requires a pointer or string view, found {}",
                            other.get_type().print_to_string()
                        )));
                    }
                };
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
            } if matches!(operation.as_str(), "byte_address" | "byte_address_i32") => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!("{operation} operation lacks a pointer"))
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let offset = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(format!("{operation} operation lacks an offset"))
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                // These helpers intentionally address raw byte storage even when the source
                // pointer's pointee is a wider erased slot type.
                Ok(unsafe {
                    self.builder
                        .build_gep(
                            self.generator.context.i8_type(),
                            pointer,
                            &[offset],
                            "byte.address",
                        )?
                        .into()
                })
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "byte_read_i32" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("byte_read_i32 operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let offset = operands
                    .get(2)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("byte_read_i32 operation lacks an index".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let address = unsafe {
                    self.builder.build_gep(
                        self.generator.context.i8_type(),
                        pointer,
                        &[offset],
                        "byte.read.address",
                    )?
                };
                let value = self
                    .builder
                    .build_load(self.generator.context.i8_type(), address, "byte.read.value")?
                    .into_int_value();
                let result_type = self.generator.basic_type(ty)?.into_int_type();
                Ok(self
                    .builder
                    .build_int_z_extend(value, result_type, "byte.read.i32")?
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "borrow_element" => {
                let pointer_operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("borrow_element operation lacks a pointer".into())
                })?;
                let pointer = self.lower_operand(pointer_operand)?.into_pointer_value();
                let Type::RawPointer { pointee, .. } = self.operand_type(pointer_operand)? else {
                    return Err(CodegenError::Unsupported(
                        "borrow_element operation requires a raw pointer".into(),
                    ));
                };
                let index = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported("borrow_element operation lacks an index".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let element = self.generator.basic_type(&pointee)?;
                // SAFETY: collection methods check the logical index before invoking this
                // intrinsic, and their storage invariant guarantees `capacity` consecutive
                // elements at `pointer`.
                let address = unsafe {
                    self.builder
                        .build_gep(element, pointer, &[index], "borrowed.element")?
                };
                if matches!(
                    ty,
                    Type::Reference { referent, .. }
                        if matches!(referent.as_ref(), Type::String | Type::Str)
                ) {
                    return self
                        .lower_borrowed_string_from_address(address, &pointee)
                        .map(Into::into);
                }
                Ok(address.into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "borrow_element_mut" => {
                let pointer_operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("borrow_element_mut operation lacks a pointer".into())
                })?;
                let pointer = self.lower_operand(pointer_operand)?.into_pointer_value();
                let Type::RawPointer { pointee, .. } = self.operand_type(pointer_operand)? else {
                    return Err(CodegenError::Unsupported(
                        "borrow_element_mut operation requires a raw pointer".into(),
                    ));
                };
                let index = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "borrow_element_mut operation lacks an index".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let element = self.generator.basic_type(&pointee)?;
                // SAFETY: collection methods check the logical index before invoking this
                // intrinsic, and their storage invariant guarantees `capacity` consecutive
                // elements at `pointer`.
                let address = unsafe {
                    self.builder
                        .build_gep(element, pointer, &[index], "borrowed.element.mut")?
                };
                if matches!(
                    ty,
                    Type::Reference { referent, .. }
                        if matches!(referent.as_ref(), Type::String | Type::Str)
                ) {
                    return self
                        .lower_borrowed_string_from_address(address, &pointee)
                        .map(Into::into);
                }
                Ok(address.into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if matches!(
                operation.as_str(),
                "borrow_mut_direct"
                    | "borrow_mut_storage"
                    | "borrow_shared_direct"
                    | "borrow_shared_storage"
            ) =>
            {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported(format!("{operation} operation lacks an operand"))
                })?;
                let pointer = self.lower_operand(operand)?.into_pointer_value();
                if matches!(
                    operation.as_str(),
                    "borrow_mut_storage" | "borrow_shared_storage"
                ) && matches!(
                    ty,
                    Type::Reference { referent, .. }
                        if self.generator.is_class_type(referent)
                ) {
                    let value = self.builder.build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        pointer,
                        "class.borrow.intrinsic",
                    )?;
                    Ok(value.into())
                } else {
                    Ok(pointer.into())
                }
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
            } if operation == "drop_value" => {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("drop_value operation lacks a pointer".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_pointer_value();
                let Type::RawPointer { pointee, .. } =
                    self.operand_type(operands.first().ok_or_else(|| {
                        CodegenError::Unsupported("drop_value operation lacks a pointer".into())
                    })?)?
                else {
                    return Err(CodegenError::Unsupported(
                        "drop_value pointer is not a raw pointer".into(),
                    ));
                };
                self.lower_drop_value_at_pointer(pointer, pointee.as_ref(), None)?;
                Ok(self.generator.context.bool_type().const_all_ones().into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if matches!(
                operation.as_str(),
                "i32_to_usize" | "u64_to_usize" | "usize_to_u64"
            ) =>
            {
                let value = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("u64_to_usize operation lacks a value".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let target = self.generator.basic_type(ty)?;
                Ok(self.lower_cast(value.into(), target)?)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "usize_to_f32" => {
                let value = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("usize_to_f32 operation lacks a value".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_int_value();
                let target = self.generator.basic_type(ty)?.into_float_type();
                Ok(self
                    .builder
                    .build_unsigned_int_to_float(value, target, "usize.to.f32")?
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "f64_to_usize" => {
                let value = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported("f64_to_usize operation lacks a value".into())
                    })
                    .and_then(|operand| self.lower_operand(operand))?
                    .into_float_value();
                let target = self.generator.basic_type(ty)?.into_int_type();
                Ok(self
                    .builder
                    .build_float_to_unsigned_int(value, target, "f64.to.usize")?
                    .into())
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
                if !self.is_copy_type(&element_type) {
                    self.lower_drop_value_at_pointer(element_pointer, &element_type, None)?;
                }
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
                if operands.len() != 2 {
                    return Err(CodegenError::Unsupported(
                        "string_from_static requires a pointer and byte length".into(),
                    ));
                }
                let text_value = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "string_from_static operation lacks an operand".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?;
                let text = match text_value {
                    BasicValueEnum::PointerValue(pointer) => pointer,
                    BasicValueEnum::StructValue(view) => self
                        .builder
                        .build_extract_value(view, 0, "string.pointer")?
                        .into_pointer_value(),
                    other => {
                        return Err(CodegenError::Unsupported(format!(
                            "string_from_static requires a text pointer, found {}",
                            other.get_type().print_to_string()
                        )));
                    }
                };
                let length = self.lower_operand(&operands[1])?;
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
            } if operation == "string_from_raw" => {
                if *ty != Type::String || operands.len() != 2 {
                    return Err(CodegenError::Unsupported(
                        "string_from_raw requires a string result, pointer, and length".into(),
                    ));
                }
                let pointer = self.lower_operand(&operands[0])?;
                let _length = self.lower_operand(&operands[1])?;
                Ok(pointer)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if matches!(
                operation.as_str(),
                "slice_from_raw_parts" | "str_from_raw_parts"
            ) =>
            {
                let pointer = operands
                    .first()
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "slice_from_raw_parts operation lacks a pointer".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?;
                let pointer = match pointer {
                    BasicValueEnum::PointerValue(pointer) => pointer,
                    BasicValueEnum::StructValue(view) => self
                        .builder
                        .build_extract_value(view, 0, "slice.pointer")?
                        .into_pointer_value(),
                    other => {
                        return Err(CodegenError::Unsupported(format!(
                            "slice_from_raw_parts operation requires a pointer or string view, found {}",
                            other.get_type().print_to_string()
                        )));
                    }
                };
                let length = operands
                    .get(1)
                    .ok_or_else(|| {
                        CodegenError::Unsupported(
                            "slice_from_raw_parts operation lacks a length".into(),
                        )
                    })
                    .and_then(|operand| self.lower_operand(operand))?;
                let structure = self.generator.basic_type(ty)?.into_struct_type();
                let value = self
                    .builder
                    .build_insert_value(structure.const_zero(), pointer, 0, "slice.pointer")?
                    .into_struct_value();
                Ok(self
                    .builder
                    .build_insert_value(value, length, 1, "slice.length")?
                    .into_struct_value()
                    .into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ..
            } if operation == "slice_length" => {
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("slice_length operation lacks a slice".into())
                })?;
                let operand_type = self.operand_type(operand)?;
                match operand_type {
                    Type::Reference { referent, .. }
                        if matches!(referent.as_ref(), Type::Slice(_)) => {}
                    Type::Slice(_) => {}
                    other => {
                        return Err(CodegenError::Unsupported(format!(
                            "slice_length requires a slice, found {other:?}"
                        )));
                    }
                }
                let slice = self.lower_operand(operand)?.into_struct_value();
                Ok(self.builder.build_extract_value(slice, 1, "slice.length")?)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "string_scalar_length" => {
                if *ty != Type::Primitive(PrimitiveType::Usize) || operands.len() != 1 {
                    return Err(CodegenError::Unsupported(
                        "string_scalar_length requires one string operand and a usize result"
                            .into(),
                    ));
                }
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported(
                        "string_scalar_length operation lacks a string operand".into(),
                    )
                })?;
                let value = self.lower_operand(operand)?;
                let (pointer, length) = self.string_parts(operand, value)?;
                Ok(self
                    .builder
                    .build_call(
                        self.generator.runtime_string_scalar_length_bytes(),
                        &[pointer.into(), length.into()],
                        "string.scalar_length",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| {
                        CodegenError::Builder("string scalar length returned void".into())
                    })?)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "string_byte_length" => {
                if *ty != Type::Primitive(PrimitiveType::Usize) || operands.len() != 1 {
                    return Err(CodegenError::Unsupported(
                        "string_byte_length requires one string operand and a usize result".into(),
                    ));
                }
                let operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported(
                        "string_byte_length operation lacks a string operand".into(),
                    )
                })?;
                let value = self.lower_operand(operand)?;
                Ok(self.string_parts(operand, value)?.1.into())
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "weak_upgrade" => {
                let source_operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("weak_upgrade operation lacks a receiver".into())
                })?;
                let source_type = self.operand_type(source_operand)?;
                let source = self.lower_operand(source_operand)?.into_pointer_value();
                let source_type = match source_type {
                    Type::Reference { referent, .. } => referent.as_ref().clone(),
                    _ => {
                        return Err(CodegenError::Unsupported(
                            "weak_upgrade receiver must be a reference".into(),
                        ));
                    }
                };
                let source_layout = self.generator.basic_type(&source_type)?.into_struct_type();
                let source_pointer = self.builder.build_struct_gep(
                    source_layout,
                    source,
                    0,
                    "weak.source.pointer.address",
                )?;
                let source_pointer = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        source_pointer,
                        "weak.source.pointer",
                    )?
                    .into_pointer_value();
                let upgraded = self
                    .builder
                    .build_call(
                        self.generator.runtime_arc_upgrade(),
                        &[source_pointer.into()],
                        "weak.upgrade",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError::Builder("weak upgrade returned void".into()))?
                    .into_pointer_value();
                let Type::Optional(inner) = ty else {
                    return Err(CodegenError::Unsupported(
                        "weak_upgrade result must be optional".into(),
                    ));
                };
                let object_layout = self.generator.class_object_type(inner)?;
                let object_size = object_layout.size_of().ok_or_else(|| {
                    CodegenError::Unsupported(
                        "arc class object has no statically known size".into(),
                    )
                })?;
                let result_layout = self.generator.basic_type(ty)?.into_struct_type();
                let result = self
                    .builder
                    .build_alloca(result_layout, "weak.upgrade.result")?;
                self.builder
                    .build_store(result, result_layout.const_zero())?;
                let present = self
                    .generator
                    .context
                    .append_basic_block(self.function, "weak.upgrade.present");
                let merge = self
                    .generator
                    .context
                    .append_basic_block(self.function, "weak.upgrade.merge");
                let is_null = self
                    .builder
                    .build_is_null(upgraded, "weak.upgrade.is_null")?;
                self.builder
                    .build_conditional_branch(is_null, merge, present)?;
                self.builder.position_at_end(present);
                let object = self
                    .builder
                    .build_call(
                        self.generator.runtime_alloc(),
                        &[object_size.into()],
                        "weak.upgrade.object",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError::Builder("allocator returned void".into()))?
                    .into_pointer_value();
                let descriptor = self
                    .generator
                    .descriptor_for_type(inner)
                    .unwrap_or_else(|| {
                        self.generator
                            .context
                            .ptr_type(AddressSpace::default())
                            .const_null()
                    });
                let descriptor_address = self.builder.build_struct_gep(
                    object_layout,
                    object,
                    0,
                    "weak.upgrade.descriptor.address",
                )?;
                self.builder.build_store(descriptor_address, descriptor)?;
                let pointer_address = self.builder.build_struct_gep(
                    object_layout,
                    object,
                    1,
                    "weak.upgrade.pointer.address",
                )?;
                self.builder.build_store(pointer_address, upgraded)?;
                let alive_address = self.builder.build_struct_gep(
                    object_layout,
                    object,
                    2,
                    "weak.upgrade.alive.address",
                )?;
                self.builder.build_store(
                    alive_address,
                    self.generator.context.bool_type().const_int(1, false),
                )?;
                let tag_address = self.builder.build_struct_gep(
                    result_layout,
                    result,
                    0,
                    "weak.upgrade.tag.address",
                )?;
                self.builder.build_store(
                    tag_address,
                    self.generator.context.bool_type().const_int(1, false),
                )?;
                let payload_address = self.builder.build_struct_gep(
                    result_layout,
                    result,
                    1,
                    "weak.upgrade.payload.address",
                )?;
                self.builder.build_store(payload_address, object)?;
                self.builder.build_unconditional_branch(merge)?;
                self.builder.position_at_end(merge);
                Ok(self
                    .builder
                    .build_load(result_layout, result, "weak.upgrade.value")?)
            }
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "task_into_promise" => {
                if operands.len() != 1 || !matches!(ty, Type::Promise { .. }) {
                    return Err(CodegenError::Unsupported(
                        "task_into_promise requires one task and a promise result".into(),
                    ));
                }
                let task = self.lower_operand(&operands[0])?.into_struct_value();
                Ok(self.builder.build_extract_value(task, 0, "task.promise")?)
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
                        self.generator.runtime_arc_retain(),
                        &[source_pointer.into()],
                        "arc.retain",
                    )?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError::Builder("arc retain returned void".into()))?
                    .into_pointer_value();
                let retained_is_null = self.builder.build_is_null(retained, "arc.retain.null")?;
                self.guard(
                    self.builder
                        .build_not(retained_is_null, "arc.retain.valid")?,
                    "Arc strong-count overflow",
                )?;
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
            Rvalue::RawOperation {
                operation,
                operands,
                ty,
            } if operation == "borrow_callable" => {
                let source_operand = operands.first().ok_or_else(|| {
                    CodegenError::Unsupported("borrow_callable operation lacks a pointer".into())
                })?;
                let Type::Function(_) = ty else {
                    return Err(CodegenError::Unsupported(
                        "borrow_callable result must be a function".into(),
                    ));
                };
                let pointer = self.lower_operand(source_operand)?.into_pointer_value();
                let callable = self
                    .builder
                    .build_load(
                        self.generator.basic_type(ty)?.into_struct_type(),
                        pointer,
                        "borrow.callable",
                    )?
                    .into_struct_value();
                let null = self
                    .generator
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null();
                Ok(self
                    .builder
                    .build_insert_value(callable, null, 2, "borrow.callable.drop")?
                    .into_struct_value()
                    .into())
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
            Type::Tuple(_) => {
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
            Type::Optional(_) => {
                let structure = self.generator.basic_type(ty)?.into_struct_type();
                let mut value = structure.const_zero();
                value = self
                    .builder
                    .build_insert_value(
                        value,
                        self.generator
                            .context
                            .bool_type()
                            .const_int(u64::from(variant.unwrap_or(0) != 0), false),
                        0,
                        "optional.tag",
                    )?
                    .into_struct_value();
                for (index, field) in fields.iter().enumerate() {
                    value = self
                        .builder
                        .build_insert_value(
                            value,
                            self.lower_operand(field)?,
                            u32::try_from(index + 1).map_err(|_| {
                                CodegenError::Unsupported("optional field limit".into())
                            })?,
                            "optional.field",
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
                    variants,
                    c_repr,
                    discriminants,
                } = &layout.kind
                    && (*c_repr || variants.iter().all(Vec::is_empty))
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
        let left_value = self.lower_operand(left)?;
        let right_value = self.lower_operand(right)?;
        if let Some(result) =
            self.lower_string_binary(operator, left, left_value, right, right_value, ty)?
        {
            return Ok(result);
        }
        if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
            && matches!(ty, Type::Optional(_))
        {
            return self.lower_optional_equality(operator, left_value, right_value);
        }

        if left_value.is_pointer_value() || right_value.is_pointer_value() {
            return self.lower_pointer_binary(operator, left_value, right_value, ty);
        }

        if left_value.is_float_value() {
            return self.lower_float_binary(operator, left_value, right_value);
        }
        self.lower_integer_binary(
            operator,
            left_value.into_int_value(),
            right_value.into_int_value(),
            ty,
        )
    }

    fn lower_integer_binary(
        &self,
        operator: BinaryOperator,
        left: IntValue<'ctx>,
        right: IntValue<'ctx>,
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
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

    fn lower_pointer_binary(
        &self,
        operator: BinaryOperator,
        left: BasicValueEnum<'ctx>,
        right: BasicValueEnum<'ctx>,
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if !left.is_pointer_value()
            || !right.is_pointer_value()
            || !matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
        {
            return Err(CodegenError::Unsupported(format!(
                "unsupported pointer binary operation: operator={operator:?}, type={ty:?}, left={}, right={}",
                left.get_type().print_to_string(),
                right.get_type().print_to_string()
            )));
        }
        let left = self.builder.build_ptr_to_int(
            left.into_pointer_value(),
            self.generator.pointer_int_type(),
            "pointer.left",
        )?;
        let right = self.builder.build_ptr_to_int(
            right.into_pointer_value(),
            self.generator.pointer_int_type(),
            "pointer.right",
        )?;
        Ok(self
            .builder
            .build_int_compare(
                integer_predicate(operator, false)?,
                left,
                right,
                "pointer.compare",
            )?
            .into())
    }

    fn lower_string_binary(
        &self,
        operator: BinaryOperator,
        left_operand: &Operand,
        left: BasicValueEnum<'ctx>,
        right_operand: &Operand,
        right: BasicValueEnum<'ctx>,
        ty: &Type,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let string_like = matches!(ty, Type::String | Type::Str)
            || matches!(
                ty,
                Type::Reference { referent, .. }
                    if matches!(referent.as_ref(), Type::String | Type::Str)
            );
        if !string_like {
            return Ok(None);
        }
        match operator {
            BinaryOperator::Equal | BinaryOperator::NotEqual => self
                .lower_string_equality(operator, left_operand, left, right_operand, right)
                .map(Some),
            BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual => self
                .lower_string_comparison(operator, left_operand, left, right_operand, right)
                .map(Some),
            _ => Ok(None),
        }
    }

    fn lower_string_equality(
        &self,
        operator: BinaryOperator,
        left_operand: &Operand,
        left: BasicValueEnum<'ctx>,
        right_operand: &Operand,
        right: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (left_pointer, left_length) = self.string_parts(left_operand, left)?;
        let (right_pointer, right_length) = self.string_parts(right_operand, right)?;
        let equal = self
            .builder
            .build_call(
                self.generator.runtime_string_equals(),
                &[
                    left_pointer.into(),
                    left_length.into(),
                    right_pointer.into(),
                    right_length.into(),
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

    fn lower_string_comparison(
        &self,
        operator: BinaryOperator,
        left_operand: &Operand,
        left: BasicValueEnum<'ctx>,
        right_operand: &Operand,
        right: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (left_pointer, left_length) = self.string_parts(left_operand, left)?;
        let (right_pointer, right_length) = self.string_parts(right_operand, right)?;
        let ordering = self
            .builder
            .build_call(
                self.generator.runtime_string_compare(),
                &[
                    left_pointer.into(),
                    left_length.into(),
                    right_pointer.into(),
                    right_length.into(),
                ],
                "string.compare",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("string comparison returned void".into()))?
            .into_int_value();
        let predicate = match operator {
            BinaryOperator::Less => IntPredicate::SLT,
            BinaryOperator::LessEqual => IntPredicate::SLE,
            BinaryOperator::Greater => IntPredicate::SGT,
            BinaryOperator::GreaterEqual => IntPredicate::SGE,
            _ => {
                return Err(CodegenError::Unsupported(
                    "invalid string comparison operator".into(),
                ));
            }
        };
        Ok(self
            .builder
            .build_int_compare(
                predicate,
                ordering,
                ordering.get_type().const_zero(),
                "string.order",
            )?
            .into())
    }

    fn string_parts(
        &self,
        operand: &Operand,
        value: BasicValueEnum<'ctx>,
    ) -> Result<(PointerValue<'ctx>, IntValue<'ctx>), CodegenError> {
        let operand_type = self.operand_type(operand)?;
        if matches!(
            &operand_type,
            Type::Reference {
                mutable: true,
                referent,
                ..
            } if matches!(referent.as_ref(), Type::String)
        ) {
            let pointer = self
                .builder
                .build_load(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    value.into_pointer_value(),
                    "mutable.string.pointer",
                )?
                .into_pointer_value();
            let length = self
                .builder
                .build_call(
                    self.generator.runtime_string_length(),
                    &[pointer.into()],
                    "mutable.string.length",
                )?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("string length returned void".into()))?
                .into_int_value();
            return Ok((pointer, length));
        }
        if matches!(
            &operand_type,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::String | Type::Str)
        ) {
            let value = value.into_struct_value();
            let pointer = self
                .builder
                .build_extract_value(value, 0, "string.pointer")?
                .into_pointer_value();
            let length = self
                .builder
                .build_extract_value(value, 1, "string.length")?
                .into_int_value();
            return Ok((pointer, length));
        }
        if let Some(place) = operand_place(operand)
            && self.place_is_fat_string_dereference(place)?
        {
            let value = self
                .builder
                .build_load(
                    self.generator.borrowed_string_type(),
                    self.place_pointer(place)?,
                    "string.dereference.view",
                )?
                .into_struct_value();
            let pointer = self
                .builder
                .build_extract_value(value, 0, "string.pointer")?
                .into_pointer_value();
            let length = self
                .builder
                .build_extract_value(value, 1, "string.length")?
                .into_int_value();
            return Ok((pointer, length));
        }
        let pointer = match operand_type {
            Type::String | Type::Str => value.into_pointer_value(),
            other => {
                return Err(CodegenError::Unsupported(format!(
                    "string operation requires string input, found {other:?}"
                )));
            }
        };
        let length = if let Operand::Constant(Constant::String(text)) = operand {
            self.generator
                .pointer_int_type()
                .const_int(u64::try_from(text.len()).unwrap_or(u64::MAX), false)
        } else {
            self.builder
                .build_call(
                    self.generator.runtime_string_length(),
                    &[pointer.into()],
                    "string.length",
                )?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("string length returned void".into()))?
                .into_int_value()
        };
        Ok((pointer, length))
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
        self.emit_undefined_sanitizer_trap(message)?;
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

    fn emit_undefined_sanitizer_trap(&self, message: &str) -> Result<(), CodegenError> {
        if !self.generator.sanitizers.contains(&Sanitizer::Undefined) {
            return Ok(());
        }
        let intrinsic = Intrinsic::find("llvm.ubsantrap").ok_or_else(|| {
            CodegenError::Unsupported("LLVM UBSan trap intrinsic unavailable".into())
        })?;
        let declaration = intrinsic
            .get_declaration(&self.generator.module, &[])
            .ok_or_else(|| {
                CodegenError::Unsupported("LLVM UBSan trap declaration unavailable".into())
            })?;
        let failure_kind = u64::from(stable_panic_code(message) % 256);
        self.builder.build_call(
            declaration,
            &[self
                .generator
                .context
                .i8_type()
                .const_int(failure_kind, false)
                .into()],
            "ubsantrap",
        )?;
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
            (BasicValueEnum::IntValue(value), BasicTypeEnum::PointerType(target)) => Ok(self
                .builder
                .build_int_to_ptr(value, target, "inttoptr")?
                .into()),
            (BasicValueEnum::PointerValue(value), BasicTypeEnum::IntType(target)) => Ok(self
                .builder
                .build_ptr_to_int(value, target, "ptrtoint")?
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
        let value = if self.is_pointer_representation(target) {
            payload.into()
        } else {
            self.builder
                .build_load(self.generator.basic_type(target)?, payload, "error.value")?
        };
        if !self.is_pointer_representation(target) {
            self.builder.build_call(
                self.generator.runtime_free(),
                &[payload.into()],
                "error.payload.free",
            )?;
        }
        self.builder.build_call(
            self.generator.runtime_free(),
            &[envelope.into()],
            "error.envelope.free",
        )?;
        Ok(value)
    }

    fn is_pointer_representation(&self, ty: &Type) -> bool {
        matches!(
            ty,
            Type::Reference {
                mutable: true,
                referent,
                ..
            } if matches!(referent.as_ref(), Type::String)
        ) || matches!(
            ty,
            Type::Reference { referent, .. }
                if !matches!(referent.as_ref(), Type::Slice(_) | Type::String | Type::Str)
        ) || matches!(
            ty,
            Type::RawPointer { .. }
                | Type::String
                | Type::Str
                | Type::Promise { .. }
                | Type::Template(_)
                | Type::ErrorUnion(_)
        ) || matches!(
            ty,
            Type::Nominal(_, _) if self.generator.is_class_type(ty)
        )
    }

    fn is_copy_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Primitive(_) | Type::RawPointer { .. } => true,
            Type::Reference { mutable, .. } => !mutable,
            Type::Optional(inner) | Type::Array(inner, _) => self.is_copy_type(inner),
            Type::Tuple(elements) | Type::Template(elements) => {
                elements.iter().all(|element| self.is_copy_type(element))
            }
            Type::Nominal(declaration, _) => self.generator.layouts.copies.contains(declaration),
            Type::ErrorUnion(effects) => effects
                .iter()
                .all(|effect| self.generator.layouts.copies.contains(effect)),
            Type::Promise { .. }
            | Type::Function(_)
            | Type::String
            | Type::Str
            | Type::Slice(_)
            | Type::DynamicInterface(_, _)
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => false,
        }
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
        let Type::Promise {
            result, effects, ..
        } = self.operand_type(value)?
        else {
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
        let flag = *self
            .drop_flags
            .get(place)
            .ok_or_else(|| CodegenError::Unsupported("drop flag place is missing".into()))?;
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

    fn set_drop_flag_value(&self, place: &Place, value: bool) -> Result<(), CodegenError> {
        let mut paths = self
            .drop_flags
            .iter()
            .filter(|(candidate, _)| place_is_prefix(place, candidate))
            .map(|(_, flag)| *flag)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Err(CodegenError::Unsupported(
                "drop flag place is missing".into(),
            ));
        }
        let value = self
            .generator
            .context
            .bool_type()
            .const_int(u64::from(value), false);
        for flag in paths.drain(..) {
            self.builder.build_store(flag, value)?;
        }
        Ok(())
    }

    fn lower_drop_value(&self, place: &Place) -> Result<(), CodegenError> {
        let ty = self.place_type(place)?;
        let pointer = self.place_pointer(place)?;
        self.lower_drop_value_at_pointer(pointer, &ty, Some(place))
    }

    /// Drops an aggregate while honoring initialization state for every statically addressable
    /// child path. The root path has already been checked by `lower_drop`; recursive children are
    /// guarded here before their destructor or field traversal runs.
    fn lower_drop_value_at_path(
        &self,
        pointer: PointerValue<'ctx>,
        ty: &Type,
        place: &Place,
    ) -> Result<(), CodegenError> {
        let Some(flag) = self.drop_flags.get(place).copied() else {
            return self.lower_drop_value_at_pointer(pointer, ty, Some(place));
        };
        let initialized = self
            .builder
            .build_load(
                self.generator.context.bool_type(),
                flag,
                "drop.path.initialized",
            )?
            .into_int_value();
        let drop_block = self
            .generator
            .context
            .append_basic_block(self.function, "drop.path.value");
        let skip_block = self
            .generator
            .context
            .append_basic_block(self.function, "drop.path.skip");
        let merge_block = self
            .generator
            .context
            .append_basic_block(self.function, "drop.path.merge");
        self.builder
            .build_conditional_branch(initialized, drop_block, skip_block)?;
        self.builder.position_at_end(drop_block);
        self.lower_drop_value_at_pointer(pointer, ty, Some(place))?;
        self.set_drop_flag_value(place, false)?;
        self.builder.build_unconditional_branch(merge_block)?;
        self.builder.position_at_end(skip_block);
        self.builder.build_unconditional_branch(merge_block)?;
        self.builder.position_at_end(merge_block);
        Ok(())
    }

    fn lower_drop_child(
        &self,
        pointer: PointerValue<'ctx>,
        ty: &Type,
        parent: Option<&Place>,
        projection: Projection,
    ) -> Result<(), CodegenError> {
        let Some(parent) = parent else {
            return self.lower_drop_value_at_pointer(pointer, ty, None);
        };
        let child = place_with_projection(parent, projection);
        self.lower_drop_value_at_path(pointer, ty, &child)
    }

    #[allow(clippy::too_many_lines)]
    fn lower_drop_value_at_pointer(
        &self,
        pointer: PointerValue<'ctx>,
        ty: &Type,
        path: Option<&Place>,
    ) -> Result<(), CodegenError> {
        let resolved = self.generator.resolve_alias(ty);
        if resolved != *ty {
            return self.lower_drop_value_at_pointer(pointer, &resolved, path);
        }
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
                    self.generator.runtime_string_free(),
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
            Type::Function(_) => {
                let callable = self
                    .builder
                    .build_load(self.generator.callable_type(), pointer, "drop.callable")?
                    .into_struct_value();
                let drop_function = self
                    .builder
                    .build_extract_value(callable, 2, "drop.callable.function")?
                    .into_pointer_value();
                let environment = self
                    .builder
                    .build_extract_value(callable, 1, "drop.callable.environment")?
                    .into_pointer_value();
                let invoke_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.callable.invoke");
                let merge_block = self
                    .generator
                    .context
                    .append_basic_block(self.function, "drop.callable.merge");
                let has_drop = self
                    .builder
                    .build_is_not_null(drop_function, "drop.callable.present")?;
                self.builder
                    .build_conditional_branch(has_drop, invoke_block, merge_block)?;
                self.builder.position_at_end(invoke_block);
                let drop_type = self.generator.context.void_type().fn_type(
                    &[self
                        .generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .as_basic_type_enum()
                        .into()],
                    false,
                );
                self.builder.build_indirect_call(
                    drop_type,
                    drop_function,
                    &[environment.into()],
                    "drop.callable.invoke",
                )?;
                self.builder.build_unconditional_branch(merge_block)?;
                self.builder.position_at_end(merge_block);
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
                self.lower_drop_child(payload, inner, path, Projection::Downcast(1))?;
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
                    self.lower_drop_child(
                        field,
                        element,
                        path,
                        Projection::Field {
                            index: u32::try_from(index).map_err(|_| {
                                CodegenError::Unsupported("tuple field limit".into())
                            })?,
                            ty: element.clone(),
                        },
                    )?;
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
                    self.lower_drop_value_at_pointer(field, element, None)?;
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
                let arguments = arguments
                    .iter()
                    .filter(|argument| !matches!(argument, Type::Lifetime(_)))
                    .cloned()
                    .collect::<Vec<_>>();
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
                    .zip(arguments)
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
                            self.lower_drop_child(
                                field_pointer,
                                &field,
                                path,
                                Projection::Field {
                                    index: u32::try_from(index).map_err(|_| {
                                        CodegenError::Unsupported("struct field limit".into())
                                    })?,
                                    ty: field.clone(),
                                },
                            )?;
                        }
                    }
                    NominalKind::Enum {
                        variants, c_repr, ..
                    } => {
                        // C-represented and fieldless enums are lowered as scalar
                        // discriminants rather than the tagged struct used by
                        // payload-carrying enums. They have no owned children to
                        // drop, so attempting to view their LLVM type as a struct
                        // would panic in inkwell.
                        if c_repr || variants.iter().all(Vec::is_empty) {
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
                                if let Some(path) = path {
                                    let variant_path = place_with_projection(
                                        path,
                                        Projection::Downcast(u32::try_from(variant).map_err(
                                            |_| {
                                                CodegenError::Unsupported(
                                                    "enum variant limit".into(),
                                                )
                                            },
                                        )?),
                                    );
                                    let field_path = place_with_projection(
                                        &variant_path,
                                        Projection::Field {
                                            index: u32::try_from(field_index).map_err(|_| {
                                                CodegenError::Unsupported("enum field limit".into())
                                            })?,
                                            ty: field.clone(),
                                        },
                                    );
                                    self.lower_drop_value_at_path(
                                        field_pointer,
                                        &field,
                                        &field_path,
                                    )?;
                                } else {
                                    self.lower_drop_value_at_pointer(field_pointer, &field, None)?;
                                }
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
                            self.lower_drop_child(
                                field_pointer,
                                field,
                                path,
                                Projection::Field {
                                    index: u32::try_from(index).map_err(|_| {
                                        CodegenError::Unsupported("class field limit".into())
                                    })?,
                                    ty: field.clone(),
                                },
                            )?;
                        }
                        self.builder.build_call(
                            self.generator.runtime_free(),
                            &[object.into()],
                            "free.class",
                        )?;
                    }
                }
            }
            Type::ErrorUnion(effects) => self.lower_error_union_drop(pointer, effects)?,
            Type::Template(_)
            | Type::DynamicInterface(_, _)
            | Type::Primitive(_)
            | Type::Str
            | Type::Slice(_)
            | Type::Reference { .. }
            | Type::RawPointer { .. }
            | Type::Generic(_)
            | Type::Lifetime(_)
            | Type::Error
            | Type::Unknown => {}
        }
        Ok(())
    }

    fn lower_error_union_drop(
        &self,
        pointer: PointerValue<'ctx>,
        effects: &[DeclarationId],
    ) -> Result<(), CodegenError> {
        let envelope = self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                pointer,
                "drop.error.envelope",
            )?
            .into_pointer_value();
        let envelope_type = self.error_union_type();
        let tag_address =
            self.builder
                .build_struct_gep(envelope_type, envelope, 0, "drop.error.tag.address")?;
        let tag = self
            .builder
            .build_load(
                self.generator.context.i64_type(),
                tag_address,
                "drop.error.tag",
            )?
            .into_int_value();
        let merge_block = self
            .generator
            .context
            .append_basic_block(self.function, "drop.error.merge");
        let mut cases = Vec::with_capacity(effects.len());
        let mut effect_blocks = Vec::with_capacity(effects.len());
        for index in 0..effects.len() {
            let block = self
                .generator
                .context
                .append_basic_block(self.function, "drop.error.effect");
            cases.push((
                self.generator
                    .context
                    .i64_type()
                    .const_int(u64::try_from(index).unwrap_or(u64::MAX), false),
                block,
            ));
            effect_blocks.push(block);
        }
        self.builder.build_switch(tag, merge_block, &cases)?;

        for (effect, block) in effects.iter().zip(effect_blocks) {
            self.builder.position_at_end(block);
            let payload_address = self.builder.build_struct_gep(
                envelope_type,
                envelope,
                1,
                "drop.error.payload.address",
            )?;
            let payload = self
                .builder
                .build_load(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    payload_address,
                    "drop.error.payload",
                )?
                .into_pointer_value();
            let payload_type = Type::Nominal(*effect, Vec::new());
            if self.is_pointer_representation(&payload_type) {
                let payload_value = self.builder.build_alloca(
                    self.generator.context.ptr_type(AddressSpace::default()),
                    "drop.error.pointer.value",
                )?;
                self.builder.build_store(payload_value, payload)?;
                self.lower_drop_value_at_pointer(payload_value, &payload_type, None)?;
            } else {
                self.lower_drop_value_at_pointer(payload, &payload_type, None)?;
                self.builder.build_call(
                    self.generator.runtime_free(),
                    &[payload.into()],
                    "drop.error.payload.free",
                )?;
            }
            self.builder.build_unconditional_branch(merge_block)?;
        }
        self.builder.position_at_end(merge_block);
        self.builder.build_call(
            self.generator.runtime_free(),
            &[envelope.into()],
            "drop.error.envelope.free",
        )?;
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
            .ok_or_else(|| {
                let candidates = self
                    .generator
                    .signatures
                    .iter()
                    .filter(|(instance, _)| instance.callable == callable)
                    .map(|(instance, signature)| format!("{instance:?}: {signature:?}"))
                    .collect::<Vec<_>>();
                CodegenError::Unsupported(format!(
                    "drop method signature is missing for {ty:?} ({callable:?}); candidates: {candidates:?}"
                ))
            })?;
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
        let direct_call = matches!(function, Operand::Constant(_));
        let arguments = direct_call
            .then(|| receiver.map(|receiver| self.lower_receiver_operand(receiver)))
            .flatten()
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
                let mut parameters = Vec::with_capacity(function_type.parameters.len() + 1);
                parameters.push(Type::RawPointer {
                    mutable: true,
                    pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
                });
                parameters.extend(function_type.parameters.iter().cloned());
                let llvm_type = self.generator.llvm_function_type(
                    &parameters,
                    &function_type.result,
                    &function_type.effects,
                )?;
                let callable = self.lower_operand(function)?.into_struct_value();
                let pointer = self
                    .builder
                    .build_extract_value(callable, 0, "callable.code")?
                    .into_pointer_value();
                let environment =
                    self.builder
                        .build_extract_value(callable, 1, "callable.environment")?;
                let mut call_arguments = Vec::with_capacity(arguments.len() + 1);
                call_arguments.push(environment.into());
                call_arguments.extend(arguments);
                (
                    self.builder.build_indirect_call(
                        llvm_type,
                        pointer,
                        &call_arguments,
                        "call",
                    )?,
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
        if self.generator.is_class_type(&receiver_type)
            && let Some(place) = operand_place(receiver)
            && matches!(place.projection.last(), Some(Projection::Dereference))
        {
            return Ok(self.lower_class_object_pointer(place)?.into());
        }
        let indirect_value_receiver = matches!(receiver_type, Type::String)
            || matches!(
                &receiver_type,
                Type::Nominal(declaration, arguments)
                    if !self
                        .generator
                        .is_class_type(&Type::Nominal(*declaration, arguments.clone()))
            );
        if indirect_value_receiver {
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
            Constant::ExternalFunction { symbol, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "external function constant lacks a function type".into(),
                    ));
                };
                let llvm_type = self.generator.llvm_function_type(
                    &function_type.parameters,
                    &function_type.result,
                    &function_type.effects,
                )?;
                let function = self
                    .generator
                    .module
                    .get_function(symbol)
                    .unwrap_or_else(|| self.generator.module.add_function(symbol, llvm_type, None));
                Ok((function, function_type.clone()))
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
        let target = self.resolve_emitted_callable_raw(callable, function_type)?;
        if self
            .generator
            .layouts
            .decorators
            .get(&callable)
            .is_some_and(|decorators| !decorators.is_empty())
        {
            return self.declare_decorator_wrapper(callable, function_type, target);
        }
        Ok(target)
    }

    fn resolve_emitted_callable_raw(
        &self,
        callable: Callable,
        function_type: &FunctionType,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let candidates = self
            .generator
            .signatures
            .iter()
            .filter(|(instance, signature)| {
                instance.callable == callable && signature_matches(signature, function_type)
            })
            .collect::<Vec<_>>();
        let exact = candidates
            .iter()
            .filter(|(_, signature)| signature_exact_match(signature, function_type))
            .map(|(instance, _)| self.generator.functions[instance])
            .collect::<Vec<_>>();
        let mut matches = if exact.is_empty() {
            candidates
                .iter()
                .map(|(instance, _)| self.generator.functions[instance])
                .collect()
        } else {
            exact
        };
        if matches.is_empty()
            && let Some(external_name) = self
                .generator
                .layouts
                .externs
                .get(&callable)
                .map(|external| external.name.as_str())
        {
            matches = self
                .generator
                .layouts
                .exports
                .iter()
                .filter(|(_, exported_name)| exported_name == &external_name)
                .flat_map(|(exported_callable, _)| {
                    self.generator
                        .signatures
                        .iter()
                        .filter(move |(instance, signature)| {
                            instance.callable == *exported_callable
                                && signature_matches(signature, function_type)
                        })
                        .map(|(instance, _)| self.generator.functions[instance])
                })
                .collect();
        }
        let function = matches.first().copied().ok_or_else(|| {
            CodegenError::Unsupported(format!("call target {callable:?} was not emitted"))
        })?;
        if matches.len() > 1 {
            return Err(CodegenError::Unsupported(format!(
                "call target {callable:?} is ambiguous after specialization"
            )));
        }
        Ok(function)
    }

    fn declare_decorator_wrapper(
        &self,
        callable: Callable,
        requested: &FunctionType,
        target: FunctionValue<'ctx>,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let decorators = self
            .generator
            .layouts
            .decorators
            .get(&callable)
            .cloned()
            .unwrap_or_default();
        if decorators.is_empty() {
            return Ok(target);
        }
        if decorators.iter().any(|decorator| {
            decorator
                .signature
                .parameters
                .first()
                .is_none_or(|parameter| matches!(parameter, Type::Unknown))
        }) {
            // Unknown-identity decorators are a source-level annotation.  They are checked for
            // signature validity but deliberately do not cross the native ABI until the target
            // type is concrete (method decorators use the exact callable contract below).
            return Ok(target);
        }
        let wrapper_name = format!(
            "tn_decorated_{}_{}",
            callable.declaration.0,
            stable_hash(&format!("{callable:?}:{requested:?}"))
        );
        if let Some(wrapper) = self.generator.module.get_function(&wrapper_name) {
            return Ok(wrapper);
        }

        // The wrapper preserves the target's concrete native ABI.  Method receivers are already
        // represented as pointers in LLVM, so the same function type can be used for both the
        // original target and its decorated entry point.
        let wrapper = self
            .generator
            .module
            .add_function(&wrapper_name, target.get_type(), None);
        wrapper.set_linkage(Linkage::Internal);
        let entry = self.generator.context.append_basic_block(wrapper, "entry");
        let builder = self.generator.context.create_builder();
        builder.position_at_end(entry);

        let target_parameters = target.get_type().get_param_types();
        let explicit_count = requested.parameters.len();
        let has_receiver = target_parameters.len() == explicit_count.saturating_add(1);
        if target_parameters.len() != explicit_count && !has_receiver {
            return Err(CodegenError::Unsupported(format!(
                "decorated callable {callable:?} has an incompatible native parameter count"
            )));
        }

        let mut adapter_parameters = vec![Type::RawPointer {
            mutable: true,
            pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
        }];
        adapter_parameters.extend(requested.parameters.clone());
        let adapter_type = self.generator.llvm_function_type(
            &adapter_parameters,
            &requested.result,
            &requested.effects,
        )?;
        let adapter_name = format!(
            "tn_decorator_target_{}_{}",
            callable.declaration.0,
            stable_hash(&format!("{callable:?}:{requested:?}:target"))
        );
        let adapter = self
            .generator
            .module
            .get_function(&adapter_name)
            .unwrap_or_else(|| {
                let function =
                    self.generator
                        .module
                        .add_function(&adapter_name, adapter_type, None);
                function.set_linkage(Linkage::Internal);
                function
            });
        if adapter.get_first_basic_block().is_none() {
            let adapter_entry = self.generator.context.append_basic_block(adapter, "entry");
            let adapter_builder = self.generator.context.create_builder();
            adapter_builder.position_at_end(adapter_entry);
            let mut adapter_parameters = adapter.get_param_iter();
            let environment = adapter_parameters
                .next()
                .ok_or_else(|| CodegenError::Builder("decorator environment is missing".into()))?;
            let explicit = adapter_parameters
                .map(|value| value.as_basic_value_enum().into())
                .collect::<Vec<BasicMetadataValueEnum>>();
            let mut target_arguments =
                Vec::with_capacity(explicit.len() + usize::from(has_receiver));
            if has_receiver {
                target_arguments.push(environment.into());
            }
            target_arguments.extend(explicit);
            let call = adapter_builder.build_call(target, &target_arguments, "decorator.target")?;
            if requested.result.as_ref() == &Type::Primitive(PrimitiveType::Void)
                && requested.effects.is_empty()
            {
                adapter_builder.build_return(None)?;
            } else {
                let value = call.try_as_basic_value().basic().ok_or_else(|| {
                    CodegenError::Builder("decorated target returned void".into())
                })?;
                adapter_builder.build_return(Some(&value))?;
            }
        }

        let pointer = self.generator.context.ptr_type(AddressSpace::default());
        let null = pointer.const_null();
        let initial_environment = if has_receiver {
            wrapper
                .get_first_param()
                .map(|value| value.into_pointer_value())
                .ok_or_else(|| CodegenError::Builder("decorated receiver is missing".into()))?
        } else {
            null
        };
        let mut decorated = self.callable_value_with_builder(
            &builder,
            adapter.as_global_value().as_pointer_value(),
            initial_environment,
            null,
            self.generator.context.i64_type().const_int(
                stable_hash(&format!("decorated:{callable:?}:{requested:?}")),
                false,
            ),
        )?;

        for (index, decorator) in decorators.iter().enumerate().rev() {
            let decorator_target =
                self.resolve_emitted_callable_raw(decorator.decorator, &decorator.signature)?;
            let mut arguments = vec![decorated.into()];
            if decorator.signature.parameters.len() == 2 {
                let context_type = decorator.signature.parameters.get(1).ok_or_else(|| {
                    CodegenError::Unsupported("decorator context is missing".into())
                })?;
                let context = self.decorator_context_value(
                    &builder,
                    context_type,
                    &decorator.name,
                    decorator.is_static,
                    decorator.is_private,
                )?;
                arguments.push(context.into());
            }
            let call = builder.build_call(
                decorator_target,
                &arguments,
                &format!("decorator.apply.{index}"),
            )?;
            decorated = call
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("decorator returned void".into()))?
                .into_struct_value();
        }

        let code = builder
            .build_extract_value(decorated, 0, "decorated.code")?
            .into_pointer_value();
        let environment = builder
            .build_extract_value(decorated, 1, "decorated.environment")?
            .into_pointer_value();
        let mut invoke_parameters = vec![Type::RawPointer {
            mutable: true,
            pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
        }];
        invoke_parameters.extend(requested.parameters.clone());
        let invoke_type = self.generator.llvm_function_type(
            &invoke_parameters,
            &requested.result,
            &requested.effects,
        )?;
        let invoke_arguments = wrapper
            .get_param_iter()
            .skip(usize::from(has_receiver))
            .map(|value| value.as_basic_value_enum().into())
            .collect::<Vec<BasicMetadataValueEnum>>();
        let mut call_arguments = Vec::with_capacity(invoke_arguments.len() + 1);
        call_arguments.push(environment.into());
        call_arguments.extend(invoke_arguments);
        let call =
            builder.build_indirect_call(invoke_type, code, &call_arguments, "decorated.call")?;
        if requested.result.as_ref() == &Type::Primitive(PrimitiveType::Void)
            && requested.effects.is_empty()
        {
            builder.build_return(None)?;
        } else {
            let value = call
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("decorated callable returned void".into()))?;
            builder.build_return(Some(&value))?;
        }
        Ok(wrapper)
    }

    fn callable_value_with_builder(
        &self,
        builder: &Builder<'ctx>,
        code: PointerValue<'ctx>,
        environment: PointerValue<'ctx>,
        drop: PointerValue<'ctx>,
        identity: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>, CodegenError> {
        let mut value = self.generator.callable_type().const_zero();
        value = builder
            .build_insert_value(value, code, 0, "decorator.callable.code")?
            .into_struct_value();
        value = builder
            .build_insert_value(value, environment, 1, "decorator.callable.environment")?
            .into_struct_value();
        let value = builder
            .build_insert_value(value, drop, 2, "decorator.callable.drop")?
            .into_struct_value();
        Ok(builder
            .build_insert_value(value, identity, 3, "decorator.callable.identity")?
            .into_struct_value())
    }

    fn decorator_context_value(
        &self,
        builder: &Builder<'ctx>,
        context_type: &Type,
        name: &str,
        is_static: bool,
        is_private: bool,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let context = self.generator.basic_type(context_type)?.into_struct_type();
        if context.count_fields() < 3 {
            return Err(CodegenError::Unsupported(
                "ClassMethodDecoratorContext must contain name, isStatic, and isPrivate".into(),
            ));
        }
        let name_bytes = builder.build_global_string_ptr(name, "decorator.context.name")?;
        let name_value = builder
            .build_call(
                self.generator.runtime_string_from_bytes(),
                &[
                    name_bytes.as_pointer_value().into(),
                    self.generator
                        .pointer_int_type()
                        .const_int(u64::try_from(name.len()).unwrap_or(u64::MAX), false)
                        .into(),
                ],
                "decorator.context.string",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("decorator context string missing".into()))?;
        let mut value = context.const_zero();
        value = builder
            .build_insert_value(value, name_value, 0, "decorator.context.name.value")?
            .into_struct_value();
        value = builder
            .build_insert_value(
                value,
                self.generator
                    .context
                    .bool_type()
                    .const_int(u64::from(is_static), false),
                1,
                "decorator.context.static",
            )?
            .into_struct_value();
        Ok(builder
            .build_insert_value(
                value,
                self.generator
                    .context
                    .bool_type()
                    .const_int(u64::from(is_private), false),
                2,
                "decorator.context.private",
            )?
            .into_struct_value()
            .into())
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

    fn lower_thread_spawn(
        &self,
        operands: &[Operand],
        handle_type: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if operands.len() != 1 {
            return Err(CodegenError::Unsupported(
                "thread_spawn expects one callable".into(),
            ));
        }
        let Type::Function(signature) = self.operand_type(&operands[0])? else {
            return Err(CodegenError::Unsupported(
                "thread_spawn requires a function value".into(),
            ));
        };
        if !signature.parameters.is_empty() || !signature.effects.is_empty() || signature.is_async {
            return Err(CodegenError::Unsupported(
                "Thread.spawn requires an infallible synchronous zero-argument callable".into(),
            ));
        }
        if signature.result.as_ref() == &Type::Primitive(PrimitiveType::Void) {
            return Err(CodegenError::Unsupported(
                "Thread.spawn requires a value-producing callable".into(),
            ));
        }
        let callable = self.lower_operand(&operands[0])?.into_struct_value();
        let pointer = self.generator.context.ptr_type(AddressSpace::default());
        let code = self
            .builder
            .build_extract_value(callable, 0, "thread.callable.code")?
            .into_pointer_value();
        let environment = self
            .builder
            .build_extract_value(callable, 1, "thread.callable.environment")?
            .into_pointer_value();
        let drop = self
            .builder
            .build_extract_value(callable, 2, "thread.callable.drop")?
            .into_pointer_value();
        let result_type = self.generator.basic_type(signature.result.as_ref())?;
        let result_size = result_type
            .size_of()
            .ok_or_else(|| CodegenError::Unsupported("thread result has no known size".into()))?;

        // The runtime owns this five-pointer state while the pthread callback runs:
        // { invoke, code, environment, drop, result }.  The generated invoke wrapper
        // supplies the typed call ABI for the concrete T, keeping the public API typed.
        let state_type = self.generator.context.struct_type(
            &[
                pointer.into(),
                pointer.into(),
                pointer.into(),
                pointer.into(),
                pointer.into(),
            ],
            false,
        );
        let wrapper_name = format!(
            "tn_thread_task_invoke_{}",
            stable_hash(&format!(
                "{}:{:?}",
                self.function.get_name().to_string_lossy(),
                operands
            ))
        );
        let invoke = if let Some(existing) = self.generator.module.get_function(&wrapper_name) {
            existing
        } else {
            let invoke_type = self
                .generator
                .context
                .void_type()
                .fn_type(&[pointer.into()], false);
            let invoke = self
                .generator
                .module
                .add_function(&wrapper_name, invoke_type, None);
            invoke.set_linkage(Linkage::Internal);
            let entry = self.generator.context.append_basic_block(invoke, "entry");
            let wrapper_builder = self.generator.context.create_builder();
            wrapper_builder.position_at_end(entry);
            let argument = invoke
                .get_first_param()
                .ok_or_else(|| CodegenError::Builder("thread invoke argument is missing".into()))?
                .into_pointer_value();
            let code_address =
                wrapper_builder.build_struct_gep(state_type, argument, 1, "thread.code.address")?;
            let code_value = wrapper_builder
                .build_load(pointer, code_address, "thread.code")?
                .into_pointer_value();
            let environment_address = wrapper_builder.build_struct_gep(
                state_type,
                argument,
                2,
                "thread.environment.address",
            )?;
            let environment_value = wrapper_builder
                .build_load(pointer, environment_address, "thread.environment")?
                .into_pointer_value();
            let result_address = wrapper_builder.build_struct_gep(
                state_type,
                argument,
                4,
                "thread.result.address",
            )?;
            let result_pointer = wrapper_builder
                .build_load(pointer, result_address, "thread.result")?
                .into_pointer_value();
            let call_type = self.generator.llvm_function_type(
                &[Type::RawPointer {
                    mutable: true,
                    pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
                }],
                &signature.result,
                &[],
            )?;
            let call = wrapper_builder.build_indirect_call(
                call_type,
                code_value,
                &[environment_value.into()],
                "thread.call",
            )?;
            let result = call
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("thread callable returned no value".into()))?;
            wrapper_builder.build_store(result_pointer, result)?;
            wrapper_builder.build_return(None)?;
            invoke
        };
        let runtime_handle = self
            .builder
            .build_call(
                self.generator.runtime_thread_spawn_task(),
                &[
                    invoke.as_global_value().as_pointer_value().into(),
                    code.into(),
                    environment.into(),
                    drop.into(),
                    result_size.into(),
                ],
                "thread.spawn",
            )?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Builder("thread spawn returned no handle".into()))?
            .into_pointer_value();
        let handle_layout = self.generator.basic_type(handle_type)?.into_struct_type();
        let value = self
            .builder
            .build_insert_value(
                handle_layout.const_zero(),
                runtime_handle,
                0,
                "thread.handle",
            )?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(
                value,
                self.generator.context.bool_type().const_zero(),
                1,
                "thread.joined",
            )?
            .into_struct_value()
            .into())
    }

    fn lower_atomic_operation(
        &self,
        operation: &str,
        operands: &[Operand],
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let pointer = |operand: &Operand| {
            self.lower_operand(operand)
                .map(BasicValueEnum::into_pointer_value)
        };
        match operation {
            "atomic_i32_load" | "atomic_u64_load" | "atomic_usize_load"
                if operands.len() == 2 && self.is_dynamic_atomic_order(&operands[1]) =>
            {
                return self.lower_dynamic_atomic_operation(operation, operands, ty, 1);
            }
            "atomic_i32_store"
            | "atomic_u64_store"
            | "atomic_usize_store"
            | "atomic_i32_fetch_add"
            | "atomic_u64_fetch_add"
            | "atomic_usize_fetch_add"
                if operands.len() == 3 && self.is_dynamic_atomic_order(&operands[2]) =>
            {
                return self.lower_dynamic_atomic_operation(operation, operands, ty, 2);
            }
            "atomic_fence" if operands.len() == 1 && self.is_dynamic_atomic_order(&operands[0]) => {
                return self.lower_dynamic_atomic_operation(operation, operands, ty, 0);
            }
            "atomic_i32_compare_exchange"
            | "atomic_u64_compare_exchange"
            | "atomic_usize_compare_exchange"
                if operands.len() == 5
                    && (self.is_dynamic_atomic_order(&operands[3])
                        || self.is_dynamic_atomic_order(&operands[4])) =>
            {
                return self.lower_dynamic_atomic_compare_exchange(operation, operands, ty);
            }
            _ => {}
        }
        match operation {
            "atomic_i32_load" | "atomic_u64_load" | "atomic_usize_load" => {
                if operands.len() != 2 {
                    return Err(CodegenError::Unsupported(format!(
                        "{operation} expects pointer and memory order"
                    )));
                }
                let integer_type = self.generator.basic_type(ty)?.into_int_type();
                let load =
                    self.builder
                        .build_load(integer_type, pointer(&operands[0])?, "atomic.load")?;
                let order = self.atomic_ordering(&operands[1])?;
                if !Self::valid_atomic_order(operation, order) {
                    return Err(CodegenError::Unsupported(format!(
                        "{order:?} ordering is invalid for {operation}"
                    )));
                }
                load.as_instruction_value()
                    .ok_or_else(|| {
                        CodegenError::Builder("atomic load is not an instruction".into())
                    })?
                    .set_atomic_ordering(order)
                    .map_err(|error| CodegenError::Builder(error.to_string()))?;
                Ok(load)
            }
            "atomic_i32_store" | "atomic_u64_store" | "atomic_usize_store" => {
                if operands.len() != 3 {
                    return Err(CodegenError::Unsupported(format!(
                        "{operation} expects pointer, value, and memory order"
                    )));
                }
                let value = self.lower_operand(&operands[1])?.into_int_value();
                let order = self.atomic_ordering(&operands[2])?;
                if !Self::valid_atomic_order(operation, order) {
                    return Err(CodegenError::Unsupported(format!(
                        "{order:?} ordering is invalid for {operation}"
                    )));
                }
                self.builder
                    .build_store(pointer(&operands[0])?, value)?
                    .set_atomic_ordering(order)
                    .map_err(|error| CodegenError::Builder(error.to_string()))?;
                Ok(value.into())
            }
            "atomic_i32_fetch_add" | "atomic_u64_fetch_add" | "atomic_usize_fetch_add" => {
                if operands.len() != 3 {
                    return Err(CodegenError::Unsupported(format!(
                        "{operation} expects pointer, delta, and memory order"
                    )));
                }
                Ok(self
                    .builder
                    .build_atomicrmw(
                        AtomicRMWBinOp::Add,
                        pointer(&operands[0])?,
                        self.lower_operand(&operands[1])?.into_int_value(),
                        self.atomic_ordering(&operands[2])?,
                    )?
                    .into())
            }
            "atomic_i32_compare_exchange"
            | "atomic_u64_compare_exchange"
            | "atomic_usize_compare_exchange" => {
                if operands.len() != 5 || *ty != Type::Primitive(PrimitiveType::Bool) {
                    return Err(CodegenError::Unsupported(format!(
                        "{operation} expects pointer, expected pointer, desired, success order, failure order, and bool result"
                    )));
                }
                let expected_pointer = pointer(&operands[1])?;
                let integer_type = self
                    .generator
                    .basic_type(&self.operand_type(&operands[2])?)?
                    .into_int_type();
                let expected = self
                    .builder
                    .build_load(integer_type, expected_pointer, "atomic.expected")?
                    .into_int_value();
                let pair = self.builder.build_cmpxchg(
                    pointer(&operands[0])?,
                    expected,
                    self.lower_operand(&operands[2])?.into_int_value(),
                    self.atomic_ordering(&operands[3])?,
                    self.atomic_ordering(&operands[4])?,
                )?;
                let observed = self
                    .builder
                    .build_extract_value(pair, 0, "atomic.observed")?
                    .into_int_value();
                self.builder.build_store(expected_pointer, observed)?;
                Ok(self
                    .builder
                    .build_extract_value(pair, 1, "atomic.exchanged")?)
            }
            "atomic_fence" => {
                if operands.len() != 1 || *ty != Type::Primitive(PrimitiveType::Bool) {
                    return Err(CodegenError::Unsupported(
                        "atomic_fence expects a memory order and bool result".into(),
                    ));
                }
                let order = self.atomic_ordering(&operands[0])?;
                if !Self::valid_atomic_order(operation, order) {
                    return Err(CodegenError::Unsupported(format!(
                        "{order:?} ordering is invalid for {operation}"
                    )));
                }
                self.builder.build_fence(order, false, "")?;
                Ok(self.generator.context.bool_type().const_all_ones().into())
            }
            _ => Err(CodegenError::Unsupported(format!(
                "unknown atomic operation {operation}"
            ))),
        }
    }

    fn lower_dynamic_atomic_operation(
        &self,
        operation: &str,
        operands: &[Operand],
        ty: &Type,
        order_index: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let order = self.lower_operand(&operands[order_index])?.into_int_value();
        let merge = self
            .generator
            .context
            .append_basic_block(self.function, "atomic.order.merge");
        let invalid = self
            .generator
            .context
            .append_basic_block(self.function, "atomic.order.invalid");
        let orderings = [
            AtomicOrdering::Monotonic,
            AtomicOrdering::Acquire,
            AtomicOrdering::Release,
            AtomicOrdering::AcquireRelease,
            AtomicOrdering::SequentiallyConsistent,
        ];
        let blocks = orderings
            .iter()
            .enumerate()
            .map(|(index, _)| {
                self.generator
                    .context
                    .append_basic_block(self.function, &format!("atomic.order.{index}"))
            })
            .collect::<Vec<_>>();
        let cases = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| (order.get_type().const_int(index as u64, false), *block))
            .collect::<Vec<_>>();
        self.builder.build_switch(order, invalid, &cases)?;
        let mut incoming = Vec::with_capacity(blocks.len());
        for (index, block) in blocks.iter().enumerate() {
            self.builder.position_at_end(*block);
            if !Self::valid_atomic_order(operation, orderings[index]) {
                self.builder.build_unconditional_branch(invalid)?;
                continue;
            }
            let mut fixed = operands.to_vec();
            fixed[order_index] = Self::atomic_order_operand(index as i128);
            let value = self.lower_atomic_operation(operation, &fixed, ty)?;
            let predecessor = self
                .builder
                .get_insert_block()
                .ok_or_else(|| CodegenError::Builder("atomic order block disappeared".into()))?;
            self.builder.build_unconditional_branch(merge)?;
            incoming.push((value, predecessor));
        }
        self.builder.position_at_end(invalid);
        self.abort_invalid_atomic_order()?;
        self.builder.position_at_end(merge);
        let phi = self
            .builder
            .build_phi(self.generator.basic_type(ty)?, "atomic.order.value")?;
        let incoming = incoming
            .iter()
            .map(|(value, block)| (value as &dyn BasicValue, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&incoming);
        Ok(phi.as_basic_value())
    }

    fn lower_dynamic_atomic_compare_exchange(
        &self,
        operation: &str,
        operands: &[Operand],
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if *ty != Type::Primitive(PrimitiveType::Bool) {
            return Err(CodegenError::Unsupported(
                "atomic compare exchange must return bool".into(),
            ));
        }
        let success = self.lower_operand(&operands[3])?.into_int_value();
        let failure = self.lower_operand(&operands[4])?.into_int_value();
        let merge = self
            .generator
            .context
            .append_basic_block(self.function, "atomic.cmpxchg.merge");
        let invalid = self
            .generator
            .context
            .append_basic_block(self.function, "atomic.cmpxchg.invalid");
        let orderings = [
            AtomicOrdering::Monotonic,
            AtomicOrdering::Acquire,
            AtomicOrdering::Release,
            AtomicOrdering::AcquireRelease,
            AtomicOrdering::SequentiallyConsistent,
        ];
        let success_blocks = (0..orderings.len())
            .map(|index| {
                self.generator
                    .context
                    .append_basic_block(self.function, &format!("atomic.cmpxchg.success.{index}"))
            })
            .collect::<Vec<_>>();
        let success_cases = success_blocks
            .iter()
            .enumerate()
            .map(|(index, block)| (success.get_type().const_int(index as u64, false), *block))
            .collect::<Vec<_>>();
        self.builder
            .build_switch(success, invalid, &success_cases)?;
        let mut incoming = Vec::new();
        for (success_index, success_block) in success_blocks.iter().enumerate() {
            self.builder.position_at_end(*success_block);
            let failure_blocks = (0..orderings.len())
                .map(|failure_index| {
                    self.generator.context.append_basic_block(
                        self.function,
                        &format!("atomic.cmpxchg.failure.{success_index}.{failure_index}"),
                    )
                })
                .collect::<Vec<_>>();
            let failure_cases = failure_blocks
                .iter()
                .enumerate()
                .map(|(index, block)| (failure.get_type().const_int(index as u64, false), *block))
                .collect::<Vec<_>>();
            self.builder
                .build_switch(failure, invalid, &failure_cases)?;
            for (failure_index, failure_block) in failure_blocks.iter().enumerate() {
                self.builder.position_at_end(*failure_block);
                if orderings[failure_index] > orderings[success_index]
                    || matches!(
                        orderings[failure_index],
                        AtomicOrdering::Release | AtomicOrdering::AcquireRelease
                    )
                {
                    self.builder.build_unconditional_branch(invalid)?;
                    continue;
                }
                let mut fixed = operands.to_vec();
                fixed[3] = Self::atomic_order_operand(success_index as i128);
                fixed[4] = Self::atomic_order_operand(failure_index as i128);
                let value = self.lower_atomic_operation(operation, &fixed, ty)?;
                let predecessor = self.builder.get_insert_block().ok_or_else(|| {
                    CodegenError::Builder("atomic compare-exchange block disappeared".into())
                })?;
                self.builder.build_unconditional_branch(merge)?;
                incoming.push((value, predecessor));
            }
        }
        self.builder.position_at_end(invalid);
        self.abort_invalid_atomic_order()?;
        self.builder.position_at_end(merge);
        let phi = self
            .builder
            .build_phi(self.generator.basic_type(ty)?, "atomic.cmpxchg.value")?;
        let incoming = incoming
            .iter()
            .map(|(value, block)| (value as &dyn BasicValue, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&incoming);
        Ok(phi.as_basic_value())
    }

    fn is_dynamic_atomic_order(&self, operand: &Operand) -> bool {
        match operand {
            Operand::Constant(Constant::Integer { .. } | Constant::Bool(_)) => false,
            _ => self.constant_enum_operand(operand).is_none(),
        }
    }

    fn atomic_order_operand(value: i128) -> Operand {
        Operand::Constant(Constant::Integer {
            value,
            ty: Type::Primitive(PrimitiveType::U8),
        })
    }

    fn abort_invalid_atomic_order(&self) -> Result<(), CodegenError> {
        self.builder.build_call(
            self.runtime_abort(),
            &[self
                .generator
                .context
                .i32_type()
                .const_int(
                    u64::from(stable_panic_code("invalid atomic memory order")),
                    false,
                )
                .into()],
            "atomic.order.abort",
        )?;
        self.builder.build_unreachable()?;
        Ok(())
    }

    fn atomic_ordering(&self, operand: &Operand) -> Result<AtomicOrdering, CodegenError> {
        let value = match operand {
            Operand::Constant(Constant::Integer { value, .. }) => *value,
            Operand::Constant(Constant::Bool(value)) => i128::from(*value),
            _ => self
                .constant_enum_operand(operand)
                .or_else(|| {
                    self.lower_operand(operand)
                        .ok()?
                        .into_int_value()
                        .get_zero_extended_constant()
                        .map(i128::from)
                })
                .ok_or_else(|| {
                    CodegenError::Unsupported(
                        "atomic memory order must be a compile-time constant".into(),
                    )
                })?,
        };
        match value {
            0 => Ok(AtomicOrdering::Monotonic),
            1 => Ok(AtomicOrdering::Acquire),
            2 => Ok(AtomicOrdering::Release),
            3 => Ok(AtomicOrdering::AcquireRelease),
            4 => Ok(AtomicOrdering::SequentiallyConsistent),
            _ => Err(CodegenError::Unsupported(format!(
                "invalid atomic memory order {value}"
            ))),
        }
    }

    fn valid_atomic_order(operation: &str, ordering: AtomicOrdering) -> bool {
        match operation {
            "atomic_i32_load" | "atomic_u64_load" | "atomic_usize_load" => !matches!(
                ordering,
                AtomicOrdering::Release | AtomicOrdering::AcquireRelease
            ),
            "atomic_i32_store" | "atomic_u64_store" | "atomic_usize_store" => !matches!(
                ordering,
                AtomicOrdering::Acquire | AtomicOrdering::AcquireRelease
            ),
            "atomic_fence" => ordering != AtomicOrdering::Monotonic,
            _ => true,
        }
    }

    fn constant_enum_operand(&self, operand: &Operand) -> Option<i128> {
        let place = match operand {
            Operand::Copy(place) | Operand::Move(place) if place.projection.is_empty() => place,
            _ => return None,
        };
        for block in self.body.blocks.iter().rev() {
            for statement in block.statements.iter().rev() {
                let StatementKind::Assign(destination, value) = &statement.kind else {
                    continue;
                };
                if destination != place {
                    continue;
                }
                match value.as_ref() {
                    Rvalue::Use(Operand::Constant(Constant::Integer { value, .. })) => {
                        return Some(*value);
                    }
                    Rvalue::Aggregate {
                        ty: Type::Nominal(declaration, _),
                        variant: Some(variant),
                        fields,
                        ..
                    } if fields.is_empty() => {
                        return self.generator.layouts.nominals.get(declaration).and_then(
                            |layout| match &layout.kind {
                                NominalKind::Enum { discriminants, .. } => {
                                    discriminants.get(*variant as usize).copied()
                                }
                                _ => None,
                            },
                        );
                    }
                    _ => return None,
                }
            }
        }
        None
    }

    fn lower_operand(&self, operand: &Operand) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match operand {
            Operand::Copy(place) | Operand::Move(place) => {
                let place_type = self.place_type(place)?;
                if self.generator.is_class_type(&place_type)
                    && matches!(place.projection.last(), Some(Projection::Dereference))
                {
                    return Ok(self.place_pointer(place)?.into());
                }
                let raw_string_dereference = place_type == Type::Str
                    && matches!(place.projection.last(), Some(Projection::Dereference))
                    && matches!(
                        self.local_type(place.local.0)?,
                        Type::RawPointer { pointee, .. } if pointee.as_ref() == &Type::Str
                    );
                if raw_string_dereference {
                    return Ok(self.place_pointer(place)?.into());
                }
                let ty = self.generator.basic_type(&place_type)?;
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

    fn callable_value(
        &self,
        code: PointerValue<'ctx>,
        environment: PointerValue<'ctx>,
        drop: PointerValue<'ctx>,
        identity: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>, CodegenError> {
        let mut value = self.generator.callable_type().const_zero();
        value = self
            .builder
            .build_insert_value(value, code, 0, "callable.code")?
            .into_struct_value();
        value = self
            .builder
            .build_insert_value(value, environment, 1, "callable.environment")?
            .into_struct_value();
        let value = self
            .builder
            .build_insert_value(value, drop, 2, "callable.drop")?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(value, identity, 3, "callable.identity")?
            .into_struct_value())
    }

    fn direct_callable_adapter(
        &self,
        callable: Callable,
        signature: &FunctionType,
        target: FunctionValue<'ctx>,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let name = format!(
            "tn_callable_adapter_{}_{}",
            callable.declaration.0,
            stable_hash(&format!("{callable:?}:{signature:?}"))
        );
        if let Some(function) = self.generator.module.get_function(&name) {
            return Ok(function);
        }
        let mut parameters = vec![Type::RawPointer {
            mutable: true,
            pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
        }];
        parameters.extend(signature.parameters.clone());
        let function_type = self.generator.llvm_function_type(
            &parameters,
            &signature.result,
            &signature.effects,
        )?;
        let adapter = self
            .generator
            .module
            .add_function(&name, function_type, None);
        adapter.set_linkage(Linkage::Internal);
        let entry = self.generator.context.append_basic_block(adapter, "entry");
        let builder = self.generator.context.create_builder();
        builder.position_at_end(entry);
        let arguments = adapter
            .get_param_iter()
            .skip(1)
            .map(|argument| argument.as_basic_value_enum().into())
            .collect::<Vec<BasicMetadataValueEnum>>();
        let call = builder.build_call(target, &arguments, "callable.target")?;
        if signature.result.as_ref() == &Type::Primitive(PrimitiveType::Void)
            && signature.effects.is_empty()
        {
            builder.build_return(None)?;
        } else {
            let value = call
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| CodegenError::Builder("callable target returned void".into()))?;
            builder.build_return(Some(&value))?;
        }
        Ok(adapter)
    }

    fn direct_external_callable_adapter(
        &self,
        symbol: &str,
        signature: &FunctionType,
        target: FunctionValue<'ctx>,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let name = format!(
            "tn_external_callable_adapter_{}",
            stable_hash(&format!("{symbol}:{signature:?}"))
        );
        if let Some(function) = self.generator.module.get_function(&name) {
            return Ok(function);
        }
        let mut parameters = vec![Type::RawPointer {
            mutable: true,
            pointee: Box::new(Type::Primitive(PrimitiveType::U8)),
        }];
        parameters.extend(signature.parameters.clone());
        let function_type = self.generator.llvm_function_type(
            &parameters,
            &signature.result,
            &signature.effects,
        )?;
        let adapter = self
            .generator
            .module
            .add_function(&name, function_type, None);
        adapter.set_linkage(Linkage::Internal);
        let entry = self.generator.context.append_basic_block(adapter, "entry");
        let builder = self.generator.context.create_builder();
        builder.position_at_end(entry);
        let arguments = adapter
            .get_param_iter()
            .skip(1)
            .map(|value| value.as_basic_value_enum().into())
            .collect::<Vec<BasicMetadataValueEnum>>();
        let call = builder.build_call(target, &arguments, "external.callable.target")?;
        if signature.result.as_ref() == &Type::Primitive(PrimitiveType::Void)
            && signature.effects.is_empty()
        {
            builder.build_return(None)?;
        } else {
            let value = call.try_as_basic_value().basic().ok_or_else(|| {
                CodegenError::Builder("external callable target returned void".into())
            })?;
            builder.build_return(Some(&value))?;
        }
        Ok(adapter)
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
        let object_pointer = self.lower_class_object_pointer(object)?;
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

    /// Resolve the heap object behind a class place, including a mutable class
    /// reference passed through a function boundary.  References are address
    /// based, so a dereference place can carry either the class pointer itself
    /// or a pointer to a slot containing that pointer; the latter needs one
    /// additional load before reading the class descriptor.
    fn lower_class_object_pointer(
        &self,
        object: &Place,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let pointer = self.place_pointer(object)?;
        if matches!(object.projection.last(), Some(Projection::Dereference)) {
            return Ok(pointer);
        }
        Ok(self
            .builder
            .build_load(
                self.generator.context.ptr_type(AddressSpace::default()),
                pointer,
                "virtual.object",
            )?
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
        let witness_source = match &source {
            Type::Reference { referent, .. } => referent.as_ref().clone(),
            source => source.clone(),
        };
        let data = if matches!(
            &source,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::String | Type::Str)
        ) {
            self.place_pointer(operand_place(operand).ok_or_else(|| {
                CodegenError::Unsupported(
                    "borrowed string interface source must be addressable".into(),
                )
            })?)?
        } else if matches!(&source, Type::Reference { .. }) {
            self.lower_operand(operand)?.into_pointer_value()
        } else if self.generator.is_class_type(&source) {
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
        let witness = match &witness_source {
            Type::Primitive(_) | Type::String | Type::Str => self
                .generator
                .builtin_witnesses
                .get(&(*interface, witness_source.clone()))
                .copied()
                .ok_or_else(|| {
                    CodegenError::Unsupported(format!(
                        "no builtin witness table for interface {interface:?} and source {witness_source:?}"
                    ))
                })?,
            Type::Nominal(target_declaration, _) => self
                .generator
                .witnesses
                .get(&(*interface, *target_declaration))
                .copied()
                .ok_or_else(|| {
                    CodegenError::Unsupported(format!(
                        "no witness table for interface {interface:?} and target {target_declaration:?}"
                    ))
                })?,
            _ => {
                return Err(CodegenError::Unsupported(format!(
                    "interface coercion source is not concrete: {witness_source:?}"
                )))
            }
        };
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
                let unit_enum = self
                    .generator
                    .layouts
                    .nominals
                    .get(declaration)
                    .is_some_and(|layout| {
                        matches!(
                            layout.kind,
                            NominalKind::Enum { ref variants, .. } if variants.iter().all(Vec::is_empty)
                        )
                    });
                Some(if c_repr || unit_enum {
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
                            matches!(
                                layout.kind,
                                NominalKind::Enum { c_repr: true, .. }
                            ) || matches!(
                                layout.kind,
                                NominalKind::Enum { ref variants, .. } if variants.iter().all(Vec::is_empty)
                            )
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

    #[allow(clippy::too_many_lines)]
    fn lower_constant(&self, constant: &Constant) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        Ok(match constant {
            Constant::Bool(value) => self
                .generator
                .context
                .bool_type()
                .const_int(u64::from(*value), false)
                .into(),
            Constant::Integer { value, ty } => {
                let payload_type = match ty {
                    Type::Optional(inner) => inner.as_ref(),
                    _ => ty,
                };
                let payload = self
                    .generator
                    .basic_type(payload_type)?
                    .into_int_type()
                    .const_int_arbitrary_precision(&u128_words(value.cast_unsigned()))
                    .into();
                self.wrap_optional_constant(ty, payload)?
            }
            Constant::Float { bits, ty } => {
                let payload_type = match ty {
                    Type::Optional(inner) => inner.as_ref(),
                    _ => ty,
                };
                let payload = match payload_type {
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
                };
                self.wrap_optional_constant(ty, payload)?
            }
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
                let callable = Callable::function(*declaration);
                let target = self.resolve_emitted_callable(callable, function_type)?;
                let adapter = self.direct_callable_adapter(callable, function_type, target)?;
                self.callable_value(
                    adapter.as_global_value().as_pointer_value(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .i64_type()
                        .const_int(stable_hash(&format!("function:{declaration:?}")), false),
                )?
                .into()
            }
            Constant::ExternalFunction { symbol, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "external function constant lacks a function type".into(),
                    ));
                };
                let llvm_type = self.generator.llvm_function_type(
                    &function_type.parameters,
                    &function_type.result,
                    &function_type.effects,
                )?;
                let target = self
                    .generator
                    .module
                    .get_function(symbol)
                    .unwrap_or_else(|| self.generator.module.add_function(symbol, llvm_type, None));
                let adapter =
                    self.direct_external_callable_adapter(symbol, function_type, target)?;
                self.callable_value(
                    adapter.as_global_value().as_pointer_value(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator.context.i64_type().const_int(
                        stable_hash(&format!("external:{symbol}:{function_type:?}")),
                        false,
                    ),
                )?
                .into()
            }
            Constant::Method { owner, member, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "method constant lacks a function type".into(),
                    ));
                };
                let callable = Callable {
                    declaration: *owner,
                    member: Some(*member),
                };
                let target = self.resolve_emitted_callable(callable, function_type)?;
                let adapter = self.direct_callable_adapter(callable, function_type, target)?;
                self.callable_value(
                    adapter.as_global_value().as_pointer_value(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .i64_type()
                        .const_int(stable_hash(&format!("method:{callable:?}")), false),
                )?
                .into()
            }
            Constant::Constructor { owner, member, ty } => {
                let Type::Function(function_type) = ty else {
                    return Err(CodegenError::Unsupported(
                        "constructor constant lacks a function type".into(),
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
                    })?
                    .function;
                let callable = Callable {
                    declaration: *owner,
                    member: *member,
                };
                let adapter = self.direct_callable_adapter(callable, function_type, target)?;
                self.callable_value(
                    adapter.as_global_value().as_pointer_value(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    self.generator
                        .context
                        .i64_type()
                        .const_int(stable_hash(&format!("constructor:{callable:?}")), false),
                )?
                .into()
            }
            Constant::String(value) => self.lower_static_string(value)?.into(),
        })
    }

    fn lower_static_string(&self, value: &str) -> Result<StructValue<'ctx>, CodegenError> {
        let length = value.len();
        let total = 16usize
            .checked_add(length)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| CodegenError::Unsupported("string literal is too large".into()))?;
        let array_length = u32::try_from(total)
            .map_err(|_| CodegenError::Unsupported("string literal exceeds LLVM limits".into()))?;
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(&STRING_HEADER_MAGIC.to_le_bytes());
        bytes.extend_from_slice(
            &u64::try_from(length)
                .map_err(|_| CodegenError::Unsupported("string literal length overflow".into()))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
        let byte_type = self.generator.context.i8_type();
        let values = bytes
            .iter()
            .map(|byte| byte_type.const_int(u64::from(*byte), false))
            .collect::<Vec<_>>();
        let array_type = byte_type.array_type(array_length);
        let initializer = byte_type.const_array(&values);
        let global = self
            .generator
            .module
            .add_global(array_type, None, "tn.string.header");
        global.set_linkage(Linkage::Private);
        global.set_constant(true);
        global.set_alignment(8);
        global.set_initializer(&initializer);
        let zero = self.generator.context.i64_type().const_zero();
        let data_offset = self.generator.context.i64_type().const_int(16, false);
        let pointer = unsafe {
            global
                .as_pointer_value()
                .const_gep(array_type, &[zero, data_offset])
        };
        let structure = self.generator.borrowed_string_type();
        let value = self
            .builder
            .build_insert_value(structure.const_zero(), pointer, 0, "string.pointer")?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(
                value,
                self.generator
                    .pointer_int_type()
                    .const_int(u64::try_from(length).unwrap_or(u64::MAX), false),
                1,
                "string.length",
            )?
            .into_struct_value())
    }

    fn wrap_optional_constant(
        &self,
        ty: &Type,
        payload: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let Type::Optional(inner) = ty else {
            return Ok(payload);
        };
        let structure = self.generator.basic_type(ty)?.into_struct_type();
        let payload = self.lower_cast(payload, self.generator.basic_type(inner)?)?;
        let value = self
            .builder
            .build_insert_value(
                structure.const_zero(),
                self.generator.context.bool_type().const_int(1, false),
                0,
                "optional.constant.present",
            )?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(value, payload, 1, "optional.constant.payload")?
            .into_struct_value()
            .into())
    }

    fn global_pointer(&self, operation: &str) -> Result<PointerValue<'ctx>, CodegenError> {
        let declaration = operation
            .split_once(':')
            .and_then(|(_, value)| value.parse::<u64>().ok())
            .map(DeclarationId)
            .ok_or_else(|| {
                CodegenError::Unsupported(format!("invalid global operation `{operation}`"))
            })?;
        self.generator
            .globals
            .get(&declaration)
            .copied()
            .ok_or_else(|| {
                CodegenError::Unsupported(format!(
                    "global declaration {declaration:?} was not emitted"
                ))
            })
    }

    /// A global load is a borrowed read. Owned values loaded from a static must not
    /// destroy the same allocation when the temporary local goes out of scope; the
    /// global store remains the owner and releases the previous value on replacement.
    fn lower_borrowed_global_value(
        &self,
        ty: &Type,
        value: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match ty {
            Type::Function(_) => {
                let callable = value.into_struct_value();
                let borrowed = self
                    .builder
                    .build_insert_value(
                        callable,
                        self.generator
                            .context
                            .ptr_type(AddressSpace::default())
                            .const_null(),
                        2,
                        "global.borrowed.callable.drop",
                    )?
                    .into_struct_value();
                Ok(borrowed.into())
            }
            Type::Optional(inner) => {
                let optional = value.into_struct_value();
                let payload = self.builder.build_extract_value(
                    optional,
                    1,
                    "global.borrowed.optional.payload",
                )?;
                let payload = self.lower_borrowed_global_value(inner, payload)?;
                let borrowed = self
                    .builder
                    .build_insert_value(optional, payload, 1, "global.borrowed.optional.value")?
                    .into_struct_value();
                Ok(borrowed.into())
            }
            _ => Ok(value),
        }
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
        let mut class_object_pointer = false;
        for projection in &place.projection {
            match projection {
                Projection::Field { index, ty: field } => {
                    let structure = if self.generator.is_class_type(&ty) {
                        let object = if class_object_pointer {
                            pointer
                        } else {
                            self.builder
                                .build_load(
                                    self.generator.context.ptr_type(AddressSpace::default()),
                                    pointer,
                                    "class.object",
                                )?
                                .into_pointer_value()
                        };
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
                    class_object_pointer = false;
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
                    let fat_reference = matches!(
                        &ty,
                        Type::Reference {
                            mutable,
                            referent,
                            ..
                        }
                            if matches!(
                                referent.as_ref(),
                                Type::Slice(_) | Type::String | Type::Str
                            ) && !(*mutable && matches!(referent.as_ref(), Type::String))
                    );
                    let class_reference = matches!(
                        &ty,
                        Type::Reference { referent, .. }
                            if self.generator.is_class_type(referent)
                    );
                    if class_reference {
                        // Class references are address-based: the reference slot stores the
                        // heap object pointer.  Load that pointer once and keep it marked as an
                        // object pointer for subsequent field or virtual-dispatch projections.
                        // In particular, borrowed objects supplied by an FFI bridge may have a
                        // null descriptor, so probing descriptor memory here would be invalid.
                        pointer = self
                            .builder
                            .build_load(
                                self.generator.context.ptr_type(AddressSpace::default()),
                                pointer,
                                "class.reference.value",
                            )?
                            .into_pointer_value();
                        class_object_pointer = true;
                    } else if !fat_reference {
                        pointer = self
                            .builder
                            .build_load(
                                self.generator.context.ptr_type(AddressSpace::default()),
                                pointer,
                                "dereference.address",
                            )?
                            .into_pointer_value();
                        class_object_pointer = self.generator.is_class_type(&referent_type);
                    }
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
                    class_object_pointer = false;
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
                        class_object_pointer = false;
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
                    class_object_pointer = self.generator.is_class_type(&ty);
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

    fn place_is_fat_string_dereference(&self, place: &Place) -> Result<bool, CodegenError> {
        if !matches!(place.projection.last(), Some(Projection::Dereference)) {
            return Ok(false);
        }
        let mut prefix = place.clone();
        prefix.projection.pop();
        Ok(matches!(
            self.place_type(&prefix)?,
            Type::Reference { referent, .. }
                if matches!(referent.as_ref(), Type::String | Type::Str)
        ))
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

    fn lower_binding_rest(
        &self,
        operands: &[Operand],
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let Type::Slice(element) = ty else {
            return Err(CodegenError::Unsupported(
                "binding rest requires a slice result".into(),
            ));
        };
        let collection = operands.first().ok_or_else(|| {
            CodegenError::Unsupported("binding rest lacks a source collection".into())
        })?;
        let start_operand = operands.get(1).ok_or_else(|| {
            CodegenError::Unsupported("binding rest lacks a starting index".into())
        })?;
        let start = self.lower_operand(start_operand)?.into_int_value();
        let collection_type = self.operand_type(collection)?;
        let collection_place = operand_place(collection).ok_or_else(|| {
            CodegenError::Unsupported("binding rest source must be addressable".into())
        })?;
        let collection_pointer = self.place_pointer(collection_place)?;
        let (data, length) = match &collection_type {
            Type::Array(source_element, array_length) => {
                if source_element.as_ref() != element.as_ref() {
                    return Err(CodegenError::Unsupported(
                        "binding rest element type does not match its source".into(),
                    ));
                }
                let length = self
                    .generator
                    .pointer_int_type()
                    .const_int(*array_length, false);
                let valid = self.builder.build_int_compare(
                    IntPredicate::ULE,
                    start,
                    length,
                    "binding.rest.start.in_bounds",
                )?;
                self.guard(valid, "binding rest starts outside the array")?;
                let array = self
                    .generator
                    .basic_type(&collection_type)?
                    .into_array_type();
                let data = unsafe {
                    self.builder.build_gep(
                        array,
                        collection_pointer,
                        &[self.generator.context.i32_type().const_zero(), start],
                        "binding.rest.array.data",
                    )?
                };
                (
                    data,
                    self.builder
                        .build_int_sub(length, start, "binding.rest.length")?,
                )
            }
            Type::Slice(source_element) => {
                if source_element.as_ref() != element.as_ref() {
                    return Err(CodegenError::Unsupported(
                        "binding rest element type does not match its source".into(),
                    ));
                }
                let slice = self
                    .generator
                    .basic_type(&collection_type)?
                    .into_struct_type();
                let data = self
                    .builder
                    .build_load(
                        self.generator.context.ptr_type(AddressSpace::default()),
                        self.builder.build_struct_gep(
                            slice,
                            collection_pointer,
                            0,
                            "binding.rest.slice.data.address",
                        )?,
                        "binding.rest.slice.data",
                    )?
                    .into_pointer_value();
                let length = self
                    .builder
                    .build_load(
                        self.generator.pointer_int_type(),
                        self.builder.build_struct_gep(
                            slice,
                            collection_pointer,
                            1,
                            "binding.rest.slice.length.address",
                        )?,
                        "binding.rest.slice.length",
                    )?
                    .into_int_value();
                let valid = self.builder.build_int_compare(
                    IntPredicate::ULE,
                    start,
                    length,
                    "binding.rest.start.in_bounds",
                )?;
                self.guard(valid, "binding rest starts outside the slice")?;
                let data = unsafe {
                    self.builder.build_gep(
                        self.generator.basic_type(element)?,
                        data,
                        &[start],
                        "binding.rest.slice.data",
                    )?
                };
                (
                    data,
                    self.builder
                        .build_int_sub(length, start, "binding.rest.length")?,
                )
            }
            other => {
                return Err(CodegenError::Unsupported(format!(
                    "binding rest requires an array or slice source, found {other:?}"
                )));
            }
        };
        let structure = self.generator.basic_type(ty)?.into_struct_type();
        let value = self
            .builder
            .build_insert_value(structure.const_zero(), data, 0, "binding.rest.data")?
            .into_struct_value();
        Ok(self
            .builder
            .build_insert_value(value, length, 1, "binding.rest.length")?
            .into_struct_value()
            .into())
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

fn closure_operand_type(body: &Body, operand: &Operand) -> Result<Type, CodegenError> {
    match operand {
        Operand::Constant(constant) => Ok(constant.ty()),
        Operand::Copy(place) | Operand::Move(place) => {
            if place.projection.is_empty() {
                body.locals
                    .get(place.local.0 as usize)
                    .map(|local| local.ty.clone())
                    .ok_or_else(|| {
                        CodegenError::Unsupported("closure capture local is missing".into())
                    })
            } else {
                Err(CodegenError::Unsupported(
                    "closure captures must refer to local storage".into(),
                ))
            }
        }
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
    emitted.parameters.len() >= requested.parameters.len()
        && emitted.parameters[emitted.parameters.len() - requested.parameters.len()..]
            .iter()
            .zip(&requested.parameters)
            .all(|(emitted, requested)| abi_type_matches(emitted, requested))
        && abi_type_matches(&emitted.result, &requested.result)
        && emitted.effects == requested.effects
}

fn signature_exact_match(emitted: &FunctionType, requested: &FunctionType) -> bool {
    emitted.parameters.len() >= requested.parameters.len()
        && emitted.parameters[emitted.parameters.len() - requested.parameters.len()..]
            == requested.parameters
        && emitted.result.as_ref() == requested.result.as_ref()
        && emitted.effects == requested.effects
}

fn abi_type_matches(left: &Type, right: &Type) -> bool {
    match (left, right) {
        (
            Type::Primitive(PrimitiveType::Isize | PrimitiveType::I64),
            Type::Primitive(PrimitiveType::Isize | PrimitiveType::I64),
        )
        | (
            Type::Primitive(PrimitiveType::Usize | PrimitiveType::U64),
            Type::Primitive(PrimitiveType::Usize | PrimitiveType::U64),
        ) => true,
        (Type::String, Type::Str) | (Type::Str, Type::String) => true,
        (
            Type::Reference {
                mutable: left_mutable,
                referent: left,
                ..
            },
            Type::Reference {
                mutable: right_mutable,
                referent: right,
                ..
            },
        ) => left_mutable == right_mutable && abi_type_matches(left, right),
        (Type::Nominal(left_id, left), Type::Nominal(right_id, right))
        | (Type::DynamicInterface(left_id, left), Type::DynamicInterface(right_id, right)) => {
            if left_id != right_id {
                return false;
            }
            let left = left
                .iter()
                .filter(|argument| !matches!(argument, Type::Lifetime(_)))
                .collect::<Vec<_>>();
            let right = right
                .iter()
                .filter(|argument| !matches!(argument, Type::Lifetime(_)))
                .collect::<Vec<_>>();
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| abi_type_matches(left, right))
        }
        (Type::Optional(left), Type::Optional(right)) | (Type::Slice(left), Type::Slice(right)) => {
            abi_type_matches(left, right)
        }
        (
            Type::RawPointer {
                mutable: left_mutable,
                pointee: left,
            },
            Type::RawPointer {
                mutable: right_mutable,
                pointee: right,
            },
        ) => left_mutable == right_mutable && abi_type_matches(left, right),
        (Type::Array(left, left_length), Type::Array(right, right_length)) => {
            left_length == right_length && abi_type_matches(left, right)
        }
        (Type::Tuple(left), Type::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| abi_type_matches(left, right))
        }
        (
            Type::Promise {
                result: left,
                effects: left_effects,
                ..
            },
            Type::Promise {
                result: right,
                effects: right_effects,
                ..
            },
        ) => left_effects == right_effects && abi_type_matches(left, right),
        _ => left == right,
    }
}

fn stable_hash(value: &str) -> u64 {
    value
        .bytes()
        .fold(14_695_981_039_346_656_037, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
}

/// Return whether a monomorphized method receiver belongs to the class whose
/// descriptor is being populated.  MIR keeps receiver arguments as references
/// for mutating methods, while immutable methods may remain nominal values;
/// descriptor lookup must normalize both forms to the same class owner.
fn class_receiver_matches(
    receiver: Option<&Type>,
    declaration: DeclarationId,
    arguments: &[Type],
) -> bool {
    let Some(receiver) = receiver else {
        return false;
    };
    match receiver {
        Type::Nominal(owner, receiver_arguments) => {
            *owner == declaration && receiver_arguments == arguments
        }
        Type::Reference { referent, .. } => {
            class_receiver_matches(Some(referent.as_ref()), declaration, arguments)
        }
        _ => false,
    }
}

fn builtin_type_name(ty: &Type) -> &'static str {
    match ty {
        Type::Primitive(PrimitiveType::Bool) => "bool",
        Type::Primitive(PrimitiveType::I8) => "i8",
        Type::Primitive(PrimitiveType::I16) => "i16",
        Type::Primitive(PrimitiveType::I32) => "i32",
        Type::Primitive(PrimitiveType::I64) => "i64",
        Type::Primitive(PrimitiveType::I128) => "i128",
        Type::Primitive(PrimitiveType::Isize) => "isize",
        Type::Primitive(PrimitiveType::U8) => "u8",
        Type::Primitive(PrimitiveType::U16) => "u16",
        Type::Primitive(PrimitiveType::U32) => "u32",
        Type::Primitive(PrimitiveType::U64) => "u64",
        Type::Primitive(PrimitiveType::U128) => "u128",
        Type::Primitive(PrimitiveType::Usize) => "usize",
        Type::Primitive(PrimitiveType::F32) => "f32",
        Type::Primitive(PrimitiveType::F64) => "f64",
        Type::Primitive(PrimitiveType::Char) => "char",
        Type::String => "string",
        Type::Str => "str",
        _ => "unsupported",
    }
}

fn builtin_type_is_signed(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Primitive(
            PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::I64
                | PrimitiveType::I128
                | PrimitiveType::Isize
                | PrimitiveType::F32
                | PrimitiveType::F64
        )
    )
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

fn place_is_prefix(parent: &Place, child: &Place) -> bool {
    parent.local == child.local
        && parent.projection.len() <= child.projection.len()
        && parent
            .projection
            .iter()
            .zip(&child.projection)
            .all(|(left, right)| left == right)
}

fn place_with_projection(parent: &Place, projection: Projection) -> Place {
    let mut child = parent.clone();
    child.projection.push(projection);
    child
}

fn instantiate_type(ty: &Type, substitutions: &BTreeMap<String, Type>) -> Type {
    match ty {
        Type::Generic(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Type::Promise {
            result,
            error,
            effects,
        } => {
            let error = instantiate_type(error, substitutions);
            Type::Promise {
                result: Box::new(instantiate_type(result, substitutions)),
                error: Box::new(error.clone()),
                effects: tn_hir::promise_effects(&error, effects),
            }
        }
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
