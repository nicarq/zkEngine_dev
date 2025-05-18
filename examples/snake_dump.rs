use std::path::PathBuf;
use zk_engine::{
    nova::{
        provider::{ipa_pc, Bn256EngineIPA},
        spartan,
        traits::Dual,
    },
    error::ZKWASMError,
    utils::logging::init_logger,
    wasm_ctx::{WASMArgsBuilder, Wasm4Ctx, ZKWASMCtx},
    wasm_snark::{StepSize, WasmSNARK},
};
use serde::Serialize;

pub type E = Bn256EngineIPA;
pub type EE1 = ipa_pc::EvaluationEngine<E>;
pub type EE2 = ipa_pc::EvaluationEngine<Dual<E>>;
pub type S1 = spartan::batched::BatchedRelaxedR1CSSNARK<E, EE1>;
pub type S2 = spartan::batched::BatchedRelaxedR1CSSNARK<Dual<E>, EE2>;

#[derive(Serialize)]
struct MemoryDump {
    inputs: String,
    trace_len: usize,
    stack_len: usize,
    mem_len: usize,
    is_state: Vec<(usize, u64, u64)>,
}

fn main() -> Result<(), ZKWASMError> {
    init_logger();

    let step_size = StepSize::new(1000);
    let inputs = "RRDDLU";

    let wasm_args = WASMArgsBuilder::default()
        .file_path(PathBuf::from(
            "third-party/snake-w4-rs/target/wasm32-unknown-unknown/release/snake_w4_rs.wasm",
        ))?
        .invoke("update")
        .func_args(vec![])
        .step_inputs(inputs.as_bytes().to_vec())
        .build();
    let wasm_ctx = Wasm4Ctx::new(wasm_args.clone());

    // Capture execution trace and initial memory state
    let (trace, is_state, is_sizes) = wasm_ctx.execution_trace()?;

    let dump = MemoryDump {
        inputs: inputs.to_string(),
        trace_len: trace.len(),
        stack_len: is_sizes.stack_len(),
        mem_len: is_sizes.mem_len(),
        is_state,
    };

    std::fs::write(
        "snake_memory.json",
        serde_json::to_string_pretty(&dump).expect("serialize"),
    )
    .map_err(|e| ZKWASMError::WASMError(e.to_string()))?;

    // Optional: prove and verify the game execution
    let pp = WasmSNARK::<E, S1, S2>::setup(step_size);
    let (snark, instance) = WasmSNARK::<E, S1, S2>::prove(&pp, &wasm_ctx, step_size)?;
    snark.verify(&pp, &instance)?;

    Ok(())
}
