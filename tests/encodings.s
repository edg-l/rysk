# Ground truth for the assembler in tests/common: every instruction format, with the
# immediate encodings that are easy to get wrong. tests/isa.rs asserts that the Rust
# encoder reproduces these bytes exactly.
#
# Uncompressed, because these are the 32-bit encodings. The compressed ones have their
# own fixture in tests/compressed.s.
  .option norvc
main:
  addi  t0, zero, 1
  addi  t1, t0, -1
  slti  t2, t0, -2048
  sltiu t3, t0, 2047
  xori  t4, t0, -1
  ori   t5, t0, 1365
  andi  t6, t0, -1366

  slli  t0, t1, 0
  slli  t0, t1, 31
  slli  t0, t1, 32
  slli  t0, t1, 63
  srli  t0, t1, 63
  srai  t0, t1, 63
  slliw t0, t1, 31
  srliw t0, t1, 31
  sraiw t0, t1, 31

  add   t0, t1, t2
  sub   t0, t1, t2
  sll   t0, t1, t2
  slt   t0, t1, t2
  sltu  t0, t1, t2
  xor   t0, t1, t2
  srl   t0, t1, t2
  sra   t0, t1, t2
  or    t0, t1, t2
  and   t0, t1, t2
  addw  t0, t1, t2
  subw  t0, t1, t2
  sllw  t0, t1, t2
  srlw  t0, t1, t2
  sraw  t0, t1, t2

  mul    t0, t1, t2
  mulh   t0, t1, t2
  mulhsu t0, t1, t2
  mulhu  t0, t1, t2
  div    t0, t1, t2
  divu   t0, t1, t2
  rem    t0, t1, t2
  remu   t0, t1, t2
  mulw   t0, t1, t2
  divw   t0, t1, t2
  divuw  t0, t1, t2
  remw   t0, t1, t2
  remuw  t0, t1, t2

  czero.eqz t0, t1, t2
  czero.nez t0, t1, t2

  lb  t0, -2048(t1)
  lh  t0, 2047(t1)
  lw  t0, 4(t1)
  ld  t0, 8(t1)
  lbu t0, -1(t1)
  lhu t0, -2(t1)
  lwu t0, -4(t1)

  sb t2, -2048(t1)
  sh t2, 2047(t1)
  sw t2, 4(t1)
  sd t2, -8(t1)

  lui   t0, 524287
  lui   t0, 1048575
  auipc t0, 1

  jal  ra, main
  jal  zero, fwd
  jalr ra, t1, -4

  csrrw  t0, mstatus, t1
  csrrs  t0, 3072, zero
  csrrc  t0, 832, t1
  csrrwi t0, 833, 31
  csrrsi t0, 834, 1
  csrrci t0, 835, 0

  lr.w      t0, (t1)
  sc.w      t0, t2, (t1)
  amoswap.w t0, t2, (t1)
  amoadd.w  t0, t2, (t1)
  amoxor.w  t0, t2, (t1)
  amoand.w  t0, t2, (t1)
  amoor.w   t0, t2, (t1)
  amomin.w  t0, t2, (t1)
  amomax.w  t0, t2, (t1)
  amominu.w t0, t2, (t1)
  amomaxu.w t0, t2, (t1)
  lr.d      t0, (t1)
  sc.d      t0, t2, (t1)
  amoswap.d t0, t2, (t1)
  amoadd.d  t0, t2, (t1)
  amoxor.d  t0, t2, (t1)
  amoand.d  t0, t2, (t1)
  amoor.d   t0, t2, (t1)
  amomin.d  t0, t2, (t1)
  amomax.d  t0, t2, (t1)
  amominu.d t0, t2, (t1)
  amomaxu.d t0, t2, (t1)

back:
  addiw t0, t1, -1
  beq  t1, t2, back
  bne  t1, t2, fwd
  blt  t1, t2, back
  bge  t1, t2, fwd
  bltu t1, t2, back
  bgeu t1, t2, fwd
fwd:
  addiw t0, t1, 1

  ecall
  ebreak
  mret
  sret
  wfi
  amoadd.b  t0, t2, (t1)
  amomin.h  t0, t2, (t1)
  amomaxu.b t0, t2, (t1)
  amoswap.h t0, t2, (t1)
  amocas.b t0, t2, (t1)
  amocas.h t0, t2, (t1)
  wrs.nto
  wrs.sto

  amocas.w t0, t2, (t1)
  amocas.d t0, t2, (t1)
  amocas.q a0, a2, (t1)

  fence
  fence.i
