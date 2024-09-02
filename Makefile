add-addi.bin: add-addi.s
	riscv64-unknown-elf-gcc -Wl,-Ttext=0x0 -nostdlib -o tests/add-addi tests/add-addi.s
	riscv64-unknown-elf-objcopy -O binary tests/add-addi tests/add-addi.bin

clean:
	rm -f tests/add-addi
	rm -f tests/add-addi.bin
	rm -f tests/*.bin
