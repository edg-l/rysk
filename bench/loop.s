# The interpreter benchmark: 10,000,000 iterations of four instructions, so 40M
# retired, which is long enough to time and short enough to profile. fib is far too
# short to measure anything but startup.
#
# The mix is deliberate: two arithmetic, one logical, one taken backward branch. It
# touches no memory beyond instruction fetch, so it measures dispatch rather than the
# load path.
main:
  lui  t0, 2441
  addi t0, t0, 1664
back:
  addi t1, t1, 1
  xor  t2, t1, t0
  addi t0, t0, -1
  bne  t0, zero, back
