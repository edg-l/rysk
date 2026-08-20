# The device benchmark, and the thing to point a window at.
#
# Every other benchmark here attaches no device and never leaves dram, so none of them
# measures what an access to the bus costs. This one is nothing but such accesses: it
# places the bochs card's two windows itself, writes the mode drm/tiny/bochs.c would
# write, and then fills the picture, which is three hundred thousand stores to a
# framebuffer per frame and nothing else.
#
# It needs the card, so it is not in the list `make bench` times:
#
#   ./target/release/rysk --display bochs bench/display.bin
#   ./target/release/rysk --display bochs --gui bench/display.bin
#
# Both modes in turn, twenty frames each, so that the four-byte pixel and the two-byte
# one are both measured, and then off the end into a trap, which is how a run here ends.
# About a second, which is the same order as the other four.
main:
  li   t0, 0x30008000        # the card's config space: ecam, device one
  li   t1, 0x40000000        # where the framebuffer goes
  li   t2, 0x41000000        # and the registers, just past it
  sw   t1, 0x10(t0)          # bar 0
  sw   t2, 0x18(t0)          # bar 2
  li   t3, 2
  sw   t3, 0x04(t0)          # command: memory space enable

  li   s0, 0                 # which frame this is

  # 640x480, four bytes to a pixel, in the order the driver writes the registers
  li   t4, 0
  sh   t4, 0x508(t2)         # enable off, while the rest is written
  li   t4, 32
  sh   t4, 0x506(t2)         # bits per pixel
  li   t4, 640
  sh   t4, 0x502(t2)         # visible width
  sh   t4, 0x50c(t2)         # virtual width
  li   t4, 480
  sh   t4, 0x504(t2)         # visible height
  li   t4, 0
  sh   t4, 0x50a(t2)         # bank
  sh   t4, 0x510(t2)         # x offset
  sh   t4, 0x512(t2)         # y offset
  li   t4, 6553
  sh   t4, 0x50e(t2)         # virtual height, the whole of video memory
  li   t4, 0x41
  sh   t4, 0x508(t2)         # enabled, linear framebuffer

  li   s3, 640
  li   s4, 480
  li   s5, 20
wide:
  mv   a0, t1
  li   s1, 0
wide_row:
  li   s2, 0
wide_col:
  add  a1, s2, s0
  andi a1, a1, 0xff
  slli a1, a1, 16            # red moves with the column and with the frame
  andi a2, s1, 0xff
  slli a2, a2, 8             # green is the row
  or   a1, a1, a2
  andi a2, s0, 0xff
  or   a1, a1, a2            # blue is the frame
  sw   a1, 0(a0)
  addi a0, a0, 4
  addi s2, s2, 1
  bne  s2, s3, wide_col
  addi s1, s1, 1
  bne  s1, s4, wide_row
  addi s0, s0, 1
  addi s5, s5, -1
  bnez s5, wide

  # 320x240, two bytes to a pixel
  li   t4, 0
  sh   t4, 0x508(t2)
  li   t4, 16
  sh   t4, 0x506(t2)
  li   t4, 320
  sh   t4, 0x502(t2)
  sh   t4, 0x50c(t2)
  li   t4, 240
  sh   t4, 0x504(t2)
  li   t4, 0
  sh   t4, 0x50a(t2)
  sh   t4, 0x510(t2)
  sh   t4, 0x512(t2)
  li   t4, 6553
  sh   t4, 0x50e(t2)
  li   t4, 0x41
  sh   t4, 0x508(t2)

  li   s3, 320
  li   s4, 240
  li   s5, 20
narrow:
  mv   a0, t1
  li   s1, 0
narrow_row:
  li   s2, 0
narrow_col:
  add  a1, s2, s0
  srli a1, a1, 1
  andi a1, a1, 0x1f
  slli a1, a1, 11            # five bits of red
  srli a2, s1, 2
  andi a2, a2, 0x3f
  slli a2, a2, 5             # six bits of green
  or   a1, a1, a2
  andi a2, s0, 0x1f          # five bits of blue
  or   a1, a1, a2
  sh   a1, 0(a0)
  addi a0, a0, 2
  addi s2, s2, 1
  bne  s2, s3, narrow_col
  addi s1, s1, 1
  bne  s1, s4, narrow_row
  addi s0, s0, 1
  addi s5, s5, -1
  bnez s5, narrow

  # and off the end into the zeroed dram behind it.
