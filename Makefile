

SRCS = $(wildcard tests/*.s)

PROGS = $(patsubst %.s,%.bin,$(SRCS))

all: $(PROGS)

%.bin: %.s
	riscv64-unknown-elf-gcc -Wl,-Ttext=0x0 -nostdlib -o $@ $<
	riscv64-unknown-elf-objcopy -O binary $@ $@
