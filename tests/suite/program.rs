use crate::common::*;
use rysk::cpu::Cpu;

// ------------------------------------------------------------------ end to end

#[test]
fn a_compiled_c_program_runs() {
    // tests/fib.c, built by the toolchain: the one case that proves rysk runs what a
    // real compiler emits rather than only what tests/common assembles.
    let code = std::fs::read("tests/fib.bin").expect("run `make test_files`");
    let mut cpu = Cpu::new(code);
    cpu.run();
    // main returns fib(10), which the calling convention leaves in a0. Asserting on the
    // return value rather than on scratch registers keeps this independent of which
    // compiler built the fixture.
    assert_eq!(cpu.reg(A0), 55);
}
