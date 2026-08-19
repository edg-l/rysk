//! What a machine is made of. One table says what exists, where it sits and which
//! interrupt line it drives, and the bus is built from it, so nothing else has to
//! agree separately about an address.
//!
//! `virt` is the QEMU machine of the same name, which is where `DRAM_BASE` and every
//! other address here comes from.

use std::io;

use crate::{
    bus::{Bus, DRAM_BASE},
    clint::{self, Clint},
    cpu::Cpu,
    device::Line,
    fdt::Fdt,
    plic::{self, Plic},
    uart::{self, Uart},
};

/// The interrupt source the first serial port drives, as the `virt` machine wires it.
const UART_IRQ: usize = 10;
/// How many interrupt sources the controller answers for, which is as many as the
/// machine wires.
const SOURCES: u32 = UART_IRQ as u32;

/// The phandles the tree refers to its two interrupt controllers by. A device says
/// which controller its line runs to, so the controllers need names.
const HART_INTC: u32 = 1;
const PLIC: u32 = 2;

/// Where the tree is left for the guest to find: high in dram, out of the way of an
/// image loaded at the bottom of it, and page aligned because a guest will map it.
pub fn fdt_base(memory: u64) -> u64 {
    DRAM_BASE + memory - 0x10_0000
}

/// Attach the machine's devices, describe them, and leave the description where the
/// guest is told to look: `a0` is the hart that is booting and `a1` is the tree, which
/// is the handover every RISC-V kernel expects from whatever ran before it.
/// What a guest is told that is not a device: the command line it was started with,
/// and where its initial ramdisk was left.
#[derive(Debug, Default)]
pub struct Boot {
    pub bootargs: Option<String>,
    pub initrd: Option<(u64, u64)>,
}

pub fn boot(cpu: &mut Cpu, isa: &str, options: &Boot) {
    virt(&mut cpu.bus);
    let memory = cpu.bus.dram.size();
    let at = fdt_base(memory);
    let tree = describe(isa, memory, options);
    assert!(
        cpu.bus.dram.write(at, &tree, 0),
        "the device tree does not fit in dram"
    );
    cpu.regs[10] = 0;
    cpu.regs[11] = at;
}

pub fn virt(bus: &mut Bus) {
    let serial = Line::default();
    let mut plic = Plic::new();
    plic.connect(UART_IRQ, serial.clone());

    bus.attach(clint::BASE, clint::SIZE, Box::new(Clint::default()));
    bus.attach(plic::BASE, plic::SIZE, Box::new(plic));
    bus.attach(
        uart::BASE,
        uart::SIZE,
        Box::new(Uart::new(serial, Box::new(io::stdout()))),
    );
}

/// The same machine, described. Firmware and a kernel read this to find what `virt`
/// attached above, so the two are written next to each other on purpose.
pub fn describe(isa: &str, memory: u64, options: &Boot) -> Vec<u8> {
    let mut fdt = Fdt::new();
    fdt.begin_node("");
    fdt.cells("#address-cells", &[2]);
    fdt.cells("#size-cells", &[2]);
    fdt.strings("compatible", &["riscv-virtio"]);
    fdt.string("model", "riscv-virtio,rysk");

    fdt.begin_node("chosen");
    fdt.string("stdout-path", "/soc/serial@10000000");
    if let Some(bootargs) = &options.bootargs {
        fdt.string("bootargs", bootargs);
    }
    // Where the ramdisk is, as two cells each, which is how a kernel finds the root
    // filesystem it was handed rather than one it has to go looking for.
    if let Some((start, end)) = options.initrd {
        fdt.cells("linux,initrd-start", &[(start >> 32) as u32, start as u32]);
        fdt.cells("linux,initrd-end", &[(end >> 32) as u32, end as u32]);
    }
    fdt.end_node();

    fdt.begin_node("cpus");
    fdt.cells("#address-cells", &[1]);
    fdt.cells("#size-cells", &[0]);
    // What `mtime` counts in, which is every timeout a guest will ever compute.
    fdt.cells("timebase-frequency", &[clint::FREQUENCY as u32]);
    fdt.begin_node("cpu@0");
    fdt.string("device_type", "cpu");
    fdt.cells("reg", &[0]);
    fdt.string("status", "okay");
    fdt.strings("compatible", &["riscv"]);
    fdt.string("riscv,isa", isa);
    fdt.string("mmu-type", "riscv,sv39");
    // The hart's own interrupt controller is the three bits of `mip` that reach it
    // directly, and it is what every other controller ultimately reports to.
    fdt.begin_node("interrupt-controller");
    fdt.cells("#interrupt-cells", &[1]);
    fdt.flag("interrupt-controller");
    fdt.strings("compatible", &["riscv,cpu-intc"]);
    fdt.cells("phandle", &[HART_INTC]);
    fdt.end_node();
    fdt.end_node();
    fdt.end_node();

    fdt.begin_node(&format!("memory@{DRAM_BASE:x}"));
    fdt.string("device_type", "memory");
    fdt.reg(DRAM_BASE, memory);
    fdt.end_node();

    fdt.begin_node("soc");
    fdt.cells("#address-cells", &[2]);
    fdt.cells("#size-cells", &[2]);
    fdt.strings("compatible", &["simple-bus"]);
    // An empty `ranges` means the addresses below are the ones the cpu uses.
    fdt.flag("ranges");

    fdt.begin_node(&format!("serial@{:x}", uart::BASE));
    fdt.strings("compatible", &["ns16550a"]);
    fdt.reg(uart::BASE, uart::SIZE);
    fdt.cells("interrupt-parent", &[PLIC]);
    fdt.cells("interrupts", &[UART_IRQ as u32]);
    // The rate a real part would divide down from. Nothing here measures time, but a
    // driver will not configure a port whose clock it does not know.
    fdt.cells("clock-frequency", &[3_686_400]);
    fdt.end_node();

    fdt.begin_node(&format!("plic@{:x}", plic::BASE));
    fdt.strings("compatible", &["riscv,plic0", "sifive,plic-1.0.0"]);
    fdt.reg(plic::BASE, plic::SIZE);
    fdt.flag("interrupt-controller");
    fdt.cells("#interrupt-cells", &[1]);
    fdt.cells("#address-cells", &[0]);
    // The two contexts it drives: this hart's external interrupt at machine level and
    // at supervisor level, causes eleven and nine.
    fdt.cells("interrupts-extended", &[HART_INTC, 11, HART_INTC, 9]);
    // How many sources it has, which is the highest one anything is wired to.
    fdt.cells("riscv,ndev", &[SOURCES]);
    fdt.cells("phandle", &[PLIC]);
    fdt.end_node();

    fdt.begin_node(&format!("clint@{:x}", clint::BASE));
    fdt.strings("compatible", &["riscv,clint0", "sifive,clint0"]);
    fdt.reg(clint::BASE, clint::SIZE);
    // Its two: the machine software and machine timer interrupts, three and seven.
    fdt.cells("interrupts-extended", &[HART_INTC, 3, HART_INTC, 7]);
    fdt.end_node();

    fdt.end_node();
    fdt.end_node();
    fdt.finish(&[])
}
