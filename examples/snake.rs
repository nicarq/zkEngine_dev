//! Example proving a score for the WASM-4 snake game.
//!
//! This example assumes the game in `third-party/snake-w4-rs` has been compiled
//! for the `wasm32-unknown-unknown` target. The compiled binary should be
//! located at `third-party/snake-w4-rs/target/wasm32-unknown-unknown/release/snake_w4_rs.wasm`.
//! The exported `play` function should run the game using the provided input
//! sequence and return the final score.

use std::path::PathBuf;
use zk_engine::{
    nova::{
        provider::{ipa_pc, Bn256EngineIPA},
        spartan,
        traits::Dual,
    },
    error::ZKWASMError,
    utils::logging::init_logger,
    wasm_ctx::{WASMArgsBuilder, WASMCtx},
    wasm_snark::{StepSize, WasmSNARK},
};

pub type E = Bn256EngineIPA;
pub type EE1 = ipa_pc::EvaluationEngine<E>;
pub type EE2 = ipa_pc::EvaluationEngine<Dual<E>>;
pub type S1 = spartan::batched::BatchedRelaxedR1CSSNARK<E, EE1>;
pub type S2 = spartan::batched::BatchedRelaxedR1CSSNARK<Dual<E>, EE2>;

fn main() -> Result<(), ZKWASMError> {
    init_logger();

    // Adjust the step size to match the compiled game's opcode count.
    let step_size = StepSize::new(1000);

    // Example sequence of inputs encoded as a single string.
    let inputs = "RRDDLU";

    // Generate setup parameters.
    let pp = WasmSNARK::<E, S1, S2>::setup(step_size);

    let wasm_args = WASMArgsBuilder::default()
        .file_path(PathBuf::from(
            "third-party/snake-w4-rs/target/wasm32-unknown-unknown/release/snake_w4_rs.wasm",
        ))?
        .invoke("play")
        .func_args(vec![inputs.to_string()])
        .build();
    let wasm_ctx = WASMCtx::new(wasm_args);

    let (snark, instance) = WasmSNARK::<E, S1, S2>::prove(&pp, &wasm_ctx, step_size)?;

    snark.verify(&pp, &instance)?;

    Ok(())
}
