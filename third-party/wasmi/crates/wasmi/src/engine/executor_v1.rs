use super::{
    bytecode::{BranchOffset, F64Const32},
    const_pool::ConstRef,
    executor::{CallKind, CallOutcome, ReturnOutcome, WasmOutcome},
    CompiledFunc, ConstPoolView,
};
use crate::{
    core::TrapCode,
    engine::{
        bytecode::{
            AddressOffset, BlockFuel, BranchTableTargets, DataSegmentIdx, ElementSegmentIdx,
            FuncIdx, GlobalIdx, Instruction, LocalDepth, SignatureIdx, TableIdx,
        },
        cache::InstanceCache,
        code_map::{CodeMap, InstructionPtr},
        config::FuelCosts,
        stack::{CallStack, ValueStackPtr},
        DropKeep, FuncFrame, ValueStack,
    },
    error::EntityGrowError,
    func::FuncEntity,
    store::ResourceLimiterRef,
    table::TableEntity,
    tracer::WitnessVM,
    FuelConsumptionMode, Func, FuncRef, StoreInner, Table, Tracer,
};
use alloc::rc::Rc;
use core::{
    cell::RefCell,
    cmp::{self},
};
use wasmi_core::{effective_address, Pages, UntypedValue};

/// Executes the given function `frame`.
///
/// # Note
///
/// This executes Wasm instructions until either the execution calls
/// into a host function or the Wasm execution has come to an end.
///
/// # Errors
///
/// If the Wasm execution traps.
#[inline(never)]
#[allow(dead_code)]
pub fn execute_wasm<'ctx, 'engine>(
    ctx: &'ctx mut StoreInner,
    cache: &'engine mut InstanceCache,
    value_stack: &'engine mut ValueStack,
    call_stack: &'engine mut CallStack,
    code_map: &'engine CodeMap,
    const_pool: ConstPoolView<'engine>,
    resource_limiter: &'ctx mut ResourceLimiterRef<'ctx>,
) -> Result<WasmOutcome, TrapCode> {
    Executor::new(
        ctx,
        cache,
        value_stack,
        call_stack,
        code_map,
        const_pool,
        None,
    )
    .execute(resource_limiter)
}

/// Executes the given function `frame`.
///
/// # Note
///
/// This executes Wasm instructions until either the execution calls
/// into a host function or the Wasm execution has come to an end.
///
/// # Errors
///
/// If the Wasm execution traps.
#[inline(never)]
pub fn execute_wasm_with_trace<'ctx, 'engine>(
    ctx: &'ctx mut StoreInner,
    cache: &'engine mut InstanceCache,
    value_stack: &'engine mut ValueStack,
    call_stack: &'engine mut CallStack,
    code_map: &'engine CodeMap,
    const_pool: ConstPoolView<'engine>,
    resource_limiter: &'ctx mut ResourceLimiterRef<'ctx>,
    tracer: Rc<RefCell<Tracer>>,
) -> Result<WasmOutcome, TrapCode> {
    Executor::new(
        ctx,
        cache,
        value_stack,
        call_stack,
        code_map,
        const_pool,
        Some(tracer),
    )
    .execute(resource_limiter)
}

/// The function signature of Wasm load operations.
type WasmLoadOp =
    fn(memory: &[u8], address: UntypedValue, offset: u32) -> Result<UntypedValue, TrapCode>;

/// The function signature of Wasm store operations.
type WasmStoreOp = fn(
    memory: &mut [u8],
    address: UntypedValue,
    offset: u32,
    value: UntypedValue,
) -> Result<(), TrapCode>;

/// The WebAssembly specification demands to return this value
/// if the `memory.grow` or `table.grow` operations fail.
const INVALID_GROWTH_ERRCODE: u32 = u32::MAX;

/// An execution context for executing a `wasmi` function frame.
#[derive(Debug)]
struct Executor<'ctx, 'engine> {
    /// Stores the value stack of live values on the Wasm stack.
    sp: ValueStackPtr,
    /// The pointer to the currently executed instruction.
    ip: InstructionPtr,
    /// Stores frequently used instance related data.
    cache: &'engine mut InstanceCache,
    /// A mutable [`StoreInner`] context.
    ///
    /// [`StoreInner`]: [`crate::StoreInner`]
    ctx: &'ctx mut StoreInner,
    /// The value stack.
    ///
    /// # Note
    ///
    /// This reference is mainly used to synchronize back state
    /// after manipulations to the value stack via `sp`.
    value_stack: &'engine mut ValueStack,
    /// The call stack.
    ///
    /// # Note
    ///
    /// This is used to store the stack of nested function calls.
    call_stack: &'engine mut CallStack,
    /// The Wasm function code map.
    ///
    /// # Note
    ///
    /// This is used to lookup Wasm function information.
    code_map: &'engine CodeMap,
    /// A read-only view to a pool of constant values.
    const_pool: ConstPoolView<'engine>,
    /// This is used to build an execution trace from the WASM module.
    tracer: Option<Rc<RefCell<Tracer>>>,
}

macro_rules! forward_call {
    ($expr:expr) => {{
        if let CallOutcome::Call {
            host_func,
            instance,
        } = $expr?
        {
            return Ok(WasmOutcome::Call {
                host_func,
                instance,
            });
        }
    }};
}

impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    /// Creates a new [`Executor`] for executing a `wasmi` function frame.
    #[inline(always)]
    pub fn new(
        ctx: &'ctx mut StoreInner,
        cache: &'engine mut InstanceCache,
        value_stack: &'engine mut ValueStack,
        call_stack: &'engine mut CallStack,
        code_map: &'engine CodeMap,
        const_pool: ConstPoolView<'engine>,
        tracer: Option<Rc<RefCell<Tracer>>>,
    ) -> Self {
        let frame = call_stack.pop().expect("must have frame on the call stack");
        let sp = value_stack.stack_ptr();
        let ip = frame.ip();
        Self {
            sp,
            ip,
            cache,
            ctx,
            value_stack,
            call_stack,
            code_map,
            const_pool,
            tracer,
        }
    }

    /// Executes the function frame until it returns or traps.
    #[inline(always)]
    fn execute(
        mut self,
        resource_limiter: &'ctx mut ResourceLimiterRef<'ctx>,
    ) -> Result<WasmOutcome, TrapCode> {
        use Instruction as Instr;
        // "Fetch, Decode, Execute" loop
        loop {
            let instr = &{*self.ip.get(&self.code_map)};

            // Get the pre-execution VM state at this timestamp
            let mut vm = if self.tracer.is_some() {
                let mut vm = WitnessVM::default();
                if let Some(tracer) = self.tracer.clone() {
                    // run this fn to get the usize value of [`ValueStackPtr`]
                    self.sync_stack_ptr();
                    let mut tracer = tracer.borrow_mut();

                    if matches!(tracer.last(), Some(Instr::HostCallStackStep)) {
                        tracer.execution_trace.extend(self.trace_host_call());
                    }

                    // Capture/Trace the necessary pre-execution values
                    vm = self.execute_instr_pre(self.value_stack.stack_ptr, self.pc());

                    // handle tracing edge cases
                    match *instr {
                        Instr::MemoryCopy => {
                            tracer
                                .execution_trace
                                .extend(self.trace_memory_copy(vm.clone()));
                        }
                        Instr::BrAdjust(..) => {
                            let drop_keep = self.fetch_drop_keep(1);
                            tracer
                                .execution_trace
                                .extend(self.trace_drop_keep(vm.clone(), drop_keep));
                        }
                        _ => {}
                    }
                }

                vm
            } else {
                WitnessVM::default()
            };

            // used to trace VM state change
            macro_rules! trace_post_state_change {
                () => {
                    if let Some(tracer) = self.tracer.clone() {
                        let mut tracer = tracer.borrow_mut();
                        tracer.set_max_sp(vm.pre_sp);
                        match *instr {
                            Instr::CallInternal(compiled_func) => {
                                tracer
                                    .execution_trace
                                    .extend(self.trace_call_internal(vm.clone(), compiled_func));
                            }
                            Instr::MemoryFill => {
                                tracer
                                    .execution_trace
                                    .extend(self.trace_memory_fill(vm.clone()));
                            }
                            Instr::Return(drop_keep) => {
                                tracer
                                    .execution_trace
                                    .extend(self.trace_drop_keep(vm.clone(), drop_keep));
                            }
                            Instr::MemoryGrow => {
                                let last = self.sp.last().to_bits() as i32;
                                if last != -1 {
                                    tracer.memory_grow(vm.Y);
                                };
                            }
                            _ => {}
                        }
                        // Get post instruction VM state changes
                        self.execute_instr_post(&mut vm, instr);
                        tracer.execution_trace.push(vm);
                    }
                };
            }

            match *instr {
                Instr::LocalGet(local_depth) => self.visit_local_get(local_depth),
                Instr::LocalSet(local_depth) => self.visit_local_set(local_depth),
                Instr::LocalTee(local_depth) => self.visit_local_tee(local_depth),
                Instr::Br(offset) => self.visit_br(offset),
                Instr::BrIfEqz(offset) => self.visit_br_if_eqz(offset),
                Instr::BrIfNez(offset) => self.visit_br_if_nez(offset),
                Instr::BrAdjust(offset) => self.visit_br_adjust(offset),
                Instr::BrAdjustIfNez(offset) => self.visit_br_adjust_if_nez(offset),
                Instr::BrTable(targets) => self.visit_br_table(targets),
                Instr::Unreachable => self.visit_unreachable()?,
                Instr::ConsumeFuel(block_fuel) => self.visit_consume_fuel(block_fuel)?,
                Instr::Return(drop_keep) => {
                    if let ReturnOutcome::Host = self.visit_ret(drop_keep) {
                        return Ok(WasmOutcome::Return);
                    }
                }
                Instr::ReturnIfNez(drop_keep) => {
                    if let ReturnOutcome::Host = self.visit_return_if_nez(drop_keep) {
                        return Ok(WasmOutcome::Return);
                    }
                }
                Instr::ReturnCallInternal(compiled_func) => {
                    self.visit_return_call_internal(compiled_func)?
                }
                Instr::ReturnCall(func) => {
                    forward_call!(self.visit_return_call(func))
                }
                Instr::ReturnCallIndirect(func_type) => {
                    forward_call!(self.visit_return_call_indirect(func_type))
                }
                Instr::CallInternal(compiled_func) => self.visit_call_internal(compiled_func)?,
                Instr::Call(func) => forward_call!(self.visit_call(func)),
                Instr::CallIndirect(func_type) => {
                    forward_call!(self.visit_call_indirect(func_type))
                }
                Instr::Drop => self.visit_drop(),
                Instr::Select => self.visit_select(),
                Instr::GlobalGet(global_idx) => self.visit_global_get(global_idx),
                Instr::GlobalSet(global_idx) => self.visit_global_set(global_idx),
                Instr::I32Load(offset) => self.visit_i32_load(offset)?,
                Instr::I64Load(offset) => self.visit_i64_load(offset)?,
                Instr::F32Load(offset) => self.visit_f32_load(offset)?,
                Instr::F64Load(offset) => self.visit_f64_load(offset)?,
                Instr::I32Load8S(offset) => self.visit_i32_load_i8_s(offset)?,
                Instr::I32Load8U(offset) => self.visit_i32_load_i8_u(offset)?,
                Instr::I32Load16S(offset) => self.visit_i32_load_i16_s(offset)?,
                Instr::I32Load16U(offset) => self.visit_i32_load_i16_u(offset)?,
                Instr::I64Load8S(offset) => self.visit_i64_load_i8_s(offset)?,
                Instr::I64Load8U(offset) => self.visit_i64_load_i8_u(offset)?,
                Instr::I64Load16S(offset) => self.visit_i64_load_i16_s(offset)?,
                Instr::I64Load16U(offset) => self.visit_i64_load_i16_u(offset)?,
                Instr::I64Load32S(offset) => self.visit_i64_load_i32_s(offset)?,
                Instr::I64Load32U(offset) => self.visit_i64_load_i32_u(offset)?,
                Instr::I32Store(offset) => self.visit_i32_store(offset)?,
                Instr::I64Store(offset) => self.visit_i64_store(offset)?,
                Instr::F32Store(offset) => self.visit_f32_store(offset)?,
                Instr::F64Store(offset) => self.visit_f64_store(offset)?,
                Instr::I32Store8(offset) => self.visit_i32_store_8(offset)?,
                Instr::I32Store16(offset) => self.visit_i32_store_16(offset)?,
                Instr::I64Store8(offset) => self.visit_i64_store_8(offset)?,
                Instr::I64Store16(offset) => self.visit_i64_store_16(offset)?,
                Instr::I64Store32(offset) => self.visit_i64_store_32(offset)?,
                Instr::MemorySize => self.visit_memory_size(),
                Instr::MemoryGrow => self.visit_memory_grow(&mut *resource_limiter)?,
                Instr::MemoryFill => self.visit_memory_fill()?,
                Instr::MemoryCopy => self.visit_memory_copy()?,
                Instr::MemoryInit(segment) => self.visit_memory_init(segment)?,
                Instr::DataDrop(segment) => self.visit_data_drop(segment),
                Instr::TableSize(table) => self.visit_table_size(table),
                Instr::TableGrow(table) => self.visit_table_grow(table, &mut *resource_limiter)?,
                Instr::TableFill(table) => self.visit_table_fill(table)?,
                Instr::TableGet(table) => self.visit_table_get(table)?,
                Instr::TableSet(table) => self.visit_table_set(table)?,
                Instr::TableCopy(dst) => self.visit_table_copy(dst)?,
                Instr::TableInit(elem) => self.visit_table_init(elem)?,
                Instr::ElemDrop(segment) => self.visit_element_drop(segment),
                Instr::RefFunc(func_index) => self.visit_ref_func(func_index),
                Instr::Const32(bytes) => self.visit_const_32(bytes),
                Instr::I64Const32(value) => self.visit_i64_const_32(value),
                Instr::F64Const32(value) => self.visit_f64_const_32(value),
                Instr::ConstRef(cref) => self.visit_const(cref),
                Instr::I32Eqz => self.visit_i32_eqz(),
                Instr::I32Eq => self.visit_i32_eq(),
                Instr::I32Ne => self.visit_i32_ne(),
                Instr::I32LtS => self.visit_i32_lt_s(),
                Instr::I32LtU => self.visit_i32_lt_u(),
                Instr::I32GtS => self.visit_i32_gt_s(),
                Instr::I32GtU => self.visit_i32_gt_u(),
                Instr::I32LeS => self.visit_i32_le_s(),
                Instr::I32LeU => self.visit_i32_le_u(),
                Instr::I32GeS => self.visit_i32_ge_s(),
                Instr::I32GeU => self.visit_i32_ge_u(),
                Instr::I64Eqz => self.visit_i64_eqz(),
                Instr::I64Eq => self.visit_i64_eq(),
                Instr::I64Ne => self.visit_i64_ne(),
                Instr::I64LtS => self.visit_i64_lt_s(),
                Instr::I64LtU => self.visit_i64_lt_u(),
                Instr::I64GtS => self.visit_i64_gt_s(),
                Instr::I64GtU => self.visit_i64_gt_u(),
                Instr::I64LeS => self.visit_i64_le_s(),
                Instr::I64LeU => self.visit_i64_le_u(),
                Instr::I64GeS => self.visit_i64_ge_s(),
                Instr::I64GeU => self.visit_i64_ge_u(),
                Instr::F32Eq => self.visit_f32_eq(),
                Instr::F32Ne => self.visit_f32_ne(),
                Instr::F32Lt => self.visit_f32_lt(),
                Instr::F32Gt => self.visit_f32_gt(),
                Instr::F32Le => self.visit_f32_le(),
                Instr::F32Ge => self.visit_f32_ge(),
                Instr::F64Eq => self.visit_f64_eq(),
                Instr::F64Ne => self.visit_f64_ne(),
                Instr::F64Lt => self.visit_f64_lt(),
                Instr::F64Gt => self.visit_f64_gt(),
                Instr::F64Le => self.visit_f64_le(),
                Instr::F64Ge => self.visit_f64_ge(),
                Instr::I32Clz => self.visit_i32_clz(),
                Instr::I32Ctz => self.visit_i32_ctz(),
                Instr::I32Popcnt => self.visit_i32_popcnt(),
                Instr::I32Add => self.visit_i32_add(),
                Instr::I32Sub => self.visit_i32_sub(),
                Instr::I32Mul => self.visit_i32_mul(),
                Instr::I32DivS => self.visit_i32_div_s()?,
                Instr::I32DivU => self.visit_i32_div_u()?,
                Instr::I32RemS => self.visit_i32_rem_s()?,
                Instr::I32RemU => self.visit_i32_rem_u()?,
                Instr::I32And => self.visit_i32_and(),
                Instr::I32Or => self.visit_i32_or(),
                Instr::I32Xor => self.visit_i32_xor(),
                Instr::I32Shl => self.visit_i32_shl(),
                Instr::I32ShrS => self.visit_i32_shr_s(),
                Instr::I32ShrU => self.visit_i32_shr_u(),
                Instr::I32Rotl => self.visit_i32_rotl(),
                Instr::I32Rotr => self.visit_i32_rotr(),
                Instr::I64Clz => self.visit_i64_clz(),
                Instr::I64Ctz => self.visit_i64_ctz(),
                Instr::I64Popcnt => self.visit_i64_popcnt(),
                Instr::I64Add => self.visit_i64_add(),
                Instr::I64Sub => self.visit_i64_sub(),
                Instr::I64Mul => self.visit_i64_mul(),
                Instr::I64DivS => self.visit_i64_div_s()?,
                Instr::I64DivU => self.visit_i64_div_u()?,
                Instr::I64RemS => self.visit_i64_rem_s()?,
                Instr::I64RemU => self.visit_i64_rem_u()?,
                Instr::I64And => self.visit_i64_and(),
                Instr::I64Or => self.visit_i64_or(),
                Instr::I64Xor => self.visit_i64_xor(),
                Instr::I64Shl => self.visit_i64_shl(),
                Instr::I64ShrS => self.visit_i64_shr_s(),
                Instr::I64ShrU => self.visit_i64_shr_u(),
                Instr::I64Rotl => self.visit_i64_rotl(),
                Instr::I64Rotr => self.visit_i64_rotr(),
                Instr::F32Abs => self.visit_f32_abs(),
                Instr::F32Neg => self.visit_f32_neg(),
                Instr::F32Ceil => self.visit_f32_ceil(),
                Instr::F32Floor => self.visit_f32_floor(),
                Instr::F32Trunc => self.visit_f32_trunc(),
                Instr::F32Nearest => self.visit_f32_nearest(),
                Instr::F32Sqrt => self.visit_f32_sqrt(),
                Instr::F32Add => self.visit_f32_add(),
                Instr::F32Sub => self.visit_f32_sub(),
                Instr::F32Mul => self.visit_f32_mul(),
                Instr::F32Div => self.visit_f32_div(),
                Instr::F32Min => self.visit_f32_min(),
                Instr::F32Max => self.visit_f32_max(),
                Instr::F32Copysign => self.visit_f32_copysign(),
                Instr::F64Abs => self.visit_f64_abs(),
                Instr::F64Neg => self.visit_f64_neg(),
                Instr::F64Ceil => self.visit_f64_ceil(),
                Instr::F64Floor => self.visit_f64_floor(),
                Instr::F64Trunc => self.visit_f64_trunc(),
                Instr::F64Nearest => self.visit_f64_nearest(),
                Instr::F64Sqrt => self.visit_f64_sqrt(),
                Instr::F64Add => self.visit_f64_add(),
                Instr::F64Sub => self.visit_f64_sub(),
                Instr::F64Mul => self.visit_f64_mul(),
                Instr::F64Div => self.visit_f64_div(),
                Instr::F64Min => self.visit_f64_min(),
                Instr::F64Max => self.visit_f64_max(),
                Instr::F64Copysign => self.visit_f64_copysign(),
                Instr::I32WrapI64 => self.visit_i32_wrap_i64(),
                Instr::I32TruncF32S => self.visit_i32_trunc_f32_s()?,
                Instr::I32TruncF32U => self.visit_i32_trunc_f32_u()?,
                Instr::I32TruncF64S => self.visit_i32_trunc_f64_s()?,
                Instr::I32TruncF64U => self.visit_i32_trunc_f64_u()?,
                Instr::I64ExtendI32S => self.visit_i64_extend_i32_s(),
                Instr::I64ExtendI32U => self.visit_i64_extend_i32_u(),
                Instr::I64TruncF32S => self.visit_i64_trunc_f32_s()?,
                Instr::I64TruncF32U => self.visit_i64_trunc_f32_u()?,
                Instr::I64TruncF64S => self.visit_i64_trunc_f64_s()?,
                Instr::I64TruncF64U => self.visit_i64_trunc_f64_u()?,
                Instr::F32ConvertI32S => self.visit_f32_convert_i32_s(),
                Instr::F32ConvertI32U => self.visit_f32_convert_i32_u(),
                Instr::F32ConvertI64S => self.visit_f32_convert_i64_s(),
                Instr::F32ConvertI64U => self.visit_f32_convert_i64_u(),
                Instr::F32DemoteF64 => self.visit_f32_demote_f64(),
                Instr::F64ConvertI32S => self.visit_f64_convert_i32_s(),
                Instr::F64ConvertI32U => self.visit_f64_convert_i32_u(),
                Instr::F64ConvertI64S => self.visit_f64_convert_i64_s(),
                Instr::F64ConvertI64U => self.visit_f64_convert_i64_u(),
                Instr::F64PromoteF32 => self.visit_f64_promote_f32(),
                Instr::I32TruncSatF32S => self.visit_i32_trunc_sat_f32_s(),
                Instr::I32TruncSatF32U => self.visit_i32_trunc_sat_f32_u(),
                Instr::I32TruncSatF64S => self.visit_i32_trunc_sat_f64_s(),
                Instr::I32TruncSatF64U => self.visit_i32_trunc_sat_f64_u(),
                Instr::I64TruncSatF32S => self.visit_i64_trunc_sat_f32_s(),
                Instr::I64TruncSatF32U => self.visit_i64_trunc_sat_f32_u(),
                Instr::I64TruncSatF64S => self.visit_i64_trunc_sat_f64_s(),
                Instr::I64TruncSatF64U => self.visit_i64_trunc_sat_f64_u(),
                Instr::I32Extend8S => self.visit_i32_extend8_s(),
                Instr::I32Extend16S => self.visit_i32_extend16_s(),
                Instr::I64Extend8S => self.visit_i64_extend8_s(),
                Instr::I64Extend16S => self.visit_i64_extend16_s(),
                Instr::I64Extend32S => self.visit_i64_extend32_s(),
                _ => {
                    unimplemented!()
                }
            }
            trace_post_state_change!()
        }
    }

    /// Executes a generic Wasm `store[N_{s|u}]` operation.
    ///
    /// # Note
    ///
    /// This can be used to emulate the following Wasm operands:
    ///
    /// - `{i32, i64, f32, f64}.load`
    /// - `{i32, i64}.load8_s`
    /// - `{i32, i64}.load8_u`
    /// - `{i32, i64}.load16_s`
    /// - `{i32, i64}.load16_u`
    /// - `i64.load32_s`
    /// - `i64.load32_u`
    #[inline(always)]
    fn execute_load_extend(
        &mut self,
        offset: AddressOffset,
        load_extend: WasmLoadOp,
    ) -> Result<(), TrapCode> {
        self.sp.try_eval_top(|address| {
            let memory = self.cache.default_memory_bytes(self.ctx);
            let value = load_extend(memory, address, offset.into_inner())?;
            Ok(value)
        })?;
        self.try_next_instr()
    }

    /// Executes a generic Wasm `store[N]` operation.
    ///
    /// # Note
    ///
    /// This can be used to emulate the following Wasm operands:
    ///
    /// - `{i32, i64, f32, f64}.store`
    /// - `{i32, i64}.store8`
    /// - `{i32, i64}.store16`
    /// - `i64.store32`
    #[inline(always)]
    fn execute_store_wrap(
        &mut self,
        offset: AddressOffset,
        store_wrap: WasmStoreOp,
    ) -> Result<(), TrapCode> {
        let (address, value) = self.sp.pop2();
        let memory = self.cache.default_memory_bytes(self.ctx);
        store_wrap(memory, address, offset.into_inner(), value)?;
        self.try_next_instr()
    }

    /// Executes an infallible unary `wasmi` instruction.
    #[inline(always)]
    fn execute_unary(&mut self, f: fn(UntypedValue) -> UntypedValue) {
        self.sp.eval_top(f);
        self.next_instr()
    }

    /// Executes a fallible unary `wasmi` instruction.
    #[inline(always)]
    fn try_execute_unary(
        &mut self,
        f: fn(UntypedValue) -> Result<UntypedValue, TrapCode>,
    ) -> Result<(), TrapCode> {
        self.sp.try_eval_top(f)?;
        self.try_next_instr()
    }

    /// Executes an infallible binary `wasmi` instruction.
    #[inline(always)]
    fn execute_binary(&mut self, f: fn(UntypedValue, UntypedValue) -> UntypedValue) {
        self.sp.eval_top2(f);
        self.next_instr()
    }

    /// Executes a fallible binary `wasmi` instruction.
    #[inline(always)]
    fn try_execute_binary(
        &mut self,
        f: fn(UntypedValue, UntypedValue) -> Result<UntypedValue, TrapCode>,
    ) -> Result<(), TrapCode> {
        self.sp.try_eval_top2(f)?;
        self.try_next_instr()
    }

    /// Shifts the instruction pointer to the next instruction.
    #[inline(always)]
    fn next_instr(&mut self) {
        self.ip.add(1)
    }

    /// Shifts the instruction pointer to the next instruction.
    ///
    /// Has a parameter `skip` to denote how many instruction words
    /// to skip to reach the next actual instruction.
    ///
    /// # Note
    ///
    /// This is used by `wasmi` instructions that have a fixed
    /// encoding size of two instruction words such as [`Instruction::Br`].
    #[inline(always)]
    fn next_instr_at(&mut self, skip: usize) {
        self.ip.add(skip)
    }

    /// Shifts the instruction pointer to the next instruction and returns `Ok(())`.
    ///
    /// # Note
    ///
    /// This is a convenience function for fallible instructions.
    #[inline(always)]
    fn try_next_instr(&mut self) -> Result<(), TrapCode> {
        self.next_instr();
        Ok(())
    }

    /// Shifts the instruction pointer to the next instruction and returns `Ok(())`.
    ///
    /// Has a parameter `skip` to denote how many instruction words
    /// to skip to reach the next actual instruction.
    ///
    /// # Note
    ///
    /// This is a convenience function for fallible instructions.
    #[inline(always)]
    fn try_next_instr_at(&mut self, skip: usize) -> Result<(), TrapCode> {
        self.next_instr_at(skip);
        Ok(())
    }

    /// Branches and adjusts the value stack.
    ///
    /// # Note
    ///
    /// Offsets the instruction pointer using the given [`BranchOffset`] and
    /// adjusts the value stack using the [`DropKeep`].
    #[inline(always)]
    fn branch_to(&mut self, offset: BranchOffset) {
        self.ip.offset(offset.to_i32() as isize)
    }

    /// Branches and adjusts the value stack.
    ///
    /// # Note
    ///
    /// Offsets the instruction pointer using the given [`BranchOffset`] and
    /// adjusts the value stack using the [`DropKeep`].
    #[inline(always)]
    fn branch_to_and_adjust(&mut self, offset: BranchOffset, drop_keep: DropKeep) {
        self.sp.drop_keep(drop_keep);
        self.branch_to(offset)
    }

    /// Synchronizes the current stack pointer with the [`ValueStack`].
    ///
    /// # Note
    ///
    /// For performance reasons we detach the stack pointer form the [`ValueStack`].
    /// Therefore it is necessary to synchronize the [`ValueStack`] upon finishing
    /// execution of a sequence of non control flow instructions.
    #[inline(always)]
    fn sync_stack_ptr(&mut self) {
        self.value_stack.sync_stack_ptr(self.sp);
    }

    /// Calls the given [`Func`].
    ///
    /// This also prepares the instruction pointer and stack pointer for
    /// the function call so that the stack and execution state is synchronized
    /// with the outer structures.
    #[inline(always)]
    fn call_func(
        &mut self,
        skip: usize,
        func: &Func,
        kind: CallKind,
    ) -> Result<CallOutcome, TrapCode> {
        self.sync_stack_ptr();
        if matches!(kind, CallKind::Nested) {
            self.next_instr_at(skip);
            self.call_stack
                .push(FuncFrame::new(self.ip, self.cache.instance()))?;
        }
        match self.ctx.resolve_func(func) {
            FuncEntity::Wasm(wasm_func) => {
                let header = self.code_map.header(wasm_func.func_body());
                // trace the zero-writes produced from `prepare_wasm_call(..)`
                if let Some(tracer) = self.tracer.clone() {
                    let mut tracer = tracer.borrow_mut();
                    tracer
                        .execution_trace
                        .extend(self.trace_call(header.len_locals(), self.value_stack.stack_ptr));
                };
                self.value_stack.prepare_wasm_call(header)?;
                self.sp = self.value_stack.stack_ptr();
                self.cache.update_instance(wasm_func.instance());
                self.ip = self.code_map.instr_ptr(header.iref());
                Ok(CallOutcome::Continue)
            }
            FuncEntity::Host(_host_func) => {
                self.cache.reset();
                Ok(CallOutcome::Call {
                    host_func: *func,
                    instance: *self.cache.instance(),
                })
            }
        }
    }

    /// Calls the given internal [`CompiledFunc`].
    ///
    /// This also prepares the instruction pointer and stack pointer for
    /// the function call so that the stack and execution state is synchronized
    /// with the outer structures.
    #[inline(always)]
    fn call_func_internal(&mut self, func: CompiledFunc, kind: CallKind) -> Result<(), TrapCode> {
        self.sync_stack_ptr();
        if matches!(kind, CallKind::Nested) {
            self.next_instr_at(1);
            self.call_stack
                .push(FuncFrame::new(self.ip, self.cache.instance()))?;
        }
        let header = self.code_map.header(func);
        self.value_stack.prepare_wasm_call(header)?;
        self.sp = self.value_stack.stack_ptr();
        self.ip = self.code_map.instr_ptr(header.iref());
        Ok(())
    }

    /// Returns to the caller.
    ///
    /// This also modifies the stack as the caller would expect it
    /// and synchronizes the execution state with the outer structures.
    #[inline(always)]
    fn ret(&mut self, drop_keep: DropKeep) -> ReturnOutcome {
        self.sp.drop_keep(drop_keep);
        self.sync_stack_ptr();
        match self.call_stack.pop() {
            Some(caller) => {
                self.ip = caller.ip();
                self.cache.update_instance(caller.instance());
                ReturnOutcome::Wasm
            }
            None => ReturnOutcome::Host,
        }
    }

    /// Consume an amount of fuel specified by `delta` if `exec` succeeds.
    ///
    /// # Note
    ///
    /// - `delta` is only evaluated if fuel metering is enabled.
    /// - `exec` is only evaluated if the remaining fuel is sufficient
    ///    for amount of required fuel determined by `delta` or if
    ///    fuel metering is disabled.
    ///
    /// # Errors
    ///
    /// - If the [`StoreInner`] ran out of fuel.
    /// - If the `exec` closure traps.
    #[inline(always)]
    fn consume_fuel_with<T, E>(
        &mut self,
        delta: impl FnOnce(&FuelCosts) -> u64,
        exec: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<TrapCode>,
    {
        match self.get_fuel_consumption_mode() {
            None => exec(self),
            Some(mode) => self.consume_fuel_with_mode(mode, delta, exec),
        }
    }

    /// Consume an amount of fuel specified by `delta` and executes `exec`.
    ///
    /// The `mode` determines when and if the fuel determined by `delta` is charged.
    ///
    /// # Errors
    ///
    /// - If the [`StoreInner`] ran out of fuel.
    /// - If the `exec` closure traps.
    #[inline(always)]
    fn consume_fuel_with_mode<T, E>(
        &mut self,
        mode: FuelConsumptionMode,
        delta: impl FnOnce(&FuelCosts) -> u64,
        exec: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<TrapCode>,
    {
        let delta = delta(self.fuel_costs());
        match mode {
            FuelConsumptionMode::Lazy => self.consume_fuel_with_lazy(delta, exec),
            FuelConsumptionMode::Eager => self.consume_fuel_with_eager(delta, exec),
        }
    }

    /// Consume an amount of fuel specified by `delta` if `exec` succeeds.
    ///
    /// Prior to executing `exec` it is checked if enough fuel is remaining
    /// determined by `delta`. The fuel is charged only after `exec` has been
    /// finished successfully.
    ///
    /// # Errors
    ///
    /// - If the [`StoreInner`] ran out of fuel.
    /// - If the `exec` closure traps.
    #[inline(always)]
    fn consume_fuel_with_lazy<T, E>(
        &mut self,
        delta: u64,
        exec: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<TrapCode>,
    {
        self.ctx.fuel().sufficient_fuel(delta)?;
        let result = exec(self)?;
        self.ctx
            .fuel_mut()
            .consume_fuel(delta)
            .expect("remaining fuel has already been approved prior");
        Ok(result)
    }

    /// Consume an amount of fuel specified by `delta` and executes `exec`.
    ///
    /// # Errors
    ///
    /// - If the [`StoreInner`] ran out of fuel.
    /// - If the `exec` closure traps.
    #[inline(always)]
    fn consume_fuel_with_eager<T, E>(
        &mut self,
        delta: u64,
        exec: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<TrapCode>,
    {
        self.ctx.fuel_mut().consume_fuel(delta)?;
        exec(self)
    }

    /// Returns a shared reference to the [`FuelCosts`] of the [`Engine`].
    ///
    /// [`Engine`]: crate::Engine
    #[inline]
    fn fuel_costs(&self) -> &FuelCosts {
        self.ctx.engine().config().fuel_costs()
    }

    /// Returns the [`FuelConsumptionMode`] of the [`Engine`].
    ///
    /// [`Engine`]: crate::Engine
    #[inline]
    fn get_fuel_consumption_mode(&self) -> Option<FuelConsumptionMode> {
        self.ctx.engine().config().get_fuel_consumption_mode()
    }

    /// Executes a `call_indirect` or `return_call_indirect` instruction.
    #[inline(always)]
    fn execute_call_indirect(
        &mut self,
        skip: usize,
        table: TableIdx,
        func_index: u32,
        func_type: SignatureIdx,
        kind: CallKind,
    ) -> Result<CallOutcome, TrapCode> {
        let table = self.cache.get_table(self.ctx, table);
        let funcref = self
            .ctx
            .resolve_table(&table)
            .get_untyped(func_index)
            .map(FuncRef::from)
            .ok_or(TrapCode::TableOutOfBounds)?;
        let func = funcref.func().ok_or(TrapCode::IndirectCallToNull)?;
        let actual_signature = self.ctx.resolve_func(func).ty_dedup();
        let expected_signature = self
            .ctx
            .resolve_instance(self.cache.instance())
            .get_signature(func_type.to_u32())
            .unwrap_or_else(|| {
                panic!("missing signature for call_indirect at index: {func_type:?}")
            });
        if actual_signature != expected_signature {
            return Err(TrapCode::BadSignature);
        }
        self.call_func(skip, func, kind)
    }
}

impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    #[inline(always)]
    fn visit_unreachable(&mut self) -> Result<(), TrapCode> {
        Err(TrapCode::UnreachableCodeReached)
    }

    #[inline(always)]
    fn visit_consume_fuel(&mut self, block_fuel: BlockFuel) -> Result<(), TrapCode> {
        // We do not have to check if fuel metering is enabled since
        // these `wasmi` instructions are only generated if fuel metering
        // is enabled to begin with.
        self.ctx.fuel_mut().consume_fuel(block_fuel.to_u64())?;
        self.try_next_instr()
    }

    /// Fetches the [`DropKeep`] parameter for an instruction.
    ///
    /// # Note
    ///
    /// - This is done by encoding an [`Instruction::Return`] instruction
    ///   word following the actual instruction where the [`DropKeep`]
    ///   paremeter belongs to.
    /// - This is required for some instructions that do not fit into
    ///   a single instruction word and store a [`DropKeep`] value in
    ///   another instruction word.
    fn fetch_drop_keep(&self, offset: usize) -> DropKeep {
        let mut addr: InstructionPtr = self.ip;
        addr.add(offset);
        match addr.get(&self.code_map) {
            Instruction::Return(drop_keep) => *drop_keep,
            _ => unreachable!("expected Return instruction word at this point"),
        }
    }

    /// Fetches the [`TableIdx`] parameter for an instruction.
    ///
    /// # Note
    ///
    /// - This is done by encoding an [`Instruction::TableGet`] instruction
    ///   word following the actual instruction where the [`TableIdx`]
    ///   paremeter belongs to.
    /// - This is required for some instructions that do not fit into
    ///   a single instruction word and store a [`TableIdx`] value in
    ///   another instruction word.
    fn fetch_table_idx(&self, offset: usize) -> TableIdx {
        let mut addr: InstructionPtr = self.ip;
        addr.add(offset);
        match addr.get(&self.code_map) {
            Instruction::TableGet(table_idx) => *table_idx,
            _ => unreachable!("expected TableGet instruction word at this point"),
        }
    }

    #[inline(always)]
    fn visit_br(&mut self, offset: BranchOffset) {
        self.branch_to(offset)
    }

    #[inline(always)]
    fn visit_br_if_eqz(&mut self, offset: BranchOffset) {
        let condition = self.sp.pop_as();
        if condition {
            self.next_instr()
        } else {
            self.branch_to(offset)
        }
    }

    #[inline(always)]
    fn visit_br_if_nez(&mut self, offset: BranchOffset) {
        let condition = self.sp.pop_as();
        if condition {
            self.branch_to(offset)
        } else {
            self.next_instr()
        }
    }

    #[inline(always)]
    fn visit_br_adjust(&mut self, offset: BranchOffset) {
        let drop_keep = self.fetch_drop_keep(1);
        self.branch_to_and_adjust(offset, drop_keep)
    }

    #[inline(always)]
    fn visit_br_adjust_if_nez(&mut self, offset: BranchOffset) {
        let condition = self.sp.pop_as();
        if condition {
            let drop_keep = self.fetch_drop_keep(1);
            self.branch_to_and_adjust(offset, drop_keep)
        } else {
            self.next_instr_at(2)
        }
    }

    #[inline(always)]
    fn visit_return_if_nez(&mut self, drop_keep: DropKeep) -> ReturnOutcome {
        let condition = self.sp.pop_as();
        if condition {
            self.ret(drop_keep)
        } else {
            self.next_instr();
            ReturnOutcome::Wasm
        }
    }

    #[inline(always)]
    fn visit_br_table(&mut self, targets: BranchTableTargets) {
        let index: u32 = self.sp.pop_as();
        // The index of the default target which is the last target of the slice.
        let max_index = targets.to_usize() - 1;
        // A normalized index will always yield a target without panicking.
        let normalized_index = cmp::min(index as usize, max_index);
        // Update `pc`:
        self.ip.add(2 * normalized_index + 1);
    }

    #[inline(always)]
    fn visit_ret(&mut self, drop_keep: DropKeep) -> ReturnOutcome {
        self.ret(drop_keep)
    }

    #[inline(always)]
    fn visit_local_get(&mut self, local_depth: LocalDepth) {
        let value = self.sp.nth_back(local_depth.to_usize());
        self.sp.push(value);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_local_set(&mut self, local_depth: LocalDepth) {
        let new_value = self.sp.pop();
        self.sp.set_nth_back(local_depth.to_usize(), new_value);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_local_tee(&mut self, local_depth: LocalDepth) {
        let new_value = self.sp.last();
        self.sp.set_nth_back(local_depth.to_usize(), new_value);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_global_get(&mut self, global_index: GlobalIdx) {
        let global_value = self.cache.get_global(self.ctx, global_index);
        self.sp.push(global_value);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_global_set(&mut self, global_index: GlobalIdx) {
        let new_value = self.sp.pop();
        self.cache.set_global(self.ctx, global_index, new_value);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_return_call_internal(&mut self, compiled_func: CompiledFunc) -> Result<(), TrapCode> {
        let drop_keep = self.fetch_drop_keep(1);
        self.sp.drop_keep(drop_keep);
        self.call_func_internal(compiled_func, CallKind::Tail)
    }

    #[inline(always)]
    fn visit_return_call(&mut self, func_index: FuncIdx) -> Result<CallOutcome, TrapCode> {
        let drop_keep = self.fetch_drop_keep(1);
        self.sp.drop_keep(drop_keep);
        let callee = self.cache.get_func(self.ctx, func_index);
        self.call_func(2, &callee, CallKind::Tail)
    }

    #[inline(always)]
    fn visit_return_call_indirect(
        &mut self,
        func_type: SignatureIdx,
    ) -> Result<CallOutcome, TrapCode> {
        let drop_keep = self.fetch_drop_keep(1);
        let table = self.fetch_table_idx(2);
        let func_index: u32 = self.sp.pop_as();
        self.sp.drop_keep(drop_keep);
        self.execute_call_indirect(3, table, func_index, func_type, CallKind::Tail)
    }

    #[inline(always)]
    fn visit_call_internal(&mut self, compiled_func: CompiledFunc) -> Result<(), TrapCode> {
        self.call_func_internal(compiled_func, CallKind::Nested)
    }

    #[inline(always)]
    fn visit_call(&mut self, func_index: FuncIdx) -> Result<CallOutcome, TrapCode> {
        let callee = self.cache.get_func(self.ctx, func_index);
        self.call_func(1, &callee, CallKind::Nested)
    }

    #[inline(always)]
    fn visit_call_indirect(&mut self, func_type: SignatureIdx) -> Result<CallOutcome, TrapCode> {
        let table = self.fetch_table_idx(1);
        let func_index: u32 = self.sp.pop_as();
        self.execute_call_indirect(2, table, func_index, func_type, CallKind::Nested)
    }

    #[inline(always)]
    fn visit_const_32(&mut self, bytes: [u8; 4]) {
        let bytes = u32::from_ne_bytes(bytes);
        self.sp.push(UntypedValue::from(bytes));
        self.next_instr()
    }

    #[inline(always)]
    fn visit_i64_const_32(&mut self, value: i32) {
        let sign_extended = i64::from(value);
        self.sp.push(UntypedValue::from(sign_extended));
        self.next_instr()
    }

    #[inline(always)]
    fn visit_f64_const_32(&mut self, value: F64Const32) {
        let promoted = value.to_f64();
        self.sp.push(UntypedValue::from(promoted));
        self.next_instr()
    }

    #[inline(always)]
    fn visit_const(&mut self, cref: ConstRef) {
        let value = self
            .const_pool
            .get(cref)
            .unwrap_or_else(|| unreachable!("missing constant value for const reference"));
        self.sp.push(value);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_drop(&mut self) {
        self.sp.drop();
        self.next_instr()
    }

    #[inline(always)]
    fn visit_select(&mut self) {
        self.sp.eval_top3(|e1, e2, e3| {
            let condition = <bool as From<UntypedValue>>::from(e3);
            if condition {
                e1
            } else {
                e2
            }
        });
        self.next_instr()
    }

    #[inline(always)]
    fn visit_memory_size(&mut self) {
        let memory = self.cache.default_memory(self.ctx);
        let result: u32 = self.ctx.resolve_memory(memory).current_pages().into();
        self.sp.push_as(result);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_memory_grow(
        &mut self,
        resource_limiter: &mut ResourceLimiterRef<'ctx>,
    ) -> Result<(), TrapCode> {
        let delta: u32 = self.sp.pop_as();
        let delta = match Pages::new(delta) {
            Some(pages) => pages,
            None => {
                // Cannot grow memory so we push the expected error value.
                self.sp.push_as(INVALID_GROWTH_ERRCODE);
                return self.try_next_instr();
            }
        };
        let result = self.consume_fuel_with(
            |costs| {
                let delta_in_bytes = delta.to_bytes().unwrap_or(0) as u64;
                costs.fuel_for_bytes(delta_in_bytes)
            },
            |this| {
                let memory = this.cache.default_memory(this.ctx);
                let new_pages = this
                    .ctx
                    .resolve_memory_mut(memory)
                    .grow(delta, resource_limiter)
                    .map(u32::from)?;
                // The `memory.grow` operation might have invalidated the cached
                // linear memory so we need to reset it in order for the cache to
                // reload in case it is used again.
                this.cache.reset_default_memory_bytes();
                Ok(new_pages)
            },
        );
        let result = match result {
            Ok(result) => result,
            Err(EntityGrowError::InvalidGrow) => INVALID_GROWTH_ERRCODE,
            Err(EntityGrowError::TrapCode(trap_code)) => return Err(trap_code),
        };
        self.sp.push_as(result);
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_memory_fill(&mut self) -> Result<(), TrapCode> {
        // The `n`, `val` and `d` variable bindings are extracted from the Wasm specification.
        let (d, val, n) = self.sp.pop3();
        let n = i32::from(n) as usize;
        let offset = i32::from(d) as usize;
        let byte = u8::from(val);
        self.consume_fuel_with(
            |costs| costs.fuel_for_bytes(n as u64),
            |this| {
                let memory = this
                    .cache
                    .default_memory_bytes(this.ctx)
                    .get_mut(offset..)
                    .and_then(|memory| memory.get_mut(..n))
                    .ok_or(TrapCode::MemoryOutOfBounds)?;
                memory.fill(byte);
                Ok(())
            },
        )?;
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_memory_copy(&mut self) -> Result<(), TrapCode> {
        // The `n`, `s` and `d` variable bindings are extracted from the Wasm specification.
        let (d, s, n) = self.sp.pop3();
        let n = i32::from(n) as usize;
        let src_offset = i32::from(s) as usize;
        let dst_offset = i32::from(d) as usize;
        self.consume_fuel_with(
            |costs| costs.fuel_for_bytes(n as u64),
            |this| {
                let data = this.cache.default_memory_bytes(this.ctx);
                // These accesses just perform the bounds checks required by the Wasm spec.
                data.get(src_offset..)
                    .and_then(|memory| memory.get(..n))
                    .ok_or(TrapCode::MemoryOutOfBounds)?;
                data.get(dst_offset..)
                    .and_then(|memory| memory.get(..n))
                    .ok_or(TrapCode::MemoryOutOfBounds)?;
                data.copy_within(src_offset..src_offset.wrapping_add(n), dst_offset);
                Ok(())
            },
        )?;
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_memory_init(&mut self, segment: DataSegmentIdx) -> Result<(), TrapCode> {
        // The `n`, `s` and `d` variable bindings are extracted from the Wasm specification.
        let (d, s, n) = self.sp.pop3();
        let n = i32::from(n) as usize;
        let src_offset = i32::from(s) as usize;
        let dst_offset = i32::from(d) as usize;
        self.consume_fuel_with(
            |costs| costs.fuel_for_bytes(n as u64),
            |this| {
                let (memory, data) = this
                    .cache
                    .get_default_memory_and_data_segment(this.ctx, segment);
                let memory = memory
                    .get_mut(dst_offset..)
                    .and_then(|memory| memory.get_mut(..n))
                    .ok_or(TrapCode::MemoryOutOfBounds)?;
                let data = data
                    .get(src_offset..)
                    .and_then(|data| data.get(..n))
                    .ok_or(TrapCode::MemoryOutOfBounds)?;
                memory.copy_from_slice(data);
                Ok(())
            },
        )?;
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_data_drop(&mut self, segment_index: DataSegmentIdx) {
        let segment = self
            .cache
            .get_data_segment(self.ctx, segment_index.to_u32());
        self.ctx.resolve_data_segment_mut(&segment).drop_bytes();
        self.next_instr();
    }

    #[inline(always)]
    fn visit_table_size(&mut self, table_index: TableIdx) {
        let table = self.cache.get_table(self.ctx, table_index);
        let size = self.ctx.resolve_table(&table).size();
        self.sp.push_as(size);
        self.next_instr()
    }

    #[inline(always)]
    fn visit_table_grow(
        &mut self,
        table_index: TableIdx,
        resource_limiter: &mut ResourceLimiterRef<'ctx>,
    ) -> Result<(), TrapCode> {
        let (init, delta) = self.sp.pop2();
        let delta: u32 = delta.into();
        let result = self.consume_fuel_with(
            |costs| costs.fuel_for_elements(u64::from(delta)),
            |this| {
                let table = this.cache.get_table(this.ctx, table_index);
                this.ctx
                    .resolve_table_mut(&table)
                    .grow_untyped(delta, init, resource_limiter)
            },
        );
        let result = match result {
            Ok(result) => result,
            Err(EntityGrowError::InvalidGrow) => INVALID_GROWTH_ERRCODE,
            Err(EntityGrowError::TrapCode(trap_code)) => return Err(trap_code),
        };
        self.sp.push_as(result);
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_table_fill(&mut self, table_index: TableIdx) -> Result<(), TrapCode> {
        // The `n`, `s` and `d` variable bindings are extracted from the Wasm specification.
        let (i, val, n) = self.sp.pop3();
        let dst: u32 = i.into();
        let len: u32 = n.into();
        self.consume_fuel_with(
            |costs| costs.fuel_for_elements(u64::from(len)),
            |this| {
                let table = this.cache.get_table(this.ctx, table_index);
                this.ctx
                    .resolve_table_mut(&table)
                    .fill_untyped(dst, val, len)?;
                Ok(())
            },
        )?;
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_table_get(&mut self, table_index: TableIdx) -> Result<(), TrapCode> {
        self.sp.try_eval_top(|index| {
            let index: u32 = index.into();
            let table = self.cache.get_table(self.ctx, table_index);
            self.ctx
                .resolve_table(&table)
                .get_untyped(index)
                .ok_or(TrapCode::TableOutOfBounds)
        })?;
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_table_set(&mut self, table_index: TableIdx) -> Result<(), TrapCode> {
        let (index, value) = self.sp.pop2();
        let index: u32 = index.into();
        let table = self.cache.get_table(self.ctx, table_index);
        self.ctx
            .resolve_table_mut(&table)
            .set_untyped(index, value)
            .map_err(|_| TrapCode::TableOutOfBounds)?;
        self.try_next_instr()
    }

    #[inline(always)]
    fn visit_table_copy(&mut self, dst: TableIdx) -> Result<(), TrapCode> {
        let src = self.fetch_table_idx(1);
        // The `n`, `s` and `d` variable bindings are extracted from the Wasm specification.
        let (d, s, n) = self.sp.pop3();
        let len = u32::from(n);
        let src_index = u32::from(s);
        let dst_index = u32::from(d);
        self.consume_fuel_with(
            |costs| costs.fuel_for_elements(u64::from(len)),
            |this| {
                // Query both tables and check if they are the same:
                let dst = this.cache.get_table(this.ctx, dst);
                let src = this.cache.get_table(this.ctx, src);
                if Table::eq(&dst, &src) {
                    // Copy within the same table:
                    let table = this.ctx.resolve_table_mut(&dst);
                    table.copy_within(dst_index, src_index, len)?;
                } else {
                    // Copy from one table to another table:
                    let (dst, src) = this.ctx.resolve_table_pair_mut(&dst, &src);
                    TableEntity::copy(dst, dst_index, src, src_index, len)?;
                }
                Ok(())
            },
        )?;
        self.try_next_instr_at(2)
    }

    #[inline(always)]
    fn visit_table_init(&mut self, elem: ElementSegmentIdx) -> Result<(), TrapCode> {
        let table = self.fetch_table_idx(1);
        // The `n`, `s` and `d` variable bindings are extracted from the Wasm specification.
        let (d, s, n) = self.sp.pop3();
        let len = u32::from(n);
        let src_index = u32::from(s);
        let dst_index = u32::from(d);
        self.consume_fuel_with(
            |costs| costs.fuel_for_elements(u64::from(len)),
            |this| {
                let (instance, table, element) = this
                    .cache
                    .get_table_and_element_segment(this.ctx, table, elem);
                table.init(dst_index, element, src_index, len, |func_index| {
                    instance
                        .get_func(func_index)
                        .unwrap_or_else(|| panic!("missing function at index {func_index}"))
                })?;
                Ok(())
            },
        )?;
        self.try_next_instr_at(2)
    }

    #[inline(always)]
    fn visit_element_drop(&mut self, segment_index: ElementSegmentIdx) {
        let segment = self.cache.get_element_segment(self.ctx, segment_index);
        self.ctx.resolve_element_segment_mut(&segment).drop_items();
        self.next_instr();
    }

    #[inline(always)]
    fn visit_ref_func(&mut self, func_index: FuncIdx) {
        let func = self.cache.get_func(self.ctx, func_index);
        let funcref = FuncRef::new(func);
        self.sp.push_as(funcref);
        self.next_instr();
    }
}

macro_rules! impl_visit_load {
  ( $( fn $visit_ident:ident($untyped_ident:ident); )* ) => {
      $(
          #[inline(always)]
          fn $visit_ident(
              &mut self,
              offset: AddressOffset,
          ) -> Result<(), TrapCode> {
              self.execute_load_extend(offset, UntypedValue::$untyped_ident)
          }
      )*
  }
}
impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    impl_visit_load! {
        fn visit_i32_load(i32_load);
        fn visit_i64_load(i64_load);
        fn visit_f32_load(f32_load);
        fn visit_f64_load(f64_load);

        fn visit_i32_load_i8_s(i32_load8_s);
        fn visit_i32_load_i8_u(i32_load8_u);
        fn visit_i32_load_i16_s(i32_load16_s);
        fn visit_i32_load_i16_u(i32_load16_u);

        fn visit_i64_load_i8_s(i64_load8_s);
        fn visit_i64_load_i8_u(i64_load8_u);
        fn visit_i64_load_i16_s(i64_load16_s);
        fn visit_i64_load_i16_u(i64_load16_u);
        fn visit_i64_load_i32_s(i64_load32_s);
        fn visit_i64_load_i32_u(i64_load32_u);
    }
}

macro_rules! impl_visit_store {
  ( $( fn $visit_ident:ident($untyped_ident:ident); )* ) => {
      $(
          #[inline(always)]
          fn $visit_ident(
              &mut self,
              offset: AddressOffset,
          ) -> Result<(), TrapCode> {
              self.execute_store_wrap(offset, UntypedValue::$untyped_ident)
          }
      )*
  }
}
impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    impl_visit_store! {
        fn visit_i32_store(i32_store);
        fn visit_i64_store(i64_store);
        fn visit_f32_store(f32_store);
        fn visit_f64_store(f64_store);

        fn visit_i32_store_8(i32_store8);
        fn visit_i32_store_16(i32_store16);

        fn visit_i64_store_8(i64_store8);
        fn visit_i64_store_16(i64_store16);
        fn visit_i64_store_32(i64_store32);
    }
}

macro_rules! impl_visit_unary {
  ( $( fn $visit_ident:ident($untyped_ident:ident); )* ) => {
      $(
          #[inline(always)]
          fn $visit_ident(&mut self) {
              self.execute_unary(UntypedValue::$untyped_ident)
          }
      )*
  }
}
impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    impl_visit_unary! {
        fn visit_i32_eqz(i32_eqz);
        fn visit_i64_eqz(i64_eqz);

        fn visit_i32_clz(i32_clz);
        fn visit_i32_ctz(i32_ctz);
        fn visit_i32_popcnt(i32_popcnt);

        fn visit_i64_clz(i64_clz);
        fn visit_i64_ctz(i64_ctz);
        fn visit_i64_popcnt(i64_popcnt);

        fn visit_f32_abs(f32_abs);
        fn visit_f32_neg(f32_neg);
        fn visit_f32_ceil(f32_ceil);
        fn visit_f32_floor(f32_floor);
        fn visit_f32_trunc(f32_trunc);
        fn visit_f32_nearest(f32_nearest);
        fn visit_f32_sqrt(f32_sqrt);

        fn visit_f64_abs(f64_abs);
        fn visit_f64_neg(f64_neg);
        fn visit_f64_ceil(f64_ceil);
        fn visit_f64_floor(f64_floor);
        fn visit_f64_trunc(f64_trunc);
        fn visit_f64_nearest(f64_nearest);
        fn visit_f64_sqrt(f64_sqrt);

        fn visit_i32_wrap_i64(i32_wrap_i64);
        fn visit_i64_extend_i32_s(i64_extend_i32_s);
        fn visit_i64_extend_i32_u(i64_extend_i32_u);

        fn visit_f32_convert_i32_s(f32_convert_i32_s);
        fn visit_f32_convert_i32_u(f32_convert_i32_u);
        fn visit_f32_convert_i64_s(f32_convert_i64_s);
        fn visit_f32_convert_i64_u(f32_convert_i64_u);
        fn visit_f32_demote_f64(f32_demote_f64);
        fn visit_f64_convert_i32_s(f64_convert_i32_s);
        fn visit_f64_convert_i32_u(f64_convert_i32_u);
        fn visit_f64_convert_i64_s(f64_convert_i64_s);
        fn visit_f64_convert_i64_u(f64_convert_i64_u);
        fn visit_f64_promote_f32(f64_promote_f32);

        fn visit_i32_extend8_s(i32_extend8_s);
        fn visit_i32_extend16_s(i32_extend16_s);
        fn visit_i64_extend8_s(i64_extend8_s);
        fn visit_i64_extend16_s(i64_extend16_s);
        fn visit_i64_extend32_s(i64_extend32_s);

        fn visit_i32_trunc_sat_f32_s(i32_trunc_sat_f32_s);
        fn visit_i32_trunc_sat_f32_u(i32_trunc_sat_f32_u);
        fn visit_i32_trunc_sat_f64_s(i32_trunc_sat_f64_s);
        fn visit_i32_trunc_sat_f64_u(i32_trunc_sat_f64_u);
        fn visit_i64_trunc_sat_f32_s(i64_trunc_sat_f32_s);
        fn visit_i64_trunc_sat_f32_u(i64_trunc_sat_f32_u);
        fn visit_i64_trunc_sat_f64_s(i64_trunc_sat_f64_s);
        fn visit_i64_trunc_sat_f64_u(i64_trunc_sat_f64_u);
    }
}

macro_rules! impl_visit_fallible_unary {
  ( $( fn $visit_ident:ident($untyped_ident:ident); )* ) => {
      $(
          #[inline(always)]
          fn $visit_ident(&mut self) -> Result<(), TrapCode> {
              self.try_execute_unary(UntypedValue::$untyped_ident)
          }
      )*
  }
}
impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    impl_visit_fallible_unary! {
        fn visit_i32_trunc_f32_s(i32_trunc_f32_s);
        fn visit_i32_trunc_f32_u(i32_trunc_f32_u);
        fn visit_i32_trunc_f64_s(i32_trunc_f64_s);
        fn visit_i32_trunc_f64_u(i32_trunc_f64_u);

        fn visit_i64_trunc_f32_s(i64_trunc_f32_s);
        fn visit_i64_trunc_f32_u(i64_trunc_f32_u);
        fn visit_i64_trunc_f64_s(i64_trunc_f64_s);
        fn visit_i64_trunc_f64_u(i64_trunc_f64_u);
    }
}

macro_rules! impl_visit_binary {
  ( $( fn $visit_ident:ident($untyped_ident:ident); )* ) => {
      $(
          #[inline(always)]
          fn $visit_ident(&mut self) {
              self.execute_binary(UntypedValue::$untyped_ident)
          }
      )*
  }
}
impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    impl_visit_binary! {
        fn visit_i32_eq(i32_eq);
        fn visit_i32_ne(i32_ne);
        fn visit_i32_lt_s(i32_lt_s);
        fn visit_i32_lt_u(i32_lt_u);
        fn visit_i32_gt_s(i32_gt_s);
        fn visit_i32_gt_u(i32_gt_u);
        fn visit_i32_le_s(i32_le_s);
        fn visit_i32_le_u(i32_le_u);
        fn visit_i32_ge_s(i32_ge_s);
        fn visit_i32_ge_u(i32_ge_u);

        fn visit_i64_eq(i64_eq);
        fn visit_i64_ne(i64_ne);
        fn visit_i64_lt_s(i64_lt_s);
        fn visit_i64_lt_u(i64_lt_u);
        fn visit_i64_gt_s(i64_gt_s);
        fn visit_i64_gt_u(i64_gt_u);
        fn visit_i64_le_s(i64_le_s);
        fn visit_i64_le_u(i64_le_u);
        fn visit_i64_ge_s(i64_ge_s);
        fn visit_i64_ge_u(i64_ge_u);

        fn visit_f32_eq(f32_eq);
        fn visit_f32_ne(f32_ne);
        fn visit_f32_lt(f32_lt);
        fn visit_f32_gt(f32_gt);
        fn visit_f32_le(f32_le);
        fn visit_f32_ge(f32_ge);

        fn visit_f64_eq(f64_eq);
        fn visit_f64_ne(f64_ne);
        fn visit_f64_lt(f64_lt);
        fn visit_f64_gt(f64_gt);
        fn visit_f64_le(f64_le);
        fn visit_f64_ge(f64_ge);

        fn visit_i32_add(i32_add);
        fn visit_i32_sub(i32_sub);
        fn visit_i32_mul(i32_mul);
        fn visit_i32_and(i32_and);
        fn visit_i32_or(i32_or);
        fn visit_i32_xor(i32_xor);
        fn visit_i32_shl(i32_shl);
        fn visit_i32_shr_s(i32_shr_s);
        fn visit_i32_shr_u(i32_shr_u);
        fn visit_i32_rotl(i32_rotl);
        fn visit_i32_rotr(i32_rotr);

        fn visit_i64_add(i64_add);
        fn visit_i64_sub(i64_sub);
        fn visit_i64_mul(i64_mul);
        fn visit_i64_and(i64_and);
        fn visit_i64_or(i64_or);
        fn visit_i64_xor(i64_xor);
        fn visit_i64_shl(i64_shl);
        fn visit_i64_shr_s(i64_shr_s);
        fn visit_i64_shr_u(i64_shr_u);
        fn visit_i64_rotl(i64_rotl);
        fn visit_i64_rotr(i64_rotr);

        fn visit_f32_add(f32_add);
        fn visit_f32_sub(f32_sub);
        fn visit_f32_mul(f32_mul);
        fn visit_f32_div(f32_div);
        fn visit_f32_min(f32_min);
        fn visit_f32_max(f32_max);
        fn visit_f32_copysign(f32_copysign);

        fn visit_f64_add(f64_add);
        fn visit_f64_sub(f64_sub);
        fn visit_f64_mul(f64_mul);
        fn visit_f64_div(f64_div);
        fn visit_f64_min(f64_min);
        fn visit_f64_max(f64_max);
        fn visit_f64_copysign(f64_copysign);
    }
}

macro_rules! impl_visit_fallible_binary {
  ( $( fn $visit_ident:ident($untyped_ident:ident); )* ) => {
      $(
          #[inline(always)]
          fn $visit_ident(&mut self) -> Result<(), TrapCode> {
              self.try_execute_binary(UntypedValue::$untyped_ident)
          }
      )*
  }
}
impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    impl_visit_fallible_binary! {
        fn visit_i32_div_s(i32_div_s);
        fn visit_i32_div_u(i32_div_u);
        fn visit_i32_rem_s(i32_rem_s);
        fn visit_i32_rem_u(i32_rem_u);

        fn visit_i64_div_s(i64_div_s);
        fn visit_i64_div_u(i64_div_u);
        fn visit_i64_rem_s(i64_rem_s);
        fn visit_i64_rem_u(i64_rem_u);
    }
}

impl<'ctx, 'engine> Executor<'ctx, 'engine> {
    /// Used to capture necessary values before state change
    fn execute_instr_pre(&self, pre_sp: usize, pc: usize) -> WitnessVM {
        use Instruction as Instr;
        let mut vm = WitnessVM::default();
        let instruction = &{*self.ip.get(&self.code_map)};
        vm.pre_sp = pre_sp;
        vm.pc = pc;

        // Keep these as post values incase we use virtual instructions that don't have a post trace
        //
        // # Note
        //
        // - Post trace will correctly capture the post values
        vm.post_sp = pre_sp;
        vm.post_pc = pc;

        vm.instr = *instruction;
        vm.J = instruction.index_j();
        match *instruction {
            Instr::LocalGet(..) => {}
            Instr::LocalSet(depth) | Instr::LocalTee(depth) => {
                vm.I = depth.to_usize() as u64;
                vm.Y = self.sp.last().to_bits();
            }
            Instr::Const32(..)
            | Instr::ConstRef(..)
            | Instr::I64Const32(..)
            | Instr::F64Const32(..) => {}
            Instr::BrIfEqz(branch_offset) => {
                vm.Y = self.sp.nth_back(1).to_bits(); // condition value
                vm.I = branch_offset.to_i32() as u32 as u64;
            }
            Instr::BrIfNez(branch_offset) => {
                vm.Y = self.sp.nth_back(1).to_bits(); // condition value
                vm.I = branch_offset.to_i32() as u32 as u64;
            }
            Instr::Br(branch_offset) => {
                vm.I = branch_offset.to_i32() as u32 as u64;
            }
            Instr::I64Add
            | Instr::I64Mul
            | Instr::I64And
            | Instr::I64Or
            | Instr::I64Xor
            | Instr::I64Sub
            | Instr::I64Shl
            | Instr::I64Rotl
            | Instr::I64Rotr
            | Instr::I64ShrU
            | Instr::I64DivS
            | Instr::I64DivU
            | Instr::I64RemS
            | Instr::I64RemU
            | Instr::I64ShrS => {
                vm.X = self.sp.nth_back(2).to_bits();
                vm.Y = self.sp.nth_back(1).to_bits();
            }
            Instr::Return(dk) => {
                vm.I = dk.drop() as u64;
            }
            Instr::CallInternal(..) => {}

            Instr::Drop => {}
            Instr::I32Store(offset)
            | Instr::I32Store8(offset)
            | Instr::I32Store16(offset)
            | Instr::F32Store(offset)
            | Instr::F64Store(offset)
            | Instr::I64Store(offset)
            | Instr::I64Store8(offset)
            | Instr::I64Store16(offset)
            | Instr::I64Store32(offset) => {
                let raw_address: u64 = self.sp.nth_back(2).to_bits();
                let value: u64 = self.sp.nth_back(1).to_bits();
                vm.X = raw_address;
                vm.Y = value;

                let offset = offset.into_inner();
                let effective_address = effective_address(raw_address as u32, offset).unwrap();
                vm.I = effective_address as u64;
            }
            Instr::I32Load(offset)
            | Instr::I32Load8U(offset)
            | Instr::I32Load8S(offset)
            | Instr::I32Load16U(offset)
            | Instr::I32Load16S(offset)
            | Instr::F32Load(offset)
            | Instr::F64Load(offset)
            | Instr::I64Load(offset)
            | Instr::I64Load8S(offset)
            | Instr::I64Load8U(offset)
            | Instr::I64Load16S(offset)
            | Instr::I64Load16U(offset)
            | Instr::I64Load32S(offset)
            | Instr::I64Load32U(offset) => {
                let raw_address_u64 = self.sp.last().to_bits();
                let raw_address = raw_address_u64 as u32;
                let offset = offset.into_inner();
                let effective_address = effective_address(offset, raw_address).unwrap();

                vm.I = effective_address as u64;
                vm.Y = raw_address_u64;
            }
            Instr::I64Clz | Instr::I64Ctz | Instr::I64Popcnt | Instr::I64Eqz | Instr::I32Eqz => {
                vm.Y = self.sp.nth_back(1).to_bits();
            }

            // visit_unary
            // TODO: We are going to use jolt techniques to handle these
            Instr::F32Abs
            | Instr::F32Neg
            | Instr::F32Ceil
            | Instr::F32Floor
            | Instr::F32Trunc
            | Instr::F32Nearest
            | Instr::F32Sqrt
            | Instr::F64Abs
            | Instr::F64Neg
            | Instr::F64Ceil
            | Instr::F64Floor
            | Instr::F64Trunc
            | Instr::F64Nearest
            | Instr::F64Sqrt
            | Instr::I32WrapI64
            | Instr::I32TruncF32S
            | Instr::I32TruncF32U
            | Instr::I32TruncF64S
            | Instr::I32TruncF64U
            | Instr::I64ExtendI32S
            | Instr::I64ExtendI32U
            | Instr::I64TruncF32S
            | Instr::I64TruncF32U
            | Instr::I64TruncF64S
            | Instr::I64TruncF64U
            | Instr::F32ConvertI32S
            | Instr::F32ConvertI32U
            | Instr::F32ConvertI64S
            | Instr::F32ConvertI64U
            | Instr::F32DemoteF64
            | Instr::F64ConvertI32S
            | Instr::F64ConvertI32U
            | Instr::F64ConvertI64S
            | Instr::F64ConvertI64U
            | Instr::F64PromoteF32
            | Instr::I32Extend8S
            | Instr::I32Extend16S
            | Instr::I64Extend8S
            | Instr::I64Extend16S
            | Instr::I64Extend32S
            | Instr::I32TruncSatF32S
            | Instr::I32TruncSatF32U
            | Instr::I32TruncSatF64S
            | Instr::I32TruncSatF64U
            | Instr::I64TruncSatF32S
            | Instr::I64TruncSatF32U
            | Instr::I64TruncSatF64S
            | Instr::I64TruncSatF64U
            | Instr::I32Clz
            | Instr::I32Ctz
            | Instr::I32Popcnt => {
                vm.Y = self.sp.last().to_bits();
            }

            // visit_binary
            // TODO: We are going to use jolt techniques to handle these
            Instr::F32Eq
            | Instr::F32Ne
            | Instr::F32Lt
            | Instr::F32Gt
            | Instr::F32Le
            | Instr::F32Ge
            | Instr::F64Eq
            | Instr::F64Ne
            | Instr::F64Lt
            | Instr::F64Gt
            | Instr::F64Le
            | Instr::F64Ge
            | Instr::F32Add
            | Instr::F32Sub
            | Instr::F32Mul
            | Instr::F32Div
            | Instr::F32Min
            | Instr::F32Max
            | Instr::F32Copysign
            | Instr::F64Add
            | Instr::F64Sub
            | Instr::F64Mul
            | Instr::F64Div
            | Instr::F64Min
            | Instr::F64Max
            | Instr::F64Copysign
            | Instr::I64Eq
            | Instr::I64Ne
            | Instr::I64LtS
            | Instr::I64LtU
            | Instr::I64GtS
            | Instr::I64GtU
            | Instr::I64LeS
            | Instr::I64LeU
            | Instr::I64GeS
            | Instr::I64GeU
            | Instr::I32Eq
            | Instr::I32Ne
            | Instr::I32LtS
            | Instr::I32LtU
            | Instr::I32GtS
            | Instr::I32GtU
            | Instr::I32LeS
            | Instr::I32LeU
            | Instr::I32GeS
            | Instr::I32GeU
            | Instr::I32Add
            | Instr::I32Sub
            | Instr::I32Mul
            | Instr::I32DivS
            | Instr::I32DivU
            | Instr::I32RemS
            | Instr::I32RemU
            | Instr::I32And
            | Instr::I32Or
            | Instr::I32Xor
            | Instr::I32Shl
            | Instr::I32ShrS
            | Instr::I32ShrU
            | Instr::I32Rotl
            | Instr::I32Rotr => {
                vm.X = self.sp.nth_back(2).to_bits();
                vm.Y = self.sp.nth_back(1).to_bits();
            }
            Instr::Select => {
                vm.X = self.sp.nth_back(3).to_bits();
                vm.Y = self.sp.nth_back(2).to_bits();
                vm.I = self.sp.nth_back(1).to_bits();
            }
            Instr::GlobalGet(idx) => {
                vm.I = idx.to_u32() as u64;
            }
            Instr::GlobalSet(idx) => {
                let value = self.sp.last().to_bits();
                vm.I = idx.to_u32() as u64;
                vm.Y = value;
            }
            Instr::BrTable(..) => {}
            Instr::BrAdjust(offset) => {
                let drop_keep = self.fetch_drop_keep(1);
                vm.P = drop_keep.drop() as u64;
                vm.I = offset.to_i32() as u32 as u64;
            }
            Instr::MemoryCopy => {
                let num_bytes_to_copy = self.sp.nth_back(1).to_bits();
                let src = self.sp.nth_back(2).to_bits();
                let destination = self.sp.nth_back(3).to_bits();
                vm.I = num_bytes_to_copy;
                vm.Y = src;
                vm.X = destination;
            }
            Instr::MemoryFill => {
                let size = self.sp.nth_back(1).to_bits();
                let value = self.sp.nth_back(2).to_bits();
                let offset = self.sp.nth_back(3).to_bits();
                vm.I = size;
                vm.Y = value;
                vm.X = offset;
            }
            Instr::Call(..) => {}
            Instr::CallIndirect(..) => {}
            Instr::MemorySize => {}
            Instr::MemoryGrow => {
                vm.Y = self.sp.last().to_bits();
            }
            _ => {
                println!("Instruction not supported: {:?}", instruction);
                unimplemented!();
            }
        }
        vm
    }

    /// Trace the affected values in the VM state change post instruction
    /// execution
    fn execute_instr_post(&mut self, vm: &mut WitnessVM, instr: &Instruction) {
        use Instruction as Instr;
        vm.post_pc = self.pc();
        vm.post_sp = self.sp();

        match *instr {
            Instr::Const32(..)
            | Instr::ConstRef(..)
            | Instr::I64Const32(..)
            | Instr::F64Const32(..) => {
                vm.I = self.sp.last().to_bits();
                vm.P = vm.I;
            }
            Instr::LocalGet(local_depth) => {
                vm.I = local_depth.to_usize() as u64;
                vm.P = self.sp.last().to_bits();
            }
            Instr::I64Add
            | Instr::I64Mul
            | Instr::I64And
            | Instr::I64Or
            | Instr::I64Xor
            | Instr::I64Sub
            | Instr::I64DivS
            | Instr::I64DivU
            | Instr::I64RemS
            | Instr::I64RemU
            | Instr::I64Shl
            | Instr::I64Rotl
            | Instr::I64Rotr
            | Instr::I64ShrU
            | Instr::I64ShrS => {
                vm.Z = self.sp.last().to_bits();
            }
            Instr::I32Store(..)
            | Instr::I32Store8(..)
            | Instr::I32Store16(..)
            | Instr::F32Store(..)
            | Instr::F64Store(..)
            | Instr::I64Store(..)
            | Instr::I64Store8(..)
            | Instr::I64Store16(..)
            | Instr::I64Store32(..) => {
                let effective_address = vm.I as usize;
                let updated_block_value1 = {
                    let mut buf = [0u8; 8];
                    let memory = self.cache.default_memory(self.ctx);
                    let memref = self.ctx.resolve_memory(&memory);
                    memref.read(effective_address / 8 * 8, &mut buf).unwrap();
                    u64::from_le_bytes(buf)
                };
                let updated_block_value2 = {
                    let mut buf = [0u8; 8];
                    let memory = self.cache.default_memory(self.ctx);
                    let memref = self.ctx.resolve_memory(&memory);

                    if memref
                        .read((effective_address / 8 + 1) * 8, &mut buf)
                        .is_ok()
                    {
                        u64::from_le_bytes(buf)
                    } else {
                        0
                    }
                };
                vm.P = updated_block_value1;
                vm.Q = updated_block_value2;
            }
            Instr::I32Load(..)
            | Instr::I32Load8U(..)
            | Instr::I32Load8S(..)
            | Instr::I32Load16U(..)
            | Instr::I32Load16S(..)
            | Instr::F32Load(..)
            | Instr::F64Load(..)
            | Instr::I64Load(..)
            | Instr::I64Load8S(..)
            | Instr::I64Load8U(..)
            | Instr::I64Load16S(..)
            | Instr::I64Load16U(..)
            | Instr::I64Load32S(..)
            | Instr::I64Load32U(..) => {
                let effective_address = vm.I as usize;
                let block_value1 = {
                    let mut buf = [0u8; 8];
                    let memory = self.cache.default_memory(self.ctx);
                    let memref = self.ctx.resolve_memory(&memory);
                    memref.read(effective_address / 8 * 8, &mut buf).unwrap();
                    u64::from_le_bytes(buf)
                };
                let block_value2 = {
                    let mut buf = [0u8; 8];
                    let memory = self.cache.default_memory(self.ctx);
                    let memref = self.ctx.resolve_memory(&memory);
                    if memref
                        .read((effective_address / 8 + 1) * 8, &mut buf)
                        .is_ok()
                    {
                        u64::from_le_bytes(buf)
                    } else {
                        0
                    }
                };
                vm.Z = self.sp.last().to_bits();
                vm.P = block_value1;
                vm.Q = block_value2;
            }
            Instr::I64Clz | Instr::I64Ctz | Instr::I64Popcnt | Instr::I64Eqz | Instr::I32Eqz => {
                vm.Z = self.sp.nth_back(1).to_bits();
            }
            Instr::F32Abs
            | Instr::F32Neg
            | Instr::F32Ceil
            | Instr::F32Floor
            | Instr::F32Trunc
            | Instr::F32Nearest
            | Instr::F32Sqrt
            | Instr::F64Abs
            | Instr::F64Neg
            | Instr::F64Ceil
            | Instr::F64Floor
            | Instr::F64Trunc
            | Instr::F64Nearest
            | Instr::F64Sqrt
            | Instr::I32WrapI64
            | Instr::I32TruncF32S
            | Instr::I32TruncF32U
            | Instr::I32TruncF64S
            | Instr::I32TruncF64U
            | Instr::I64ExtendI32S
            | Instr::I64ExtendI32U
            | Instr::I64TruncF32S
            | Instr::I64TruncF32U
            | Instr::I64TruncF64S
            | Instr::I64TruncF64U
            | Instr::F32ConvertI32S
            | Instr::F32ConvertI32U
            | Instr::F32ConvertI64S
            | Instr::F32ConvertI64U
            | Instr::F32DemoteF64
            | Instr::F64ConvertI32S
            | Instr::F64ConvertI32U
            | Instr::F64ConvertI64S
            | Instr::F64ConvertI64U
            | Instr::F64PromoteF32
            | Instr::I32Extend8S
            | Instr::I32Extend16S
            | Instr::I64Extend8S
            | Instr::I64Extend16S
            | Instr::I64Extend32S
            | Instr::I32TruncSatF32S
            | Instr::I32TruncSatF32U
            | Instr::I32TruncSatF64S
            | Instr::I32TruncSatF64U
            | Instr::I64TruncSatF32S
            | Instr::I64TruncSatF32U
            | Instr::I64TruncSatF64S
            | Instr::I64TruncSatF64U
            | Instr::I32Clz
            | Instr::I32Ctz
            | Instr::I32Popcnt => {
                vm.Z = self.sp.last().to_bits();
            }
            Instr::F32Eq
            | Instr::F32Ne
            | Instr::F32Lt
            | Instr::F32Gt
            | Instr::F32Le
            | Instr::F32Ge
            | Instr::F64Eq
            | Instr::F64Ne
            | Instr::F64Lt
            | Instr::F64Gt
            | Instr::F64Le
            | Instr::F64Ge
            | Instr::F32Add
            | Instr::F32Sub
            | Instr::F32Mul
            | Instr::F32Div
            | Instr::F32Min
            | Instr::F32Max
            | Instr::F32Copysign
            | Instr::F64Add
            | Instr::F64Sub
            | Instr::F64Mul
            | Instr::F64Div
            | Instr::F64Min
            | Instr::F64Max
            | Instr::F64Copysign
            | Instr::I64Eq
            | Instr::I64Ne
            | Instr::I64LtS
            | Instr::I64LtU
            | Instr::I64GtS
            | Instr::I64GtU
            | Instr::I64LeS
            | Instr::I64LeU
            | Instr::I64GeS
            | Instr::I64GeU
            | Instr::I32Eq
            | Instr::I32Ne
            | Instr::I32LtS
            | Instr::I32LtU
            | Instr::I32GtS
            | Instr::I32GtU
            | Instr::I32LeS
            | Instr::I32LeU
            | Instr::I32GeS
            | Instr::I32GeU
            | Instr::I32Add
            | Instr::I32Sub
            | Instr::I32Mul
            | Instr::I32DivS
            | Instr::I32DivU
            | Instr::I32RemS
            | Instr::I32RemU
            | Instr::I32And
            | Instr::I32Or
            | Instr::I32Xor
            | Instr::I32Shl
            | Instr::I32ShrS
            | Instr::I32ShrU
            | Instr::I32Rotl
            | Instr::I32Rotr => {
                vm.Z = self.sp.last().to_bits();
            }
            Instr::Select => {
                vm.Z = self.sp.last().to_bits();
            }
            Instr::GlobalGet(..) => {
                vm.Y = self.sp.last().to_bits();
            }
            Instr::MemorySize => {
                vm.Y = self.sp.last().to_bits();
            }
            Instr::MemoryGrow => {
                vm.P = self.sp.last().to_bits();
            }
            Instr::BrTable(..) => {
                vm.I = (vm.post_pc - vm.pc) as u64;
            }
            _ => {}
        }
    }

    /// Special tracing method to handle drop keeps.
    ///
    /// # Note
    ///
    /// - Each VM state holds one read of the keep value and one write to the new address for the keep value.
    ///
    /// - We don't need to trace the dropped values because they will be overwritten or shadowed due to how the stack pointer works.
    /// The stack doesn't actually pop values but instead shadows them by moving the stack pointer to precede those values.
    fn trace_drop_keep(&self, mut init_vm: WitnessVM, drop_keep: DropKeep) -> Vec<WitnessVM> {
        use Instruction as Instr;
        let mut vms = Vec::new();
        init_vm.instr = Instr::DropKeep;
        init_vm.J = init_vm.instr.index_j();

        // `drop` value is traced because we neeed it to calculate the write address for the keep value.
        init_vm.I = drop_keep.drop() as u64;

        // # How the drop keep is traced
        //
        // 1. Determine how many values need to be "kept".
        // 2. Iterate over the keep values, tracing their value and their position in the stack.
        //    The position is stored in `vm.P`, and the value is accessed from the stack via `self.sp.nth_back(keep as usize)`.
        let keep = drop_keep.keep();
        for keep in 1..=keep {
            // Create a new drop_keep VM state, as we trace one keep value per step.
            let mut vm = init_vm.clone();

            // Get the position of the keep value in the stack before it is written to the new address.
            vm.P = keep as u64;

            // Get the value of the keep value.
            vm.Y = self.sp.nth_back(keep as usize).to_bits();

            // This is all we need to trace, so we add the VM state to the list of traced states.
            vms.push(vm);
        }

        vms
    }

    /// Helper method to read memory and return a vector of `u64` values
    fn read_memory(&mut self, start_addr: u64, size: u64) -> Vec<u64> {
        let memory = self.cache.default_memory(self.ctx);
        let memref = self.ctx.resolve_memory(&memory);
        let mut values = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < size {
            let mut buf = [0u8; 8];
            memref
                .read((start_addr / 8 + j) as usize * 8, &mut buf)
                .unwrap();
            let val = u64::from_le_bytes(buf);
            values.push(val);
            i += 8;
            j += 1;
        }
        if start_addr % 8 != 0 {
            let mut buf = [0u8; 8];
            memref
                .read((start_addr / 8 + j) as usize * 8, &mut buf)
                .unwrap();
            let val = u64::from_le_bytes(buf);
            values.push(val);
        }
        values
    }

    /// Special method to handle memory copy
    fn trace_memory_fill(&mut self, mut init_vm: WitnessVM) -> Vec<WitnessVM> {
        use Instruction as Instr;
        let size = init_vm.I;
        let offset = init_vm.X;
        init_vm.instr = Instr::MemoryFillStep;
        init_vm.J = init_vm.instr.index_j();
        let mut vms = Vec::new();
        let new_val_vec = self.read_memory(offset, size);
        for (i, new_val) in new_val_vec.into_iter().enumerate() {
            let mut vm = init_vm.clone();
            vm.P = new_val;
            vm.X = offset / 8 + i as u64;
            vms.push(vm);
        }
        vms
    }

    /// Special method to handle memory copy
    fn trace_memory_copy(&mut self, mut init_vm: WitnessVM) -> Vec<WitnessVM> {
        use Instruction as Instr;
        let num_bytes_to_copy = init_vm.I;
        let src = init_vm.Y;
        let destination = init_vm.X;
        init_vm.instr = Instr::MemoryCopyStep;
        init_vm.J = init_vm.instr.index_j();
        let val_vec = self.read_memory(src, num_bytes_to_copy);
        let mut vms = Vec::new();
        for (i, val) in val_vec.into_iter().enumerate() {
            let mut vm = init_vm.clone();
            vm.P = val;
            vm.Y = src / 8 + i as u64;
            vm.X = destination / 8 + i as u64;
            vms.push(vm);
        }
        vms
    }

    /// Special method to handle CallInternal
    fn trace_call_internal(
        &self,
        mut init_vm: WitnessVM,
        compiled_func: CompiledFunc,
    ) -> Vec<WitnessVM> {
        use Instruction as Instr;
        let len = self.code_map.header(compiled_func).len_locals();
        let pre_sp = init_vm.pre_sp;
        init_vm.instr = Instr::CallZeroWrite;
        init_vm.J = init_vm.instr.index_j();
        let mut vms = Vec::new();
        for i in 0..len {
            let mut vm = init_vm.clone();
            vm.pre_sp = pre_sp + i;
            vm.post_sp = vm.pre_sp + 1;
            vms.push(vm);
        }
        vms
    }

    /// Special method to handle Call instruction
    fn trace_call(&self, len: usize, pre_sp: usize) -> Vec<WitnessVM> {
        use Instruction as Instr;
        let mut init_vm = WitnessVM::default();
        init_vm.instr = Instr::CallZeroWriteIndirect;
        init_vm.J = init_vm.instr.index_j();
        let mut vms = Vec::new();
        for i in 0..len {
            let mut vm = init_vm.clone();
            vm.pre_sp = pre_sp + i;
            vm.post_sp = vm.pre_sp + 1;
            vms.push(vm);
        }
        vms
    }

    /// Special tracing method to handle host calls
    fn trace_host_call(&mut self) -> Vec<WitnessVM> {
        use Instruction as Instr;
        let mut init_vm = WitnessVM::default();
        init_vm.instr = Instr::HostCallStep;
        init_vm.J = init_vm.instr.index_j();
        let memory = self.cache.default_memory(self.ctx);
        let memref = self.ctx.resolve_memory(&memory);
        let pages: u32 = self
            .ctx
            .resolve_memory(self.cache.default_memory(self.ctx))
            .current_pages()
            .into();
        let mut vms = Vec::new();
        for i in 0..(pages * 8192) {
            let mut vm = init_vm.clone();
            let mut buf = [0u8; 8];
            memref.read(i as usize * 8, &mut buf).unwrap();
            // address
            vm.Y = i as u64;
            // value
            vm.P = u64::from_le_bytes(buf);
            vms.push(vm);
        }
        let mut new_vm = WitnessVM::default();
        new_vm.instr = Instr::Call(FuncIdx::from(0));
        new_vm.J = new_vm.instr.index_j();
        new_vm.post_sp = self.sp();
        new_vm.post_pc = self.pc();
        vms.push(new_vm);
        vms
    }

    /// Get `usize` value for the pc
    fn pc(&self) -> usize {
        self.ip.pc()
    }

    /// Get 'usize' value for the sp
    fn sp(&mut self) -> usize {
        self.sync_stack_ptr();
        self.value_stack.stack_ptr
    }
}
