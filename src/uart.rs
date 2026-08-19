//! A 16550 UART: the register model, and nothing about where its bytes come from or
//! go. The backend is a `Write` it was handed, so the same device serves a terminal,
//! a log file and a test that reads back what a program printed.
//!
//! What a 16550 is is not written down in a RISC-V specification; this follows the
//! part, which every operating system already has a driver for.

use std::io::Write;

use crate::{
    device::{Device, Line},
    trap::Exception,
};

pub const BASE: u64 = 0x1000_0000;
pub const SIZE: u64 = 0x100;

/// Receive buffer when reading, transmit holding register when writing, and the low
/// half of the divisor when the latch is open.
const RBR: u64 = 0;
/// Interrupt enable, or the high half of the divisor.
const IER: u64 = 1;
/// Interrupt identification when reading, fifo control when writing.
const IIR: u64 = 2;
/// Line control. Its top bit is the divisor latch.
const LCR: u64 = 3;
const MCR: u64 = 4;
/// Line status: what the port is ready to do.
const LSR: u64 = 5;
const MSR: u64 = 6;
const SCR: u64 = 7;

/// Bits of `IER`: a byte arrived, and the transmitter went idle.
const IER_RX: u8 = 1 << 0;
const IER_TX: u8 = 1 << 1;
/// Bits of `LSR`: a byte is waiting, the holding register is empty, and the shift
/// register is too. The last two are always true here because a write is a write.
const LSR_RX: u8 = 1 << 0;
const LSR_TX_EMPTY: u8 = 1 << 5;
const LSR_IDLE: u8 = 1 << 6;
/// The divisor latch, which turns the first two registers into the baud rate.
const LCR_DLAB: u8 = 1 << 7;

/// The interrupt codes of `IIR`, in the priority the part defines. Bit 0 clear means
/// an interrupt is being reported at all.
const IIR_NONE: u8 = 0b0001;
const IIR_TX: u8 = 0b0010;
const IIR_RX: u8 = 0b0100;

pub struct Uart {
    out: Box<dyn Write + Send>,
    line: Line,
    /// What has been typed and not yet read. One byte, which is what a 16550 without
    /// its fifo enabled holds.
    rx: Option<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: u16,
}

impl std::fmt::Debug for Uart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uart").field("rx", &self.rx).finish()
    }
}

impl Uart {
    pub fn new(line: Line, out: Box<dyn Write + Send>) -> Self {
        Self {
            out,
            line,
            rx: None,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            divisor: 1,
        }
    }

    /// Hand the port a byte, as typing one would. It is dropped if the last has not
    /// been read yet, which is the overrun a real port reports and a guest that is not
    /// keeping up deserves.
    pub fn receive(&mut self, byte: u8) {
        if self.rx.is_none() {
            self.rx = Some(byte);
        }
        self.update();
    }

    /// Which interrupt the port is reporting, if any. The transmitter is always idle,
    /// so an enabled transmit interrupt is always asserted.
    fn interrupt(&self) -> u8 {
        if self.ier & IER_RX != 0 && self.rx.is_some() {
            IIR_RX
        } else if self.ier & IER_TX != 0 {
            IIR_TX
        } else {
            IIR_NONE
        }
    }

    fn update(&mut self) {
        self.line.set(self.interrupt() != IIR_NONE);
    }

    fn status(&self) -> u8 {
        let mut lsr = LSR_TX_EMPTY | LSR_IDLE;
        if self.rx.is_some() {
            lsr |= LSR_RX;
        }
        lsr
    }
}

impl Device for Uart {
    fn load(&mut self, offset: u64, _size: u64) -> Result<u64, Exception> {
        let latched = self.lcr & LCR_DLAB != 0;
        let value = match offset {
            RBR if latched => self.divisor as u8,
            RBR => self.rx.take().unwrap_or(0),
            IER if latched => (self.divisor >> 8) as u8,
            IER => self.ier,
            IIR => self.interrupt(),
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => self.status(),
            // Nothing is wired to the modem control lines, so they read as the port
            // being ready and nothing having changed.
            MSR => 0b1011_0000,
            SCR => self.scr,
            _ => return Err(Exception::LoadAccessFault(offset)),
        };
        self.update();
        Ok(value as u64)
    }

    fn store(&mut self, offset: u64, _size: u64, value: u64) -> Result<(), Exception> {
        let byte = value as u8;
        let latched = self.lcr & LCR_DLAB != 0;
        match offset {
            RBR if latched => self.divisor = (self.divisor & 0xff00) | byte as u16,
            RBR => {
                let _ = self.out.write_all(&[byte]);
                let _ = self.out.flush();
            }
            IER if latched => self.divisor = (self.divisor & 0x00ff) | ((byte as u16) << 8),
            IER => self.ier = byte,
            // There is no fifo to control, and the part ignores what it cannot do.
            IIR => {}
            LCR => self.lcr = byte,
            MCR => self.mcr = byte,
            // Line and modem status are what the port reports, not what it is told.
            LSR | MSR => {}
            SCR => self.scr = byte,
            _ => return Err(Exception::StoreAmoAccessFault(offset)),
        }
        self.update();
        Ok(())
    }
}
