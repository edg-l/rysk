# What software floating point costs, against bench/loop.s, which is the same shape in
# integers: two operations, a decrement and a taken backward branch, ten million times.
#
# The two chosen are the cheap ones. A double add and a double multiply are what a
# compiler emits most of, and each is one instruction on the host and several hundred
# here, because every one of them goes through rounding this machine does in integers.
main:
  lui  t0, 2441
  addi t0, t0, 1664
  fcvt.d.l f1, t0
  fcvt.d.l f2, t0
back:
  fadd.d f3, f1, f2
  fmul.d f4, f1, f2
  addi t0, t0, -1
  bne  t0, zero, back
