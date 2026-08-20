<div align="center">

<img src="docs/logo.svg" alt="rysk" width="340">

**A RISC-V emulator, written in Rust.**

RV64GC with privilege modes, Sv39 paging, several harts, and a PCI bus with a
display, a keyboard, a mouse and a disk on it that stock Linux drivers bind to.

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

| set | what it covers |
|---|---|
| **RV64I** | the base integer set, with `fence` ordering the host as well, since a hart is a host thread, and `fence.i` emptying the decoded instructions |
| **RV64M** | `mul`, `div`, `rem` and their unsigned and `W` forms |
| **RV64A** | `lr`/`sc` against a reservation per hart, and atomic memory operations that are one host atomic rather than a load and a store |
| **RV64FD** | single and double floating point, done in integers because no host rounds the five ways this machine can, with the arithmetic the host reaches the same way handed to it |
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
                       hart 0 … hart N        a host thread each, or turns
                            │
   dram ────────────────────┼──────────────── 0x8000_0000
   clint ───────────────────┤                 0x0200_0000   mtime, mtimecmp, msip
   plic  or  aplic ─────────┤                 0x0c00_0000   wires
             imsic ─────────┤                 0x2400_0000   messages
   16550 ───────────────────┤                 0x1000_0000
   pcie ecam ───────────────┤                 0x3000_0000   256 buses
        windows ────────────┘                 0x4000_0000, 0x4_0000_0000
             │
             ├── bochs display   1234:1111   a framebuffer and a monitor
             ├── xhci            1b36:000d   a keyboard and a mouse
             └── nvme            1b36:0010   a disk
```

**More than one hart.** `-smp N` gives the guest N harts sharing one bus, each on
a host thread of its own, so they really do run at once: a Linux boot to a shell
on four harts is 1.31x what it is taking turns. Nothing in the machine orders one
hart against another; the guest's own `fence` and its atomics are what do that,
which is what they are for. Memory is shared without a lock and every atomic
instruction is one the host carries out indivisibly.

Taking turns is still there, a quantum of instructions each on one host thread,
and `--schedule turns` asks for it. Two policies rather than one replacing the
other, because threads are what make parallel guest work parallel and turns are
what make one run repeat another, which a frontend that records and replays will
need. QEMU draws the same line.

Each hart has its own `mhartid`, reservation, address-translation cache and
decoded instructions, and no hart invalidates another's caches, which is what the
architecture says: crossing harts is an IPI plus a local fence, which is what
SBI's remote fences are.

**Paging.** Sv39 translates, permissions are checked against the mode the access
is for, and `MPRV`, `TVM`, `TW` and `TSR` all mean something. A cache in front of
the walk is what keeps most walks from happening.

**A bus the guest enumerates.** A PCIe root complex with config space at ECAM, a
32-bit and a 64-bit window that base address registers are handed addresses
from, and INTx swizzled onto four wires. A function declares its capabilities in
a list the complex places and chains: MSI-X, with its vector table and pending
array inside one of the function's own windows, and PCI Express, which is what
makes the kernel call these root complex integrated endpoints. A function that
masters the bus reaches guest memory directly rather than through the bus that is
holding it, and one that interrupts says what it is asserting and lets the
complex deliver it, since the complex is what would have to send the message.

**Things on it that drivers bind to**, all off unless asked for:

```bash
rysk --display bochs     # a framebuffer, and a monitor it answers for
rysk --usb hid           # an xHCI controller, a keyboard and a mouse
rysk --disk disk.img     # an NVMe controller with that file behind it
rysk --disk 64M          # or that many mebibytes of memory, gone when the run ends
```

`drm/tiny/bochs.c` finds sixteen mebibytes of framebuffer and reads the modes out
of a generated EDID block; `xhci-pci`, `usbhid` and `hid-generic` enumerate the
two devices and register them under `/dev/input`; `nvme` finds `/dev/nvme0n1`,
and a filesystem mounted on it writes through to the file on the host. None of
them is patched, and each was checked by booting the same kernel under
`qemu-system-riscv64` with the equivalent `-device` and diffing what the driver
printed.

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
cargo run --release -- -smp 4 prog.bin         # four harts, a host thread each
cargo run --release -- -smp 4 --schedule turns prog.bin   # or taking turns on one
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

### A window

`--gui` puts the guest's screen in one, scaled to fit and nearest-neighbour, with
the mode it is in and whether the machine is still running above it. Closing the
window stops the machine at an instruction boundary, so the run ends the way it
does headless.

```bash
cargo run --release -- --display bochs --gui bench/display.bin
```

A run is headless unless `--gui` asks for a window, since that is what the
corpus, the benchmarks and CI run. Whether the window is *compiled* is a separate
question and a cargo feature: `gui` is on by default, and
`--no-default-features` leaves a whole GPU stack out of a build that will never
open one.

Nothing repaints on a timer. A thread asks the card whether a page of video
memory moved, which is a bit per page, and only then is a frame laid out and
presented, so a machine drawing nothing costs nothing.

### Booting Linux

Rysk stands in for a boot rom, so it leaves what firmware expects: the hart id in
`a0`, the device tree in `a1`, and in `a2` a structure naming the next stage.

```bash
rysk -m 1024 -smp 4 --aia aplic-imsic \
     --initrd initrd.gz --append "console=ttyS0 rdinit=/bin/sh" \
     fw_dynamic.bin vmlinux@0x80200000
```

OpenSBI comes up, hands off to the kernel in supervisor mode, and the kernel
enumerates the PCI bus, brings up the other harts and reaches a shell. Add
`--display bochs --usb hid --disk disk.img` and it finds a card, two input
devices and a disk on the way.

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
  fpu.rs       IEEE arithmetic in integers, and the host where it agrees
  mmu.rs       Sv39: the walk, the permission rules, and the cache in front
  block.rs     the decoded instructions, as the straight-line runs they were decoded as
  machine.rs   what a machine is made of, and the two ways its harts get to run
  bus.rs       address decode: dram, then a binary search over the devices
  device.rs    the Device trait, the Line and Msi a device raises, and the Dma it transfers through
  clint.rs     the timer and the software-interrupt bit, one of each per hart
  plic.rs      wired external interrupts, claimed and completed
  aplic.rs     their replacement: domains, source modes, and forwarding
  imsic.rs     interrupt files: what a message arrives in
  pci.rs       a root complex: config space, the windows, INTx, MSI-X and Express
  uart.rs      a 16550
  bochs.rs     a display: a framebuffer, the registers that shape it, and which pages moved
  gui.rs       the window, and the machine running on a thread behind it
  edid.rs      the block a monitor answers with, generated rather than modelled
  xhci.rs      a USB host controller: its rings, its contexts, and its ports
  usb.rs       what a device is from the controller's side
  hid.rs       the keyboard and the mouse, and what a frontend presses
  nvme.rs      a disk controller: queue pairs in guest memory, and the pointers into it
  disk.rs      where the blocks actually are, which is a file or some bytes
  fdt.rs       the device tree, built from the same table that builds the bus
  dram.rs      the bytes every hart shares, behind sized load and store helpers
  shared.rs    those bytes, and the argument for reaching them without a lock
  elf.rs       enough ELF64 to place an image and find its symbols
  htif.rs      how the riscv-tests corpus reports pass or fail
  main.rs      argv, tracing, run, dump
tests/
  isa.rs       the hand-written suite, one test binary
  suite/       its chapters, by extension and by device
  common/      an assembler and a harness to run what it emits
  compliance.rs  the official riscv-tests corpus
  devicetree.rs  the tree rysk builds, read back
  softfloat.rs   the float arithmetic, held against the host's
```

Each file has one job. Decoding is separate from execution, so `Op` is a small
enum an interpreter can dispatch on and `Inst` knows how to print itself, which
is where the disassembly in a trace comes from. A hart never holds the bus:
memory and devices belong to the machine and arrive at every instruction as an
argument, which is what lets several harts share one address space.

## Status

Working, and not finished. What is missing, roughly in the order it matters:

| missing | where it stands |
|---|---|
| **Input, and a raw terminal** | `--gui` presents the framebuffer, but nothing pushes a keystroke into the USB keyboard yet, and the serial port's terminal is still line buffered, so typing at a guest shell arrives a line at a time |
| **Panels** | the window shows the guest's screen and nothing about the machine behind it, so a black screen is still a mystery rather than a `pc` and a trap |
| **A network** | there is no interface on the bus, so a guest has no way off the machine |
| **Determinism** | no run repeats another yet. `--schedule turns` fixes the order the harts run in, which is the half of it that threads give up, but the devices still advance with the wall clock rather than with retired instructions |
| **Debug triggers** | the one corpus test that does not pass, `rv64mi-p-breakpoint`, wants them |
| **The hypervisor extension** | no H, so no VS mode and no guest interrupt files |
| **Threaded dispatch** | the interpreter is a `match`, and doing better wants guaranteed tail calls, which stable Rust does not have |

## License

AGPL-3.0. See [LICENSE](LICENSE).
