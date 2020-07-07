use crate::layout_id::LayoutIds;
use crate::llvm::convert::{
    basic_type_from_layout, collection, get_fn_type, get_ptr_type, ptr_int,
};
use bumpalo::collections::Vec;
use bumpalo::Bump;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::module::{Linkage, Module};
use inkwell::passes::{PassManager, PassManagerBuilder};
use inkwell::types::{BasicTypeEnum, FunctionType, IntType, PointerType, StructType};
use inkwell::values::BasicValueEnum::{self, *};
use inkwell::values::{FunctionValue, IntValue, PointerValue, StructValue};
use inkwell::AddressSpace;
use inkwell::{FloatPredicate, IntPredicate, OptimizationLevel};
use roc_collections::all::ImMap;
use roc_module::symbol::{Interns, Symbol};
use roc_mono::expr::{Expr, Proc};
use roc_mono::layout::{Builtin, Layout};
use target_lexicon::CallingConvention;

/// This is for Inkwell's FunctionValue::verify - we want to know the verification
/// output in debug builds, but we don't want it to print to stdout in release builds!
#[cfg(debug_assertions)]
const PRINT_FN_VERIFICATION_OUTPUT: bool = true;

#[cfg(not(debug_assertions))]
const PRINT_FN_VERIFICATION_OUTPUT: bool = false;

pub enum OptLevel {
    Normal,
    Optimize,
}

type Scope<'a, 'ctx> = ImMap<Symbol, (Layout<'a>, PointerValue<'ctx>)>;

pub struct Env<'a, 'ctx, 'env> {
    pub arena: &'a Bump,
    pub context: &'ctx Context,
    pub builder: &'env Builder<'ctx>,
    pub module: &'ctx Module<'ctx>,
    pub interns: Interns,
    pub ptr_bytes: u32,
}

impl<'a, 'ctx, 'env> Env<'a, 'ctx, 'env> {
    pub fn ptr_int(&self) -> IntType<'ctx> {
        ptr_int(self.context, self.ptr_bytes)
    }
}

pub fn module_from_builtins<'ctx>(ctx: &'ctx Context, module_name: &str) -> Module<'ctx> {
    let memory_buffer =
        MemoryBuffer::create_from_memory_range(include_bytes!("builtins.bc"), module_name);

    let module = Module::parse_bitcode_from_buffer(&memory_buffer, ctx)
        .unwrap_or_else(|err| panic!("Unable to import builtins bitcode. LLVM error: {:?}", err));

    // Add LLVM intrinsics.
    add_intrinsics(ctx, &module);

    module
}

fn add_intrinsics<'ctx>(ctx: &'ctx Context, module: &Module<'ctx>) {
    // List of all supported LLVM intrinsics:
    //
    // https://releases.llvm.org/10.0.0/docs/LangRef.html#standard-c-library-intrinsics
    let i64_type = ctx.i64_type();
    let f64_type = ctx.f64_type();

    add_intrinsic(
        module,
        LLVM_SQRT_F64,
        f64_type.fn_type(&[f64_type.into()], false),
    );

    add_intrinsic(
        module,
        LLVM_LROUND_I64_F64,
        i64_type.fn_type(&[f64_type.into()], false),
    );

    add_intrinsic(
        module,
        LLVM_FABS_F64,
        f64_type.fn_type(&[f64_type.into()], false),
    );

    add_intrinsic(
        module,
        LLVM_SIN_F64,
        f64_type.fn_type(&[f64_type.into()], false),
    );

    add_intrinsic(
        module,
        LLVM_COS_F64,
        f64_type.fn_type(&[f64_type.into()], false),
    );
}

static LLVM_SQRT_F64: &str = "llvm.sqrt.f64";
static LLVM_LROUND_I64_F64: &str = "llvm.lround.i64.f64";
static LLVM_FABS_F64: &str = "llvm.fabs.f64";
static LLVM_SIN_F64: &str = "llvm.sin.f64";
static LLVM_COS_F64: &str = "llvm.cos.f64";

fn add_intrinsic<'ctx>(
    module: &Module<'ctx>,
    intrinsic_name: &'static str,
    fn_type: FunctionType<'ctx>,
) -> FunctionValue<'ctx> {
    let fn_val = module.add_function(intrinsic_name, fn_type, None);

    fn_val.set_call_conventions(C_CALL_CONV);

    fn_val
}

pub fn add_passes(fpm: &PassManager<FunctionValue<'_>>, opt_level: OptLevel) {
    // tail-call elimination is always on
    fpm.add_instruction_combining_pass();
    fpm.add_tail_call_elimination_pass();

    let pmb = PassManagerBuilder::create();

    // Enable more optimizations when running cargo test --release
    match opt_level {
        OptLevel::Normal => {
            pmb.set_optimization_level(OptimizationLevel::None);
        }
        OptLevel::Optimize => {
            // Default is O2, Aggressive is O3
            //
            // See https://llvm.org/doxygen/CodeGen_8h_source.html
            pmb.set_optimization_level(OptimizationLevel::Aggressive);

            // TODO figure out how enabling these individually differs from
            // the broad "aggressive optimizations" setting.

            // fpm.add_reassociate_pass();
            // fpm.add_basic_alias_analysis_pass();
            // fpm.add_promote_memory_to_register_pass();
            // fpm.add_cfg_simplification_pass();
            // fpm.add_gvn_pass();
            // TODO figure out why enabling any of these (even alone) causes LLVM to segfault
            // fpm.add_strip_dead_prototypes_pass();
            // fpm.add_dead_arg_elimination_pass();
            // fpm.add_function_inlining_pass();
            // pmb.set_inliner_with_threshold(4);
        }
    }

    pmb.populate_function_pass_manager(&fpm);
}

#[allow(clippy::cognitive_complexity)]
pub fn build_expr<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    scope: &Scope<'a, 'ctx>,
    parent: FunctionValue<'ctx>,
    expr: &Expr<'a>,
) -> BasicValueEnum<'ctx> {
    use roc_mono::expr::Expr::*;

    match expr {
        Int(num) => env.context.i64_type().const_int(*num as u64, true).into(),
        Float(num) => env.context.f64_type().const_float(*num).into(),
        Bool(b) => env.context.bool_type().const_int(*b as u64, false).into(),
        Byte(b) => env.context.i8_type().const_int(*b as u64, false).into(),
        Cond {
            branch_symbol,
            pass: (pass_stores, pass_expr),
            fail: (fail_stores, fail_expr),
            ret_layout,
            ..
        } => {
            let pass = env.arena.alloc(Expr::Store(pass_stores, pass_expr));
            let fail = env.arena.alloc(Expr::Store(fail_stores, fail_expr));

            let ret_type =
                basic_type_from_layout(env.arena, env.context, &ret_layout, env.ptr_bytes);

            let cond_expr = load_symbol(env, scope, branch_symbol);

            match cond_expr {
                IntValue(value) => {
                    // This is a call tobuild_basic_phi2, except inlined to prevent
                    // problems with lifetimes and closures involving layout_ids.
                    let builder = env.builder;
                    let context = env.context;

                    // build blocks
                    let then_block = context.append_basic_block(parent, "then");
                    let else_block = context.append_basic_block(parent, "else");
                    let cont_block = context.append_basic_block(parent, "branchcont");

                    builder.build_conditional_branch(value, then_block, else_block);

                    // build then block
                    builder.position_at_end(then_block);
                    let then_val = build_expr(env, layout_ids, scope, parent, pass);
                    builder.build_unconditional_branch(cont_block);

                    let then_block = builder.get_insert_block().unwrap();

                    // build else block
                    builder.position_at_end(else_block);
                    let else_val = build_expr(env, layout_ids, scope, parent, fail);
                    builder.build_unconditional_branch(cont_block);

                    let else_block = builder.get_insert_block().unwrap();

                    // emit merge block
                    builder.position_at_end(cont_block);

                    let phi = builder.build_phi(ret_type, "branch");

                    phi.add_incoming(&[(&then_val, then_block), (&else_val, else_block)]);

                    phi.as_basic_value()
                }
                _ => panic!(
                    "Tried to make a branch out of an invalid condition: cond_expr = {:?}",
                    cond_expr,
                ),
            }
        }
        Switch {
            cond,
            branches,
            default_branch: (default_stores, default_expr),
            ret_layout,
            cond_layout,
        } => {
            let ret_type =
                basic_type_from_layout(env.arena, env.context, &ret_layout, env.ptr_bytes);

            let default_branch = env.arena.alloc(Expr::Store(default_stores, default_expr));

            let mut combined = Vec::with_capacity_in(branches.len(), env.arena);

            for (int, stores, expr) in branches.iter() {
                combined.push((*int, Expr::Store(stores, expr)));
            }

            let switch_args = SwitchArgs {
                cond_layout: cond_layout.clone(),
                cond_expr: cond,
                branches: combined.into_bump_slice(),
                default_branch,
                ret_type,
            };

            build_switch(env, layout_ids, scope, parent, switch_args)
        }
        Store(stores, ret) => {
            let mut scope = im_rc::HashMap::clone(scope);
            let context = &env.context;

            for (symbol, layout, expr) in stores.iter() {
                let val = build_expr(env, layout_ids, &scope, parent, &expr);
                let expr_bt = basic_type_from_layout(env.arena, context, &layout, env.ptr_bytes);
                let alloca = create_entry_block_alloca(
                    env,
                    parent,
                    expr_bt,
                    symbol.ident_string(&env.interns),
                );

                env.builder.build_store(alloca, val);

                // Make a new scope which includes the binding we just encountered.
                // This should be done *after* compiling the bound expr, since any
                // recursive (in the LetRec sense) bindings should already have
                // been extracted as procedures. Nothing in here should need to
                // access itself!
                scope = im_rc::HashMap::clone(&scope);

                scope.insert(*symbol, (layout.clone(), alloca));
            }

            build_expr(env, layout_ids, &scope, parent, ret)
        }
        CallByName { name, layout, args } => match *name {
            Symbol::BOOL_OR => {
                // The (||) operator
                debug_assert!(args.len() == 2);

                let comparison =
                    build_expr(env, layout_ids, scope, parent, &args[0].0).into_int_value();
                let build_then = || env.context.bool_type().const_int(true as u64, false).into();
                let build_else = || build_expr(env, layout_ids, scope, parent, &args[1].0);

                let ret_type = env.context.bool_type().into();

                build_basic_phi2(env, parent, comparison, build_then, build_else, ret_type)
            }
            Symbol::BOOL_AND => {
                // The (&&) operator
                debug_assert!(args.len() == 2);

                let comparison =
                    build_expr(env, layout_ids, scope, parent, &args[0].0).into_int_value();
                let build_then = || build_expr(env, layout_ids, scope, parent, &args[1].0);
                let build_else = || {
                    env.context
                        .bool_type()
                        .const_int(false as u64, false)
                        .into()
                };

                let ret_type = env.context.bool_type().into();

                build_basic_phi2(env, parent, comparison, build_then, build_else, ret_type)
            }
            Symbol::BOOL_NOT => {
                // The (!) operator
                debug_assert!(args.len() == 1);

                let arg = build_expr(env, layout_ids, scope, parent, &args[0].0);

                let int_val = env.builder.build_not(arg.into_int_value(), "bool_not");

                BasicValueEnum::IntValue(int_val)
            }
            _ => {
                let mut arg_tuples: Vec<(BasicValueEnum, &'a Layout<'a>)> =
                    Vec::with_capacity_in(args.len(), env.arena);

                for (arg, arg_layout) in args.iter() {
                    arg_tuples.push((build_expr(env, layout_ids, scope, parent, arg), arg_layout));
                }

                call_with_args(
                    env,
                    layout_ids,
                    layout,
                    *name,
                    parent,
                    arg_tuples.into_bump_slice(),
                )
            }
        },
        FunctionPointer(symbol, layout) => {
            let fn_name = layout_ids
                .get(*symbol, layout)
                .to_symbol_string(*symbol, &env.interns);
            let ptr = env
                .module
                .get_function(fn_name.as_str())
                .unwrap_or_else(|| panic!("Could not get pointer to unknown function {:?}", symbol))
                .as_global_value()
                .as_pointer_value();

            BasicValueEnum::PointerValue(ptr)
        }
        CallByPointer(sub_expr, args, _var) => {
            let mut arg_vals: Vec<BasicValueEnum> = Vec::with_capacity_in(args.len(), env.arena);

            for arg in args.iter() {
                arg_vals.push(build_expr(env, layout_ids, scope, parent, arg));
            }

            let call = match build_expr(env, layout_ids, scope, parent, sub_expr) {
                BasicValueEnum::PointerValue(ptr) => {
                    env.builder.build_call(ptr, arg_vals.as_slice(), "tmp")
                }
                non_ptr => {
                    panic!(
                        "Tried to call by pointer, but encountered a non-pointer: {:?}",
                        non_ptr
                    );
                }
            };

            // TODO FIXME this should not be hardcoded!
            // Need to look up what calling convention is the right one for that function.
            // If this is an external-facing function, it'll use the C calling convention.
            // If it's an internal-only function, it should (someday) use the fast calling conention.
            call.set_call_convention(C_CALL_CONV);

            call.try_as_basic_value()
                .left()
                .unwrap_or_else(|| panic!("LLVM error: Invalid call by pointer."))
        }
        Load(symbol) => load_symbol(env, scope, symbol),
        Str(str_literal) => {
            if str_literal.is_empty() {
                panic!("TODO build an empty string in LLVM");
            } else {
                let ctx = env.context;
                let builder = env.builder;
                let str_len = str_literal.len() + 1/* TODO drop the +1 when we have structs and this is no longer a NUL-terminated CString.*/;

                let byte_type = ctx.i8_type();
                let nul_terminator = byte_type.const_zero();
                let len_val = ctx.i64_type().const_int(str_len as u64, false);
                let ptr = env
                    .builder
                    .build_array_malloc(ctx.i8_type(), len_val, "str_ptr")
                    .unwrap();

                // TODO check if malloc returned null; if so, runtime error for OOM!

                // Copy the bytes from the string literal into the array
                for (index, byte) in str_literal.bytes().enumerate() {
                    let index_val = ctx.i64_type().const_int(index as u64, false);
                    let elem_ptr =
                        unsafe { builder.build_in_bounds_gep(ptr, &[index_val], "byte") };

                    builder.build_store(elem_ptr, byte_type.const_int(byte as u64, false));
                }

                // Add a NUL terminator at the end.
                // TODO: Instead of NUL-terminating, return a struct
                // with the pointer and also the length and capacity.
                let index_val = ctx.i64_type().const_int(str_len as u64 - 1, false);
                let elem_ptr =
                    unsafe { builder.build_in_bounds_gep(ptr, &[index_val], "nul_terminator") };

                builder.build_store(elem_ptr, nul_terminator);

                BasicValueEnum::PointerValue(ptr)
            }
        }
        Array { elem_layout, elems } => {
            let ctx = env.context;
            let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
            let builder = env.builder;

            if elems.is_empty() {
                empty_list(env)
            } else {
                let len_u64 = elems.len() as u64;
                let elem_bytes = elem_layout.stack_size(env.ptr_bytes) as u64;

                let ptr = {
                    let bytes_len = elem_bytes * len_u64;
                    let len_type = env.ptr_int();
                    let len = len_type.const_int(bytes_len, false);

                    env.builder
                        .build_array_malloc(elem_type, len, "create_list_ptr")
                        .unwrap()

                    // TODO check if malloc returned null; if so, runtime error for OOM!
                };

                // Copy the elements from the list literal into the array
                for (index, elem) in elems.iter().enumerate() {
                    let index_val = ctx.i64_type().const_int(index as u64, false);
                    let elem_ptr =
                        unsafe { builder.build_in_bounds_gep(ptr, &[index_val], "index") };
                    let val = build_expr(env, layout_ids, &scope, parent, &elem);

                    builder.build_store(elem_ptr, val);
                }

                let ptr_bytes = env.ptr_bytes;
                let int_type = ptr_int(ctx, ptr_bytes);
                let ptr_as_int = builder.build_ptr_to_int(ptr, int_type, "list_cast_ptr");
                let struct_type = collection(ctx, ptr_bytes);
                let len = BasicValueEnum::IntValue(env.ptr_int().const_int(len_u64, false));
                let mut struct_val;

                // Store the pointer
                struct_val = builder
                    .build_insert_value(
                        struct_type.get_undef(),
                        ptr_as_int,
                        Builtin::WRAPPER_PTR,
                        "insert_ptr",
                    )
                    .unwrap();

                // Store the length
                struct_val = builder
                    .build_insert_value(struct_val, len, Builtin::WRAPPER_LEN, "insert_len")
                    .unwrap();

                //
                builder.build_bitcast(
                    struct_val.into_struct_value(),
                    collection(ctx, ptr_bytes),
                    "cast_collection",
                )
            }
        }

        Struct(sorted_fields) => {
            let ctx = env.context;
            let builder = env.builder;

            // Determine types
            let num_fields = sorted_fields.len();
            let mut field_types = Vec::with_capacity_in(num_fields, env.arena);
            let mut field_vals = Vec::with_capacity_in(num_fields, env.arena);

            for (field_expr, field_layout) in sorted_fields.iter() {
                let val = build_expr(env, layout_ids, &scope, parent, field_expr);
                let field_type =
                    basic_type_from_layout(env.arena, env.context, &field_layout, env.ptr_bytes);

                field_types.push(field_type);
                field_vals.push(val);
            }

            // Create the struct_type
            let struct_type = ctx.struct_type(field_types.into_bump_slice(), false);
            let mut struct_val = struct_type.const_zero().into();

            // Insert field exprs into struct_val
            for (index, field_val) in field_vals.into_iter().enumerate() {
                struct_val = builder
                    .build_insert_value(struct_val, field_val, index as u32, "insert_field")
                    .unwrap();
            }

            BasicValueEnum::StructValue(struct_val.into_struct_value())
        }
        Tag {
            union_size,
            arguments,
            ..
        } if *union_size == 1 => {
            let it = arguments.iter();

            let ctx = env.context;
            let builder = env.builder;

            // Determine types
            let num_fields = arguments.len() + 1;
            let mut field_types = Vec::with_capacity_in(num_fields, env.arena);
            let mut field_vals = Vec::with_capacity_in(num_fields, env.arena);

            for (field_expr, field_layout) in it {
                let val = build_expr(env, layout_ids, &scope, parent, field_expr);
                let field_type =
                    basic_type_from_layout(env.arena, env.context, &field_layout, env.ptr_bytes);

                field_types.push(field_type);
                field_vals.push(val);
            }

            // Create the struct_type
            let struct_type = ctx.struct_type(field_types.into_bump_slice(), false);
            let mut struct_val = struct_type.const_zero().into();

            // Insert field exprs into struct_val
            for (index, field_val) in field_vals.into_iter().enumerate() {
                struct_val = builder
                    .build_insert_value(struct_val, field_val, index as u32, "insert_field")
                    .unwrap();
            }

            BasicValueEnum::StructValue(struct_val.into_struct_value())
        }
        Tag {
            arguments,
            tag_layout,
            ..
        } => {
            let ptr_size = env.ptr_bytes;

            let whole_size = tag_layout.stack_size(ptr_size);
            let mut filler = tag_layout.stack_size(ptr_size);

            let ctx = env.context;
            let builder = env.builder;

            // Determine types
            let num_fields = arguments.len() + 1;
            let mut field_types = Vec::with_capacity_in(num_fields, env.arena);
            let mut field_vals = Vec::with_capacity_in(num_fields, env.arena);

            for (field_expr, field_layout) in arguments.iter() {
                let val = build_expr(env, layout_ids, &scope, parent, field_expr);
                let field_type =
                    basic_type_from_layout(env.arena, env.context, &field_layout, ptr_size);

                field_types.push(field_type);
                field_vals.push(val);

                let field_size = field_layout.stack_size(ptr_size);
                filler -= field_size;
            }

            // TODO verify that this is required (better safe than sorry)
            if filler > 0 {
                field_types.push(env.context.i8_type().array_type(filler).into());
            }

            // Create the struct_type
            let struct_type = ctx.struct_type(field_types.into_bump_slice(), false);
            let mut struct_val = struct_type.const_zero().into();

            // Insert field exprs into struct_val
            for (index, field_val) in field_vals.into_iter().enumerate() {
                struct_val = builder
                    .build_insert_value(struct_val, field_val, index as u32, "insert_field")
                    .unwrap();
            }

            // How we create tag values
            //
            // The memory layout of tags can be different. e.g. in
            //
            // [ Ok Int, Err Str ]
            //
            // the `Ok` tag stores a 64-bit integer, the `Err` tag stores a struct.
            // All tags of a union must have the same length, for easy addressing (e.g. array lookups).
            // So we need to ask for the maximum of all tag's sizes, even if most tags won't use
            // all that memory, and certainly won't use it in the same way (the tags have fields of
            // different types/sizes)
            //
            // In llvm, we must be explicit about the type of value we're creating: we can't just
            // make a unspecified block of memory. So what we do is create a byte array of the
            // desired size. Then when we know which tag we have (which is here, in this function),
            // we need to cast that down to the array of bytes that llvm expects
            //
            // There is the bitcast instruction, but it doesn't work for arrays. So we need to jump
            // through some hoops using store and load to get this to work: the array is put into a
            // one-element struct, which can be cast to the desired type.
            //
            // This tricks comes from
            // https://github.com/raviqqe/ssf/blob/bc32aae68940d5bddf5984128e85af75ca4f4686/ssf-llvm/src/expression_compiler.rs#L116

            let array_type = ctx.i8_type().array_type(whole_size);

            let result = cast_basic_basic(
                builder,
                struct_val.into_struct_value().into(),
                array_type.into(),
            );

            // For unclear reasons, we can't cast an array to a struct on the other side.
            // the solution is to wrap the array in a struct (yea...)
            let wrapper_type = ctx.struct_type(&[array_type.into()], false);
            let mut wrapper_val = wrapper_type.const_zero().into();
            wrapper_val = builder
                .build_insert_value(wrapper_val, result, 0, "insert_field")
                .unwrap();
            wrapper_val.into_struct_value().into()
        }
        AccessAtIndex {
            index,
            expr,
            is_unwrapped,
            ..
        } if *is_unwrapped => {
            let builder = env.builder;

            // Get Struct val
            // Since this is a one-element tag union, we get the correct struct immediately
            let argument = build_expr(env, layout_ids, &scope, parent, expr).into_struct_value();

            builder
                .build_extract_value(
                    argument,
                    *index as u32,
                    env.arena.alloc(format!("tag_field_access_{}_", index)),
                )
                .unwrap()
        }

        AccessAtIndex {
            index,
            expr,
            field_layouts,
            ..
        } => {
            let builder = env.builder;

            // Determine types, assumes the descriminant is in the field layouts
            let num_fields = field_layouts.len();
            let mut field_types = Vec::with_capacity_in(num_fields, env.arena);
            let ptr_bytes = env.ptr_bytes;

            for field_layout in field_layouts.iter() {
                let field_type =
                    basic_type_from_layout(env.arena, env.context, &field_layout, ptr_bytes);
                field_types.push(field_type);
            }

            // Create the struct_type
            let struct_type = env
                .context
                .struct_type(field_types.into_bump_slice(), false);

            // cast the argument bytes into the desired shape for this tag
            let argument = build_expr(env, layout_ids, &scope, parent, expr).into_struct_value();

            let struct_value = cast_struct_struct(builder, argument, struct_type);

            builder
                .build_extract_value(struct_value, *index as u32, "")
                .expect("desired field did not decode")
        }
        _ => {
            panic!("I don't yet know how to LLVM build {:?}", expr);
        }
    }
}

fn load_symbol<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    scope: &Scope<'a, 'ctx>,
    symbol: &Symbol,
) -> BasicValueEnum<'ctx> {
    match scope.get(symbol) {
        Some((_, ptr)) => env
            .builder
            .build_load(*ptr, symbol.ident_string(&env.interns)),
        None => panic!("Could not find a var for {:?} in scope {:?}", symbol, scope),
    }
}

/// Cast a struct to another struct of the same (or smaller?) size
fn cast_struct_struct<'ctx>(
    builder: &Builder<'ctx>,
    from_value: StructValue<'ctx>,
    to_type: StructType<'ctx>,
) -> StructValue<'ctx> {
    cast_basic_basic(builder, from_value.into(), to_type.into()).into_struct_value()
}

/// Cast a value to another value of the same (or smaller?) size
fn cast_basic_basic<'ctx>(
    builder: &Builder<'ctx>,
    from_value: BasicValueEnum<'ctx>,
    to_type: BasicTypeEnum<'ctx>,
) -> BasicValueEnum<'ctx> {
    use inkwell::types::BasicType;
    // store the value in memory
    let argument_pointer = builder.build_alloca(from_value.get_type(), "");
    builder.build_store(argument_pointer, from_value);

    // then read it back as a different type
    let to_type_pointer = builder
        .build_bitcast(
            argument_pointer,
            to_type.ptr_type(inkwell::AddressSpace::Generic),
            "",
        )
        .into_pointer_value();

    builder.build_load(to_type_pointer, "")
}

fn extract_tag_discriminant<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    from_value: StructValue<'ctx>,
) -> IntValue<'ctx> {
    let struct_type = env
        .context
        .struct_type(&[env.context.i64_type().into()], false);

    let struct_value = cast_struct_struct(env.builder, from_value, struct_type);

    env.builder
        .build_extract_value(struct_value, 0, "")
        .expect("desired field did not decode")
        .into_int_value()
}

struct SwitchArgs<'a, 'ctx> {
    pub cond_expr: &'a Expr<'a>,
    pub cond_layout: Layout<'a>,
    pub branches: &'a [(u64, Expr<'a>)],
    pub default_branch: &'a Expr<'a>,
    pub ret_type: BasicTypeEnum<'ctx>,
}

fn build_switch<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    scope: &Scope<'a, 'ctx>,
    parent: FunctionValue<'ctx>,
    switch_args: SwitchArgs<'a, 'ctx>,
) -> BasicValueEnum<'ctx> {
    let arena = env.arena;
    let builder = env.builder;
    let context = env.context;
    let SwitchArgs {
        branches,
        cond_expr,
        mut cond_layout,
        default_branch,
        ret_type,
        ..
    } = switch_args;

    let cont_block = context.append_basic_block(parent, "cont");

    // Build the condition
    let cond = match cond_layout {
        Layout::Builtin(Builtin::Float64) => {
            // float matches are done on the bit pattern
            cond_layout = Layout::Builtin(Builtin::Int64);
            let full_cond = build_expr(env, layout_ids, scope, parent, cond_expr);

            builder
                .build_bitcast(full_cond, env.context.i64_type(), "")
                .into_int_value()
        }
        Layout::Union(_) => {
            // we match on the discriminant, not the whole Tag
            cond_layout = Layout::Builtin(Builtin::Int64);
            let full_cond =
                build_expr(env, layout_ids, scope, parent, cond_expr).into_struct_value();

            extract_tag_discriminant(env, full_cond)
        }
        Layout::Builtin(_) => {
            build_expr(env, layout_ids, scope, parent, cond_expr).into_int_value()
        }
        other => todo!("Build switch value from layout: {:?}", other),
    };

    // Build the cases
    let mut incoming = Vec::with_capacity_in(branches.len(), arena);
    let mut cases = Vec::with_capacity_in(branches.len(), arena);

    for (int, _) in branches.iter() {
        // Switch constants must all be same type as switch value!
        // e.g. this is incorrect, and will trigger a LLVM warning:
        //
        //   switch i8 %apple1, label %default [
        //     i64 2, label %branch2
        //     i64 0, label %branch0
        //     i64 1, label %branch1
        //   ]
        //
        // they either need to all be i8, or i64
        let int_val = match cond_layout {
            Layout::Builtin(Builtin::Int64) => context.i64_type().const_int(*int as u64, false),
            Layout::Builtin(Builtin::Bool) => context.bool_type().const_int(*int as u64, false),
            Layout::Builtin(Builtin::Byte) => context.i8_type().const_int(*int as u64, false),
            _ => panic!("Can't cast to cond_layout = {:?}", cond_layout),
        };
        let block = context.append_basic_block(parent, format!("branch{}", int).as_str());

        cases.push((int_val, block));
    }

    let default_block = context.append_basic_block(parent, "default");

    builder.build_switch(cond, default_block, &cases);

    for ((_, branch_expr), (_, block)) in branches.iter().zip(cases) {
        builder.position_at_end(block);

        let branch_val = build_expr(env, layout_ids, scope, parent, branch_expr);

        builder.build_unconditional_branch(cont_block);

        incoming.push((branch_val, block));
    }

    // The block for the conditional's default branch.
    builder.position_at_end(default_block);

    let default_val = build_expr(env, layout_ids, scope, parent, default_branch);

    builder.build_unconditional_branch(cont_block);

    incoming.push((default_val, default_block));

    // emit merge block
    builder.position_at_end(cont_block);

    let phi = builder.build_phi(ret_type, "branch");

    for (branch_val, block) in incoming {
        phi.add_incoming(&[(&Into::<BasicValueEnum>::into(branch_val), block)]);
    }

    phi.as_basic_value()
}

fn build_basic_phi2<'a, 'ctx, 'env, PassFn, FailFn>(
    env: &Env<'a, 'ctx, 'env>,
    parent: FunctionValue<'ctx>,
    comparison: IntValue<'ctx>,
    mut build_pass: PassFn,
    mut build_fail: FailFn,
    ret_type: BasicTypeEnum<'ctx>,
) -> BasicValueEnum<'ctx>
where
    PassFn: FnMut() -> BasicValueEnum<'ctx>,
    FailFn: FnMut() -> BasicValueEnum<'ctx>,
{
    let builder = env.builder;
    let context = env.context;

    // build blocks
    let then_block = context.append_basic_block(parent, "then");
    let else_block = context.append_basic_block(parent, "else");
    let cont_block = context.append_basic_block(parent, "branchcont");

    builder.build_conditional_branch(comparison, then_block, else_block);

    // build then block
    builder.position_at_end(then_block);
    let then_val = build_pass();
    builder.build_unconditional_branch(cont_block);

    let then_block = builder.get_insert_block().unwrap();

    // build else block
    builder.position_at_end(else_block);
    let else_val = build_fail();
    builder.build_unconditional_branch(cont_block);

    let else_block = builder.get_insert_block().unwrap();

    // emit merge block
    builder.position_at_end(cont_block);

    let phi = builder.build_phi(ret_type, "branch");

    phi.add_incoming(&[(&then_val, then_block), (&else_val, else_block)]);

    phi.as_basic_value()
}

/// TODO could this be added to Inkwell itself as a method on BasicValueEnum?
fn set_name(bv_enum: BasicValueEnum<'_>, name: &str) {
    match bv_enum {
        ArrayValue(val) => val.set_name(name),
        IntValue(val) => val.set_name(name),
        FloatValue(val) => val.set_name(name),
        PointerValue(val) => val.set_name(name),
        StructValue(val) => val.set_name(name),
        VectorValue(val) => val.set_name(name),
    }
}

/// Creates a new stack allocation instruction in the entry block of the function.
pub fn create_entry_block_alloca<'a, 'ctx>(
    env: &Env<'a, 'ctx, '_>,
    parent: FunctionValue<'_>,
    basic_type: BasicTypeEnum<'ctx>,
    name: &str,
) -> PointerValue<'ctx> {
    let builder = env.context.create_builder();
    let entry = parent.get_first_basic_block().unwrap();

    match entry.get_first_instruction() {
        Some(first_instr) => builder.position_before(&first_instr),
        None => builder.position_at_end(entry),
    }

    builder.build_alloca(basic_type, name)
}

pub fn build_proc_header<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    symbol: Symbol,
    layout: &Layout<'a>,
    proc: &Proc<'a>,
) -> (FunctionValue<'ctx>, Vec<'a, BasicTypeEnum<'ctx>>) {
    let args = proc.args;
    let arena = env.arena;
    let context = &env.context;
    let ret_type = basic_type_from_layout(arena, context, &proc.ret_layout, env.ptr_bytes);
    let mut arg_basic_types = Vec::with_capacity_in(args.len(), arena);
    let mut arg_symbols = Vec::new_in(arena);

    for (layout, arg_symbol) in args.iter() {
        let arg_type = basic_type_from_layout(arena, env.context, &layout, env.ptr_bytes);

        arg_basic_types.push(arg_type);
        arg_symbols.push(arg_symbol);
    }

    let fn_type = get_fn_type(&ret_type, &arg_basic_types);

    let fn_name = layout_ids
        .get(symbol, layout)
        .to_symbol_string(symbol, &env.interns);
    let fn_val = env
        .module
        .add_function(fn_name.as_str(), fn_type, Some(Linkage::Private));

    fn_val.set_call_conventions(fn_val.get_call_conventions());

    (fn_val, arg_basic_types)
}

pub fn build_proc<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    proc: Proc<'a>,
    fn_val: FunctionValue<'ctx>,
    arg_basic_types: Vec<'a, BasicTypeEnum<'ctx>>,
) {
    let args = proc.args;
    let context = &env.context;

    // Add a basic block for the entry point
    let entry = context.append_basic_block(fn_val, "entry");
    let builder = env.builder;

    builder.position_at_end(entry);

    let mut scope = ImMap::default();

    // Add args to scope
    for ((arg_val, arg_type), (layout, arg_symbol)) in
        fn_val.get_param_iter().zip(arg_basic_types).zip(args)
    {
        set_name(arg_val, arg_symbol.ident_string(&env.interns));

        let alloca =
            create_entry_block_alloca(env, fn_val, arg_type, arg_symbol.ident_string(&env.interns));

        builder.build_store(alloca, arg_val);

        scope.insert(*arg_symbol, (layout.clone(), alloca));
    }

    let body = build_expr(env, layout_ids, &scope, fn_val, &proc.body);

    builder.build_return(Some(&body));
}

pub fn verify_fn(fn_val: FunctionValue<'_>) {
    if !fn_val.verify(PRINT_FN_VERIFICATION_OUTPUT) {
        unsafe {
            fn_val.delete();
        }

        panic!("Invalid generated fn_val.")
    }
}

#[inline(always)]
#[allow(clippy::cognitive_complexity)]
fn call_with_args<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    layout: &Layout<'a>,
    symbol: Symbol,
    parent: FunctionValue<'ctx>,
    args: &[(BasicValueEnum<'ctx>, &'a Layout<'a>)],
) -> BasicValueEnum<'ctx> {
    match symbol {
        Symbol::INT_ADD | Symbol::NUM_ADD => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_add(
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "add_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::FLOAT_ADD => {
            debug_assert!(args.len() == 2);

            let float_val = env.builder.build_float_add(
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "add_f64",
            );

            BasicValueEnum::FloatValue(float_val)
        }
        Symbol::INT_SUB | Symbol::NUM_SUB => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_sub(
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "sub_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::FLOAT_DIV => {
            debug_assert!(args.len() == 2);

            let float_val = env.builder.build_float_div(
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "div_f64",
            );

            BasicValueEnum::FloatValue(float_val)
        }
        Symbol::FLOAT_SUB => {
            debug_assert!(args.len() == 2);

            let float_val = env.builder.build_float_sub(
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "sub_f64",
            );

            BasicValueEnum::FloatValue(float_val)
        }
        Symbol::FLOAT_ABS => call_intrinsic(LLVM_FABS_F64, env, args),
        Symbol::INT_GTE | Symbol::NUM_GTE => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_int_compare(
                IntPredicate::SGE,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "gte_i64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::FLOAT_GTE => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_float_compare(
                FloatPredicate::OGE,
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "gte_F64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::INT_GT | Symbol::NUM_GT => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_int_compare(
                IntPredicate::SGT,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "gt_i64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::FLOAT_GT => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_float_compare(
                FloatPredicate::OGT,
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "gt_f64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::INT_LTE | Symbol::NUM_LTE => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_int_compare(
                IntPredicate::SLE,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "lte_i64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::FLOAT_LTE => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_float_compare(
                FloatPredicate::OLE,
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "lte_f64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::INT_LT | Symbol::NUM_LT => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_int_compare(
                IntPredicate::SLT,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "lt_i64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::FLOAT_LT => {
            debug_assert!(args.len() == 2);

            let bool_val = env.builder.build_float_compare(
                FloatPredicate::OLT,
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "lt_f64",
            );

            BasicValueEnum::IntValue(bool_val)
        }
        Symbol::FLOAT_SIN => call_intrinsic(LLVM_SIN_F64, env, args),
        Symbol::FLOAT_COS => call_intrinsic(LLVM_COS_F64, env, args),
        Symbol::NUM_MUL => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_mul(
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "mul_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::NUM_NEG => {
            debug_assert!(args.len() == 1);

            let int_val = env
                .builder
                .build_int_neg(args[0].0.into_int_value(), "negate_i64");

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::LIST_LEN => {
            debug_assert!(args.len() == 1);

            BasicValueEnum::IntValue(load_list_len(env.builder, args[0].0.into_struct_value()))
        }
        Symbol::LIST_IS_EMPTY => {
            debug_assert!(args.len() == 1);

            let list_struct = args[0].0.into_struct_value();
            let builder = env.builder;
            let list_len = load_list_len(builder, list_struct);
            let zero = env.ptr_int().const_zero();
            let answer = builder.build_int_compare(IntPredicate::EQ, list_len, zero, "is_zero");

            BasicValueEnum::IntValue(answer)
        }
        Symbol::INT_REM_UNSAFE => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_unsigned_rem(
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "rem_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::INT_EQ_I64 => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_compare(
                IntPredicate::EQ,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "cmp_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::INT_NEQ_I64 => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_compare(
                IntPredicate::NE,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "cmp_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::INT_EQ_I1 => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_compare(
                IntPredicate::EQ,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "cmp_i1",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::INT_NEQ_I1 => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_compare(
                IntPredicate::NE,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "cmp_i1",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::INT_EQ_I8 => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_compare(
                IntPredicate::EQ,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "cmp_i8",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::INT_NEQ_I8 => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_compare(
                IntPredicate::NE,
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "cmp_i8",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::NUM_TO_FLOAT => {
            // TODO specialize this to be not just for i64!
            let builtin_fn_name = "i64_to_f64_";

            let fn_val = env
                .module
                .get_function(builtin_fn_name)
                .unwrap_or_else(|| panic!("Unrecognized builtin function: {:?} - if you're working on the Roc compiler, do you need to rebuild the bitcode? See compiler/builtins/bitcode/README.md", builtin_fn_name));

            let mut arg_vals: Vec<BasicValueEnum> = Vec::with_capacity_in(args.len(), env.arena);

            for (arg, _layout) in args.iter() {
                arg_vals.push(*arg);
            }

            let call = env
                .builder
                .build_call(fn_val, arg_vals.into_bump_slice(), "call_builtin");

            call.set_call_convention(fn_val.get_call_conventions());

            call.try_as_basic_value()
                .left()
                .unwrap_or_else(|| panic!("LLVM error: Invalid call for builtin {:?}", symbol))
        }
        Symbol::FLOAT_EQ => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_float_compare(
                FloatPredicate::OEQ,
                args[0].0.into_float_value(),
                args[1].0.into_float_value(),
                "cmp_f64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        Symbol::LIST_GET_UNSAFE => {
            let builder = env.builder;

            // List.get : List elem, Int -> [ Ok elem, OutOfBounds ]*
            debug_assert!(args.len() == 2);

            let (_, list_layout) = &args[0];

            let wrapper_struct = args[0].0.into_struct_value();
            let elem_index = args[1].0.into_int_value();

            match list_layout {
                Layout::Builtin(Builtin::List(elem_layout)) => {
                    let ctx = env.context;
                    let elem_type =
                        basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
                    let ptr_type = get_ptr_type(&elem_type, AddressSpace::Generic);
                    // Load the pointer to the array data
                    let array_data_ptr = load_list_ptr(builder, wrapper_struct, ptr_type);

                    // Assume the bounds have already been checked earlier
                    // (e.g. by List.get or List.first, which wrap List.#getUnsafe)
                    let elem_ptr = unsafe {
                        builder.build_in_bounds_gep(array_data_ptr, &[elem_index], "elem")
                    };

                    builder.build_load(elem_ptr, "List.get")
                }
                _ => {
                    unreachable!("Invalid List layout for List.get: {:?}", list_layout);
                }
            }
        }
        Symbol::FLOAT_SQRT => call_intrinsic(LLVM_SQRT_F64, env, args),
        Symbol::FLOAT_ROUND => call_intrinsic(LLVM_LROUND_I64_F64, env, args),
        Symbol::LIST_SET => list_set(parent, args, env, InPlace::Clone),
        Symbol::LIST_SET_IN_PLACE => list_set(parent, args, env, InPlace::InPlace),
        Symbol::LIST_PUSH => list_push(args, env),
        Symbol::LIST_SINGLE => {
            // List.single : a -> List a
            debug_assert!(args.len() == 1);

            let (elem, elem_layout) = args[0];

            let builder = env.builder;
            let ctx = env.context;

            let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
            let elem_bytes = elem_layout.stack_size(env.ptr_bytes) as u64;

            let ptr = {
                let bytes_len = elem_bytes;
                let len_type = env.ptr_int();
                let len = len_type.const_int(bytes_len, false);

                env.builder
                    .build_array_malloc(elem_type, len, "create_list_ptr")
                    .unwrap()

                // TODO check if malloc returned null; if so, runtime error for OOM!
            };

            // Put the element into the list
            let elem_ptr = unsafe {
                builder.build_in_bounds_gep(
                    ptr,
                    &[ctx.i64_type().const_int(
                        // 0 as in 0 index of our new list
                        0 as u64, false,
                    )],
                    "index",
                )
            };

            builder.build_store(elem_ptr, elem);

            let ptr_bytes = env.ptr_bytes;
            let int_type = ptr_int(ctx, ptr_bytes);
            let ptr_as_int = builder.build_ptr_to_int(ptr, int_type, "list_cast_ptr");
            let struct_type = collection(ctx, ptr_bytes);
            let len = BasicValueEnum::IntValue(env.ptr_int().const_int(1, false));

            let mut struct_val;

            // Store the pointer
            struct_val = builder
                .build_insert_value(
                    struct_type.get_undef(),
                    ptr_as_int,
                    Builtin::WRAPPER_PTR,
                    "insert_ptr",
                )
                .unwrap();

            // Store the length
            struct_val = builder
                .build_insert_value(struct_val, len, Builtin::WRAPPER_LEN, "insert_len")
                .unwrap();

            //
            builder.build_bitcast(
                struct_val.into_struct_value(),
                collection(ctx, ptr_bytes),
                "cast_collection",
            )
        }
        Symbol::LIST_REPEAT => {
            // List.repeat : Int, elem -> List elem
            debug_assert!(args.len() == 2);

            // Number of repeats
            let list_len = args[0].0.into_int_value();

            let builder = env.builder;
            let ctx = env.context;

            let (elem, elem_layout) = args[1];
            let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);

            // list_len > 0
            // We have to do a loop below, continuously adding the `elem`
            // to the output list `List elem` until we have reached the
            // number of repeats. This `comparison` is used to check
            // if we need to do any looping; because if we dont, then we
            // dont need to allocate memory for the index or the check
            // if index != 0
            let comparison = builder.build_int_compare(
                IntPredicate::UGT,
                list_len,
                ctx.i64_type().const_int(0, false),
                "atleastzero",
            );

            let build_then = || {
                // Allocate space for the new array that we'll copy into.
                let elem_bytes = elem_layout.stack_size(env.ptr_bytes) as u64;

                let list_ptr = {
                    let bytes_len = elem_bytes;
                    let len_type = env.ptr_int();
                    let len = len_type.const_int(bytes_len, false);

                    env.builder
                        .build_array_malloc(elem_type, len, "create_list_ptr")
                        .unwrap()

                    // TODO check if malloc returned null; if so, runtime error for OOM!
                };

                let index_name = "#index";
                let start_alloca = builder.build_alloca(ctx.i64_type(), index_name);

                builder.build_store(start_alloca, list_len);

                let loop_bb = ctx.append_basic_block(parent, "loop");
                builder.build_unconditional_branch(loop_bb);
                builder.position_at_end(loop_bb);

                // #index = #index - 1
                let curr_index = builder
                    .build_load(start_alloca, index_name)
                    .into_int_value();
                let next_index = builder.build_int_sub(
                    curr_index,
                    ctx.i64_type().const_int(1, false),
                    "nextindex",
                );

                builder.build_store(start_alloca, next_index);
                let elem_ptr =
                    unsafe { builder.build_in_bounds_gep(list_ptr, &[curr_index], "load_index") };

                // Mutate the new array in-place to change the element.
                builder.build_store(elem_ptr, elem);

                // #index != 0
                let end_cond = builder.build_int_compare(
                    IntPredicate::NE,
                    ctx.i64_type().const_int(0, false),
                    curr_index,
                    "loopcond",
                );

                let after_bb = ctx.append_basic_block(parent, "afterloop");

                builder.build_conditional_branch(end_cond, loop_bb, after_bb);
                builder.position_at_end(after_bb);

                let ptr_bytes = env.ptr_bytes;
                let int_type = ptr_int(ctx, ptr_bytes);
                let ptr_as_int = builder.build_ptr_to_int(list_ptr, int_type, "list_cast_ptr");
                let struct_type = collection(ctx, ptr_bytes);

                let mut struct_val;

                // Store the pointer
                struct_val = builder
                    .build_insert_value(
                        struct_type.get_undef(),
                        ptr_as_int,
                        Builtin::WRAPPER_PTR,
                        "insert_ptr",
                    )
                    .unwrap();

                // Store the length
                struct_val = builder
                    .build_insert_value(struct_val, list_len, Builtin::WRAPPER_LEN, "insert_len")
                    .unwrap();

                builder.build_bitcast(
                    struct_val.into_struct_value(),
                    collection(ctx, ptr_bytes),
                    "cast_collection",
                )
            };

            let build_else = || empty_list(env);

            let struct_type = collection(ctx, env.ptr_bytes);

            build_basic_phi2(
                env,
                parent,
                comparison,
                build_then,
                build_else,
                BasicTypeEnum::StructType(struct_type),
            )
        }
        Symbol::LIST_REVERSE => {
            // List.reverse : List elem -> List elem
            debug_assert_eq!(args.len(), 1);

            let (list, list_layout) = &args[0];

            let wrapper_struct = list.into_struct_value();

            let builder = env.builder;
            let ctx = env.context;

            let list_len = load_list_len(builder, wrapper_struct);

            // list_len > 0
            // We do this check to avoid allocating memory. If the input
            // list is empty, then we can just return an empty list.
            let comparison = builder.build_int_compare(
                IntPredicate::UGT,
                list_len,
                ctx.i64_type().const_int(0, false),
                "greaterthanzero",
            );

            let build_then = || {
                match list_layout {
                    Layout::Builtin(Builtin::List(elem_layout)) => {
                        // Allocate space for the new array that we'll copy into.
                        let elem_type =
                            basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);

                        let ptr_type = get_ptr_type(&elem_type, AddressSpace::Generic);

                        let reversed_list_ptr = env
                            .builder
                            .build_array_malloc(elem_type, list_len, "create_reversed_list_ptr")
                            .unwrap();

                        // TODO check if malloc returned null; if so, runtime error for OOM!

                        let index_name = "#index";
                        let start_alloca = builder.build_alloca(ctx.i64_type(), index_name);

                        // Start at the last element in the list.
                        let last_elem_index = builder.build_int_sub(
                            list_len,
                            ctx.i64_type().const_int(1, false),
                            "lastelemindex",
                        );
                        builder.build_store(start_alloca, last_elem_index);

                        let loop_bb = ctx.append_basic_block(parent, "loop");
                        builder.build_unconditional_branch(loop_bb);
                        builder.position_at_end(loop_bb);

                        // #index = #index - 1
                        let curr_index = builder
                            .build_load(start_alloca, index_name)
                            .into_int_value();
                        let next_index = builder.build_int_sub(
                            curr_index,
                            ctx.i64_type().const_int(1, false),
                            "nextindex",
                        );

                        builder.build_store(start_alloca, next_index);

                        let list_ptr = load_list_ptr(builder, wrapper_struct, ptr_type);

                        // The pointer to the element in the input list
                        let elem_ptr = unsafe {
                            builder.build_in_bounds_gep(list_ptr, &[curr_index], "load_index")
                        };

                        // The pointer to the element in the reversed list
                        let reverse_elem_ptr = unsafe {
                            builder.build_in_bounds_gep(
                                reversed_list_ptr,
                                &[builder.build_int_sub(
                                    list_len,
                                    builder.build_int_add(
                                        curr_index,
                                        ctx.i64_type().const_int(1, false),
                                        "curr_index_plus_one",
                                    ),
                                    "next_index",
                                )],
                                "load_index_reversed_list",
                            )
                        };

                        let elem = builder.build_load(elem_ptr, "get_elem");

                        // Mutate the new array in-place to change the element.
                        builder.build_store(reverse_elem_ptr, elem);

                        // #index != 0
                        let end_cond = builder.build_int_compare(
                            IntPredicate::NE,
                            ctx.i64_type().const_int(0, false),
                            curr_index,
                            "loopcond",
                        );

                        let after_bb = ctx.append_basic_block(parent, "afterloop");

                        builder.build_conditional_branch(end_cond, loop_bb, after_bb);
                        builder.position_at_end(after_bb);

                        let ptr_bytes = env.ptr_bytes;
                        let int_type = ptr_int(ctx, ptr_bytes);
                        let ptr_as_int =
                            builder.build_ptr_to_int(reversed_list_ptr, int_type, "list_cast_ptr");
                        let struct_type = collection(ctx, ptr_bytes);

                        let mut struct_val;

                        // Store the pointer
                        struct_val = builder
                            .build_insert_value(
                                struct_type.get_undef(),
                                ptr_as_int,
                                Builtin::WRAPPER_PTR,
                                "insert_ptr",
                            )
                            .unwrap();

                        // Store the length
                        struct_val = builder
                            .build_insert_value(
                                struct_val,
                                list_len,
                                Builtin::WRAPPER_LEN,
                                "insert_len",
                            )
                            .unwrap();

                        builder.build_bitcast(
                            struct_val.into_struct_value(),
                            collection(ctx, ptr_bytes),
                            "cast_collection",
                        )
                    }
                    Layout::Builtin(Builtin::EmptyList) => empty_list(env),
                    _ => {
                        unreachable!("Invalid List layout for List.get: {:?}", list_layout);
                    }
                }
            };

            let build_else = || empty_list(env);

            let struct_type = collection(ctx, env.ptr_bytes);

            build_basic_phi2(
                env,
                parent,
                comparison,
                build_then,
                build_else,
                BasicTypeEnum::StructType(struct_type),
            )
        }
        Symbol::INT_DIV_UNSAFE => {
            debug_assert!(args.len() == 2);

            let int_val = env.builder.build_int_signed_div(
                args[0].0.into_int_value(),
                args[1].0.into_int_value(),
                "div_i64",
            );

            BasicValueEnum::IntValue(int_val)
        }
        _ => {
            let fn_name = layout_ids
                .get(symbol, layout)
                .to_symbol_string(symbol, &env.interns);
            let fn_val = env
                .module
                .get_function(fn_name.as_str())
                .unwrap_or_else(|| panic!("Unrecognized function: {:?}", symbol));

            let mut arg_vals: Vec<BasicValueEnum> = Vec::with_capacity_in(args.len(), env.arena);

            for (arg, _layout) in args.iter() {
                arg_vals.push(*arg);
            }

            let call = env
                .builder
                .build_call(fn_val, arg_vals.into_bump_slice(), "call");

            call.set_call_convention(fn_val.get_call_conventions());

            call.try_as_basic_value()
                .left()
                .unwrap_or_else(|| panic!("LLVM error: Invalid call by name for name {:?}", symbol))
        }
    }
}

fn call_intrinsic<'a, 'ctx, 'env>(
    intrinsic_name: &'static str,
    env: &Env<'a, 'ctx, 'env>,
    args: &[(BasicValueEnum<'ctx>, &'a Layout<'a>)],
) -> BasicValueEnum<'ctx> {
    let fn_val = env
        .module
        .get_function(intrinsic_name)
        .unwrap_or_else(|| panic!("Unrecognized intrinsic function: {}", intrinsic_name));

    let mut arg_vals: Vec<BasicValueEnum> = Vec::with_capacity_in(args.len(), env.arena);

    for (arg, _layout) in args.iter() {
        arg_vals.push(*arg);
    }

    let call = env
        .builder
        .build_call(fn_val, arg_vals.into_bump_slice(), "call");

    call.set_call_convention(fn_val.get_call_conventions());

    call.try_as_basic_value().left().unwrap_or_else(|| {
        panic!(
            "LLVM error: Invalid call by name for intrinsic {}",
            intrinsic_name
        )
    })
}

fn load_list_len<'ctx>(
    builder: &Builder<'ctx>,
    wrapper_struct: StructValue<'ctx>,
) -> IntValue<'ctx> {
    builder
        .build_extract_value(wrapper_struct, Builtin::WRAPPER_LEN, "list_len")
        .unwrap()
        .into_int_value()
}

fn load_list_ptr<'ctx>(
    builder: &Builder<'ctx>,
    wrapper_struct: StructValue<'ctx>,
    ptr_type: PointerType<'ctx>,
) -> PointerValue<'ctx> {
    let ptr_as_int = builder
        .build_extract_value(wrapper_struct, Builtin::WRAPPER_PTR, "read_list_ptr")
        .unwrap()
        .into_int_value();

    builder.build_int_to_ptr(ptr_as_int, ptr_type, "list_cast_ptr")
}

fn clone_nonempty_list<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    list_len: IntValue<'ctx>,
    elems_ptr: PointerValue<'ctx>,
    elem_layout: &Layout<'_>,
) -> (StructValue<'ctx>, PointerValue<'ctx>) {
    let builder = env.builder;
    let ctx = env.context;
    let ptr_bytes = env.ptr_bytes;

    // Calculate the number of bytes we'll need to allocate.
    let elem_bytes = env
        .ptr_int()
        .const_int(elem_layout.stack_size(env.ptr_bytes) as u64, false);
    let size = env
        .builder
        .build_int_mul(elem_bytes, list_len, "mul_len_by_elem_bytes");

    // Allocate space for the new array that we'll copy into.
    let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
    let clone_ptr = builder
        .build_array_malloc(elem_type, list_len, "list_ptr")
        .unwrap();
    let int_type = ptr_int(ctx, ptr_bytes);
    let ptr_as_int = builder.build_ptr_to_int(clone_ptr, int_type, "list_cast_ptr");

    // TODO check if malloc returned null; if so, runtime error for OOM!

    // Either memcpy or deep clone the array elements
    if elem_layout.safe_to_memcpy() {
        // Copy the bytes from the original array into the new
        // one we just malloc'd.
        //
        // TODO how do we decide when to do the small memcpy vs the normal one?
        builder.build_memcpy(clone_ptr, ptr_bytes, elems_ptr, ptr_bytes, size);
    } else {
        panic!("TODO Cranelift currently only knows how to clone list elements that are Copy.");
    }

    // Create a fresh wrapper struct for the newly populated array
    let struct_type = collection(ctx, env.ptr_bytes);
    let mut struct_val;

    // Store the pointer
    struct_val = builder
        .build_insert_value(
            struct_type.get_undef(),
            ptr_as_int,
            Builtin::WRAPPER_PTR,
            "insert_ptr",
        )
        .unwrap();

    // Store the length
    struct_val = builder
        .build_insert_value(struct_val, list_len, Builtin::WRAPPER_LEN, "insert_len")
        .unwrap();

    let answer = builder
        .build_bitcast(
            struct_val.into_struct_value(),
            collection(ctx, ptr_bytes),
            "cast_collection",
        )
        .into_struct_value();

    (answer, clone_ptr)
}

enum InPlace {
    InPlace,
    Clone,
}

fn empty_list<'a, 'ctx, 'env>(env: &Env<'a, 'ctx, 'env>) -> BasicValueEnum<'ctx> {
    let ctx = env.context;

    let struct_type = collection(ctx, env.ptr_bytes);

    // The pointer should be null (aka zero) and the length should be zero,
    // so the whole struct should be a const_zero
    BasicValueEnum::StructValue(struct_type.const_zero())
}

fn bounds_check_comparison<'ctx>(
    builder: &Builder<'ctx>,
    elem_index: IntValue<'ctx>,
    len: IntValue<'ctx>,
) -> IntValue<'ctx> {
    // Note: Check for index < length as the "true" condition,
    // to avoid misprediction. (In practice this should usually pass,
    // and CPUs generally default to predicting that a forward jump
    // shouldn't be taken; that is, they predict "else" won't be taken.)
    builder.build_int_compare(IntPredicate::ULT, elem_index, len, "bounds_check")
}

fn list_push<'a, 'ctx, 'env>(
    args: &[(BasicValueEnum<'ctx>, &'a Layout<'a>)],
    env: &Env<'a, 'ctx, 'env>,
) -> BasicValueEnum<'ctx> {
    // List.push List elem, elem -> List elem
    let builder = env.builder;
    let ctx = env.context;

    debug_assert!(args.len() == 2);

    let original_wrapper = args[0].0.into_struct_value();

    // Load the usize length from the wrapper.
    let list_len = load_list_len(builder, original_wrapper);

    let (elem, elem_layout) = args[1];
    let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
    let ptr_type = get_ptr_type(&elem_type, AddressSpace::Generic);

    let elems_ptr = load_list_ptr(builder, original_wrapper, ptr_type);

    // The output list length, which is the old list length + 1
    let new_list_len = env.builder.build_int_add(
        ctx.i64_type().const_int(1 as u64, false),
        list_len,
        "new_list_length",
    );

    let ctx = env.context;
    let ptr_bytes = env.ptr_bytes;

    // Calculate the number of bytes we'll need to allocate.
    let elem_bytes = env
        .ptr_int()
        .const_int(elem_layout.stack_size(env.ptr_bytes) as u64, false);

    // This is the size of the list coming in, before we have added an element
    // to the end.
    let list_size = env
        .builder
        .build_int_mul(elem_bytes, list_len, "mul_old_len_by_elem_bytes");

    // Allocate space for the new array that we'll copy into.
    let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
    let clone_ptr = builder
        .build_array_malloc(elem_type, new_list_len, "list_ptr")
        .unwrap();
    let int_type = ptr_int(ctx, ptr_bytes);
    let ptr_as_int = builder.build_ptr_to_int(clone_ptr, int_type, "list_cast_ptr");

    // TODO check if malloc returned null; if so, runtime error for OOM!

    if elem_layout.safe_to_memcpy() {
        // Copy the bytes from the original array into the new
        // one we just malloc'd.
        //
        // TODO how do we decide when to do the small memcpy vs the normal one?
        builder.build_memcpy(clone_ptr, ptr_bytes, elems_ptr, ptr_bytes, list_size);
    } else {
        panic!("TODO Cranelift currently only knows how to clone list elements that are Copy.");
    }

    // Create a fresh wrapper struct for the newly populated array
    let struct_type = collection(ctx, env.ptr_bytes);
    let mut struct_val;

    // Store the pointer
    struct_val = builder
        .build_insert_value(
            struct_type.get_undef(),
            ptr_as_int,
            Builtin::WRAPPER_PTR,
            "insert_ptr",
        )
        .unwrap();

    // Store the length
    struct_val = builder
        .build_insert_value(struct_val, new_list_len, Builtin::WRAPPER_LEN, "insert_len")
        .unwrap();

    let answer = builder.build_bitcast(
        struct_val.into_struct_value(),
        collection(ctx, ptr_bytes),
        "cast_collection",
    );

    let elem_ptr = unsafe { builder.build_in_bounds_gep(clone_ptr, &[list_len], "load_index") };

    builder.build_store(elem_ptr, elem);

    answer
}

fn list_set<'a, 'ctx, 'env>(
    parent: FunctionValue<'ctx>,
    args: &[(BasicValueEnum<'ctx>, &'a Layout<'a>)],
    env: &Env<'a, 'ctx, 'env>,
    in_place: InPlace,
) -> BasicValueEnum<'ctx> {
    // List.set : List elem, Int, elem -> List elem
    let builder = env.builder;

    debug_assert!(args.len() == 3);

    let original_wrapper = args[0].0.into_struct_value();
    let elem_index = args[1].0.into_int_value();

    // Load the usize length from the wrapper. We need it for bounds checking.
    let list_len = load_list_len(builder, original_wrapper);

    // Bounds check: only proceed if index < length.
    // Otherwise, return the list unaltered.
    let comparison = bounds_check_comparison(builder, elem_index, list_len);

    // If the index is in bounds, clone and mutate in place.
    let build_then = || {
        let (elem, elem_layout) = args[2];
        let ctx = env.context;
        let elem_type = basic_type_from_layout(env.arena, ctx, elem_layout, env.ptr_bytes);
        let ptr_type = get_ptr_type(&elem_type, AddressSpace::Generic);
        let (new_wrapper, array_data_ptr) = match in_place {
            InPlace::InPlace => (
                original_wrapper,
                load_list_ptr(builder, original_wrapper, ptr_type),
            ),
            InPlace::Clone => clone_nonempty_list(
                env,
                list_len,
                load_list_ptr(builder, original_wrapper, ptr_type),
                elem_layout,
            ),
        };

        // If we got here, we passed the bounds check, so this is an in-bounds GEP
        let elem_ptr =
            unsafe { builder.build_in_bounds_gep(array_data_ptr, &[elem_index], "load_index") };

        // Mutate the new array in-place to change the element.
        builder.build_store(elem_ptr, elem);

        BasicValueEnum::StructValue(new_wrapper)
    };

    // If the index was out of bounds, return the original list unaltered.
    let build_else = || BasicValueEnum::StructValue(original_wrapper);
    let ret_type = original_wrapper.get_type();

    build_basic_phi2(
        env,
        parent,
        comparison,
        build_then,
        build_else,
        ret_type.into(),
    )
}

/// Translates a target_lexicon::Triple to a LLVM calling convention u32
/// as described in https://llvm.org/doxygen/namespacellvm_1_1CallingConv.html
pub fn get_call_conventions(cc: CallingConvention) -> u32 {
    use CallingConvention::*;

    // For now, we're returning 0 for the C calling convention on all of these.
    // Not sure if we should be picking something more specific!
    match cc {
        SystemV => C_CALL_CONV,
        WasmBasicCAbi => C_CALL_CONV,
        WindowsFastcall => C_CALL_CONV,
    }
}

/// Source: https://llvm.org/doxygen/namespacellvm_1_1CallingConv.html
pub static C_CALL_CONV: u32 = 0;
pub static COLD_CALL_CONV: u32 = 9;
