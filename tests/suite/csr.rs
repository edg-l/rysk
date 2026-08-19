use crate::common::*;
use rysk::csr::{MISA, MISA_MXL_64, misa_extension};

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
