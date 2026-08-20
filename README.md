<div align="center">

<img src="docs/logo.svg" alt="rysk" width="340">

**A RISC-V emulator, written in Rust.**

RV64GC with privilege modes, Sv39 paging, several harts, and a PCI bus with a
display, a keyboard, a mouse and a disk on it that stock Linux drivers bind to.
Boots Linux, and opens a window you can stop it in and look around.

[![Rust](https://github.com/edg-l/rysk/actions/workflows/rust.yml/badge.svg)](https://github.com/edg-l/rysk/actions/workflows/rust.yml)

[Quick start](#quick-start) &middot;
[The machine](#the-machine) &middot;
[Testing](#testing) &middot;
[Status](#status)

</div>

---

## Quick start

```bash
cargo install --path .        # or: cargo build --release, for target/release/rysk
```

```bash
rysk prog.bin                 # a flat binary, loaded at 0x8000_0000
rysk -smp 4 prog.bin          # four harts, a host thread each
rysk vmlinux                  # or an ELF, started at its entry point
```

A run ends on a trap nothing is installed to handle, and says where it stopped:
the register file, the control registers holding something, and the last trap the
hart took.

### Boot Linux

```bash
scripts/boot-linux.sh          # fetches a kernel, an initramfs and OpenSBI, then boots
scripts/boot-linux.sh --gui    # the same, with a screen and the panels beside it
```

None of that is in the repository — a kernel and an initramfs are tens of megabytes
and belong to Debian — so the script fetches them once, pinned to versions rysk has
booted, and runs the build beside it. By hand it is:

```bash
rysk -m 1024 -smp 4 --aia aplic-imsic \
     --initrd initrd.gz --append "console=ttyS0 rdinit=/bin/sh" \
     fw_dynamic.bin vmlinux@0x80200000
```

Rysk stands in for a boot rom, so it leaves what firmware expects: the hart id in
`a0`, the device tree in `a1`, and in `a2` a structure naming the next stage.

OpenSBI comes up, hands off to the kernel in supervisor mode, and the kernel
enumerates the PCI bus, brings up the other harts and reaches a shell.

### Give it a screen, a keyboard and a disk

```bash
rysk --display bochs --usb hid --disk disk.img --gui ...
```

`--gui` opens a window on the guest's screen and types at its USB keyboard. Every
device is off unless asked for, and a run stays headless without `--gui`, which is
what CI and the benchmarks want; `--no-default-features` leaves the window out of
the build entirely.

### Look inside it

The window is also the debugger. Beside the guest's picture are its registers with
whatever just moved picked out, the control registers, the memory, a disassembly
with `pc` on it, the traps the hart has taken, and what every device on the bus is
doing. Run, pause, step an instruction, step to the next trap, or click a line of
the disassembly to break on it.

```bash
rysk --symbols System.map --gui ...   # names for addresses, from an ELF or a System.map
```

A kernel is a raw image with no symbol table in it, so its names ship beside it;
give rysk either and a `pc` reads as `<schedule_timeout+0x4c>` rather than as a
number. Everything the window shows is read while the machine is stopped, so the
values are the machine's rather than a snapshot torn out from under a running hart,
and reading never touches a device: half the registers on this bus *do* something
when read, and a panel that polled them would eat the guest's input by showing it.

### Watch it execute

Tracing is a feature rather than a flag, because the spans cost several times what
interpreting the instruction does; without it they compile to nothing.

```bash
cargo install --path . --features trace
RUST_LOG=debug rysk prog.bin   # one line per instruction
RUST_LOG=trace rysk prog.bin   # bus loads and stores too
```

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

Memory starts at `0x8000_0000` and the devices sit where the QEMU `virt` machine
puts them. The guest is handed a device tree built from the same table that
attached them, so the two cannot disagree, and nothing about the guest is
special-cased: it enumerates the bus, walks page tables, takes interrupts and
starts its other harts the way it would on a board.

| what | how it works here |
|---|---|
| **RV64GC** | the base integer set, multiply, atomics, compressed instructions, and single and double floating point done in integers because no host rounds the five ways this machine can |
| **Zicsr, Zicntr, Zifencei** | the CSR instructions, `cycle`/`time`/`instret`, and instruction-fetch fencing |
| **Zicond, Zacas, Zabha, Zawrs** | conditional zeroing, compare-and-swap up to a quadword, byte and halfword atomics, and the wait-on-reservation hints |
| **Privilege modes** | machine, supervisor and user, with `medeleg` delegation and real `mret`/`sret`. A CSR either exists or raises an illegal instruction |
| **Sv39 paging** | permissions checked against the mode the access is for, with `MPRV`, `TVM`, `TW` and `TSR` all meaning something |
| **Several harts** | `-smp N`, a host thread each, ordered by the guest's own `fence` and atomics rather than by the schedule. A boot on four harts is 1.31x taking turns |

Interrupts come two ways, picked as QEMU picks them:

```bash
rysk --aia none          # a PLIC: wires all the way to the hart
rysk --aia aplic         # an APLIC delivering its own, a domain per level
rysk --aia aplic-imsic   # and forwarding them to an interrupt file per hart, as messages
```

With `aplic-imsic` the harts have `Smaia` and `Ssaia`. Devices on the PCI bus
declare their capabilities in a list the complex chains, master the bus without
going back through it, and say what they are asserting rather than interrupting
from inside their own access.

## Testing

```bash
cargo test    # the whole suite
make corpus   # fetch and build riscv-tests, which cargo test also runs
make test     # rebuild the fixtures and the corpus, then the same suite
```

`tests/isa.rs` is written here and needs nothing. `tests/compliance.rs` runs the
official [riscv-tests](https://github.com/riscv-software-src/riscv-tests) corpus,
134 programs across eight groups, and fails rather than skipping if the corpus is
missing. A test known not to pass is listed with what it waits on, and one that
starts passing is reported as a failure, so the list can only shrink.

`tests/common` is a small assembler, so a test is an array of instructions:

```rust
#[test]
fn immediate_shifts_take_six_bits_of_shift_amount() {
    let machine = prog(&[slli(T1, T0, 40)]).reg(T0, 1).run();
    assert_eq!(machine.reg(T1), 1 << 40);
}
```

Two fixtures come from a real toolchain, so the tests cannot agree with a
misunderstanding twice: `tests/encodings.s` is assembled ground truth the Rust
assembler is checked against, and `tests/fib.c` is a compiled C program. For
anything a real driver binds to there is a third check: boot the same image under
rysk and under `qemu-system-riscv64`, and diff what the driver says.

## Status

Working, and not finished. What is missing, roughly in the order it matters:

| missing | where it stands |
|---|---|
| **A raw terminal** | the serial port's terminal is line buffered, so typing at a guest shell arrives a line at a time |
| **A network** | there is no interface on the bus, so a guest has no way off the machine |
| **Determinism** | no run repeats another. `--schedule turns` fixes the order the harts run in, but the devices still advance with the wall clock |
| **A control channel** | the window can drive the machine and nothing else can: there is no socket to run it headless from, and no gdbstub |
| **Debug triggers** | the one corpus test that does not pass, `rv64mi-p-breakpoint`, wants them |
| **The hypervisor extension** | no H, so no VS mode and no guest interrupt files |

## License

AGPL-3.0. See [LICENSE](LICENSE).
