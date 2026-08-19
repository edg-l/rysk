main:
  addi sp, sp, -16
  addi t0, zero, 7
  sw t0, 0(sp)

  lr.w t1, (sp)
  sw t0, 0(sp)
  sc.w t2, t0, (sp)

  lr.w t3, (sp)
  sc.w t4, t0, (sp)
