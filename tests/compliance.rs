//! The official riscv-tests corpus, run as one test per group.
//!
//! `make corpus` builds it; the suite fails loudly rather than skipping if it is
//! missing, because a compliance suite that quietly does not run is worse than none.

use std::{fs, path::PathBuf};

use rysk::{cpu::Cpu, elf, htif};

/// Generous for a corpus test, which is a few thousand instructions, and short enough
/// that a program which will never finish says so quickly.
const MAX_STEPS: u64 = 10_000_000;

fn corpus() -> PathBuf {
    match std::env::var_os("RISCV_TESTS") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/rysk/isa"),
    }
}

/// Run every test whose name starts with `group`, and report all of the failures
/// rather than only the first.
fn run_group(group: &str) {
    let dir = corpus();
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| {
        panic!(
            "no corpus at {}: {e}\nrun `make corpus`, or set RISCV_TESTS to where it is",
            dir.display()
        )
    });

    let mut tests: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(group))
        })
        .collect();
    tests.sort();

    assert!(
        !tests.is_empty(),
        "no {group} tests in {}: run `make corpus`",
        dir.display()
    );

    let mut failures = Vec::new();
    for test in &tests {
        let name = test.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = fs::read(test).expect("read test");
        let image = elf::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let tohost = htif::tohost(&image)
            .unwrap_or_else(|| panic!("{name} has no tohost symbol, so it cannot report"));
        let mut cpu = Cpu::from_elf(&image).unwrap_or_else(|e| panic!("{name}: {e}"));

        match htif::run(&mut cpu, tohost, MAX_STEPS) {
            htif::Outcome::Passed => {}
            outcome => failures.push(format!("  {name}: {outcome}")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} {group} tests failed:\n{}",
        failures.len(),
        tests.len(),
        failures.join("\n")
    );
    println!("{group}: {} passed", tests.len());
}

#[test]
fn rv64ui_the_base_integer_set() {
    run_group("rv64ui-p-");
}

#[test]
fn rv64um_multiply_and_divide() {
    run_group("rv64um-p-");
}

#[test]
fn rv64ua_atomics() {
    run_group("rv64ua-p-");
}
