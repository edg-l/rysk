use crate::common::*;
use rysk::inst::decode;

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
        ecall(), ebreak(), mret(), sret(), wfi(),
        amoadd_b(T0, T2, T1), amomin_h(T0, T2, T1), amomaxu_b(T0, T2, T1),
        amoswap_h(T0, T2, T1), amocas_b(T0, T2, T1), amocas_h(T0, T2, T1),
        wrs_nto(), wrs_sto(),
        amocas_w(T0, T2, T1), amocas_d(T0, T2, T1), amocas_q(A0, A2, T1),
        fence(), fence_i(),
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

/// The same for the compressed encoders, whose immediates are scattered across the
/// halfword in nine arrangements and are the easiest thing here to get wrong.
#[test]
fn compressed_encoder_matches_the_toolchain() {
    #[rustfmt::skip]
    let expected: Vec<u16> = vec![
        c_nop(), c_addi(T0, 1), c_addi(T0, -32), c_addiw(T0, 31),
        c_li(A0, -1), c_lui(A1, 0xfffe0u32 as i32),
        c_addi16sp(-512), c_addi16sp(496),
        c_addi4spn(A0, 4), c_addi4spn(A5, 1020),
        c_slli(T0, 63), c_srli(A0, 1), c_srai(A0, 63), c_andi(A0, -1),
        c_sub(A0, A1), c_xor(A0, A1), c_or(A0, A1), c_and(A0, A1),
        c_subw(A0, A1), c_addw(A0, A1),
        c_lw(A0, A1, 0), c_lw(A0, A1, 124), c_ld(A0, A1, 8), c_ld(A0, A1, 248),
        c_sw(A0, A1, 4), c_sd(A0, A1, 16),
        c_lwsp(T0, 4), c_lwsp(T0, 252), c_ldsp(T0, 8), c_ldsp(T0, 504),
        c_swsp(T0, 8), c_sdsp(T0, 16),
        c_mv(T0, T1), c_add(T0, T1), c_jr(T0), c_jalr(T0), c_ebreak(),
        // `back` is the instruction after c.ebreak, and `fwd` the last one
        c_j(0), c_j(6), c_beqz(A0, -4), c_bnez(A0, 2),
        c_nop(),
    ];

    let bytes = std::fs::read("tests/compressed.bin").expect("run `make test_files`");
    let actual: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|h| u16::from_le_bytes(h.try_into().unwrap()))
        .collect();

    assert_eq!(actual.len(), expected.len(), "instruction count");
    for (n, (a, e)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(
            a,
            e,
            "instruction {n} at {:#x}: {a:#06x} != {e:#06x}",
            n * 2
        );
    }
}

/// The decoder is also the disassembler, and a trace is only useful if it reads like
/// what the assembler was given.
#[test]
fn decoding_round_trips_to_readable_assembly() {
    let cases: &[(u32, &str)] = &[
        (addi(SP, SP, -32), "addi sp, sp, -32"),
        (sd(RA, SP, 24), "sd ra, 24(sp)"),
        (lw(A0, S0, -20), "lw a0, -20(s0)"),
        (lbu(T0, T1, 4), "lbu t0, 4(t1)"),
        (add(A0, A1, A2), "add a0, a1, a2"),
        (slli(T0, T1, 40), "slli t0, t1, 40"),
        (bne(T1, ZERO, -8), "bne t1, zero, -8"),
        (beq(T1, T2, 12), "beq t1, t2, +12"),
        (jal(RA, -240), "jal ra, -240"),
        (jalr(RA, T1, -4), "jalr ra, -4(t1)"),
        (lui(T0, 0xfffff), "lui t0, 0xfffff"),
        (auipc(RA, 0), "auipc ra, 0x0"),
        (csrrw(ZERO, 0x305, T0), "csrrw zero, 0x305, t0"),
        (csrrci(T0, 0x340, 8), "csrrci t0, 0x340, 8"),
        (mret(), "mret"),
        (sret(), "sret"),
        (fence(), "fence"),
        (fence_i(), "fence.i"),
        (ecall(), "ecall"),
        (lr_w(T0, ZERO, T1), "lr.w t0, (t1)"),
        (sc_d(T0, T2, T1), "sc.d t0, t2, (t1)"),
        (amomaxu_w(T0, T2, T1), "amomaxu.w t0, t2, (t1)"),
        (amocas_q(A0, A2, T1), "amocas.q a0, a2, (t1)"),
        (amoadd_b(T0, T2, T1), "amoadd.b t0, t2, (t1)"),
        (wrs_sto(), "wrs.sto"),
        (czero_eqz(A0, A1, A2), "czero.eqz a0, a1, a2"),
    ];
    for &(encoding, text) in cases {
        let inst = decode(encoding).expect("should decode");
        assert_eq!(inst.to_string(), text, "for {encoding:#010x}");
    }
}

#[test]
fn a_word_that_encodes_nothing_is_rejected() {
    for word in [0x0000_0000, 0xffff_ffff, 0x0000_0077, 0xfe00_0033] {
        assert!(decode(word).is_err(), "{word:#010x} should not decode");
    }
}
