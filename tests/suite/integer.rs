use crate::common::*;
use rysk::{bus::DRAM_BASE, dram::DRAM_SIZE};

// ------------------------------------------------------------------ rv64i, integer

#[test]
fn addi_sign_extends_its_immediate() {
    let machine = run(&[
        addi(T0, ZERO, -1),
        addi(T1, ZERO, 2047),
        addi(T2, ZERO, -2048),
    ]);
    assert_eq!(machine.reg(T0), u64::MAX);
    assert_eq!(machine.reg(T1), 2047);
    assert_eq!(machine.reg(T2), (-2048i64) as u64);
}

#[test]
fn add_and_sub_wrap() {
    let machine = prog(&[add(T2, T0, T1), sub(T3, T0, T1)])
        .reg(T0, 0)
        .reg(T1, 1)
        .run();
    assert_eq!(machine.reg(T2), 1);
    assert_eq!(machine.reg(T3), u64::MAX);
}

#[test]
fn the_bitwise_operations_and_their_immediate_forms_agree() {
    let machine = prog(&[
        and(A0, T0, T1),
        or(A1, T0, T1),
        xor(A2, T0, T1),
        andi(A3, T0, 0b0101),
        ori(A4, T0, 0b0101),
        xori(A5, T0, 0b0101),
    ])
    .reg(T0, 0b1100)
    .reg(T1, 0b0101)
    .run();
    assert_eq!(machine.reg(A0), 0b0100);
    assert_eq!(machine.reg(A1), 0b1101);
    assert_eq!(machine.reg(A2), 0b1001);
    assert_eq!(machine.reg(A3), 0b0100);
    assert_eq!(machine.reg(A4), 0b1101);
    assert_eq!(machine.reg(A5), 0b1001);
}

#[test]
fn an_access_outside_dram_stops_the_machine_rather_than_panicking() {
    let below = prog(&[ld(T1, T0, 0), addi(T2, ZERO, 1)]).reg(T0, 0).run();
    assert_eq!(below.reg(T2), 0, "execution stopped at the faulting load");

    let straddling = prog(&[ld(T1, T0, 0), addi(T2, ZERO, 1)])
        .reg(T0, DRAM_BASE + DRAM_SIZE - 4)
        .run();
    assert_eq!(
        straddling.reg(T2),
        0,
        "a load may not run off the top of dram"
    );
}

#[test]
fn writes_to_x0_are_discarded() {
    let machine = run(&[addi(ZERO, ZERO, 42), lui(ZERO, 1), jal(ZERO, 4)]);
    assert_eq!(machine.reg(ZERO), 0);
}

#[test]
fn slt_compares_signed_and_sltu_unsigned() {
    let machine = prog(&[
        slt(T2, T0, T1),
        sltu(T3, T0, T1),
        slti(T4, T0, 1),
        sltiu(T5, T0, 1),
    ])
    .reg(T0, u64::MAX) // -1 signed, the largest value unsigned
    .reg(T1, 1)
    .run();
    assert_eq!(machine.reg(T2), 1, "-1 < 1 signed");
    assert_eq!(machine.reg(T3), 0, "u64::MAX < 1 unsigned is false");
    assert_eq!(machine.reg(T4), 1, "-1 < 1 signed");
    assert_eq!(machine.reg(T5), 0, "u64::MAX < 1 unsigned is false");
}

#[test]
fn lui_loads_a_sign_extended_upper_immediate() {
    let machine = run(&[lui(T0, 0x00001), lui(T1, 0xfffff), lui(T2, 0x80000)]);
    assert_eq!(machine.reg(T0), 0x1000);
    assert_eq!(machine.reg(T1), 0xffff_ffff_ffff_f000);
    assert_eq!(machine.reg(T2), 0xffff_ffff_8000_0000);
}

#[test]
fn auipc_adds_to_the_address_of_the_instruction() {
    let machine = run(&[nop(), auipc(T0, 1)]);
    assert_eq!(machine.reg(T0), DRAM_BASE + 4 + 0x1000);
}

// ------------------------------------------------------------------ rv64i, shifts

#[test]
fn immediate_shifts_move_in_the_right_direction() {
    let machine = prog(&[slli(T1, T0, 4), srli(T2, T0, 4)])
        .reg(T0, 0x1234)
        .run();
    assert_eq!(machine.reg(T1), 0x1234 << 4);
    assert_eq!(machine.reg(T2), 0x1234 >> 4);
}

#[test]
fn immediate_shifts_take_six_bits_of_shift_amount() {
    let machine = prog(&[slli(T1, T0, 40), slli(T2, T0, 63), srli(T3, T4, 32)])
        .reg(T0, 1)
        .reg(T4, 1 << 40)
        .run();
    assert_eq!(machine.reg(T1), 1 << 40);
    assert_eq!(machine.reg(T2), 1 << 63);
    assert_eq!(machine.reg(T3), 1 << 8);
}

#[test]
fn srai_keeps_the_sign() {
    let machine = prog(&[srai(T1, T0, 40), srai(T2, T3, 40)])
        .reg(T0, u64::MAX)
        .reg(T3, i64::MAX as u64)
        .run();
    assert_eq!(machine.reg(T1), u64::MAX);
    assert_eq!(machine.reg(T2), (i64::MAX >> 40) as u64);
}

#[test]
fn register_shifts_mask_the_amount_to_six_bits() {
    let machine = prog(&[sll(T2, T0, T1), srl(T3, T0, T1), sra(T4, T0, T1)])
        .reg(T0, u64::MAX)
        .reg(T1, 64 + 4) // only the low six bits count, so this shifts by 4
        .run();
    assert_eq!(machine.reg(T2), u64::MAX << 4);
    assert_eq!(machine.reg(T3), u64::MAX >> 4);
    assert_eq!(machine.reg(T4), u64::MAX);
}

#[test]
fn word_shifts_operate_on_32_bits_and_sign_extend() {
    let machine = prog(&[
        slliw(T1, T0, 28),
        srliw(T2, T0, 4),
        sraiw(T3, T4, 4),
        sllw(T5, T0, T6),
    ])
    .reg(T0, 0x0000_0000_ffff_ffff)
    .reg(T4, 0xffff_ffff_8000_0000)
    .reg(T6, 32 + 28) // masked to five bits, so 28
    .run();
    assert_eq!(machine.reg(T1), 0xffff_ffff_f000_0000, "slliw sign-extends");
    assert_eq!(
        machine.reg(T2),
        0x0fff_ffff,
        "srliw is a logical 32-bit shift"
    );
    assert_eq!(
        machine.reg(T3),
        0xffff_ffff_f800_0000,
        "sraiw keeps the sign"
    );
    assert_eq!(
        machine.reg(T5),
        0xffff_ffff_f000_0000,
        "sllw masks to five bits"
    );
}

// ------------------------------------------------------------------ rv64i, word ops

#[test]
fn word_arithmetic_sign_extends_from_32_bits() {
    let machine = prog(&[addw(T2, T0, T1), subw(T3, T0, T1), addiw(T4, T0, 1)])
        .reg(T0, 0x7fff_ffff)
        .reg(T1, 1)
        .run();
    assert_eq!(
        machine.reg(T2),
        0xffff_ffff_8000_0000,
        "overflow wraps into the sign"
    );
    assert_eq!(machine.reg(T3), 0x7fff_fffe);
    assert_eq!(machine.reg(T4), 0xffff_ffff_8000_0000);
}

#[test]
fn word_arithmetic_ignores_the_upper_half_of_its_operands() {
    let machine = prog(&[addw(T2, T0, T1)])
        .reg(T0, 0xdead_beef_0000_0001)
        .reg(T1, 0xcafe_0000_0000_0002)
        .run();
    assert_eq!(machine.reg(T2), 3);
}

// ------------------------------------------------------------------ zicond

#[test]
fn czero_moves_or_zeroes_on_the_condition() {
    let machine = prog(&[
        czero_eqz(T2, T0, T1),
        czero_nez(T3, T0, T1),
        czero_eqz(T4, T0, T5),
        czero_nez(T6, T0, T5),
    ])
    .reg(T0, 0x1234)
    .reg(T1, 1)
    .reg(T5, 0)
    .run();
    assert_eq!(
        machine.reg(T2),
        0x1234,
        "condition is nonzero, so the value passes"
    );
    assert_eq!(
        machine.reg(T3),
        0,
        "condition is nonzero, so the result is zero"
    );
    assert_eq!(
        machine.reg(T4),
        0,
        "condition is zero, so the result is zero"
    );
    assert_eq!(
        machine.reg(T6),
        0x1234,
        "condition is zero, so the value passes"
    );
}
