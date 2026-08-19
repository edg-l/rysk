# riscv64-unknown-elf-gcc if it is installed, otherwise clang, whose integrated
# assembler and lld target RISC-V without a cross toolchain.
CROSS := riscv64-unknown-elf

ifneq ($(shell command -v $(CROSS)-gcc),)
CC      = $(CROSS)-gcc -march=rv64g_zicond
LDFLAGS = -Wl,-Ttext=0x0
LDLINK  =
OBJCOPY = $(CROSS)-objcopy
else
CC      = clang --target=$(CROSS) -march=rv64g_zicond -mno-relax
LDFLAGS = -fuse-ld=lld -Wl,--image-base=0,-Ttext=0x0
LDLINK  = -fuse-ld=lld
OBJCOPY = llvm-objcopy
endif

SRCS = $(wildcard tests/*.s tests/*.c)

PROGS = $(patsubst %.s,%.bin,$(SRCS))
C_PROGS = $(patsubst %.c,%.bin,$(SRCS))

.PHONY: test
test: test_files corpus
	cargo t

test_files: $(PROGS) $(C_PROGS)

%.bin: %.s
	$(CC) $(LDFLAGS) -nostdlib -o $@ $<
	$(OBJCOPY) -O binary $@ $@

%.s: %.c
	$(CC) -S $< -o $@

# The official riscv-tests corpus, built with the same flags its own Makefile uses.
# rysk implements the base integer set, multiply and atomics, so those are the groups
# that can pass; rv64mi wants supervisor mode and interrupts.
CACHE     ?= $(HOME)/.cache/rysk
CORPUS    ?= $(CACHE)/isa
CORPUS_SRC = $(CACHE)/riscv-tests
CORPUS_URL = https://github.com/riscv-software-src/riscv-tests
# Pinned so a rebuild is reproducible and CI cannot change what it gates on without
# the change showing up here.
CORPUS_REV = 2ebecad997fa58cd9e5724340ba75aa4b59bd1d0
GROUPS     = rv64ui rv64um rv64ua

.PHONY: corpus
corpus: $(CORPUS)/.stamp

$(CORPUS_SRC):
	git init -q $@
	git -C $@ remote add origin $(CORPUS_URL)
	git -C $@ fetch -q --depth 1 origin $(CORPUS_REV)
	git -C $@ checkout -q FETCH_HEAD
	# env/ is the riscv-test-env submodule, and holds the linker script and the
	# riscv_test.h every test includes.
	git -C $@ submodule update -q --init --depth 1

$(CORPUS)/.stamp: | $(CORPUS_SRC)
	@mkdir -p $(CORPUS)
	@for group in $(GROUPS); do \
	  for src in $(CORPUS_SRC)/isa/$$group/*.S; do \
	    name=$$(basename $$src .S); \
	    $(CC) -mabi=lp64 -static -mcmodel=medany -fvisibility=hidden -nostdlib \
	      -nostartfiles $(LDLINK) -I$(CORPUS_SRC)/env/p -I$(CORPUS_SRC)/isa/macros/scalar \
	      -T $(CORPUS_SRC)/env/p/link.ld $$src -o $(CORPUS)/$$group-p-$$name || exit 1; \
	  done; \
	done
	@touch $@
	@echo "built $$(ls $(CORPUS) | grep -c .) corpus tests into $(CORPUS)"

# The interpreter benchmark. The .bin is committed like the test fixtures, so
# profiling needs no cross toolchain.
.PHONY: bench
bench: bench/loop.bin
	cargo build --release
	hyperfine -w3 -r15 -N './target/release/rysk $<'

# Same run under perf. dwarf unwinding, not fp: rust omits frame pointers and fp
# walks garbage.
.PHONY: bench-profile
bench-profile: bench/loop.bin
	CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
	perf record --call-graph dwarf -F 999 -o bench/perf.data ./target/release/rysk $<
	perf report -i bench/perf.data --no-children --percent-limit 1 --stdio

.PHONY: clean
clean:
	rm -rf tests/*.bin bench/perf.data

.PHONY: clean-corpus
clean-corpus:
	rm -rf $(CORPUS)
