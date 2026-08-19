# Ground truth for the compressed encoders in tests/common, whose immediates are
# scattered across the halfword in nine different arrangements and are the easiest
# thing in the instruction set to get wrong. tests/isa.rs asserts that the Rust encoder
# reproduces these bytes exactly.
  .option rvc
main:
  c.nop
  c.addi   t0, 1
  c.addi   t0, -32
  c.addiw  t0, 31
  c.li     a0, -1
  c.lui    a1, 0xfffe0
  c.addi16sp sp, -512
  c.addi16sp sp, 496
  c.addi4spn a0, sp, 4
  c.addi4spn a5, sp, 1020
  c.slli   t0, 63
  c.srli   a0, 1
  c.srai   a0, 63
  c.andi   a0, -1
  c.sub    a0, a1
  c.xor    a0, a1
  c.or     a0, a1
  c.and    a0, a1
  c.subw   a0, a1
  c.addw   a0, a1
  c.lw     a0, 0(a1)
  c.lw     a0, 124(a1)
  c.ld     a0, 8(a1)
  c.ld     a0, 248(a1)
  c.sw     a0, 4(a1)
  c.sd     a0, 16(a1)
  c.lwsp   t0, 4(sp)
  c.lwsp   t0, 252(sp)
  c.ldsp   t0, 8(sp)
  c.ldsp   t0, 504(sp)
  c.swsp   t0, 8(sp)
  c.sdsp   t0, 16(sp)
  c.mv     t0, t1
  c.add    t0, t1
  c.jr     t0
  c.jalr   t0
  c.ebreak
back:
  c.j      back
  c.j      fwd
  c.beqz   a0, back
  c.bnez   a0, fwd
fwd:
  c.nop
