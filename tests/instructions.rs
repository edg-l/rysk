use std::{fs::File, io::Read};

use rstest::rstest;
use rysk::cpu::Cpu;

#[rstest]
#[case::addi("tests/addi.bin", &[(31, 6)], &[], &[])]
#[case::csr("tests/csr.bin", &[(5, 1), (6, 2), (7, 3)], &[], &[(256, 4), (261, 5), (321, 6), (768, 1), (773, 2), (833, 3)])]
#[case::fib("tests/fib.bin", &[(14, 1), (15, 0x37)], &[], &[])]
#[case::ldsd("tests/ldsd.bin", &[(6, u64::MAX), (28, 0x88), (29, 0x8899aabbccddeeff)], &[], &[])]
#[case::csrrc("tests/csrrc.bin", &[(7, 15), (28, 10), (29, 10), (30, 2)], &[], &[(0x340, 2)])]
#[case::remzero("tests/remzero.bin", &[(7, 42), (28, 42), (29, 42), (30, 42), (31, u64::MAX)], &[], &[])]
#[case::lrsc("tests/lrsc.bin", &[(6, 7), (7, 1), (28, 7), (29, 0)], &[], &[])]
#[case::shifts("tests/shifts.bin", &[(6, 1 << 40), (7, 16), (29, u64::MAX), (30, 15), (31, 1 << 63)], &[], &[])]
fn run_test(
    #[case] path: &str,
    #[case] expected_regs: &[(usize, u64)],
    #[case] expected_mem: &[(usize, u8)],
    #[case] expected_csr: &[(usize, u64)],
) {
    let mut file = File::open(path).expect("did you run 'make test' ?");
    let mut code = Vec::new();
    file.read_to_end(&mut code).unwrap();

    let mut cpu = Cpu::new(code);
    cpu.run().unwrap();

    cpu.dump_registers();
    cpu.dump_csr();

    assert_eq!(cpu.regs[0], 0, "zero register is not 0");

    for (reg, value) in expected_regs {
        assert_eq!(cpu.regs[*reg], *value, "register mismatch");
    }

    for (addr, value) in expected_mem {
        assert_eq!(cpu.bus.dram.dram[*addr], *value, "memory mismatch");
    }

    for (addr, value) in expected_csr {
        assert_eq!(cpu.csrs[*addr], *value, "csrs mismatch");
    }
}
