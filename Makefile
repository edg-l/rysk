

SRCS = $(wildcard tests/*.s tests/*.c)

PROGS = $(patsubst %.s,%.bin,$(SRCS))
C_PROGS = $(patsubst %.c,%.bin,$(SRCS))

.PHONY: test
test: test_files
	cargo t

test_files: $(PROGS) $(C_PROGS)

%.bin: %.s
	riscv64-unknown-elf-gcc -Wl,-Ttext=0x0 -nostdlib -o $@ $<
	riscv64-unknown-elf-objcopy -O binary $@ $@

%.s: %.c
	riscv64-unknown-elf-gcc -S $< -o $@

.PHONY: clean
clean:
	rm -rf tests/*.bin
