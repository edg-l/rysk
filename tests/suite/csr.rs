use crate::common::*;
use rysk::csr::{
    MISA, MISA_MXL_64, MSTATUS_MIE, MSTATUS_MPP, MSTATUS_MPP_M, MSTATUS_SIE, S_INTERRUPTS,
    misa_extension,
};

// ------------------------------------------------------------------ zicsr

const MSCRATCH: u32 = 0x340;

#[test]
fn csrrw_swaps_and_csrrs_sets_and_csrrc_clears() {
    let cpu = prog(&[
        csrrw(ZERO, MSCRATCH, T0),
        csrrs(T2, MSCRATCH, T1),
        csrrc(T3, MSCRATCH, T1),
        csrrs(T4, MSCRATCH, ZERO),
    ])
    .reg(T0, 0b1000)
    .reg(T1, 0b0101)
    .run();
    assert_eq!(cpu.reg(T2), 0b1000, "the old value is read back");
    assert_eq!(cpu.reg(T3), 0b1101, "csrrs set the low bits");
    assert_eq!(cpu.reg(T4), 0b1000, "csrrc cleared exactly the bits in rs1");
    assert_eq!(cpu.csrs[MSCRATCH as usize], 0b1000);
}

#[test]
fn the_immediate_csr_forms_use_the_rs1_field_as_a_value() {
    let cpu = run(&[
        csrrwi(ZERO, MSCRATCH, 0b11111),
        csrrci(T0, MSCRATCH, 0b01010),
        csrrsi(T1, MSCRATCH, 0b00010),
        csrrsi(T2, MSCRATCH, 0),
    ]);
    assert_eq!(cpu.reg(T0), 0b11111);
    assert_eq!(cpu.reg(T1), 0b10101);
    assert_eq!(cpu.reg(T2), 0b10111);
}

#[test]
fn a_csr_read_with_x0_as_the_source_does_not_write() {
    let cpu = prog(&[csrrw(ZERO, MSCRATCH, T0), csrrs(T1, MSCRATCH, ZERO)])
        .reg(T0, 0xabc)
        .run();
    assert_eq!(cpu.reg(T1), 0xabc);
    assert_eq!(cpu.csrs[MSCRATCH as usize], 0xabc);
}

#[test]
fn misa_reports_the_width_and_the_extensions_that_are_implemented() {
    let cpu = run(&[csrrs(A0, 0x301, ZERO)]);
    assert_eq!(cpu.reg(A0) >> 62, 2, "MXL of two means XLEN is 64");
    for letter in *b"ima" {
        assert_ne!(
            cpu.reg(A0) & misa_extension(letter),
            0,
            "{} should be reported",
            letter as char
        );
    }
    for letter in *b"fdc" {
        assert_eq!(
            cpu.reg(A0) & misa_extension(letter),
            0,
            "{} is not implemented",
            letter as char
        );
    }
}

#[test]
fn misa_ignores_writes_because_no_extension_can_be_turned_off() {
    let cpu = prog(&[csrrw(ZERO, 0x301, T0), csrrs(A0, 0x301, ZERO)])
        .reg(T0, 0)
        .run();
    assert_eq!(cpu.reg(A0), cpu.csrs[MISA]);
    assert_eq!(cpu.reg(A0) >> 62, 2);
    assert_eq!(cpu.reg(A0) & MISA_MXL_64, MISA_MXL_64);
}

// ------------------------------------------------- the supervisor's view

const MSTATUS: u32 = 0x300;
const SSTATUS: u32 = 0x100;
const MIE: u32 = 0x304;
const SIE: u32 = 0x104;
const MIP: u32 = 0x344;
const SIP: u32 = 0x144;
const MIDELEG: u32 = 0x303;

const SSIP: u64 = 1 << 1;
const STIP: u64 = 1 << 5;
const MSIP: u64 = 1 << 3;
const MTIP: u64 = 1 << 7;

#[test]
fn mideleg_holds_only_the_interrupts_that_can_be_delegated() {
    let cpu = prog(&[csrrw(ZERO, MIDELEG, T0), csrrs(A0, MIDELEG, ZERO)])
        .reg(T0, !0)
        .run();
    assert_eq!(
        cpu.reg(A0),
        S_INTERRUPTS,
        "the machine-level and unimplemented bits are read-only zero"
    );
}

#[test]
fn sie_shows_only_the_interrupts_mideleg_delegates() {
    let cpu = prog(&[
        csrrw(ZERO, MIDELEG, T0),
        csrrw(ZERO, MIE, T1),
        csrrs(A0, SIE, ZERO),
    ])
    .reg(T0, STIP)
    .reg(T1, !0)
    .run();
    assert_eq!(cpu.reg(A0), STIP, "only the delegated timer interrupt");
}

#[test]
fn writing_sie_writes_mie_and_leaves_the_undelegated_bits_alone() {
    let cpu = prog(&[
        csrrw(ZERO, MIDELEG, T0),
        csrrw(ZERO, MIE, T1),
        csrrw(ZERO, SIE, ZERO),
        csrrs(A0, MIE, ZERO),
    ])
    .reg(T0, STIP)
    .reg(T1, STIP | MTIP)
    .run();
    assert_eq!(
        cpu.reg(A0),
        MTIP,
        "stie cleared through sie, mtie untouched"
    );
}

#[test]
fn sip_is_a_window_onto_mip_and_not_a_register_of_its_own() {
    let cpu = prog(&[
        csrrw(ZERO, MIDELEG, T0),
        csrrw(ZERO, MIP, T1),
        csrrs(A0, SIP, ZERO),
        csrrw(ZERO, SIP, ZERO),
        csrrs(A1, MIP, ZERO),
    ])
    .reg(T0, SSIP)
    .reg(T1, SSIP | MSIP)
    .run();
    assert_eq!(
        cpu.reg(A0),
        SSIP,
        "the delegated software interrupt shows through"
    );
    assert_eq!(
        cpu.reg(A1),
        MSIP,
        "clearing sip cleared it in mip, not in storage of its own"
    );
}

#[test]
fn sstatus_shows_the_supervisor_fields_of_mstatus_and_no_others() {
    let cpu = prog(&[csrrw(ZERO, MSTATUS, T0), csrrs(A0, SSTATUS, ZERO)])
        .reg(T0, (1 << MSTATUS_SIE) | (1 << MSTATUS_MIE) | MSTATUS_MPP_M)
        .run();
    assert_eq!(
        cpu.reg(A0),
        1 << MSTATUS_SIE,
        "mie and mpp are machine state a supervisor cannot see"
    );
}

#[test]
fn writing_sstatus_leaves_the_machine_fields_of_mstatus_alone() {
    let cpu = prog(&[
        csrrw(ZERO, MSTATUS, T0),
        csrrw(ZERO, SSTATUS, T1),
        csrrs(A0, MSTATUS, ZERO),
    ])
    .reg(T0, (1 << MSTATUS_MIE) | MSTATUS_MPP_M)
    .reg(T1, !0)
    .run();
    assert_ne!(
        cpu.reg(A0) & (1 << MSTATUS_MIE),
        0,
        "mie survives a write of ones through sstatus"
    );
    assert_eq!(cpu.reg(A0) & MSTATUS_MPP, MSTATUS_MPP_M, "and so does mpp");
    assert_ne!(
        cpu.reg(A0) & (1 << MSTATUS_SIE),
        0,
        "while the supervisor fields did take the write"
    );
}
