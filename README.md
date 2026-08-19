<div align="center">

<img src="docs/logo.svg" alt="rysk" width="340">

**A RISC-V emulator, written in Rust.**

RV64GC with privilege modes, Sv39 paging, several harts and enough devices that
OpenSBI and Linux both boot on it.

[![Rust](https://github.com/edg-l/rysk/actions/workflows/rust.yml/badge.svg)](https://github.com/edg-l/rysk/actions/workflows/rust.yml)

[Quick start](#quick-start) &middot;
[What it does](#what-it-does) &middot;
[The machine](#the-machine) &middot;
[Testing](#testing) &middot;
[Layout](#layout) &middot;
[Status](#status)

</div>

---

## What it does

Rysk builds a machine and runs it. Memory starts at `0x8000_0000`, the devices
sit where the QEMU `virt` machine puts them, and the guest is handed a device
tree rysk builds from the same table that attached them, so the two cannot
disagree. Nothing about the guest is special-cased: it enumerates a PCI bus,
walks page tables, takes interrupts and starts its other harts the way it would
on a board.

| | |
|---|---|
| **RV64I** | the base integer set, with `fence` as the no-op it is on one in-order hart and `fence.i` emptying the decoded instructions |
| **RV64M** | `mul`, `div`, `rem` and their unsigned and `W` forms |
| **RV64A** | `lr`/`sc` against a reservation per hart, and the atomic memory operations |
| **RV64FD** | single and double floating point, done in integers rather than handed to the host, because no host rounds the five ways this machine can |
| **RV64C** | the compressed instructions, expanded into what their 32-bit form would decode to |
| **Zicsr, Zicntr** | the CSR instructions, and `cycle`, `time` and `instret` |
| **Zicond, Zacas, Zabha, Zawrs** | conditional zeroing, compare-and-swap up to a quadword, byte and halfword atomics, and the two wait-on-reservation hints |
| **Zifencei, Svade** | instruction-fetch fencing, and the accessed and dirty bits left to software |

Traps and privilege modes are real. `ecall`, `ebreak`, illegal instructions,
access faults and misaligned jumps and atomics enter `mtvec`, or `stvec` when
`medeleg` delegates them, and `mret` and `sret` return. Machine, supervisor and
user mode are a mode on the hart, and a CSR access is checked against the
privilege its address encodes. A CSR either exists or raises an illegal
instruction: with a flat register file, firmware probed its way to believing
this machine had extensions it does not have, and then used one.

## The machine

```
                       hart 0 … hart N        one host thread, a quantum each
                            │
   dram ────────────────────┼──────────────── 0x8000_0000
   clint ───────────────────┤                 0x0200_0000   mtime, mtimecmp, msip
   plic  or  aplic ─────────┤                 0x0c00_0000   wires
             imsic ─────────┤                 0x2400_0000   messages
   16550 ───────────────────┤                 0x1000_0000
   pcie ecam ───────────────┤                 0x3000_0000   256 buses
        windows ────────────┘                 0x4000_0000, 0x4_0000_0000
```

**More than one hart.** `-smp N` gives the guest N harts sharing one bus, taking
turns on one host thread a quantum of instructions at a time. A switch only
happens between whole instructions, so an atomic is atomic because nothing can
interleave with it. Each hart has its own `mhartid`, reservation,
address-translation cache and decoded instructions, and no hart invalidates
another's caches, which is what the architecture says: crossing harts is an IPI
plus a local fence, which is what SBI's remote fences are.

**Paging.** Sv39 translates, permissions are checked against the mode the access
is for, and `MPRV`, `TVM`, `TW` and `TSR` all mean something. A cache in front of
the walk is what keeps most walks from happening.

**A bus the guest enumerates.** A PCIe root complex with config space at ECAM, a
32-bit and a 64-bit window that base address registers are handed addresses
from, and INTx swizzled onto four wires. A function that sends messages instead
carries an MSI-X capability, with its vector table and pending array inside one
of its own windows.

**Two interrupt architectures**, picked the way QEMU picks them:

```bash
rysk --aia none          # a PLIC: wires all the way to the hart
rysk --aia aplic         # an APLIC delivering its own interrupts, a domain per level
rysk --aia aplic-imsic   # and forwarding them to an interrupt file per hart, as messages
```

With `aplic-imsic` the harts have `Smaia` and `Ssaia`: an interrupt file per
privilege level per hart, reached from the bus at the page a message is written
to and from the hart through `miselect`/`mireg`, `mtopei` and `mtopi`.

## Quick start

```bash
cargo run -- tests/fib.bin                     # a flat binary, loaded at DRAM_BASE
cargo run -- ~/.cache/rysk/isa/rv64ui-p-add    # or an ELF, started at its entry point
cargo run --release -- -smp 4 prog.bin         # the same machine with four harts
```

That prints the register file and the non-zero CSRs at the end of the run. To
watch it execute, build the tracing in and set `RUST_LOG`:

```bash
RUST_LOG=debug cargo run --features trace -- tests/fib.bin   # one line per instruction
RUST_LOG=trace cargo run --features trace -- tests/fib.bin   # bus loads and stores too
```

The `trace` feature is off by default because the spans and their fields cost
several times what interpreting the instruction does; without it the tracing
compiles to nothing.

### Booting Linux

Rysk stands in for a boot rom, so it leaves what firmware expects: the hart id in
`a0`, the device tree in `a1`, and in `a2` a structure naming the next stage.

```bash
rysk -m 1024 -smp 4 --aia aplic-imsic \
     --initrd initrd.gz --append "console=ttyS0 rdinit=/bin/sh" \
     fw_dynamic.bin vmlinux@0x80200000
```

OpenSBI comes up, hands off to the kernel in supervisor mode, and the kernel
enumerates the PCI bus, brings up the other harts and reaches a shell.

## Testing

```bash
cargo test    # the whole suite
make corpus   # fetch and build riscv-tests, which cargo test also runs
make test     # rebuild the fixtures and the corpus, then the same suite
```

Two suites. `tests/isa.rs` is written here and needs nothing;
`tests/compliance.rs` runs the official
[riscv-tests](https://github.com/riscv-software-src/riscv-tests) corpus, 134
programs across `rv64ui`, `rv64um`, `rv64ua`, `rv64uc`, `rv64uf`, `rv64ud`,
`rv64si` and `rv64mi`, and fails rather than skipping if the corpus is missing.
A test that is known not to pass is listed with what it is waiting on, and one
that starts passing is reported as a failure, so the list can only shrink.

`tests/common` is a small assembler, so a test is a Rust array of instructions
run on a fresh machine:

```rust
#[test]
fn immediate_shifts_take_six_bits_of_shift_amount() {
    let machine = prog(&[slli(T1, T0, 40)]).reg(T0, 1).run();
    assert_eq!(machine.reg(T1), 1 << 40);
}
```

`reg` preloads a register so a test does not have to build its inputs in
assembly, `device` puts something on the bus, and execution ends on the zero word
past the last instruction.

Two fixtures come from a real toolchain and need `make test_files` to rebuild.
`tests/encodings.s` is assembled ground truth that the Rust assembler is checked
against, so the tests cannot agree with a misunderstanding twice; `tests/fib.c`
is a compiled C program, the one case that proves rysk runs what a compiler
emits.

For anything a real driver binds to there is a third check: boot the same image
under rysk and under `qemu-system-riscv64` and diff what the driver says.

## Layout

```
src/
  inst.rs      decoding: a word becomes an Op and its operands, and prints itself
  rvc.rs       the compressed instructions, expanded into what they mean
  cpu.rs       one hart: its state, trap entry and return, and execute
  csr.rs       control and status register numbers, and the mstatus layout
  trap.rs      the cause numbers of both kinds of trap, and the mtval each owes
  fpu.rs       IEEE arithmetic in integers, because rounding is the point
  mmu.rs       Sv39: the walk, the permission rules, and the cache in front
  icache.rs    the instructions already decoded, by the address they came from
  machine.rs   what a machine is made of, and the loop that gives each hart a turn
  bus.rs       address decode: dram, then a binary search over the devices
  device.rs    the Device trait, and the Line and Msi a device raises
  clint.rs     the timer and the software-interrupt bit, one of each per hart
  plic.rs      wired external interrupts, claimed and completed
  aplic.rs     their replacement: domains, source modes, and forwarding
  imsic.rs     interrupt files: what a message arrives in
  pci.rs       a root complex: config space, the windows, INTx and MSI-X
  uart.rs      a 16550
  fdt.rs       the device tree, built from the same table that builds the bus
  dram.rs      a Vec<u8> behind sized load and store helpers
  elf.rs       enough ELF64 to place an image and find its symbols
  htif.rs      how the riscv-tests corpus reports pass or fail
  main.rs      argv, tracing, run, dump
tests/
  isa.rs       the hand-written suite, one test binary
  suite/       its chapters, by extension and by device
  common/      an assembler and a harness to run what it emits
  compliance.rs  the official riscv-tests corpus
  devicetree.rs  the tree rysk builds, read back
```

Each file has one job. Decoding is separate from execution, so `Op` is a small
enum an interpreter can dispatch on and `Inst` knows how to print itself, which
is where the disassembly in a trace comes from. A hart never holds the bus:
memory and devices belong to the machine and arrive at every instruction as an
argument, which is what lets several harts share one address space.

## Status

Working, and not finished. What is missing, roughly in the order it matters:

| | |
|---|---|
| **Real devices** | nothing is plugged into the PCI bus yet, so BAR routing and MSI-X are proven by test functions rather than by a driver. A display, xHCI with a USB keyboard, and NVMe are next |
| **A window** | the machine has no frontend: output goes to stdout and the terminal is still line buffered, so typing at a guest shell arrives a line at a time |
| **Determinism** | the schedule is fixed but a run is not reproducible, because the devices advance with the wall clock rather than with retired instructions |
| **Debug triggers** | the one corpus test that does not pass, `rv64mi-p-breakpoint`, wants them |
| **The hypervisor extension** | no H, so no VS mode and no guest interrupt files |
| **Threaded dispatch** | the interpreter is a `match`, and doing better wants guaranteed tail calls, which stable Rust does not have |

## License

AGPL-3.0. See [LICENSE](LICENSE).
