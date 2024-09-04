

SRCS = $(wildcard tests/*.s)

PROGS = $(patsubst %.s,%.bin,$(SRCS))

.PHONY: test
test: test_files
	cargo t

test_files: $(PROGS)

%.bin: %.s
	riscv64-unknown-elf-gcc -Wl,-Ttext=0x0 -nostdlib -o $@ $<
	riscv64-unknown-elf-objcopy -O binary $@ $@
