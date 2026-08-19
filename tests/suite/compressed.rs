use crate::common::*;
use rysk::{bus::DRAM_BASE, trap::Exception};

// ------------------------------------------------------------- compressed

/// The two halves of a 32-bit instruction, low first, for mixing widths in one
/// program the way real code does.
fn wide(inst: u32) -> [u16; 2] {
    [inst as u16, (inst >> 16) as u16]
}

#[test]
fn a_compressed_instruction_is_two_bytes_long() {
    // Three halfwords: if either of the first two were taken as four bytes, the third
    // would never run and a0 would not be three.
    let cpu = halves(&[c_li(A0, 1), c_addi(A0, 1), c_addi(A0, 1)]).run();
    assert_eq!(cpu.reg(A0), 3);
}

#[test]
fn the_two_widths_mix_in_one_stream() {
    let [lo, hi] = wide(addi(A1, ZERO, 10));
    let cpu = halves(&[c_li(A0, 1), lo, hi, c_addi(A0, 1)]).run();
    assert_eq!(
        (cpu.reg(A0), cpu.reg(A1)),
        (2, 10),
        "all three ran, in order"
    );
}

#[test]
fn the_short_register_fields_name_x8_to_x15() {
    let cpu = halves(&[c_li(A0, 7), c_li(A1, 3), c_sub(A0, A1)]).run();
    assert_eq!(cpu.reg(A0), 4, "a0 and a1, not x8 and x11 or x0 and x3");
}

#[test]
fn the_stack_pointer_forms_reach_the_stack() {
    let cpu = halves(&[
        c_li(T0, 21),
        c_sdsp(T0, 8),
        c_ldsp(T1, 8),
        c_addi4spn(A0, 8),
    ])
    .reg(SP, SCRATCH)
    .run();
    assert_eq!(cpu.reg(T1), 21, "it went to the stack and came back");
    assert_eq!(cpu.load(SCRATCH + 8, 8), 21, "at the offset it named");
    assert_eq!(
        cpu.reg(A0),
        SCRATCH + 8,
        "and addi4spn scales its immediate"
    );
}

#[test]
fn the_stack_pointer_moves_in_units_of_sixteen_bytes() {
    let cpu = halves(&[c_addi16sp(-32), c_addi16sp(16)])
        .reg(SP, SCRATCH)
        .run();
    assert_eq!(cpu.reg(SP), SCRATCH - 16);
}

#[test]
fn a_compressed_branch_and_jump_reach_where_they_say() {
    // c.beqz is not taken, c.j skips the instruction after it.
    let cpu = halves(&[
        c_li(A0, 1),
        c_beqz(A0, 8),
        c_j(4),
        c_li(A1, 1), // jumped over
        c_li(A2, 1),
    ])
    .run();
    assert_eq!(cpu.reg(A1), 0, "the jump cleared it");
    assert_eq!(cpu.reg(A2), 1, "and landed here");

    let cpu = halves(&[c_li(A0, 0), c_beqz(A0, 4), c_li(A1, 1), c_li(A2, 1)]).run();
    assert_eq!(cpu.reg(A1), 0, "the branch was taken");
    assert_eq!(cpu.reg(A2), 1);
}

#[test]
fn a_compressed_jump_register_links_the_right_return_address() {
    // c.jalr puts the address after itself in ra, which is two bytes on.
    let cpu = halves(&[c_li(T0, 0), c_jalr(T0)])
        .reg(T0, DRAM_BASE + 6)
        .run();
    assert_eq!(cpu.reg(RA), DRAM_BASE + 4, "past the two-byte call");
}

#[test]
fn c_lui_and_c_li_build_the_constants_they_are_named_for() {
    let cpu = halves(&[c_li(A0, -1), c_lui(A1, 0xfffe0u32 as i32)]).run();
    assert_eq!(cpu.reg(A0), u64::MAX, "c.li sign-extends");
    assert_eq!(cpu.reg(A1), 0xffff_ffff_fffe_0000, "and so does c.lui");
}

#[test]
fn the_shifts_take_a_sixth_bit_from_the_top_of_the_halfword() {
    let cpu = halves(&[c_li(A0, 1), c_slli(A0, 40), c_srli(A0, 39)]).run();
    assert_eq!(cpu.reg(A0), 2, "a shift of more than 31 is still a shift");
}

#[test]
fn the_encodings_the_manual_reserves_are_rejected() {
    // c.addi4spn with a zero immediate is the all-zero halfword the manual defines as
    // illegal, so that a jump into blank memory stops rather than wanders.
    halves(&[0]).expect(Exception::IllegalInstruction(0));
    let no_offset = c_addi4spn(A0, 0);
    halves(&[no_offset]).expect(Exception::IllegalInstruction(no_offset as u64));
    // A load from the stack has nowhere to put what it read when it names no register.
    let no_destination = c_ldsp(ZERO, 8);
    halves(&[no_destination]).expect(Exception::IllegalInstruction(no_destination as u64));
    // And a jump through a register that names none has nowhere to go.
    let nowhere = c_jr(ZERO);
    halves(&[nowhere]).expect(Exception::IllegalInstruction(nowhere as u64));
}

#[test]
fn a_hint_retires_and_changes_nothing() {
    // c.add with x0 as its destination is a hint, which is exactly what the base
    // instruction it expands to already does.
    let cpu = halves(&[c_li(A0, 5), c_add(ZERO, A0), c_addi(A0, 1)]).run();
    assert_eq!(cpu.reg(A0), 6, "the instruction after it ran");
    assert_eq!(cpu.reg(ZERO), 0);
}
