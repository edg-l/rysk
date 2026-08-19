mod common;

use common::*;
use rysk::{bus::DRAM_BASE, cpu::Cpu, dram::DRAM_SIZE};

// ------------------------------------------------------------------ assembler

/// The assembler in `common` has to agree with a real one, or every test below is
/// asserting against the same misunderstanding twice. `tests/encodings.s` is assembled
/// by the toolchain; this reproduces it instruction for instruction.
#[test]
fn encoder_matches_the_toolchain() {
    #[rustfmt::skip]
    let expected: Vec<u32> = vec![
        addi(T0, ZERO, 1), addi(T1, T0, -1), slti(T2, T0, -2048), sltiu(T3, T0, 2047),
        xori(T4, T0, -1), ori(T5, T0, 1365), andi(T6, T0, -1366),

        slli(T0, T1, 0), slli(T0, T1, 31), slli(T0, T1, 32), slli(T0, T1, 63),
        srli(T0, T1, 63), srai(T0, T1, 63),
        slliw(T0, T1, 31), srliw(T0, T1, 31), sraiw(T0, T1, 31),

        add(T0, T1, T2), sub(T0, T1, T2), sll(T0, T1, T2), slt(T0, T1, T2),
        sltu(T0, T1, T2), xor(T0, T1, T2), srl(T0, T1, T2), sra(T0, T1, T2),
        or(T0, T1, T2), and(T0, T1, T2),
        addw(T0, T1, T2), subw(T0, T1, T2), sllw(T0, T1, T2), srlw(T0, T1, T2),
        sraw(T0, T1, T2),

        mul(T0, T1, T2), mulh(T0, T1, T2), mulhsu(T0, T1, T2), mulhu(T0, T1, T2),
        div(T0, T1, T2), divu(T0, T1, T2), rem(T0, T1, T2), remu(T0, T1, T2),
        mulw(T0, T1, T2), divw(T0, T1, T2), divuw(T0, T1, T2), remw(T0, T1, T2),
        remuw(T0, T1, T2),

        czero_eqz(T0, T1, T2), czero_nez(T0, T1, T2),

        lb(T0, T1, -2048), lh(T0, T1, 2047), lw(T0, T1, 4), ld(T0, T1, 8),
        lbu(T0, T1, -1), lhu(T0, T1, -2), lwu(T0, T1, -4),

        sb(T2, T1, -2048), sh(T2, T1, 2047), sw(T2, T1, 4), sd(T2, T1, -8),

        lui(T0, 524287), lui(T0, 1048575), auipc(T0, 1),

        jal(RA, -240), jal(ZERO, 148), jalr(RA, T1, -4),

        csrrw(T0, 0x300, T1), csrrs(T0, 3072, ZERO), csrrc(T0, 832, T1),
        csrrwi(T0, 833, 31), csrrsi(T0, 834, 1), csrrci(T0, 835, 0),

        lr_w(T0, ZERO, T1), sc_w(T0, T2, T1), amoswap_w(T0, T2, T1),
        amoadd_w(T0, T2, T1), amoxor_w(T0, T2, T1), amoand_w(T0, T2, T1),
        amoor_w(T0, T2, T1), amomin_w(T0, T2, T1), amomax_w(T0, T2, T1),
        amominu_w(T0, T2, T1), amomaxu_w(T0, T2, T1),
        lr_d(T0, ZERO, T1), sc_d(T0, T2, T1), amoswap_d(T0, T2, T1),
        amoadd_d(T0, T2, T1), amoxor_d(T0, T2, T1), amoand_d(T0, T2, T1),
        amoor_d(T0, T2, T1), amomin_d(T0, T2, T1), amomax_d(T0, T2, T1),
        amominu_d(T0, T2, T1), amomaxu_d(T0, T2, T1),

        addiw(T0, T1, -1),
        beq(T1, T2, -4), bne(T1, T2, 20), blt(T1, T2, -12),
        bge(T1, T2, 12), bltu(T1, T2, -20), bgeu(T1, T2, 4),
        addiw(T0, T1, 1),
    ];

    let bytes = std::fs::read("tests/encodings.bin").expect("run `make test_files`");
    let actual: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
        .collect();

    assert_eq!(actual.len(), expected.len(), "instruction count");
    for (n, (a, e)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(
            a,
            e,
            "instruction {n} at {:#x}: {a:#010x} != {e:#010x}",
            n * 4
        );
    }
}

// ------------------------------------------------------------------ rv64i, integer

#[test]
fn addi_sign_extends_its_immediate() {
    let cpu = run(&[
        addi(T0, ZERO, -1),
        addi(T1, ZERO, 2047),
        addi(T2, ZERO, -2048),
    ]);
    assert_eq!(cpu.reg(T0), u64::MAX);
    assert_eq!(cpu.reg(T1), 2047);
    assert_eq!(cpu.reg(T2), (-2048i64) as u64);
}

#[test]
fn add_and_sub_wrap() {
    let cpu = prog(&[add(T2, T0, T1), sub(T3, T0, T1)])
        .reg(T0, 0)
        .reg(T1, 1)
        .run();
    assert_eq!(cpu.reg(T2), 1);
    assert_eq!(cpu.reg(T3), u64::MAX);
}

#[test]
fn the_bitwise_operations_and_their_immediate_forms_agree() {
    let cpu = prog(&[
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
    assert_eq!(cpu.reg(A0), 0b0100);
    assert_eq!(cpu.reg(A1), 0b1101);
    assert_eq!(cpu.reg(A2), 0b1001);
    assert_eq!(cpu.reg(A3), 0b0100);
    assert_eq!(cpu.reg(A4), 0b1101);
    assert_eq!(cpu.reg(A5), 0b1001);
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
    let cpu = run(&[addi(ZERO, ZERO, 42), lui(ZERO, 1), jal(ZERO, 4)]);
    assert_eq!(cpu.reg(ZERO), 0);
}

#[test]
fn slt_compares_signed_and_sltu_unsigned() {
    let cpu = prog(&[
        slt(T2, T0, T1),
        sltu(T3, T0, T1),
        slti(T4, T0, 1),
        sltiu(T5, T0, 1),
    ])
    .reg(T0, u64::MAX) // -1 signed, the largest value unsigned
    .reg(T1, 1)
    .run();
    assert_eq!(cpu.reg(T2), 1, "-1 < 1 signed");
    assert_eq!(cpu.reg(T3), 0, "u64::MAX < 1 unsigned is false");
    assert_eq!(cpu.reg(T4), 1, "-1 < 1 signed");
    assert_eq!(cpu.reg(T5), 0, "u64::MAX < 1 unsigned is false");
}

#[test]
fn lui_loads_a_sign_extended_upper_immediate() {
    let cpu = run(&[lui(T0, 0x00001), lui(T1, 0xfffff), lui(T2, 0x80000)]);
    assert_eq!(cpu.reg(T0), 0x1000);
    assert_eq!(cpu.reg(T1), 0xffff_ffff_ffff_f000);
    assert_eq!(cpu.reg(T2), 0xffff_ffff_8000_0000);
}

#[test]
fn auipc_adds_to_the_address_of_the_instruction() {
    let cpu = run(&[nop(), auipc(T0, 1)]);
    assert_eq!(cpu.reg(T0), DRAM_BASE + 4 + 0x1000);
}

// ------------------------------------------------------------------ rv64i, shifts

#[test]
fn immediate_shifts_move_in_the_right_direction() {
    let cpu = prog(&[slli(T1, T0, 4), srli(T2, T0, 4)])
        .reg(T0, 0x1234)
        .run();
    assert_eq!(cpu.reg(T1), 0x1234 << 4);
    assert_eq!(cpu.reg(T2), 0x1234 >> 4);
}

#[test]
fn immediate_shifts_take_six_bits_of_shift_amount() {
    let cpu = prog(&[slli(T1, T0, 40), slli(T2, T0, 63), srli(T3, T4, 32)])
        .reg(T0, 1)
        .reg(T4, 1 << 40)
        .run();
    assert_eq!(cpu.reg(T1), 1 << 40);
    assert_eq!(cpu.reg(T2), 1 << 63);
    assert_eq!(cpu.reg(T3), 1 << 8);
}

#[test]
fn srai_keeps_the_sign() {
    let cpu = prog(&[srai(T1, T0, 40), srai(T2, T3, 40)])
        .reg(T0, u64::MAX)
        .reg(T3, i64::MAX as u64)
        .run();
    assert_eq!(cpu.reg(T1), u64::MAX);
    assert_eq!(cpu.reg(T2), (i64::MAX >> 40) as u64);
}

#[test]
fn register_shifts_mask_the_amount_to_six_bits() {
    let cpu = prog(&[sll(T2, T0, T1), srl(T3, T0, T1), sra(T4, T0, T1)])
        .reg(T0, u64::MAX)
        .reg(T1, 64 + 4) // only the low six bits count, so this shifts by 4
        .run();
    assert_eq!(cpu.reg(T2), u64::MAX << 4);
    assert_eq!(cpu.reg(T3), u64::MAX >> 4);
    assert_eq!(cpu.reg(T4), u64::MAX);
}

#[test]
fn word_shifts_operate_on_32_bits_and_sign_extend() {
    let cpu = prog(&[
        slliw(T1, T0, 28),
        srliw(T2, T0, 4),
        sraiw(T3, T4, 4),
        sllw(T5, T0, T6),
    ])
    .reg(T0, 0x0000_0000_ffff_ffff)
    .reg(T4, 0xffff_ffff_8000_0000)
    .reg(T6, 32 + 28) // masked to five bits, so 28
    .run();
    assert_eq!(cpu.reg(T1), 0xffff_ffff_f000_0000, "slliw sign-extends");
    assert_eq!(cpu.reg(T2), 0x0fff_ffff, "srliw is a logical 32-bit shift");
    assert_eq!(cpu.reg(T3), 0xffff_ffff_f800_0000, "sraiw keeps the sign");
    assert_eq!(
        cpu.reg(T5),
        0xffff_ffff_f000_0000,
        "sllw masks to five bits"
    );
}

// ------------------------------------------------------------------ rv64i, word ops

#[test]
fn word_arithmetic_sign_extends_from_32_bits() {
    let cpu = prog(&[addw(T2, T0, T1), subw(T3, T0, T1), addiw(T4, T0, 1)])
        .reg(T0, 0x7fff_ffff)
        .reg(T1, 1)
        .run();
    assert_eq!(
        cpu.reg(T2),
        0xffff_ffff_8000_0000,
        "overflow wraps into the sign"
    );
    assert_eq!(cpu.reg(T3), 0x7fff_fffe);
    assert_eq!(cpu.reg(T4), 0xffff_ffff_8000_0000);
}

#[test]
fn word_arithmetic_ignores_the_upper_half_of_its_operands() {
    let cpu = prog(&[addw(T2, T0, T1)])
        .reg(T0, 0xdead_beef_0000_0001)
        .reg(T1, 0xcafe_0000_0000_0002)
        .run();
    assert_eq!(cpu.reg(T2), 3);
}

// ------------------------------------------------------------------ loads and stores

#[test]
fn stores_and_loads_round_trip_every_width() {
    let cpu = prog(&[
        sd(T1, T0, 0),
        ld(T2, T0, 0),
        sw(T1, T0, 16),
        lwu(T3, T0, 16),
        sh(T1, T0, 24),
        lhu(T4, T0, 24),
        sb(T1, T0, 32),
        lbu(T5, T0, 32),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 0x8899_aabb_ccdd_eeff)
    .run();
    assert_eq!(cpu.reg(T2), 0x8899_aabb_ccdd_eeff);
    assert_eq!(cpu.reg(T3), 0xccdd_eeff);
    assert_eq!(cpu.reg(T4), 0xeeff);
    assert_eq!(cpu.reg(T5), 0xff);
}

#[test]
fn a_doubleword_keeps_its_top_bits() {
    // Storing and loading are wrong symmetrically if the byte lanes are misplaced, so
    // the value has to be one whose top bits are dropped rather than merely shuffled.
    let cpu = prog(&[sd(T1, T0, 0), ld(T2, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, u64::MAX)
        .run();
    assert_eq!(cpu.reg(T2), u64::MAX);
}

#[test]
fn a_doubleword_lands_in_the_right_byte_lanes() {
    let cpu = prog(&[sd(T1, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 0x0807_0605_0403_0201)
        .run();
    for byte in 0..8u64 {
        assert_eq!(cpu.load(SCRATCH + byte, 1), byte + 1, "byte {byte}");
    }
}

#[test]
fn narrow_loads_sign_extend_and_their_unsigned_forms_do_not() {
    let cpu = prog(&[
        sd(T1, T0, 0),
        lb(T2, T0, 0),
        lbu(T3, T0, 0),
        lh(T4, T0, 0),
        lhu(T5, T0, 0),
        lw(T6, T0, 0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 0xffff_ffff_ffff_ffff)
    .run();
    assert_eq!(cpu.reg(T2), u64::MAX);
    assert_eq!(cpu.reg(T3), 0xff);
    assert_eq!(cpu.reg(T4), u64::MAX);
    assert_eq!(cpu.reg(T5), 0xffff);
    assert_eq!(cpu.reg(T6), u64::MAX);
}

#[test]
fn a_negative_offset_addresses_below_the_base() {
    let cpu = prog(&[sd(T1, T0, -8), ld(T2, T0, -8)])
        .reg(T0, SCRATCH)
        .reg(T1, 0x1234)
        .run();
    assert_eq!(cpu.reg(T2), 0x1234);
    assert_eq!(cpu.load(SCRATCH - 8, 8), 0x1234);
}

// ------------------------------------------------------------------ rv64m

#[test]
fn mul_returns_the_low_half_and_mulh_the_high_half() {
    let cpu = prog(&[
        mul(T2, T0, T1),
        mulh(T3, T0, T1),
        mulhu(T4, T0, T1),
        mulhsu(T5, T0, T1),
    ])
    .reg(T0, u64::MAX) // -1
    .reg(T1, 2)
    .run();
    assert_eq!(cpu.reg(T2), (-2i64) as u64);
    assert_eq!(cpu.reg(T3), u64::MAX, "-1 * 2 is negative");
    assert_eq!(cpu.reg(T4), 1, "u64::MAX * 2 overflows by one");
    assert_eq!(cpu.reg(T5), u64::MAX, "signed times unsigned");
}

#[test]
fn division_by_zero_gives_all_ones_and_the_dividend() {
    let cpu = prog(&[
        div(T2, T0, T1),
        divu(T3, T0, T1),
        rem(T4, T0, T1),
        remu(T5, T0, T1),
    ])
    .reg(T0, 42)
    .reg(T1, 0)
    .run();
    assert_eq!(cpu.reg(T2), u64::MAX);
    assert_eq!(cpu.reg(T3), u64::MAX);
    assert_eq!(
        cpu.reg(T4),
        42,
        "the remainder of division by zero is the dividend"
    );
    assert_eq!(cpu.reg(T5), 42);
}

#[test]
fn word_division_by_zero_sign_extends_the_dividend() {
    let cpu = prog(&[
        divw(T2, T0, T1),
        remw(T3, T0, T1),
        divuw(T4, T0, T1),
        remuw(T5, T0, T1),
    ])
    .reg(T0, 0xffff_ffff_8000_0000) // -2^31 as a word
    .reg(T1, 0)
    .run();
    assert_eq!(cpu.reg(T2), u64::MAX);
    assert_eq!(cpu.reg(T3), 0xffff_ffff_8000_0000);
    assert_eq!(cpu.reg(T4), u64::MAX);
    assert_eq!(cpu.reg(T5), 0xffff_ffff_8000_0000);
}

#[test]
fn signed_division_overflow_returns_the_dividend_and_no_remainder() {
    let cpu = prog(&[div(T2, T0, T1), rem(T3, T0, T1)])
        .reg(T0, i64::MIN as u64)
        .reg(T1, u64::MAX) // -1
        .run();
    assert_eq!(cpu.reg(T2), i64::MIN as u64);
    assert_eq!(cpu.reg(T3), 0);
}

#[test]
fn word_division_overflow_returns_the_dividend_and_no_remainder() {
    let cpu = prog(&[divw(T2, T0, T1), remw(T3, T0, T1)])
        .reg(T0, 0xffff_ffff_8000_0000)
        .reg(T1, u64::MAX)
        .run();
    assert_eq!(cpu.reg(T2), 0xffff_ffff_8000_0000);
    assert_eq!(cpu.reg(T3), 0);
}

#[test]
fn word_multiply_sign_extends_its_result() {
    let cpu = prog(&[mulw(T2, T0, T1)])
        .reg(T0, 0x0001_0000)
        .reg(T1, 0x0001_0000)
        .run();
    assert_eq!(cpu.reg(T2), 0, "the product's low 32 bits are zero");
}

// ------------------------------------------------------------------ zicond

#[test]
fn czero_moves_or_zeroes_on_the_condition() {
    let cpu = prog(&[
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
        cpu.reg(T2),
        0x1234,
        "condition is nonzero, so the value passes"
    );
    assert_eq!(
        cpu.reg(T3),
        0,
        "condition is nonzero, so the result is zero"
    );
    assert_eq!(cpu.reg(T4), 0, "condition is zero, so the result is zero");
    assert_eq!(
        cpu.reg(T6),
        0x1234,
        "condition is zero, so the value passes"
    );
}

// ------------------------------------------------------------------ control flow

/// Each branch is followed by an `addi` that only runs when it is not taken, and the
/// target is an `addi` past that. Taken leaves 2, not taken leaves 1.
fn branch_taken(inst: u32, lhs: u64, rhs: u64) -> bool {
    let cpu = prog(&[inst, addi(T2, ZERO, 1), jal(ZERO, 8), addi(T2, ZERO, 2)])
        .reg(T0, lhs)
        .reg(T1, rhs)
        .run();
    match cpu.reg(T2) {
        1 => false,
        2 => true,
        other => panic!("branch test landed somewhere unexpected: t2 = {other}"),
    }
}

#[test]
fn branches_compare_the_way_their_names_say() {
    let neg = (-1i64) as u64;
    assert!(branch_taken(beq(T0, T1, 12), 5, 5));
    assert!(!branch_taken(beq(T0, T1, 12), 5, 6));
    assert!(branch_taken(bne(T0, T1, 12), 5, 6));
    assert!(!branch_taken(bne(T0, T1, 12), 5, 5));

    assert!(branch_taken(blt(T0, T1, 12), neg, 1), "-1 < 1 signed");
    assert!(
        !branch_taken(bltu(T0, T1, 12), neg, 1),
        "-1 is huge unsigned"
    );
    assert!(branch_taken(bge(T0, T1, 12), 1, neg), "1 >= -1 signed");
    assert!(
        branch_taken(bgeu(T0, T1, 12), neg, 1),
        "-1 is huge unsigned"
    );
    assert!(branch_taken(bge(T0, T1, 12), 5, 5), "greater or equal");
    assert!(branch_taken(bgeu(T0, T1, 12), 5, 5));
}

#[test]
fn a_backward_branch_closes_a_loop() {
    // t1 counts down from 5 to 0, t2 accumulates the iterations.
    let cpu = prog(&[
        addi(T1, ZERO, 5),
        addi(T2, ZERO, 0),
        addi(T2, T2, 1),
        addi(T1, T1, -1),
        bne(T1, ZERO, -8),
    ])
    .run();
    assert_eq!(cpu.reg(T2), 5);
    assert_eq!(cpu.reg(T1), 0);
}

#[test]
fn jal_links_the_following_instruction() {
    let cpu = run(&[jal(RA, 8), addi(T0, ZERO, 1), addi(T1, ZERO, 2)]);
    assert_eq!(cpu.reg(RA), DRAM_BASE + 4, "the return address is pc + 4");
    assert_eq!(cpu.reg(T0), 0, "the skipped instruction did not run");
    assert_eq!(cpu.reg(T1), 2);
}

#[test]
fn jalr_clears_the_low_bit_of_its_target() {
    // An odd target must be rounded down, not fetched at an odd address.
    let cpu = prog(&[jalr(RA, T0, 1), addi(T1, ZERO, 1), addi(T2, ZERO, 2)])
        .reg(T0, DRAM_BASE + 8)
        .run();
    assert_eq!(cpu.reg(T1), 0, "the skipped instruction did not run");
    assert_eq!(cpu.reg(T2), 2);
}

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

// ------------------------------------------------------------------ zalrsc

#[test]
fn a_store_conditional_succeeds_right_after_its_reservation() {
    let cpu = prog(&[
        sw(T1, T0, 0),
        lr_w(T2, ZERO, T0),
        sc_w(T3, T4, T0),
        lw(T5, T0, 0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 7)
    .reg(T4, 9)
    .run();
    assert_eq!(cpu.reg(T2), 7, "the reserved load returns what was there");
    assert_eq!(cpu.reg(T3), 0, "zero means it succeeded");
    assert_eq!(cpu.reg(T5), 9, "and the store happened");
}

#[test]
fn an_intervening_store_of_the_same_value_still_breaks_the_reservation() {
    let cpu = prog(&[
        sw(T1, T0, 0),
        lr_w(T2, ZERO, T0),
        sw(T1, T0, 0), // the same value, so only a reservation can tell it happened
        sc_w(T3, T4, T0),
        lw(T5, T0, 0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 7)
    .reg(T4, 9)
    .run();
    assert_ne!(cpu.reg(T3), 0, "nonzero means it failed");
    assert_eq!(cpu.reg(T5), 7, "and nothing was written");
}

#[test]
fn a_store_conditional_without_a_reservation_fails() {
    let cpu = prog(&[sw(T1, T0, 0), sc_w(T3, T4, T0), lw(T5, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 7)
        .reg(T4, 9)
        .run();
    assert_ne!(cpu.reg(T3), 0);
    assert_eq!(cpu.reg(T5), 7);
}

#[test]
fn a_second_store_conditional_fails_because_the_first_released_the_reservation() {
    let cpu = prog(&[lr_w(T2, ZERO, T0), sc_w(T3, T4, T0), sc_w(T5, T4, T0)])
        .reg(T0, SCRATCH)
        .reg(T4, 9)
        .run();
    assert_eq!(cpu.reg(T3), 0);
    assert_ne!(cpu.reg(T5), 0);
}

#[test]
fn a_store_outside_the_reservation_leaves_it_alone() {
    let cpu = prog(&[lr_w(T2, ZERO, T0), sw(T1, T0, 64), sc_w(T3, T4, T0)])
        .reg(T0, SCRATCH)
        .reg(T1, 1)
        .reg(T4, 9)
        .run();
    assert_eq!(
        cpu.reg(T3),
        0,
        "a store 64 bytes away is not in the reservation set"
    );
}

#[test]
fn a_doubleword_reservation_covers_the_whole_doubleword() {
    let cpu = prog(&[
        lr_d(T2, ZERO, T0),
        sw(T1, T0, 4), // the upper half of the reserved doubleword
        sc_d(T3, T4, T0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 1)
    .reg(T4, 9)
    .run();
    assert_ne!(cpu.reg(T3), 0);
}

// ------------------------------------------------------------------ zaamo

/// Every atomic memory operation, as (instruction, initial memory, operand, result).
#[test]
fn atomic_memory_operations_return_the_old_value_and_store_the_new() {
    let cases: &[(&str, u32, u64, u64, u64)] = &[
        ("amoswap.d", amoswap_d(T2, T1, T0), 5, 9, 9),
        ("amoadd.d", amoadd_d(T2, T1, T0), 5, 9, 14),
        ("amoxor.d", amoxor_d(T2, T1, T0), 0b1100, 0b1010, 0b0110),
        ("amoand.d", amoand_d(T2, T1, T0), 0b1100, 0b1010, 0b1000),
        ("amoor.d", amoor_d(T2, T1, T0), 0b1100, 0b1010, 0b1110),
        (
            "amomin.d",
            amomin_d(T2, T1, T0),
            (-1i64) as u64,
            1,
            (-1i64) as u64,
        ),
        ("amomax.d", amomax_d(T2, T1, T0), (-1i64) as u64, 1, 1),
        ("amominu.d", amominu_d(T2, T1, T0), (-1i64) as u64, 1, 1),
        (
            "amomaxu.d",
            amomaxu_d(T2, T1, T0),
            (-1i64) as u64,
            1,
            (-1i64) as u64,
        ),
    ];

    for &(name, inst, initial, operand, expected) in cases {
        let cpu = prog(&[sd(T3, T0, 0), inst, ld(T4, T0, 0)])
            .reg(T0, SCRATCH)
            .reg(T1, operand)
            .reg(T3, initial)
            .run();
        assert_eq!(cpu.reg(T2), initial, "{name} returns the old value");
        assert_eq!(cpu.reg(T4), expected, "{name} stores the new value");
    }
}

#[test]
fn word_atomics_sign_extend_what_they_return() {
    let cpu = prog(&[sw(T3, T0, 0), amoswap_w(T2, T1, T0), lwu(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 1)
        .reg(T3, 0xffff_ffff)
        .run();
    assert_eq!(
        cpu.reg(T2),
        u64::MAX,
        "the old word comes back sign-extended"
    );
    assert_eq!(cpu.reg(T4), 1);
}

#[test]
fn word_atomics_leave_the_neighbouring_word_alone() {
    let cpu = prog(&[sd(T3, T0, 0), amoswap_w(T2, T1, T0), ld(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 0xaaaa_aaaa)
        .reg(T3, 0x1111_1111_2222_2222)
        .run();
    assert_eq!(cpu.reg(T4), 0x1111_1111_aaaa_aaaa);
}

// ------------------------------------------------------------------ end to end

#[test]
fn a_compiled_c_program_runs() {
    // tests/fib.c, built by the toolchain: the one case that proves rysk runs what a
    // real compiler emits rather than only what tests/common assembles.
    let code = std::fs::read("tests/fib.bin").expect("run `make test_files`");
    let mut cpu = Cpu::new(code);
    cpu.run().unwrap();
    assert_eq!(cpu.reg(A4), 1);
    assert_eq!(cpu.reg(A5), 0x37, "fib(10) is 55");
}
