//! The inspection surface: what a window's panels, a control channel and a debugger
//! all ask a machine, and what it answers.

use crate::common::*;
use rysk::{
    bus::DRAM_BASE,
    csr::{MTVEC, Mode},
    debug::{Address, Session, Stop, Symbols},
    device::{Device, Report, Value},
    machine::State,
    trap::Exception,
};

fn session(code: &[u32]) -> Session {
    Session::new(prog(code).machine())
}

// -------------------------------------------------------------- registers

#[test]
fn registers_are_read_where_the_machine_stopped() {
    let mut session = session(&[addi(A0, ZERO, 7), addi(A1, ZERO, 9)]);
    session.step(0);
    session.step(0);

    let regs = session.registers(0);
    assert_eq!(regs.x[A0 as usize], 7);
    assert_eq!(regs.x[A1 as usize], 9);
    assert_eq!(regs.x[0], 0, "x0 is always zero");
    assert_eq!(regs.pc, DRAM_BASE + 8, "and pc is what runs next");
    assert_eq!(regs.mode, Mode::Machine);
    assert_eq!(regs.instret, 2);
}

#[test]
fn a_named_control_register_is_read_without_the_side_effect_of_reading_it() {
    let mut session = session(&[csrrw(ZERO, MTVEC as u32, T0)]);
    session.machine_mut().harts[0].regs[T0 as usize] = DRAM_BASE + 0x40;
    session.step(0);

    let named = session.csrs(0);
    let mtvec = named
        .iter()
        .find(|csr| csr.name == "mtvec")
        .expect("mtvec is one a person reads");
    assert_eq!(mtvec.value, DRAM_BASE + 0x40);
    assert_eq!(mtvec.addr, MTVEC);
    assert_eq!(session.csr(0, MTVEC), mtvec.value, "and one at a time");
}

// ------------------------------------------------------------------ memory

#[test]
fn memory_reads_what_is_there_and_says_where_nothing_is() {
    let mut session = Session::new(
        prog(&[nop()])
            .memory(SCRATCH, 0x0123_4567_89ab_cdef)
            .machine(),
    );

    let window = session.memory(0, Address::Physical(SCRATCH), 8);
    assert_eq!(window.word(0, 8), Some(0x0123_4567_89ab_cdef));
    assert_eq!(window.word(0, 2), Some(0xcdef), "and any part of it");

    // Below dram there is no memory, only the addresses devices answer at.
    let nothing = session.memory(0, Address::Physical(0x1000), 4);
    assert!(
        nothing.bytes.iter().all(Option::is_none),
        "nothing answered, and it says so rather than guessing"
    );
    assert_eq!(nothing.word(0, 4), None);
}

/// A device that counts the reads it is given, so that a test can prove it was not.
#[derive(Debug, Default)]
struct Counting(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Device for Counting {
    fn describe(&self) -> Report {
        Report::new(
            "counting",
            vec![rysk::device::field(
                "reads",
                Value::Count(self.0.load(std::sync::atomic::Ordering::Relaxed)),
            )],
        )
    }

    fn load(&mut self, _offset: u64, _size: u64) -> Result<u64, Exception> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(0)
    }

    fn store(&mut self, _offset: u64, _size: u64, _value: u64) -> Result<(), Exception> {
        Ok(())
    }
}

#[test]
fn reading_memory_never_reads_a_device() {
    // The reason the rule exists: a read of a serial port's receive register consumes
    // the byte, and a read of an interrupt file's top register claims the interrupt.
    // An inspector that reached them would eat the guest's input by looking at it.
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut session = Session::new(
        prog(&[nop()])
            .device(0x1000_0000, 0x1000, Box::new(Counting(reads.clone())))
            .machine(),
    );

    let window = session.memory(0, Address::Physical(0x1000_0000), 16);
    assert!(window.bytes.iter().all(Option::is_none), "it read nothing");
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "and the device was never touched"
    );
}

// ------------------------------------------------------------- disassembly

#[test]
fn disassembly_decodes_out_of_memory_and_resolves_what_it_can() {
    let mut session = session(&[addi(A0, ZERO, 1), jal(RA, 8), addi(A1, ZERO, 2)]);
    let lines = session.disassemble(0, Address::Physical(DRAM_BASE), 3);

    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].at, DRAM_BASE);
    assert_eq!(lines[0].length, 4);
    assert_eq!(
        lines[0].inst.expect("it decodes").to_string(),
        "addi a0, zero, 1"
    );
    assert_eq!(lines[0].target, None, "an addi goes nowhere");

    // `Display for Inst` prints a jump's offset, since an instruction does not know
    // where it is. Resolving it against the address is what this adds.
    assert_eq!(lines[1].target, Some(DRAM_BASE + 4 + 8));
    assert_eq!(lines[2].at, DRAM_BASE + 8);
}

#[test]
fn disassembly_follows_the_length_of_a_compressed_instruction() {
    // `c.addi a0, 1` then `c.nop`, two bytes each, so a disassembler that assumed four
    // would read the second from the middle of nothing.
    let mut session = Session::new(halves(&[0x0505, 0x0001]).machine());
    let lines = session.disassemble(0, Address::Physical(DRAM_BASE), 2);

    assert_eq!(lines[0].length, 2);
    assert_eq!(lines[1].at, DRAM_BASE + 2, "and the next one is two along");
    assert_eq!(lines[1].length, 2);
}

// ----------------------------------------------------------------- symbols

#[test]
fn a_symbol_names_the_addresses_up_to_the_next_one() {
    let symbols = Symbols::new([
        ("start".to_owned(), 0x8000_0000),
        ("main".to_owned(), 0x8000_0100),
    ]);

    assert_eq!(symbols.address("main"), Some(0x8000_0100));
    assert_eq!(symbols.address("absent"), None);
    assert_eq!(symbols.nearest(0x8000_0000), Some(("start", 0)));
    assert_eq!(symbols.nearest(0x8000_00ff), Some(("start", 0xff)));
    assert_eq!(
        symbols.nearest(0x8000_0100),
        Some(("main", 0)),
        "the next one begins where the one before it ends"
    );
    assert_eq!(
        symbols.nearest(0x7fff_ffff),
        None,
        "before everything named is not in anything"
    );
    assert_eq!(
        symbols.nearest(0x9000_0000),
        None,
        "and neither is far past the last, which has a bounded span rather than the \
         rest of the address space"
    );
}

#[test]
fn the_last_name_given_for_an_address_is_the_one_it_resolves_to() {
    // Which is what lets a `--symbols` file beat the image's own table: an address that
    // answers with two names answers with neither, so one of them has to win, and the
    // one asked for by hand is handed over last.
    let symbols = Symbols::new([
        ("from_the_image".to_owned(), 0x8000_0100),
        ("from_the_file".to_owned(), 0x8000_0100),
    ]);

    assert_eq!(symbols.nearest(0x8000_0100), Some(("from_the_file", 0)));
    // Both are still names, so a breakpoint can be set on either.
    assert_eq!(symbols.address("from_the_image"), Some(0x8000_0100));
    assert_eq!(symbols.address("from_the_file"), Some(0x8000_0100));
}

// -------------------------------------------------------------- breakpoints

#[test]
fn a_breakpoint_stops_before_the_instruction_it_is_on() {
    let mut session = session(&[addi(A0, ZERO, 1), addi(A0, ZERO, 2), addi(A0, ZERO, 3)]);
    session.set_breakpoint(DRAM_BASE + 8);

    assert_eq!(
        session.resume(),
        Stop::Breakpoint {
            hart: 0,
            pc: DRAM_BASE + 8
        }
    );
    assert_eq!(
        session.registers(0).x[A0 as usize],
        2,
        "the instruction before it ran, and the one it is on did not"
    );
    assert_eq!(session.state(), State::Paused, "and it can carry on");
}

#[test]
fn resuming_from_a_breakpoint_carries_on_past_it() {
    // Otherwise a breakpoint inside a loop is one nothing can step out of: the resume
    // would find the same address under the same pc and stop again without moving.
    const TOP: u64 = DRAM_BASE + 4;
    let mut session = session(&[
        addi(A0, ZERO, 0),
        addi(A0, A0, 1), // the top of the loop, and where the breakpoint goes
        jal(ZERO, -4),
    ]);
    session.set_breakpoint(TOP);

    assert_eq!(session.resume(), Stop::Breakpoint { hart: 0, pc: TOP });
    assert_eq!(session.registers(0).x[A0 as usize], 0, "it has not run yet");

    assert_eq!(
        session.resume(),
        Stop::Breakpoint { hart: 0, pc: TOP },
        "round the loop and back to it"
    );
    assert_eq!(session.registers(0).x[A0 as usize], 1, "having run once");

    assert_eq!(session.resume(), Stop::Breakpoint { hart: 0, pc: TOP });
    assert_eq!(session.registers(0).x[A0 as usize], 2);
}

#[test]
fn a_breakpoint_can_be_set_by_the_name_of_a_symbol() {
    let mut session = Session::with_symbols(
        prog(&[addi(A0, ZERO, 1), addi(A0, ZERO, 2)]).machine(),
        Symbols::new([("second".to_owned(), DRAM_BASE + 4)]),
    );

    assert_eq!(session.break_at("second"), Some(DRAM_BASE + 4));
    assert_eq!(session.break_at("nothing_called_this"), None);
    assert_eq!(
        session.resume(),
        Stop::Breakpoint {
            hart: 0,
            pc: DRAM_BASE + 4
        }
    );
}

#[test]
fn a_cleared_breakpoint_stops_nothing() {
    let mut session = session(&[addi(A0, ZERO, 1), addi(A0, ZERO, 2)]);
    session.set_breakpoint(DRAM_BASE + 4);
    assert!(session.clear_breakpoint(DRAM_BASE + 4));
    assert!(!session.clear_breakpoint(DRAM_BASE + 4), "and only once");

    // With nothing left to watch for, the run ends where a run ends: on the zeroed
    // word past the program.
    assert!(matches!(session.resume(), Stop::Halted(_)));
}

// ------------------------------------------------------------- watchpoints

#[test]
fn a_watchpoint_says_what_changed_and_what_it_was() {
    let mut session = Session::new(
        prog(&[addi(T1, ZERO, 42), sd(T1, T0, 0), addi(A0, ZERO, 1)])
            .reg(T0, SCRATCH)
            .machine(),
    );
    session.set_watchpoint(Address::Physical(SCRATCH), 8);

    assert_eq!(
        session.resume(),
        Stop::Watchpoint {
            hart: 0,
            at: Address::Physical(SCRATCH),
            was: 0,
            now: 42,
        }
    );
    assert_eq!(
        session.registers(0).pc,
        DRAM_BASE + 8,
        "it stopped after the store that did it"
    );
}

#[test]
fn a_watchpoint_that_nothing_writes_stops_nothing() {
    let mut session = Session::new(
        prog(&[addi(T1, ZERO, 42), sd(T1, T0, 0)])
            .reg(T0, SCRATCH)
            .machine(),
    );
    // Watching the word after the one the program writes.
    session.set_watchpoint(Address::Physical(SCRATCH + 8), 8);
    assert!(matches!(session.resume(), Stop::Halted(_)));
}

// ---------------------------------------------------------------- stepping

#[test]
fn a_step_retires_one_instruction() {
    let mut session = session(&[addi(A0, ZERO, 1), addi(A0, ZERO, 2)]);
    assert_eq!(
        session.step(0),
        Stop::Stepped {
            hart: 0,
            retired: 1
        }
    );
    assert_eq!(session.registers(0).x[A0 as usize], 1);
    assert_eq!(session.registers(0).pc, DRAM_BASE + 4);
}

#[test]
fn stepping_to_a_trap_stops_in_the_handler_and_says_what_it_was() {
    const HANDLER: u64 = DRAM_BASE + 4 * 4;
    let mut session = Session::new(
        prog(&[
            csrrw(ZERO, MTVEC as u32, T0),
            addi(A0, ZERO, 1),
            ecall(),
            addi(A1, ZERO, 1),
            // handler
            addi(A2, ZERO, 1),
        ])
        .reg(T0, HANDLER)
        .machine(),
    );

    let Stop::Trapped { hart, taken } = session.step_to_trap(0, 100) else {
        panic!("it should have reached the ecall");
    };
    assert_eq!(hart, 0);
    assert_eq!(taken.trap, Exception::EnvironmentCall(Mode::Machine).into());
    assert_eq!(taken.pc, DRAM_BASE + 8, "the ecall");
    assert_eq!(taken.handler, HANDLER);
    assert_eq!(
        session.registers(0).pc,
        HANDLER,
        "and it is sitting at the handler's first instruction"
    );
}

#[test]
fn stepping_to_a_trap_that_never_comes_gives_up_after_the_limit() {
    let mut session = session(&[addi(A0, A0, 1), jal(ZERO, -4)]);
    assert_eq!(
        session.step_to_trap(0, 20),
        Stop::Stepped {
            hart: 0,
            retired: 20
        }
    );
}

// --------------------------------------------------------------- the trap log

#[test]
fn the_traps_a_hart_took_are_read_newest_first() {
    const HANDLER: u64 = DRAM_BASE + 5 * 4;
    let mut session = Session::new(
        prog(&[
            csrrw(ZERO, MTVEC as u32, T0),
            ecall(),
            ebreak(),
            csrrw(ZERO, MTVEC as u32, ZERO),
            jal(ZERO, 16),
            // handler: step over whatever raised it and return
            csrrs(T1, 0x341, ZERO),
            addi(T1, T1, 4),
            csrrw(ZERO, 0x341, T1),
            mret(),
        ])
        .reg(T0, HANDLER)
        .machine(),
    );
    session.resume();

    let traps = session.traps(0);
    assert_eq!(session.traps_taken(0), 2);
    assert_eq!(traps.len(), 2);
    assert_eq!(
        traps[0].trap,
        Exception::Breakpoint(DRAM_BASE + 8).into(),
        "newest first, which is the ebreak"
    );
    assert_eq!(
        traps[1].trap,
        Exception::EnvironmentCall(Mode::Machine).into(),
        "and the ecall before it"
    );
    assert_eq!(traps[1].seq, 0);
}

// -------------------------------------------------------------- the devices

#[test]
fn the_device_map_says_where_each_device_is_and_what_it_is_doing() {
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let session = Session::new(
        prog(&[nop()])
            .device(0x1000_0000, 0x1000, Box::new(Counting(reads.clone())))
            .machine(),
    );

    let devices = session.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "counting");
    assert_eq!(
        (devices[0].base, devices[0].size),
        (0x1000_0000, 0x1000),
        "the bus fills the range in, since a device never learns its own base"
    );
    assert_eq!(devices[0].field("reads"), Some(&Value::Count(0)));
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "and asking did not count as a read"
    );
}
