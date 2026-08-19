# riscv64-unknown-elf-gcc if it is installed, otherwise clang, whose integrated
# assembler and lld target RISC-V without a cross toolchain.
CROSS := riscv64-unknown-elf

ifneq ($(shell command -v $(CROSS)-gcc),)
CC      = $(CROSS)-gcc -march=rv64g
LDFLAGS = -Wl,-Ttext=0x0
OBJCOPY = $(CROSS)-objcopy
else
CC      = clang --target=$(CROSS) -march=rv64g -mno-relax
LDFLAGS = -fuse-ld=lld -Wl,--image-base=0,-Ttext=0x0
OBJCOPY = llvm-objcopy
endif

SRCS = $(wildcard tests/*.s tests/*.c)

PROGS = $(patsubst %.s,%.bin,$(SRCS))
C_PROGS = $(patsubst %.c,%.bin,$(SRCS))

.PHONY: test
test: test_files
	cargo t

test_files: $(PROGS) $(C_PROGS)

%.bin: %.s
	$(CC) $(LDFLAGS) -nostdlib -o $@ $<
	$(OBJCOPY) -O binary $@ $@

%.s: %.c
	$(CC) -S $< -o $@

.PHONY: clean
clean:
	rm -rf tests/*.bin
