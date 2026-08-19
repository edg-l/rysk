# riscv64-unknown-elf-gcc if it is installed, otherwise clang, whose integrated
# assembler and lld target RISC-V without a cross toolchain.
CROSS := riscv64-unknown-elf
# The clang to use, for where the one on the path is too old to know an extension rysk
# implements. CI installs a versioned one and names it here.
CLANG   ?= clang
LLD     ?= lld
LLVM_OBJCOPY ?= llvm-objcopy
ARCH    := rv64gc_zicond_zacas_zabha_zawrs

ifneq ($(shell command -v $(CROSS)-gcc),)
CC      = $(CROSS)-gcc -march=$(ARCH)
LDFLAGS = -Wl,-Ttext=0x0
LDLINK  =
OBJCOPY = $(CROSS)-objcopy
else
CC      = $(CLANG) --target=$(CROSS) -march=$(ARCH) -mno-relax
LDFLAGS = -fuse-ld=$(LLD) -Wl,--image-base=0,-Ttext=0x0
LDLINK  = -fuse-ld=$(LLD)
OBJCOPY = $(LLVM_OBJCOPY)
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
CACHE     ?= $(HOME)/.cache/rysk
CORPUS    ?= $(CACHE)/isa
CORPUS_SRC = $(CACHE)/riscv-tests
CORPUS_URL = https://github.com/riscv-software-src/riscv-tests
# Pinned so a rebuild is reproducible and CI cannot change what it gates on without
# the change showing up here.
CORPUS_REV = 2ebecad997fa58cd9e5724340ba75aa4b59bd1d0
GROUPS     = rv64ui rv64um rv64ua rv64uc rv64uf rv64ud rv64si rv64mi
# The corpus is assembled from a copy of its sources, patched for what clang will not
# take. Two things: the supervisor and machine groups define a handler riscv_test.h has
# already declared weak, and clang refuses to rebind a weak symbol to global where gcc
# allows it; and `tcontrol` is a debug-mode csr only recent assemblers know by name, so
# it goes in by the number the corpus's own encoding.h gives it. Four of the rv64mi
# tests include ../rv64si sources directly, so the copy keeps the tree shape.
PATCHED    = $(CACHE)/patched

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
	@rm -rf $(PATCHED)
	@for group in $(GROUPS); do \
	  mkdir -p $(PATCHED)/$$group; \
	  for src in $(CORPUS_SRC)/isa/$$group/*.S; do \
	    sed -E -e 's/\.global (m|s)tvec_handler/.weak \1tvec_handler/' \
	           -e 's/(csr[a-z]+[[:space:]]+)tcontrol/\10x7a5/' \
	      $$src > $(PATCHED)/$$group/$$(basename $$src); \
	  done; \
	done
	@for group in $(GROUPS); do \
	  for src in $(PATCHED)/$$group/*.S; do \
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
bench: bench/loop.bin bench/paging.bin
	cargo build --release
	hyperfine -w3 -r15 -N -L prog $^ './target/release/rysk {prog}'

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
