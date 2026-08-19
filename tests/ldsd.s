main:
  addi sp, sp, -16

  li t0, -1
  sd t0, 0(sp)
  ld t1, 0(sp)

  li t2, 0x8899aabbccddeeff
  sd t2, 8(sp)
  lbu t3, 15(sp)
  ld t4, 8(sp)
