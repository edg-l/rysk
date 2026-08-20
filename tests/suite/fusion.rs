//! The pairs a block is built out of as one instruction, against the two instructions
//! they stand for. Every one of them has to leave the machine where the pair would.

use crate::common::*;
use rysk::{bus::DRAM_BASE, trap::Exception};

#[test]
fn lui_and_addi_make_one_constant() {
    let machine = prog(&[lui(T0, 0x12345), addi(T0, T0, 0x678)]).run();
    assert_eq!(machine.reg(T0), 0x1234_5678);
}

#[test]
fn lui_and_addiw_make_one_thirty_two_bit_constant() {
    // The sum has bit 31 set before `addiw` narrows it, so a pair fused as if it were
    // `addi` would keep the sign bits the `lui` had and this would be negative.
    let machine = prog(&[lui(T0, 0x80000), addiw(T0, T0, -1)]).run();
    assert_eq!(machine.reg(T0), 0x7fff_ffff);
}

#[test]
fn auipc_and_addi_make_one_address() {
    let machine = prog(&[auipc(T0, 0), addi(T0, T0, 8)]).run();
    assert_eq!(
        machine.reg(T0),
        DRAM_BASE + 8,
        "relative to the auipc, which is where the pair is"
    );
}

#[test]
fn auipc_and_jalr_make_one_call() {
    let machine = prog(&[
        auipc(RA, 0),
        jalr(RA, RA, 12),
        addi(A0, ZERO, 1),
        addi(A1, ZERO, 1),
    ])
    .run();
    assert_eq!(machine.reg(A1), 1, "it called what the pair adds up to");
    assert_eq!(machine.reg(A0), 0, "over the instruction in between");
    assert_eq!(machine.reg(RA), DRAM_BASE + 8, "and linked past the pair");
}

#[test]
fn a_pair_that_keeps_what_the_first_of_it_wrote_is_two_instructions() {
    let machine = prog(&[lui(T0, 0x12345), addi(T1, T0, 0x678)]).run();
    assert_eq!(machine.reg(T0), 0x1234_5000, "the first still happened");
    assert_eq!(machine.reg(T1), 0x1234_5678);
}

#[test]
fn a_pair_through_x0_passes_nothing_along() {
    // `x0` is not written, so the `jalr` adds its offset to zero rather than to the
    // address the `auipc` computed, and the jump lands where nothing is.
    prog(&[auipc(ZERO, 0x1000), jalr(ZERO, ZERO, 8)]).expect(Exception::InstructionAccessFault(8));
}

#[test]
fn a_fused_pair_retires_as_two_instructions() {
    let machine = prog(&[
        lui(T0, 0x12345),
        addi(T0, T0, 0x678),
        csrrs(A0, rysk::csr::MINSTRET as u32, ZERO),
    ])
    .run();
    assert_eq!(machine.reg(A0), 2, "both halves of the pair counted");
}
