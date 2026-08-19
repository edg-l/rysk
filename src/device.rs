//! What the bus talks to, other than dram.
//!
//! A device is a model behind this trait and nothing else: it is told what was read or
//! written at an offset from wherever it was placed, and it says whether it is
//! asserting an interrupt. It never learns its own address, which controller its line
//! runs to, or what else exists.

use crate::exception::Exception;

pub trait Device: std::fmt::Debug {
    /// Read `size` bits at `offset` from the device's base.
    ///
    /// This takes `&mut self` because a read can be an action rather than a question:
    /// a uart's receive register consumes the byte it returns, and a plic's claim
    /// register hands over an interrupt.
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception>;

    /// Write `size` bits at `offset` from the device's base.
    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception>;

    /// Whether the device is asserting its interrupt line. A device with no line of its
    /// own answers by never asserting one.
    fn pending(&self) -> bool {
        false
    }
}
