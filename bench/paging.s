# The translation benchmark: the same shape as loop.s, but running in supervisor mode
# through a three-level page table and touching memory every iteration, so it measures
# the address translation rather than the dispatch.
#
# Five instructions per iteration, two of which are a load and a store, over 8,000,000
# iterations: 40M retired, and 16M translations.
#
# The map is one gigabyte identity mapped from a single root entry, which is what a
# supervisor's own text and data live in, plus a four kilobyte page mapped elsewhere so
# the loop's working set is a translation the walk has to do rather than the one it
# just did.

  .equ DRAM,   0x80000000
  .equ ROOT,   0x80002000
  .equ MID,    0x80003000
  .equ LEAF,   0x80004000
  .equ FRAME,  0x80005000
  .equ WINDOW, 0x1000

  .equ PTE_V, 1
  .equ PTE_R, 2
  .equ PTE_W, 4
  .equ PTE_X, 8
  .equ PTE_A, 0x40
  .equ PTE_D, 0x80
  .equ LEAF_FLAGS, PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D

main:
  # root[2] maps the gigabyte holding dram as itself
  li   t0, ROOT
  li   t1, (DRAM >> 12) << 10 | LEAF_FLAGS
  sd   t1, 16(t0)

  # root[0] -> mid, mid[0] -> leaf, leaf[1] -> the frame the loop touches
  li   t1, (MID >> 12) << 10 | PTE_V
  sd   t1, 0(t0)
  li   t0, MID
  li   t1, (LEAF >> 12) << 10 | PTE_V
  sd   t1, 0(t0)
  li   t0, LEAF
  li   t1, (FRAME >> 12) << 10 | LEAF_FLAGS
  sd   t1, 8(t0)

  # satp: sv39 over the root, then drop to supervisor mode at the loop
  li   t0, ROOT >> 12
  li   t1, 8 << 60
  or   t0, t0, t1
  csrw satp, t0
  sfence.vma

  la   t0, loop
  csrw mepc, t0
  li   t0, 3 << 11
  csrc mstatus, t0
  li   t0, 1 << 11
  csrs mstatus, t0
  mret

loop:
  li   t0, 8000000
  li   t3, WINDOW
back:
  ld   t1, 0(t3)
  addi t1, t1, 1
  sd   t1, 0(t3)
  addi t0, t0, -1
  bne  t0, zero, back

  # nothing is installed to take this, so it is how the run ends
  unimp
