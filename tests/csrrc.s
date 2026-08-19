main:
  addi t0, zero, 15
  csrrw zero, mscratch, t0
  addi t1, zero, 5
  csrrc t2, mscratch, t1
  csrrs t3, mscratch, zero
  csrrci t4, mscratch, 8
  csrrs t5, mscratch, zero
