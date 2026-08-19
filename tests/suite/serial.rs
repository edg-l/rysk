use crate::common::*;
use rysk::{
    bus::DRAM_BASE,
    csr::{MEIE, MEIP, MIE, MIP, MSTATUS, MSTATUS_MIE, MTVEC},
    device::Line,
    plic::{self, Plic},
    trap::{Exception, INTERRUPT, Interrupt},
    uart::{self, Keyboard, Uart},
};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

// ------------------------------------------------------- the serial port

/// A backend a test can read back, standing in for the terminal the machine would
/// normally be talking to.
#[derive(Clone, Default)]
struct Printed(Arc<Mutex<Vec<u8>>>);

impl Write for Printed {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Printed {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// A machine with a serial port, its output captured, and `t0` pointing at it.
fn serial(code: &[u32], typed: Option<u8>) -> (Program, Printed) {
    let printed = Printed::default();
    let keyboard = Keyboard::default();
    if let Some(byte) = typed {
        keyboard.typed(&[byte]);
    }
    let port = Uart::new(Line::default(), keyboard, Box::new(printed.clone()));
    let program = prog(code)
        .device(uart::BASE, uart::SIZE, Box::new(port))
        .reg(T0, uart::BASE);
    (program, printed)
}

#[test]
fn a_byte_written_to_the_port_reaches_the_other_end() {
    let (program, printed) = serial(
        &[
            addi(T1, ZERO, b'h' as i32),
            sb(T1, T0, 0),
            addi(T1, ZERO, b'i' as i32),
            sb(T1, T0, 0),
        ],
        None,
    );
    program.run();
    assert_eq!(printed.text(), "hi");
}

#[test]
fn the_port_says_it_is_ready_to_send_and_has_nothing_to_read() {
    let (program, _) = serial(&[lbu(A0, T0, 5)], None);
    let machine = program.run();
    assert_eq!(machine.reg(A0) & 1, 0, "nothing has been typed");
    assert_ne!(machine.reg(A0) & (1 << 5), 0, "and it is ready to send");
}

#[test]
fn a_byte_that_arrives_is_read_once() {
    let (program, _) = serial(
        &[lbu(A0, T0, 5), lbu(A1, T0, 0), lbu(A2, T0, 5)],
        Some(b'x'),
    );
    let machine = program.run();
    assert_ne!(machine.reg(A0) & 1, 0, "the port says a byte is waiting");
    assert_eq!(
        machine.reg(A1),
        b'x' as u64,
        "which is the byte that was typed"
    );
    assert_eq!(machine.reg(A2) & 1, 0, "and reading it took it");
}

#[test]
fn the_divisor_latch_puts_the_baud_rate_where_the_data_registers_were() {
    let (program, printed) = serial(
        &[
            addi(T1, ZERO, 0x80),
            sb(T1, T0, 3),
            addi(T1, ZERO, 0x0d),
            sb(T1, T0, 0),
            lbu(A0, T0, 0),
            sb(ZERO, T0, 3),
            lbu(A1, T0, 0),
        ],
        None,
    );
    let machine = program.run();
    assert_eq!(
        machine.reg(A0),
        0x0d,
        "the divisor reads back with the latch open"
    );
    assert_eq!(printed.text(), "", "and nothing was sent");
    assert_eq!(
        machine.reg(A1),
        0,
        "with it closed the register is the port again"
    );
}

// ------------------------------------------------------- the controller

/// The interrupt source the serial port is wired to on the `virt` machine.
const UART_IRQ: u64 = 10;
const PRIORITY: i32 = 4 * UART_IRQ as i32;
const ENABLE: u64 = 0x2000;
const THRESHOLD: u64 = 0x20_0000;
const CLAIM: i32 = 4;

/// A machine with a serial port wired to source 10 of a controller. `t0` is the port,
/// `t1` the controller, `t2` its enable word and `t3` its machine context.
fn wired(code: &[u32], typed: Option<u8>) -> Program {
    let line = Line::default();
    let mut controller = Plic::new(1);
    controller.connect(UART_IRQ as usize, line.clone());
    let keyboard = Keyboard::default();
    if let Some(byte) = typed {
        keyboard.typed(&[byte]);
    }
    let port = Uart::new(line, keyboard, Box::new(io::sink()));
    prog(code)
        .device(uart::BASE, uart::SIZE, Box::new(port))
        .device(plic::BASE, plic::SIZE, Box::new(controller))
        .reg(T0, uart::BASE)
        .reg(T1, plic::BASE)
        .reg(T2, plic::BASE + ENABLE)
        .reg(T3, plic::BASE + THRESHOLD)
}

/// Give source 10 a priority, enable it for the machine context, and let the port
/// interrupt when a byte is waiting.
fn arm() -> [u32; 5] {
    [
        addi(T4, ZERO, 1),
        sw(T4, T1, PRIORITY),
        slli(T5, T4, UART_IRQ as u32),
        sw(T5, T2, 0),
        sb(T4, T0, 1),
    ]
}

#[test]
fn a_source_below_the_threshold_is_not_offered() {
    let armed = arm();
    let machine = wired(
        &[
            armed[0],
            armed[1],
            armed[2],
            armed[3],
            armed[4],
            // a threshold at the source's own priority masks it: the comparison is
            // strictly greater
            addi(T6, ZERO, 1),
            sw(T6, T3, 0),
            csrrs(A0, MIP as u32, ZERO),
            sw(ZERO, T3, 0),
            csrrs(A1, MIP as u32, ZERO),
        ],
        Some(b'!'),
    )
    .run();
    assert_eq!(machine.reg(A0) & MEIP, 0, "masked by the threshold");
    assert_ne!(machine.reg(A1) & MEIP, 0, "and offered once it is lowered");
}

#[test]
fn claiming_a_source_stops_it_being_offered_until_it_is_completed() {
    let armed = arm();
    let machine = wired(
        &[
            armed[0],
            armed[1],
            armed[2],
            armed[3],
            armed[4],
            lw(A0, T3, CLAIM),
            csrrs(A1, MIP as u32, ZERO),
            sw(A0, T3, CLAIM),
            csrrs(A2, MIP as u32, ZERO),
        ],
        Some(b'!'),
    )
    .run();
    assert_eq!(machine.reg(A0), UART_IRQ, "the claim named the serial port");
    assert_eq!(
        machine.reg(A1) & MEIP,
        0,
        "which is not offered again while claimed"
    );
    assert_ne!(
        machine.reg(A2) & MEIP,
        0,
        "and is offered once more after completing it, since the byte is still there"
    );
}

#[test]
fn a_typed_byte_becomes_an_external_interrupt() {
    let armed = arm();
    let machine = wired(
        &[
            armed[0],
            armed[1],
            armed[2],
            armed[3],
            armed[4],
            addi(A1, ZERO, 1),
            // the handler: claim the source, read the byte that caused it, complete
            // the claim, and let the run end
            lw(A0, T3, CLAIM),
            lbu(A2, T0, 0),
            sw(A0, T3, CLAIM),
            csrrw(ZERO, MIE as u32, ZERO),
            csrrw(ZERO, MTVEC as u32, ZERO),
        ],
        Some(b'!'),
    )
    .csr(MIE, MEIE)
    .csr(MSTATUS, 1 << MSTATUS_MIE)
    .csr(MTVEC, DRAM_BASE + 6 * 4)
    .expect(Exception::IllegalInstruction(0));
    assert_eq!(
        machine.reg(A1),
        0,
        "taken as soon as the port was told to interrupt"
    );
    assert_eq!(
        machine.reg(A0),
        UART_IRQ,
        "the controller named the serial port"
    );
    assert_eq!(
        machine.reg(A2),
        b'!' as u64,
        "and the byte was there to read"
    );
    assert_eq!(
        machine.csr(rysk::csr::MCAUSE),
        INTERRUPT | Interrupt::MachineExternal as u64
    );
}
