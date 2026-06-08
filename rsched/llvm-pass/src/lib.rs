use llvm_plugin::inkwell::llvm_sys::core::{
    LLVMGetNamedGlobalAlias, LLVMSetOperand, LLVMSetValueName2,
};
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
        });
    }
    if name == "rsched-atomics<direct-pthread>" {
        return Some(RschedAtomicsPass {
            direct_pthread: true,
            musl_libc: false,
        });
    }
    if name == "rsched-atomics<musl-libc>" {
        return Some(RschedAtomicsPass {
            direct_pthread: false,
            musl_libc: true,
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
        changed |= instrument_atomics(module);
        if self.musl_libc {
            changed |= wrap_musl_pthread_implementations(module);
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
];

#[derive(Clone, Copy)]
struct MuslRewrite {
    implementation: &'static str,
    public: &'static str,
    real: &'static str,
    rsched: &'static str,
    diverges: bool,
}

fn wrap_musl_pthread_implementations(module: &mut Module<'_>) -> bool {
    let mut changed = false;
    for rewrite in MUSL_REWRITES {
        let Some(original) = module.get_function(rewrite.implementation) else {
            continue;
        };
        if original.count_basic_blocks() == 0 {
            continue;
        }

        set_value_name(original.as_value_ref(), rewrite.real);
        rename_global_alias(module, rewrite.public);

        let real = module
            .get_function(rewrite.real)
            .expect("renamed musl implementation");
        build_guarded_wrapper(module, rewrite.implementation, real, rewrite);
        if rewrite.public != rewrite.implementation {
            build_guarded_wrapper(module, rewrite.public, real, rewrite);
        }
        changed = true;
    }
    changed
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
    rewrite: &MuslRewrite,
) {
    let context = module.get_context();
    let function_type = real.get_type();
    let wrapper = module.add_function(name, function_type, Some(real.get_linkage()));
    let rsched = module
        .get_function(rewrite.rsched)
        .unwrap_or_else(|| module.add_function(rewrite.rsched, function_type, None));
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
    let activate_type = context.void_type().fn_type(&[], false);
    let activate = module
        .get_function("rsched_activate_instrumented_libc")
        .unwrap_or_else(|| {
            module.add_function("rsched_activate_instrumented_libc", activate_type, None)
        });
    builder
        .build_call(activate, &[], "")
        .expect("activate instrumented libc hooks");
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
    build_guarded_call(&builder, outer, rsched, exit, &args, rewrite.diverges);
    build_guarded_call(&builder, nested, real, exit, &args, rewrite.diverges);
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

const MUSL_REWRITES: &[MuslRewrite] = &[
    MuslRewrite {
        implementation: "__pthread_create",
        public: "pthread_create",
        real: "__rsched_real_pthread_create",
        rsched: "rsched_pthread_create",
        diverges: false,
    },
    MuslRewrite {
        implementation: "__pthread_join",
        public: "pthread_join",
        real: "__rsched_real_pthread_join",
        rsched: "rsched_pthread_join",
        diverges: false,
    },
    MuslRewrite {
        implementation: "__pthread_exit",
        public: "pthread_exit",
        real: "__rsched_real_pthread_exit",
        rsched: "rsched_pthread_exit",
        diverges: true,
    },
    MuslRewrite {
        implementation: "pthread_mutexattr_init",
        public: "pthread_mutexattr_init",
        real: "__rsched_real_pthread_mutexattr_init",
        rsched: "rsched_pthread_mutexattr_init",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_mutexattr_settype",
        public: "pthread_mutexattr_settype",
        real: "__rsched_real_pthread_mutexattr_settype",
        rsched: "rsched_pthread_mutexattr_settype",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_mutexattr_destroy",
        public: "pthread_mutexattr_destroy",
        real: "__rsched_real_pthread_mutexattr_destroy",
        rsched: "rsched_pthread_mutexattr_destroy",
        diverges: false,
    },
    MuslRewrite {
        implementation: "__pthread_mutex_init",
        public: "pthread_mutex_init",
        real: "__rsched_real_pthread_mutex_init",
        rsched: "rsched_pthread_mutex_init",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_mutex_destroy",
        public: "pthread_mutex_destroy",
        real: "__rsched_real_pthread_mutex_destroy",
        rsched: "rsched_pthread_mutex_destroy",
        diverges: false,
    },
    MuslRewrite {
        implementation: "__pthread_mutex_lock",
        public: "pthread_mutex_lock",
        real: "__rsched_real_pthread_mutex_lock",
        rsched: "rsched_pthread_mutex_lock",
        diverges: false,
    },
    MuslRewrite {
        implementation: "__pthread_mutex_trylock",
        public: "pthread_mutex_trylock",
        real: "__rsched_real_pthread_mutex_trylock",
        rsched: "rsched_pthread_mutex_trylock",
        diverges: false,
    },
    MuslRewrite {
        implementation: "__pthread_mutex_unlock",
        public: "pthread_mutex_unlock",
        real: "__rsched_real_pthread_mutex_unlock",
        rsched: "rsched_pthread_mutex_unlock",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_cond_wait",
        public: "pthread_cond_wait",
        real: "__rsched_real_pthread_cond_wait",
        rsched: "rsched_pthread_cond_wait",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_cond_signal",
        public: "pthread_cond_signal",
        real: "__rsched_real_pthread_cond_signal",
        rsched: "rsched_pthread_cond_signal",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_cond_broadcast",
        public: "pthread_cond_broadcast",
        real: "__rsched_real_pthread_cond_broadcast",
        rsched: "rsched_pthread_cond_broadcast",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_barrier_init",
        public: "pthread_barrier_init",
        real: "__rsched_real_pthread_barrier_init",
        rsched: "rsched_pthread_barrier_init",
        diverges: false,
    },
    MuslRewrite {
        implementation: "pthread_barrier_wait",
        public: "pthread_barrier_wait",
        real: "__rsched_real_pthread_barrier_wait",
        rsched: "rsched_pthread_barrier_wait",
        diverges: false,
    },
    MuslRewrite {
        implementation: "sched_yield",
        public: "sched_yield",
        real: "__rsched_real_sched_yield",
        rsched: "rsched_sched_yield",
        diverges: false,
    },
];
