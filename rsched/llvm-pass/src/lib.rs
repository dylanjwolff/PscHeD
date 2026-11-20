use llvm_plugin::inkwell::llvm_sys::core::{
    LLVMAddAlias2, LLVMAliasGetAliasee, LLVMConstIntGetZExtValue, LLVMGetCalledValue,
    LLVMGetFirstGlobal, LLVMGetFirstGlobalAlias, LLVMGetInlineAsmAsmString,
    LLVMGetNamedGlobalAlias, LLVMGetNextGlobal, LLVMGetNextGlobalAlias, LLVMGetOperand,
    LLVMGetUndef, LLVMGetValueName2, LLVMGlobalGetValueType, LLVMInstructionEraseFromParent,
    LLVMIsAConstantInt, LLVMIsAGlobalAlias, LLVMIsAInlineAsm, LLVMReplaceAllUsesWith,
    LLVMSetLinkage, LLVMSetOperand, LLVMSetValueName2, LLVMSetVisibility, LLVMTypeOf,
};
use llvm_plugin::inkwell::llvm_sys::{LLVMLinkage, LLVMVisibility};
use llvm_plugin::inkwell::module::Module;
use llvm_plugin::inkwell::types::{AnyTypeEnum, BasicMetadataTypeEnum, BasicTypeEnum};
use llvm_plugin::inkwell::values::{
    AsValueRef, BasicMetadataValueEnum, BasicValueEnum, CallSiteValue, FunctionValue,
    InstructionOpcode, InstructionValue, PointerValue,
};
use llvm_plugin::inkwell::{AddressSpace, AtomicOrdering};
use llvm_plugin::{
    LlvmModulePass, ModuleAnalysisManager, PassBuilder, PipelineParsing, PreservedAnalyses,
};

const PASS_NAME: &str = "rsched-atomics";

#[derive(Clone, Copy, Debug)]
struct RschedAtomicsPass {
    direct_pthread: bool,
    musl_libc: bool,
    glibc_libc: bool,
}

#[llvm_plugin::plugin(name = "rsched_llvm_pass", version = "0.1.0")]
fn plugin_registrar(builder: &mut PassBuilder) {
    builder.add_module_pipeline_parsing_callback(|name, manager| {
        let Some(pass) = parse_pass_name(name) else {
            return PipelineParsing::NotParsed;
        };
        manager.add_pass(pass);
        PipelineParsing::Parsed
    });
}

fn parse_pass_name(name: &str) -> Option<RschedAtomicsPass> {
    if name == PASS_NAME {
        return Some(RschedAtomicsPass {
            direct_pthread: false,
            musl_libc: false,
            glibc_libc: false,
        });
    }
    if name == "rsched-atomics<direct-pthread>" {
        return Some(RschedAtomicsPass {
            direct_pthread: true,
            musl_libc: false,
            glibc_libc: false,
        });
    }
    if name == "rsched-atomics<musl-libc>" {
        return Some(RschedAtomicsPass {
            direct_pthread: false,
            musl_libc: true,
            glibc_libc: false,
        });
    }
    if name == "rsched-atomics<glibc-libc>" {
        return Some(RschedAtomicsPass {
            direct_pthread: false,
            musl_libc: false,
            glibc_libc: true,
        });
    }
    None
}

impl LlvmModulePass for RschedAtomicsPass {
    fn run_pass(
        &self,
        module: &mut Module<'_>,
        _manager: &ModuleAnalysisManager,
    ) -> PreservedAnalyses {
        let mut changed = false;
        changed |= wrap_fuzzer_entrypoint(module);
        // libc synchronization internals use futex syscalls that may be
        // emitted as inline assembly. Scheduling at those internal atomics
        // can run a waiter while its lock holder is parked, blocking the
        // provider's only active kernel thread. The libc wrappers below are
        // the semantic scheduling points; application mode still instruments
        // every LLVM atomic operation.
        if !self.musl_libc && !self.glibc_libc {
            changed |= instrument_atomics(module);
        }
        if self.musl_libc {
            changed |= redirect_syscall_definition(module);
            changed |= wrap_libc_pthread_implementations(module, MUSL_REWRITES, true);
            changed |= wrap_libc_io_functions(module, MUSL_IO_REWRITES);
            changed |= rewrite_clone_calls(module);
            changed |= rewrite_thread_exits(module);
        }
        if self.glibc_libc {
            changed |= materialize_glibc_hidden_declarations(module);
            changed |= normalize_glibc_hidden_aliases(module);
            changed |= redirect_syscall_definition(module);
            changed |= wrap_libc_pthread_implementations(module, GLIBC_REWRITES, false);
            changed |= wrap_libc_pthread_implementations(module, GLIBC_PUBLIC_REWRITES, true);
            changed |= wrap_libc_io_functions(module, GLIBC_IO_REWRITES);
            changed |=
                wrap_glibc_hidden_implementations(module, GLIBC_REWRITES, GLIBC_HIDDEN_NAMES);
            changed |= wrap_glibc_hidden_implementations(
                module,
                GLIBC_PUBLIC_REWRITES,
                GLIBC_PUBLIC_HIDDEN_NAMES,
            );
            changed |= restore_glibc_hidden_helpers(module);
            changed |= rewrite_glibc_hidden_calls(module);
            changed |= rewrite_glibc_hidden_declarations(module);
            changed |= rewrite_clone_internal_calls(module);
            changed |= rewrite_clone_calls(module);
        }
        if self.direct_pthread {
            changed |= rewrite_pthread_calls(module);
        }

        if changed {
            PreservedAnalyses::None
        } else {
            PreservedAnalyses::All
        }
    }
}

fn materialize_glibc_hidden_declarations(module: &mut Module<'_>) -> bool {
    const PREFIX: &str = "__rsched_hidden_ref_";
    let mut names = Vec::new();
    let mut marker = unsafe { LLVMGetFirstGlobal(module.as_mut_ptr()) };
    while !marker.is_null() {
        if let Some(marker_name) = value_name(marker)
            && let Some(rest) = marker_name.strip_prefix(PREFIX)
            && let Some((_, original_name)) = rest.split_once('_')
        {
            names.push(original_name.to_owned());
        }
        marker = unsafe { LLVMGetNextGlobal(marker) };
    }

    let mut changed = false;
    for original_name in names {
        let Some(original) = module.get_function(&original_name) else {
            continue;
        };
        let hidden_name = format!("__GI_{original_name}");
        let hidden_alias = unsafe {
            LLVMGetNamedGlobalAlias(
                module.as_mut_ptr(),
                hidden_name.as_ptr().cast(),
                hidden_name.len(),
            )
        };
        if module.get_function(&hidden_name).is_none() && hidden_alias.is_null() {
            module.add_function(&hidden_name, original.get_type(), None);
            changed = true;
        }
    }
    changed
}

fn normalize_glibc_hidden_aliases(module: &mut Module<'_>) -> bool {
    let mut aliases = Vec::new();
    let mut alias = unsafe { LLVMGetFirstGlobalAlias(module.as_mut_ptr()) };
    while !alias.is_null() {
        let next = unsafe { LLVMGetNextGlobalAlias(alias) };
        let Some(hidden_name) = value_name(alias) else {
            alias = next;
            continue;
        };
        let Some(original_name) = hidden_name.strip_prefix("__GI_").map(str::to_owned) else {
            alias = next;
            continue;
        };
        let aliasee = unsafe { LLVMAliasGetAliasee(alias) };
        let aliasee_is_alias = unsafe { !LLVMIsAGlobalAlias(aliasee).is_null() };
        if !aliasee_is_alias && value_name(aliasee).as_deref() == Some(original_name.as_str()) {
            aliases.push((alias, aliasee, hidden_name, original_name));
        }
        alias = next;
    }

    for (alias, aliasee, hidden_name, original_name) in &aliases {
        let public_name = std::ffi::CString::new(original_name.as_str())
            .expect("glibc symbol name contains no NUL");
        set_value_name(*alias, &format!("__rsched_hidden_alias_{original_name}"));
        set_value_name(*aliasee, hidden_name);
        unsafe {
            LLVMSetLinkage(*alias, LLVMLinkage::LLVMPrivateLinkage);
            LLVMSetVisibility(*aliasee, LLVMVisibility::LLVMHiddenVisibility);
            LLVMAddAlias2(
                module.as_mut_ptr(),
                LLVMGlobalGetValueType(*aliasee),
                0,
                *aliasee,
                public_name.as_ptr(),
            );
        }
    }
    !aliases.is_empty()
}

fn value_name(value: llvm_plugin::inkwell::llvm_sys::prelude::LLVMValueRef) -> Option<String> {
    let mut len = 0;
    let name = unsafe { LLVMGetValueName2(value, &mut len) };
    if name.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(name.cast(), len) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn wrap_fuzzer_entrypoint(module: &mut Module<'_>) -> bool {
    let Some(original) = module.get_function("LLVMFuzzerTestOneInput") else {
        return false;
    };
    let wrapper_name = "__rsched_original_LLVMFuzzerTestOneInput";
    if module.get_function(wrapper_name).is_some() {
        return false;
    }

    unsafe {
        LLVMSetValueName2(
            original.as_value_ref(),
            wrapper_name.as_ptr().cast(),
            wrapper_name.len(),
        );
    }

    let context = module.get_context();
    let ptr_ty = context.ptr_type(AddressSpace::default());
    let usize_ty = context.i64_type();
    let i32_ty = context.i32_type();
    let fuzzer_ty = i32_ty.fn_type(
        &[
            BasicMetadataTypeEnum::PointerType(ptr_ty),
            BasicMetadataTypeEnum::IntType(usize_ty),
        ],
        false,
    );
    let helper_ty = i32_ty.fn_type(
        &[
            BasicMetadataTypeEnum::PointerType(ptr_ty),
            BasicMetadataTypeEnum::IntType(usize_ty),
            BasicMetadataTypeEnum::PointerType(ptr_ty),
        ],
        false,
    );
    let helper = module
        .get_function("rsched_fuzzer_test_one_input")
        .unwrap_or_else(|| module.add_function("rsched_fuzzer_test_one_input", helper_ty, None));
    let wrapper = module.add_function("LLVMFuzzerTestOneInput", fuzzer_ty, None);
    wrapper.set_linkage(original.get_linkage());

    let block = context.append_basic_block(wrapper, "entry");
    let builder = context.create_builder();
    builder.position_at_end(block);
    let data = wrapper
        .get_nth_param(0)
        .expect("fuzzer wrapper data parameter");
    let size = wrapper
        .get_nth_param(1)
        .expect("fuzzer wrapper size parameter");
    let original_ptr = original.as_global_value().as_pointer_value();
    let result = builder
        .build_call(
            helper,
            &[
                BasicMetadataValueEnum::from(data),
                BasicMetadataValueEnum::from(size),
                BasicMetadataValueEnum::PointerValue(original_ptr),
            ],
            "rsched.fuzzer.result",
        )
        .expect("build rsched fuzzer helper call")
        .try_as_basic_value()
        .left()
        .expect("rsched fuzzer helper returns int")
        .into_int_value();
    builder
        .build_return(Some(&result))
        .expect("build fuzzer wrapper return");

    true
}

fn instrument_atomics(module: &mut Module<'_>) -> bool {
    let context = module.get_context();
    let void_ty = context.void_type();
    let i8_ptr_ty = context.ptr_type(AddressSpace::default());
    let usize_ty = context.i64_type();
    let i32_ty = context.i32_type();
    let hook_ty = void_ty.fn_type(
        &[
            BasicMetadataTypeEnum::PointerType(i8_ptr_ty),
            BasicMetadataTypeEnum::IntType(usize_ty),
            BasicMetadataTypeEnum::IntType(i32_ty),
        ],
        false,
    );
    let hook = module
        .get_function("rsched_atomic_instrument")
        .unwrap_or_else(|| module.add_function("rsched_atomic_instrument", hook_ty, None));
    let builder = context.create_builder();

    let mut changed = false;
    let mut function = module.get_first_function();
    while let Some(func) = function {
        let mut block = func.get_first_basic_block();
        while let Some(bb) = block {
            let mut inst = bb.get_first_instruction();
            while let Some(i) = inst {
                inst = i.get_next_instruction();
                let Some((ptr, size, access)) = atomic_info(module, &i) else {
                    continue;
                };

                builder.position_before(&i);
                let ptr = builder
                    .build_pointer_cast(ptr, i8_ptr_ty, "rsched.atomic.ptr")
                    .expect("build pointer cast for rsched atomic hook");
                let size = usize_ty.const_int(size, false);
                let access = i32_ty.const_int(access as u64, false);
                builder
                    .build_call(
                        hook,
                        &[
                            BasicMetadataValueEnum::PointerValue(ptr),
                            BasicMetadataValueEnum::IntValue(size),
                            BasicMetadataValueEnum::IntValue(access),
                        ],
                        "",
                    )
                    .expect("build rsched atomic hook call");
                changed = true;
            }
            block = bb.get_next_basic_block();
        }
        function = func.get_next_function();
    }

    changed
}

fn atomic_info<'ctx>(
    _module: &Module<'ctx>,
    inst: &InstructionValue<'ctx>,
) -> Option<(PointerValue<'ctx>, u64, u32)> {
    let (access, ptr, size) = match inst.get_opcode() {
        InstructionOpcode::Load => {
            if inst.get_atomic_ordering().ok()? == AtomicOrdering::NotAtomic {
                return None;
            }
            (
                0,
                atomic_pointer_operand(inst, 0)?,
                any_type_size(inst.get_type())?,
            )
        }
        InstructionOpcode::Store => {
            if inst.get_atomic_ordering().ok()? == AtomicOrdering::NotAtomic {
                return None;
            }
            let value = inst.get_operand(0)?.left()?;
            (
                1,
                atomic_pointer_operand(inst, 1)?,
                basic_type_size(value.get_type())?,
            )
        }
        InstructionOpcode::AtomicRMW => {
            let value = inst.get_operand(1)?.left()?;
            (
                2,
                atomic_pointer_operand(inst, 0)?,
                basic_type_size(value.get_type())?,
            )
        }
        InstructionOpcode::AtomicCmpXchg => {
            let value = inst.get_operand(1)?.left()?;
            (
                2,
                atomic_pointer_operand(inst, 0)?,
                basic_type_size(value.get_type())?,
            )
        }
        _ => return None,
    };

    Some((ptr, size, access))
}

fn atomic_pointer_operand<'ctx>(
    inst: &InstructionValue<'ctx>,
    operand: u32,
) -> Option<PointerValue<'ctx>> {
    match inst.get_operand(operand)?.left()? {
        BasicValueEnum::PointerValue(ptr) => Some(ptr),
        _ => None,
    }
}

fn any_type_size(ty: AnyTypeEnum<'_>) -> Option<u64> {
    match ty {
        AnyTypeEnum::IntType(int) => Some(bytes_for_bits(int.get_bit_width())),
        AnyTypeEnum::PointerType(_) => Some(8),
        AnyTypeEnum::FloatType(float) => float.size_of().get_zero_extended_constant(),
        AnyTypeEnum::ArrayType(array) => array.size_of()?.get_zero_extended_constant(),
        AnyTypeEnum::StructType(strukt) => strukt.size_of()?.get_zero_extended_constant(),
        AnyTypeEnum::VectorType(vector) => vector.size_of()?.get_zero_extended_constant(),
        AnyTypeEnum::VoidType(_) | AnyTypeEnum::FunctionType(_) => None,
    }
}

fn basic_type_size(ty: BasicTypeEnum<'_>) -> Option<u64> {
    match ty {
        BasicTypeEnum::IntType(int) => Some(bytes_for_bits(int.get_bit_width())),
        BasicTypeEnum::PointerType(_) => Some(8),
        BasicTypeEnum::FloatType(float) => float.size_of().get_zero_extended_constant(),
        BasicTypeEnum::ArrayType(array) => array.size_of()?.get_zero_extended_constant(),
        BasicTypeEnum::StructType(strukt) => strukt.size_of()?.get_zero_extended_constant(),
        BasicTypeEnum::VectorType(vector) => vector.size_of()?.get_zero_extended_constant(),
    }
}

fn bytes_for_bits(bits: u32) -> u64 {
    u64::from(bits.div_ceil(8))
}

fn rewrite_pthread_calls(module: &mut Module<'_>) -> bool {
    let mut changed = false;
    for (from, to) in PTHREAD_REWRITES {
        let Some(old) = module.get_function(from) else {
            continue;
        };
        let new = module
            .get_function(to)
            .unwrap_or_else(|| module.add_function(to, old.get_type(), None));
        changed |= rewrite_function_uses(module, old, new);
    }
    changed
}

fn rewrite_function_uses(
    module: &Module<'_>,
    from: FunctionValue<'_>,
    to: FunctionValue<'_>,
) -> bool {
    let mut changed = false;
    let mut function = module.get_first_function();
    while let Some(func) = function {
        let mut block = func.get_first_basic_block();
        while let Some(bb) = block {
            let mut inst = bb.get_first_instruction();
            while let Some(i) = inst {
                inst = i.get_next_instruction();
                if CallSiteValue::try_from(i).is_err() {
                    continue;
                }
                let callee_operand = i.get_num_operands() - 1;
                let Some(callee) = i
                    .get_operand(callee_operand)
                    .and_then(|operand| operand.left())
                else {
                    continue;
                };
                if callee.as_value_ref() != from.as_value_ref() {
                    continue;
                }
                unsafe {
                    LLVMSetOperand(i.as_value_ref(), callee_operand, to.as_value_ref());
                }
                changed = true;
            }
            block = bb.get_next_basic_block();
        }
        function = func.get_next_function();
    }
    changed
}

fn rewrite_clone_calls(module: &mut Module<'_>) -> bool {
    let Some(old) = module.get_function("__clone") else {
        return false;
    };
    let new = module
        .get_function("rsched_clone")
        .unwrap_or_else(|| module.add_function("rsched_clone", old.get_type(), None));
    rewrite_function_uses(module, old, new)
}

/// x86_64 `SYS_exit`, which ends one thread (process exit uses exit_group).
const SYS_EXIT: u64 = 60;

/// Routes musl's thread-ending `exit` syscalls, which are inline assembly by
/// the time the pass sees them, and its `__unmapself` (unmap the stack, then
/// exit) through rsched, which ends a coroutine task instead of the kernel
/// thread every task shares.
fn rewrite_thread_exits(module: &mut Module<'_>) -> bool {
    let mut changed = false;
    if let Some(old) = module.get_function("__unmapself") {
        let new = module
            .get_function("rsched_unmapself")
            .unwrap_or_else(|| module.add_function("rsched_unmapself", old.get_type(), None));
        changed |= rewrite_function_uses(module, old, new);
    }

    let mut exits = Vec::new();
    let mut function = module.get_first_function();
    while let Some(func) = function {
        let mut block = func.get_first_basic_block();
        while let Some(bb) = block {
            let mut inst = bb.get_first_instruction();
            while let Some(i) = inst {
                inst = i.get_next_instruction();
                if i.get_opcode() == InstructionOpcode::Call && is_exit_syscall_asm(i) {
                    exits.push(i);
                }
            }
            block = bb.get_next_basic_block();
        }
        function = func.get_next_function();
    }
    if exits.is_empty() {
        return changed;
    }

    let context = module.get_context();
    let exit_type = context.void_type().fn_type(&[], false);
    let thread_exit = module
        .get_function("rsched_thread_exit")
        .unwrap_or_else(|| module.add_function("rsched_thread_exit", exit_type, None));
    let builder = context.create_builder();
    for exit in exits {
        builder.position_before(&exit);
        builder
            .build_call(thread_exit, &[], "")
            .expect("call rsched_thread_exit");
        unsafe {
            let value = exit.as_value_ref();
            LLVMReplaceAllUsesWith(value, LLVMGetUndef(LLVMTypeOf(value)));
            LLVMInstructionEraseFromParent(value);
        }
    }
    true
}

fn is_exit_syscall_asm(call: InstructionValue<'_>) -> bool {
    unsafe {
        let value = call.as_value_ref();
        let callee = LLVMGetCalledValue(value);
        if callee.is_null() || LLVMIsAInlineAsm(callee).is_null() {
            return false;
        }
        let mut len = 0;
        let text = LLVMGetInlineAsmAsmString(callee, &mut len);
        let text = std::slice::from_raw_parts(text.cast::<u8>(), len);
        if text.trim_ascii() != b"syscall" {
            return false;
        }
        let number = LLVMGetOperand(value, 0);
        !number.is_null()
            && !LLVMIsAConstantInt(number).is_null()
            && LLVMConstIntGetZExtValue(number) == SYS_EXIT
    }
}

fn rewrite_clone_internal_calls(module: &mut Module<'_>) -> bool {
    let Some(old) = module.get_function("__clone_internal") else {
        return false;
    };
    let new = module
        .get_function("rsched_clone_internal")
        .unwrap_or_else(|| module.add_function("rsched_clone_internal", old.get_type(), None));
    rewrite_function_uses(module, old, new)
}

fn redirect_syscall_definition(module: &mut Module<'_>) -> bool {
    let Some(syscall) = module.get_function("syscall") else {
        return false;
    };
    if syscall.count_basic_blocks() == 0 {
        return false;
    }

    set_value_name(syscall.as_value_ref(), "__rsched_replaced_syscall");
    module.set_inline_assembly(
        ".text\n\
         .globl syscall\n\
         .type syscall,@function\n\
         syscall:\n\
         \tjmp rsched_libc_syscall\n\
         .size syscall, .-syscall\n",
    );
    true
}

const PTHREAD_REWRITES: &[(&str, &str)] = &[
    ("pthread_create", "rsched_pthread_create"),
    ("pthread_join", "rsched_pthread_join"),
    ("pthread_exit", "rsched_pthread_exit"),
    ("pthread_mutex_lock", "rsched_pthread_mutex_lock"),
    ("pthread_mutex_trylock", "rsched_pthread_mutex_trylock"),
    ("pthread_mutex_unlock", "rsched_pthread_mutex_unlock"),
    ("pthread_cond_wait", "rsched_pthread_cond_wait"),
    ("pthread_cond_signal", "rsched_pthread_cond_signal"),
    ("pthread_cond_broadcast", "rsched_pthread_cond_broadcast"),
    ("pthread_barrier_init", "rsched_pthread_barrier_init"),
    ("pthread_barrier_wait", "rsched_pthread_barrier_wait"),
    ("sched_yield", "rsched_sched_yield"),
    ("fork", "rsched_fork"),
    ("waitpid", "rsched_waitpid"),
    ("execv", "rsched_execv"),
    ("execve", "rsched_execve"),
    ("_exit", "rsched_process_exit_status"),
    ("syscall", "rsched_syscall"),
    ("read", "rsched_read"),
    ("write", "rsched_write"),
    ("readv", "rsched_readv"),
    ("writev", "rsched_writev"),
    ("recv", "rsched_recv"),
    ("recvfrom", "rsched_recvfrom"),
    ("recvmsg", "rsched_recvmsg"),
    ("send", "rsched_send"),
    ("sendto", "rsched_sendto"),
    ("sendmsg", "rsched_sendmsg"),
    ("accept", "rsched_accept"),
    ("accept4", "rsched_accept4"),
    ("connect", "rsched_connect"),
    ("poll", "rsched_poll"),
];

#[derive(Clone, Copy)]
struct LibcRewrite {
    implementation: &'static str,
    public: &'static str,
    real: &'static str,
    rsched: &'static str,
    diverges: bool,
}

fn wrap_libc_pthread_implementations(
    module: &mut Module<'_>,
    rewrites: &[LibcRewrite],
    wrap_public: bool,
) -> bool {
    let mut changed = false;
    for rewrite in rewrites {
        let Some(original) = module.get_function(rewrite.implementation) else {
            continue;
        };
        if original.count_basic_blocks() == 0 {
            continue;
        }

        set_value_name(original.as_value_ref(), rewrite.real);
        if wrap_public {
            rename_global_alias(module, rewrite.public);
        }

        let real = module
            .get_function(rewrite.real)
            .expect("renamed musl implementation");
        build_guarded_wrapper(
            module,
            rewrite.implementation,
            real,
            rewrite.rsched,
            rewrite.diverges,
            true,
        );
        if wrap_public && rewrite.public != rewrite.implementation {
            build_guarded_wrapper(
                module,
                rewrite.public,
                real,
                rewrite.rsched,
                rewrite.diverges,
                true,
            );
        }
        changed = true;
    }
    changed
}

/// File and socket operations: `(implementation, public, rsched)`.
type IoRewrite = (&'static str, &'static str, &'static str);

/// Wraps only the public I/O symbol that applications call. libc's own
/// callers keep the implementation, and the wrapper does not activate rsched:
/// musl's dynamic loader, which is part of libc, reads files before any task
/// exists.
fn wrap_libc_io_functions(module: &mut Module<'_>, rewrites: &[IoRewrite]) -> bool {
    let mut changed = false;
    for &(implementation, public, rsched) in rewrites {
        let Some(real) = defined_function(module, implementation) else {
            continue;
        };
        if implementation == public {
            set_value_name(real.as_value_ref(), &format!("__rsched_real_{public}"));
        } else {
            rename_global_alias(module, public);
        }
        build_guarded_wrapper(module, public, real, rsched, false, false);
        changed = true;
    }
    changed
}

/// The function defined under `name`, following an alias to its target.
fn defined_function<'ctx>(module: &Module<'ctx>, name: &str) -> Option<FunctionValue<'ctx>> {
    let function = module.get_function(name).or_else(|| {
        let alias = unsafe {
            LLVMGetNamedGlobalAlias(module.as_mut_ptr(), name.as_ptr().cast(), name.len())
        };
        if alias.is_null() {
            return None;
        }
        unsafe { FunctionValue::new(LLVMAliasGetAliasee(alias)) }
    })?;
    (function.count_basic_blocks() > 0).then_some(function)
}

fn wrap_glibc_hidden_implementations(
    module: &mut Module<'_>,
    rewrites: &[LibcRewrite],
    hidden_names: &[&str],
) -> bool {
    let mut changed = false;
    for (rewrite, hidden) in rewrites.iter().zip(hidden_names) {
        let Some(real) = module.get_function(rewrite.real) else {
            continue;
        };
        rename_global_alias(module, hidden);
        build_guarded_wrapper(
            module,
            hidden,
            real,
            rewrite.rsched,
            rewrite.diverges,
            false,
        );
        changed = true;
    }
    changed
}

fn rewrite_glibc_hidden_calls(module: &mut Module<'_>) -> bool {
    let mut changed = false;
    for (from_name, to_name) in [
        ("__pthread_cond_broadcast", "__GI___pthread_cond_broadcast"),
        ("__pthread_cond_signal", "__GI___pthread_cond_signal"),
        (
            "__pthread_getattr_default_np",
            "__GI___pthread_getattr_default_np",
        ),
    ] {
        let Some(from) = module.get_function(from_name) else {
            continue;
        };
        let to = module
            .get_function(to_name)
            .unwrap_or_else(|| module.add_function(to_name, from.get_type(), None));
        changed |= rewrite_function_uses(module, from, to);
    }
    changed
}

fn rewrite_glibc_hidden_declarations(module: &mut Module<'_>) -> bool {
    let mut rewrites = Vec::new();
    let mut hidden = module.get_first_function();
    while let Some(function) = hidden {
        hidden = function.get_next_function();
        let Ok(name) = function.get_name().to_str() else {
            continue;
        };
        let Some(original_name) = name.strip_prefix("__GI_") else {
            continue;
        };
        let Some(original) = module.get_function(original_name) else {
            continue;
        };
        if original.as_value_ref() != function.as_value_ref() {
            rewrites.push((original, function));
        }
    }

    rewrites
        .into_iter()
        .fold(false, |changed, (original, hidden)| {
            rewrite_function_uses(module, original, hidden) || changed
        })
}

fn restore_glibc_hidden_helpers(module: &mut Module<'_>) -> bool {
    let mut changed = false;
    for (target_name, hidden_name) in [
        (
            "___pthread_cond_timedwait64",
            "__GI___pthread_cond_timedwait",
        ),
        (
            "___pthread_cond_timedwait64",
            "__GI___pthread_cond_timedwait64",
        ),
        (
            "___pthread_cond_clockwait64",
            "__GI___pthread_cond_clockwait",
        ),
        (
            "___pthread_cond_clockwait64",
            "__GI___pthread_cond_clockwait64",
        ),
        (
            "__pthread_mutex_unlock_usercnt",
            "__GI___pthread_mutex_unlock_usercnt",
        ),
    ] {
        let Some(target) = module.get_function(target_name) else {
            continue;
        };
        if target.count_basic_blocks() == 0 {
            continue;
        }
        if module.get_function(hidden_name).is_some() {
            continue;
        }
        build_forwarding_wrapper(module, hidden_name, target);
        changed = true;
    }
    changed
}

fn build_forwarding_wrapper<'ctx>(
    module: &mut Module<'ctx>,
    name: &str,
    target: FunctionValue<'ctx>,
) {
    let context = module.get_context();
    let wrapper = module.add_function(name, target.get_type(), Some(target.get_linkage()));
    let entry = context.append_basic_block(wrapper, "entry");
    let builder = context.create_builder();
    builder.position_at_end(entry);
    let args: Vec<BasicMetadataValueEnum<'ctx>> = wrapper
        .get_params()
        .into_iter()
        .map(BasicMetadataValueEnum::from)
        .collect();
    let call = builder
        .build_call(target, &args, "result")
        .expect("build glibc hidden helper call");
    match call.try_as_basic_value().left() {
        Some(value) => builder
            .build_return(Some(&value))
            .expect("return glibc hidden helper value"),
        None => builder
            .build_return(None)
            .expect("return from glibc hidden helper"),
    };
}

fn rename_global_alias(module: &Module<'_>, name: &str) {
    let alias =
        unsafe { LLVMGetNamedGlobalAlias(module.as_mut_ptr(), name.as_ptr().cast(), name.len()) };
    if !alias.is_null() {
        set_value_name(alias, &format!("__rsched_real_alias_{name}"));
    }
}

fn set_value_name(value: llvm_plugin::inkwell::llvm_sys::prelude::LLVMValueRef, name: &str) {
    unsafe {
        LLVMSetValueName2(value, name.as_ptr().cast(), name.len());
    }
}

fn build_guarded_wrapper<'ctx>(
    module: &mut Module<'ctx>,
    name: &str,
    real: FunctionValue<'ctx>,
    rsched_name: &str,
    diverges: bool,
    activate_hooks: bool,
) {
    let context = module.get_context();
    let function_type = real.get_type();
    let wrapper = module.add_function(name, function_type, Some(real.get_linkage()));
    let rsched = module
        .get_function(rsched_name)
        .unwrap_or_else(|| module.add_function(rsched_name, function_type, None));
    let try_enter_type = context.bool_type().fn_type(&[], false);
    let try_enter = module
        .get_function("rsched_try_enter")
        .unwrap_or_else(|| module.add_function("rsched_try_enter", try_enter_type, None));
    let exit_type = context.void_type().fn_type(&[], false);
    let exit = module
        .get_function("rsched_exit")
        .unwrap_or_else(|| module.add_function("rsched_exit", exit_type, None));

    let entry = context.append_basic_block(wrapper, "entry");
    let outer = context.append_basic_block(wrapper, "rsched.outer");
    let nested = context.append_basic_block(wrapper, "rsched.nested");
    let builder = context.create_builder();
    builder.position_at_end(entry);
    if activate_hooks {
        let activate_type = context.void_type().fn_type(&[], false);
        let activate = module
            .get_function("rsched_activate_instrumented_libc")
            .unwrap_or_else(|| {
                module.add_function("rsched_activate_instrumented_libc", activate_type, None)
            });
        builder
            .build_call(activate, &[], "")
            .expect("activate instrumented libc hooks");
    }
    let is_outer = builder
        .build_call(try_enter, &[], "rsched.is_outer")
        .expect("build rsched recursion guard")
        .try_as_basic_value()
        .left()
        .expect("rsched_try_enter returns bool")
        .into_int_value();
    builder
        .build_conditional_branch(is_outer, outer, nested)
        .expect("build rsched wrapper branch");

    let args: Vec<BasicMetadataValueEnum<'ctx>> = wrapper
        .get_params()
        .into_iter()
        .map(BasicMetadataValueEnum::from)
        .collect();
    build_guarded_call(&builder, outer, rsched, exit, &args, diverges);
    build_guarded_call(&builder, nested, real, exit, &args, diverges);
}

fn build_guarded_call<'ctx>(
    builder: &llvm_plugin::inkwell::builder::Builder<'ctx>,
    block: llvm_plugin::inkwell::basic_block::BasicBlock<'ctx>,
    target: FunctionValue<'ctx>,
    exit: FunctionValue<'ctx>,
    args: &[BasicMetadataValueEnum<'ctx>],
    diverges: bool,
) {
    builder.position_at_end(block);
    let call = builder
        .build_call(target, args, "rsched.result")
        .expect("build guarded musl call");
    if diverges {
        builder
            .build_unreachable()
            .expect("build diverging musl wrapper");
        return;
    }

    builder
        .build_call(exit, &[], "")
        .expect("build rsched recursion guard exit");
    match call.try_as_basic_value().left() {
        Some(value) => builder
            .build_return(Some(&value))
            .expect("return guarded musl call value"),
        None => builder
            .build_return(None)
            .expect("return from guarded musl call"),
    };
}

const MUSL_REWRITES: &[LibcRewrite] = &[
    LibcRewrite {
        implementation: "__pthread_create",
        public: "pthread_create",
        real: "__rsched_real_pthread_create",
        rsched: "rsched_pthread_create",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__pthread_join",
        public: "pthread_join",
        real: "__rsched_real_pthread_join",
        rsched: "rsched_pthread_join",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_mutexattr_init",
        public: "pthread_mutexattr_init",
        real: "__rsched_real_pthread_mutexattr_init",
        rsched: "rsched_pthread_mutexattr_init",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_mutexattr_settype",
        public: "pthread_mutexattr_settype",
        real: "__rsched_real_pthread_mutexattr_settype",
        rsched: "rsched_pthread_mutexattr_settype",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_mutexattr_destroy",
        public: "pthread_mutexattr_destroy",
        real: "__rsched_real_pthread_mutexattr_destroy",
        rsched: "rsched_pthread_mutexattr_destroy",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__pthread_mutex_init",
        public: "pthread_mutex_init",
        real: "__rsched_real_pthread_mutex_init",
        rsched: "rsched_pthread_mutex_init",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_mutex_destroy",
        public: "pthread_mutex_destroy",
        real: "__rsched_real_pthread_mutex_destroy",
        rsched: "rsched_pthread_mutex_destroy",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__pthread_mutex_lock",
        public: "pthread_mutex_lock",
        real: "__rsched_real_pthread_mutex_lock",
        rsched: "rsched_pthread_mutex_lock",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__pthread_mutex_trylock",
        public: "pthread_mutex_trylock",
        real: "__rsched_real_pthread_mutex_trylock",
        rsched: "rsched_pthread_mutex_trylock",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__pthread_mutex_unlock",
        public: "pthread_mutex_unlock",
        real: "__rsched_real_pthread_mutex_unlock",
        rsched: "rsched_pthread_mutex_unlock",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_cond_wait",
        public: "pthread_cond_wait",
        real: "__rsched_real_pthread_cond_wait",
        rsched: "rsched_pthread_cond_wait",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_cond_signal",
        public: "pthread_cond_signal",
        real: "__rsched_real_pthread_cond_signal",
        rsched: "rsched_pthread_cond_signal",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_cond_broadcast",
        public: "pthread_cond_broadcast",
        real: "__rsched_real_pthread_cond_broadcast",
        rsched: "rsched_pthread_cond_broadcast",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_barrier_init",
        public: "pthread_barrier_init",
        real: "__rsched_real_pthread_barrier_init",
        rsched: "rsched_pthread_barrier_init",
        diverges: false,
    },
    LibcRewrite {
        implementation: "pthread_barrier_wait",
        public: "pthread_barrier_wait",
        real: "__rsched_real_pthread_barrier_wait",
        rsched: "rsched_pthread_barrier_wait",
        diverges: false,
    },
    LibcRewrite {
        implementation: "sched_yield",
        public: "sched_yield",
        real: "__rsched_real_sched_yield",
        rsched: "rsched_sched_yield",
        diverges: false,
    },
    LibcRewrite {
        implementation: "waitpid",
        public: "waitpid",
        real: "__rsched_real_waitpid",
        rsched: "rsched_waitpid",
        diverges: false,
    },
    LibcRewrite {
        implementation: "fork",
        public: "fork",
        real: "__rsched_real_fork",
        rsched: "rsched_fork",
        diverges: false,
    },
    LibcRewrite {
        implementation: "_exit",
        public: "_exit",
        real: "__rsched_real_process_exit",
        rsched: "rsched_process_exit_status",
        diverges: true,
    },
    // exit() ends in _Exit, not _exit, so a forked child that calls exit()
    // must hand the schedule back here too.
    LibcRewrite {
        implementation: "_Exit",
        public: "_Exit",
        real: "__rsched_real_Exit",
        rsched: "rsched_process_exit_status",
        diverges: true,
    },
];

const GLIBC_REWRITES: &[LibcRewrite] = &[
    LibcRewrite {
        implementation: "__pthread_create_2_1",
        public: "pthread_create",
        real: "__rsched_real_pthread_create",
        rsched: "rsched_pthread_create",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_join",
        public: "pthread_join",
        real: "__rsched_real_pthread_join",
        rsched: "rsched_pthread_join",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutexattr_init",
        public: "pthread_mutexattr_init",
        real: "__rsched_real_pthread_mutexattr_init",
        rsched: "rsched_pthread_mutexattr_init",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutexattr_settype",
        public: "pthread_mutexattr_settype",
        real: "__rsched_real_pthread_mutexattr_settype",
        rsched: "rsched_pthread_mutexattr_settype",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutexattr_destroy",
        public: "pthread_mutexattr_destroy",
        real: "__rsched_real_pthread_mutexattr_destroy",
        rsched: "rsched_pthread_mutexattr_destroy",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutex_init",
        public: "pthread_mutex_init",
        real: "__rsched_real_pthread_mutex_init",
        rsched: "rsched_pthread_mutex_init",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutex_destroy",
        public: "pthread_mutex_destroy",
        real: "__rsched_real_pthread_mutex_destroy",
        rsched: "rsched_pthread_mutex_destroy",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutex_lock",
        public: "pthread_mutex_lock",
        real: "__rsched_real_pthread_mutex_lock",
        rsched: "rsched_pthread_mutex_lock",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutex_trylock",
        public: "pthread_mutex_trylock",
        real: "__rsched_real_pthread_mutex_trylock",
        rsched: "rsched_pthread_mutex_trylock",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_mutex_unlock",
        public: "pthread_mutex_unlock",
        real: "__rsched_real_pthread_mutex_unlock",
        rsched: "rsched_pthread_mutex_unlock",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_cond_wait",
        public: "pthread_cond_wait",
        real: "__rsched_real_pthread_cond_wait",
        rsched: "rsched_pthread_cond_wait",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_cond_signal",
        public: "pthread_cond_signal",
        real: "__rsched_real_pthread_cond_signal",
        rsched: "rsched_pthread_cond_signal",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_cond_broadcast",
        public: "pthread_cond_broadcast",
        real: "__rsched_real_pthread_cond_broadcast",
        rsched: "rsched_pthread_cond_broadcast",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_barrier_init",
        public: "pthread_barrier_init",
        real: "__rsched_real_pthread_barrier_init",
        rsched: "rsched_pthread_barrier_init",
        diverges: false,
    },
    LibcRewrite {
        implementation: "___pthread_barrier_wait",
        public: "pthread_barrier_wait",
        real: "__rsched_real_pthread_barrier_wait",
        rsched: "rsched_pthread_barrier_wait",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__sched_yield",
        public: "sched_yield",
        real: "__rsched_real_sched_yield",
        rsched: "rsched_sched_yield",
        diverges: false,
    },
    LibcRewrite {
        implementation: "_exit",
        public: "_exit",
        real: "__rsched_real_process_exit",
        rsched: "rsched_process_exit_status",
        diverges: true,
    },
];

const MUSL_IO_REWRITES: &[IoRewrite] = &[
    ("read", "read", "rsched_read"),
    ("write", "write", "rsched_write"),
    ("readv", "readv", "rsched_readv"),
    ("writev", "writev", "rsched_writev"),
    ("recv", "recv", "rsched_recv"),
    ("recvfrom", "recvfrom", "rsched_recvfrom"),
    ("recvmsg", "recvmsg", "rsched_recvmsg"),
    ("send", "send", "rsched_send"),
    ("sendto", "sendto", "rsched_sendto"),
    ("sendmsg", "sendmsg", "rsched_sendmsg"),
    ("accept", "accept", "rsched_accept"),
    ("accept4", "accept4", "rsched_accept4"),
    ("connect", "connect", "rsched_connect"),
    ("poll", "poll", "rsched_poll"),
];

const GLIBC_IO_REWRITES: &[IoRewrite] = &[
    ("__libc_read", "read", "rsched_read"),
    ("__libc_write", "write", "rsched_write"),
    ("__readv", "readv", "rsched_readv"),
    ("__writev", "writev", "rsched_writev"),
    ("__libc_recv", "recv", "rsched_recv"),
    ("__libc_recvfrom", "recvfrom", "rsched_recvfrom"),
    ("__libc_recvmsg", "recvmsg", "rsched_recvmsg"),
    ("__libc_send", "send", "rsched_send"),
    ("__libc_sendto", "sendto", "rsched_sendto"),
    ("__libc_sendmsg", "sendmsg", "rsched_sendmsg"),
    ("__libc_accept", "accept", "rsched_accept"),
    ("accept4", "accept4", "rsched_accept4"),
    ("__libc_connect", "connect", "rsched_connect"),
    ("__poll", "poll", "rsched_poll"),
];

const GLIBC_PUBLIC_REWRITES: &[LibcRewrite] = &[
    LibcRewrite {
        implementation: "__waitpid",
        public: "waitpid",
        real: "__rsched_real_waitpid",
        rsched: "rsched_waitpid",
        diverges: false,
    },
    LibcRewrite {
        implementation: "__libc_fork",
        public: "fork",
        real: "__rsched_real_fork",
        rsched: "rsched_fork",
        diverges: false,
    },
];

const GLIBC_PUBLIC_HIDDEN_NAMES: &[&str] = &["__GI___waitpid", "__GI___fork"];

const GLIBC_HIDDEN_NAMES: &[&str] = &[
    "__GI___pthread_create",
    "__GI___pthread_join",
    "__GI___pthread_mutexattr_init",
    "__GI___pthread_mutexattr_settype",
    "__GI___pthread_mutexattr_destroy",
    "__GI___pthread_mutex_init",
    "__GI___pthread_mutex_destroy",
    "__GI___pthread_mutex_lock",
    "__GI___pthread_mutex_trylock",
    "__GI___pthread_mutex_unlock",
    "__GI___pthread_cond_wait",
    "__GI___pthread_cond_signal",
    "__GI___pthread_cond_broadcast",
    "__GI___pthread_barrier_init",
    "__GI___pthread_barrier_wait",
    "__GI___sched_yield",
    "__GI__exit",
];
