use std::{fs::File, io::Read};

use rstest::rstest;
use rysk::cpu::Cpu;

#[rstest]
#[case::addi("tests/addi.bin", &[(31, 6)], &[], &[])]
#[case::csr("tests/csr.bin", &[(5, 1), (6, 2), (7, 3)], &[], &[(256, 4), (261, 5), (321, 6), (768, 1), (773, 2), (833, 3)])]
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
