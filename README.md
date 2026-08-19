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
| **RV64I** | the base integer set: loads and stores, the ALU, branches, `jal`/`jalr`, `lui`/`auipc`, and the `*W` word forms |
| **RV64M** | `mul`, `mulh`, `mulhu`, `mulhsu`, `div`, `rem`, and their unsigned and `W` variants |
| **Zaamo** | the atomic memory operations, word and doubleword |
| **Zalrsc** | `lr`/`sc`, against a reservation table that watches the address for writes |
| **Zicsr** | `csrrw`, `csrrs`, `csrrc` and their immediate forms, over a flat 4096-entry CSR file |
| **Zicntr** | `cycle` and `instret`, counted per instruction; `time` from the host clock |
| **Zicond** | `czero.eqz`, `czero.nez` |

Execution stops when `pc` reaches zero, when a fetch falls below `DRAM_BASE`, or
on `ecall`/`ebreak`. Nothing traps: no handler runs, no cause is recorded, and
the program simply ends.

## Quick start

```bash
cargo run -- tests/fib.bin
```

That prints the register file and the non-zero CSRs at the end of the run. To
watch it execute, set `RUST_LOG`:

```bash
RUST_LOG=debug cargo run -- tests/fib.bin   # one line per instruction
RUST_LOG=trace cargo run -- tests/fib.bin   # every bus load and store as well
```

## Testing

```bash
cargo test    # runs the committed test binaries
make test     # reassembles them first, then runs the same suite
```

Each case is a small program in `tests/`, assembled or compiled by
`riscv64-unknown-elf-gcc` and flattened with `objcopy`. The resulting `.bin` is
committed, so `cargo test` and CI need no cross toolchain; only `make test`
does.

A new case is three steps: write `tests/<name>.s` or `tests/<name>.c`, run
`make test_files`, and add a row to `tests/instructions.rs` naming the
registers, memory bytes and CSRs the program should end with.

```rust
#[case::fib("tests/fib.bin", &[(14, 1), (15, 0x37)], &[], &[])]
```

## Layout

```
src/
  cpu.rs      the emulator: fetch, decode, execute, and the CSR file
  bus.rs      address decode, and the LR/SC reservation table
  dram.rs     128 MiB of RAM behind sized load and store helpers
  main.rs     argv, tracing, run, dump
tests/        .s and .c fixtures, their assembled .bin, and the rstest suite
```

`cpu.rs` is the whole machine. `Cpu::execute` is one match on the opcode,
nested on `funct3` and `funct7` from there, in the order the encoding tables in
the manual list them.

## Status

Working, and not finished. What is missing, roughly in the order it matters:

| | |
|---|---|
| **Traps** | no `mret`, no trap vector, no cause codes. `ecall` halts instead of trapping |
| **Zacas** | `amocas.w/d/q` is the next extension in |
| **Zifencei, `fence`** | unimplemented, and an unknown opcode panics rather than raising an illegal-instruction exception |
| **Privilege modes** | machine mode is assumed, never enforced. The CSR file has no WARL or access checks |
| **Devices** | no CLINT, no PLIC, no UART. The bus decodes DRAM and nothing else |
| **Compliance** | tested by a handful of hand-written programs, not by `riscv-tests` |

## License

AGPL-3.0. See [LICENSE](LICENSE).
