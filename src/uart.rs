//! A 16550 UART: the register model, and nothing about where its bytes come from or
//! go. The backend is a `Write` it was handed, so the same device serves a terminal,
//! a log file and a test that reads back what a program printed.
//!
//! What a 16550 is is not written down in a RISC-V specification; this follows the
//! part, which every operating system already has a driver for.

use std::{
    collections::VecDeque,
    io::Write,
    sync::{Arc, Mutex},
};

use crate::{
    device::{Device, Line, Report, Value, field},
    trap::Exception,
};

/// What has been typed and not yet read.
///
/// Shared, because whatever is doing the typing is not the hart: a terminal on another
/// thread, a window, or a test handing over a line at a time. The port is the model
/// and this is the end of it that faces the world, which is the same split as the
/// `Write` it sends to.
#[derive(Debug, Clone, Default)]
pub struct Keyboard(Arc<Mutex<VecDeque<u8>>>);

impl Keyboard {
    /// Hand the port some bytes, as typing them would.
    pub fn typed(&self, bytes: &[u8]) {
        self.0.lock().unwrap().extend(bytes);
    }

    fn take(&self) -> Option<u8> {
        self.0.lock().unwrap().pop_front()
    }

    fn waiting(&self) -> bool {
        !self.0.lock().unwrap().is_empty()
    }
}

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
    keyboard: Keyboard,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: u16,
}

impl std::fmt::Debug for Uart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uart")
            .field("waiting", &self.keyboard.waiting())
            .finish()
    }
}

impl Uart {
    pub fn new(line: Line, keyboard: Keyboard, out: Box<dyn Write + Send>) -> Self {
        Self {
            out,
            line,
            keyboard,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            divisor: 1,
        }
    }

    /// Which interrupt the port is reporting, if any. The transmitter is always idle,
    /// so an enabled transmit interrupt is always asserted.
    fn interrupt(&self) -> u8 {
        if self.ier & IER_RX != 0 && self.keyboard.waiting() {
            IIR_RX
        } else if self.ier & IER_TX != 0 {
            IIR_TX
        } else {
            IIR_NONE
        }
    }

    fn update(&self) {
        self.line.set(self.interrupt() != IIR_NONE);
    }

    fn status(&self) -> u8 {
        let mut lsr = LSR_TX_EMPTY | LSR_IDLE;
        if self.keyboard.waiting() {
            lsr |= LSR_RX;
        }
        lsr
    }
}

impl Device for Uart {
    fn describe(&self) -> Report {
        Report::new(
            "uart",
            vec![
                field("typed waiting", Value::Flag(self.keyboard.waiting())),
                field("lsr", Value::Bits(u64::from(self.status()))),
                field("ier", Value::Bits(u64::from(self.ier))),
                field("lcr", Value::Bits(u64::from(self.lcr))),
                field("mcr", Value::Bits(u64::from(self.mcr))),
                field("scr", Value::Bits(u64::from(self.scr))),
                field("divisor", Value::Count(u64::from(self.divisor))),
                field("interrupting", Value::Flag(self.line.is_raised())),
            ],
        )
    }

    fn load(&mut self, offset: u64, _size: u64) -> Result<u64, Exception> {
        let latched = self.lcr & LCR_DLAB != 0;
        let value = match offset {
            RBR if latched => self.divisor as u8,
            RBR => self.keyboard.take().unwrap_or(0),
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

    /// Nothing in `mip` is the port's to drive: its line runs to a controller, which
    /// is what drives one. What it needs told is that time has passed, because a byte
    /// can arrive from the other end while the hart is doing something else, or
    /// nothing at all.
    fn poll(&mut self) {
        self.update();
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
