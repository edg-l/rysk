//! What a machine is made of. One table says what exists and where, and the bus is
//! built from it, so nothing else has to agree about an address.
//!
//! `virt` is the QEMU machine of the same name, which is where `DRAM_BASE` and every
//! other address here comes from.

use crate::{
    bus::Bus,
    clint::{self, Clint},
};

pub fn virt(bus: &mut Bus) {
    bus.attach(clint::BASE, clint::SIZE, Box::new(Clint::default()));
}
