# The expensive two, which are a loop in software where they are one instruction with a
# long latency on the host. Kept apart from bench/float.s because the ratio is not the
# same, and a fast path that helps one may not help the other.
main:
  lui  t0, 2441
  addi t0, t0, 1664
  fcvt.d.l f1, t0
  fcvt.d.l f2, t0
back:
  fdiv.d f3, f1, f2
  fsqrt.d f4, f1
  addi t0, t0, -1
  bne  t0, zero, back
