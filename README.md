<div align="center">

<img src="docs/logo.svg" alt="Rysk" width="380">

**A RISC-V emulator, written in Rust.**

RV64 with the M and A extensions, CSRs and counters, running flat binaries
against the QEMU `virt` memory map.

[![Rust](https://github.com/edg-l/rysk/actions/workflows/rust.yml/badge.svg)](https://github.com/edg-l/rysk/actions/workflows/rust.yml)

[Quick start](#quick-start) &middot;
[What it does](#what-it-does) &middot;
[Testing](#testing) &middot;
[Layout](#layout) &middot;
[Status](#status)

</div>

---

## What it does

Rysk loads a flat binary at `0x8000_0000`, points `sp` at the top of 128 MiB of
DRAM, and interprets one instruction at a time. One hart, one address space, no
privilege modes: everything runs as if in machine mode with translation off.

| | |
|---|---|
| **RV64I** | the base integer set: loads and stores, the ALU, branches, `jal`/`jalr`, `lui`/`auipc`, the `*W` word forms, and `fence` as the no-op it is on one in-order hart |
| **RV64M** | `mul`, `mulh`, `mulhu`, `mulhsu`, `div`, `rem`, and their unsigned and `W` variants |
| **Zaamo** | the atomic memory operations, word and doubleword |
| **Zalrsc** | `lr`/`sc`, against a reservation table that watches the address for writes |
| **Zicsr** | `csrrw`, `csrrs`, `csrrc` and their immediate forms, over a flat 4096-entry CSR file |
| **Zicntr** | `cycle` and `instret`, counted per instruction; `time` from the host clock |
| **Zicond** | `czero.eqz`, `czero.nez` |

An image that carries a `tohost` symbol is treated as a test and its result is
read from there, which is how the compliance corpus reports.

`ecall`, `ebreak`, an illegal instruction, an access outside dram and a
misaligned jump or atomic all raise: rysk records `mepc`, `mcause` and `mtval`,
stacks the interrupt-enable bit and enters the handler in `mtvec`, and `mret`
comes back. With no handler installed there is nowhere to deliver, so that is
where a program ends, and `run` hands the trap back saying why.

## Quick start

```bash
cargo run -- tests/fib.bin              # a flat binary, loaded at DRAM_BASE
cargo run -- rv64ui-p-add               # or an ELF, started at its entry point
```

That prints the register file and the non-zero CSRs at the end of the run. To
watch it execute, build the tracing in and set `RUST_LOG`:

```bash
RUST_LOG=debug cargo run --features trace -- tests/fib.bin   # one line per instruction
RUST_LOG=trace cargo run --features trace -- tests/fib.bin   # bus loads and stores too
```

The `trace` feature is off by default because the spans and their fields cost
about six times what interpreting the instruction does. Without it the tracing
compiles to nothing, and rysk runs at roughly 78 million instructions per
second on `bench/loop.bin`.

## Testing

```bash
cargo test    # the whole suite
make corpus   # fetch and build riscv-tests, which cargo test also runs
make test     # rebuild the fixtures and the corpus, then the same suite
```

Two suites. `tests/isa.rs` is written here and needs nothing; `tests/compliance.rs`
runs the official [riscv-tests](https://github.com/riscv-software-src/riscv-tests)
corpus, 86 programs across `rv64ui`, `rv64um` and `rv64ua`, and fails rather than
skipping if the corpus is missing.

`tests/common` is a small assembler, so a test is a Rust array of instructions
run on a fresh machine:

```rust
#[test]
fn immediate_shifts_take_six_bits_of_shift_amount() {
    let cpu = prog(&[slli(T1, T0, 40)]).reg(T0, 1).run();
    assert_eq!(cpu.reg(T1), 1 << 40);
}
```

`reg` preloads a register so a test does not have to build its inputs in
assembly, and execution ends on the zero word past the last instruction.

Two fixtures come from a real toolchain and need `make test_files` to rebuild.
`tests/encodings.s` is assembled ground truth that the Rust assembler is checked
against, so the tests cannot agree with a misunderstanding twice; `tests/fib.c`
is a compiled C program, the one case that proves rysk runs what a compiler
emits.

## Layout

```
src/
  inst.rs      decoding: a word becomes an Op and its operands, and prints itself
  cpu.rs       the machine: registers, the run loop, traps, and execute
  csr.rs       control and status register numbers, and the mstatus layout
  exception.rs the causes a trap can have, and what each owes mtval
  bus.rs       address decode, and the LR/SC reservation
  dram.rs      128 MiB of RAM behind sized load and store helpers
  elf.rs       enough ELF64 to place an image and find its symbols
  htif.rs      how the riscv-tests corpus reports pass or fail
  main.rs      argv, tracing, run, dump
tests/
  isa.rs       the hand-written suite, one test binary
  suite/       its chapters, by extension
  common/      an assembler and a harness to run what it emits
  compliance.rs  the official riscv-tests corpus
```

Decoding is separate from execution, so `Op` is a small enum an interpreter can
dispatch on and `Inst` knows how to print itself. That is where the disassembly
in a trace comes from.

## Status

Working, and not finished. What is missing, roughly in the order it matters:

| | |
|---|---|
| **Interrupts** | traps work, but nothing can raise one asynchronously yet, so `wfi` retires immediately and `mie`/`mip` go unread |
| **Zacas** | `amocas.w/d/q` is the next extension in |
| **Zifencei, `fence`** | unimplemented, so both are illegal instructions |
| **Privilege modes** | machine mode is assumed, never enforced. The CSR file has no WARL or access checks |
| **Devices** | no CLINT, no PLIC, no UART. The bus decodes DRAM and nothing else |
| **Compliance** | `rv64ui`, `rv64um` and `rv64ua` pass in full. `rv64mi` needs supervisor mode and interrupts before it can |

## License

AGPL-3.0. See [LICENSE](LICENSE).
