//! Running generated code in the `rv64` emulator: the host-side executor that
//! lets the whole compile-and-run path be tested without a RISC-V machine.

use super::abi::*;
use super::{JitHost, Ret};
use alloc::vec::Vec;
use rv64::emu::Emulator;

/// Where the emulated memory starts.
const BASE: u64 = 0x1_0000;
/// Calls to addresses from here up are runtime services (`MAGIC + 16 * service`).
const MAGIC: u64 = 0xF000_0000;
const MAX_STEPS: usize = 2_000_000_000;

/// Run `code` as the real machine would: `ctx` in `a0`, runtime-table calls
/// serviced by `host`. Caller-saved registers are scribbled after every service
/// call, so code that wrongly relied on them surviving would fail here instead
/// of working by luck.
pub fn emulated_execute(code: &[u8], frame_bytes: usize, host: &mut dyn JitHost) -> u64 {
    let code_len = (code.len() + 15) & !15;
    let ctx = BASE + code_len as u64;
    let ctx_len = (8 * SERVICE_COUNT + 15) & !15;
    let stack = (frame_bytes + 1024 + 15) & !15;
    let mut emulator = Emulator::new(BASE, code_len + ctx_len + stack);
    emulator.write_bytes(BASE, code).expect("code fits");
    for service in 0..SERVICE_COUNT {
        emulator
            .write_u64(ctx + 8 * service as u64, MAGIC + 16 * service as u64)
            .expect("table fits");
    }
    let result = emulator.call(BASE, &[ctx], MAX_STEPS, &mut |target, e| {
        let service = ((target - MAGIC) / 16) as usize;
        let (a1, a2, a3) = (e.x[11], e.x[12], e.x[13]);
        let ret: Ret = match service {
            SVC_VAR => host.var(a1),
            SVC_CONST => host.constant(a1),
            SVC_NEG => host.neg(a1),
            SVC_INFIX => host.infix(a1, a2, a3),
            SVC_CALL => {
                let args: Vec<u64> = (0..a2)
                    .map(|i| e.read_u64(a3 + 8 * i).expect("argument array in memory"))
                    .collect();
                host.call(a1, &args)
            }
            SVC_DISCARD => host.discard(a1),
            SVC_REPEAT_COUNT => host.repeat_count(a1),
            SVC_TRUTH => host.truth(a1, a2),
            SVC_FAIL => host.fail(a1),
            other => panic!("generated code called unknown service {other}"),
        };
        // Everything the ABI lets a callee clobber, except the registers the
        // emulator itself needs.
        for register in [5, 6, 7, 12, 13, 14, 15, 16, 17, 28, 29, 30, 31] {
            e.x[register] = 0xBAD0_BAD0_BAD0_BAD0;
        }
        for (register, slot) in e.f.iter_mut().enumerate() {
            if !(8..=9).contains(&register) && !(18..=27).contains(&register) {
                *slot = 0x7FF8_BAD0_BAD0_BAD0;
            }
        }
        e.x[10] = ret.value;
        e.x[11] = ret.err;
    });
    match result {
        Ok(status) => status,
        Err(error) => panic!("generated code faulted in the emulator: {error:?}"),
    }
}
