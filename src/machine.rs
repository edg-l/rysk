//! What a machine is made of. One table says what exists, where it sits and which
//! interrupt line it drives, and the bus is built from it, so nothing else has to
//! agree separately about an address.
//!
//! `virt` is the QEMU machine of the same name, which is where `DRAM_BASE` and every
//! other address here comes from.

use std::io;

use crate::{
    bus::Bus,
    clint::{self, Clint},
    device::Line,
    plic::{self, Plic},
    uart::{self, Uart},
};

/// The interrupt source the first serial port drives, as the `virt` machine wires it.
const UART_IRQ: usize = 10;

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
