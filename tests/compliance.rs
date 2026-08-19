//! The official riscv-tests corpus, run as one test per group.
//!
//! `make corpus` builds it; the suite fails loudly rather than skipping if it is
//! missing, because a compliance suite that quietly does not run is worse than none.

use std::{fs, path::PathBuf};

use rysk::{dram::DRAM_SIZE, elf, htif, machine, machine::Machine};

/// Generous for a corpus test, which is a few thousand instructions, and short enough
/// that a program which will never finish says so quickly.
const MAX_STEPS: u64 = 10_000_000;

fn corpus() -> PathBuf {
    match std::env::var_os("RISCV_TESTS") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/rysk/isa"),
    }
}

/// Tests that need something rysk does not have yet, and what each of them waits on.
/// A test on this list is expected to fail; one that starts passing is reported, so the
/// list can only shrink and cannot quietly go stale.
const WAITING: &[(&str, &str)] = &[("rv64mi-p-breakpoint", "debug triggers")];

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
        let mut hart =
            Machine::from_elf(&image, DRAM_SIZE, 1).unwrap_or_else(|e| panic!("{name}: {e}"));
        machine::virt(&mut hart.bus, 1);

        let waiting = WAITING
            .iter()
            .find(|(test, _)| *test == name)
            .map(|(_, on)| on);
        match (htif::run(&mut hart, tohost, MAX_STEPS), waiting) {
            (htif::Outcome::Passed, None) => {}
            (htif::Outcome::Passed, Some(on)) => failures.push(format!(
                "  {name}: passes now, but is still listed as waiting on {on}"
            )),
            (outcome, None) => failures.push(format!("  {name}: {outcome}")),
            (_, Some(_)) => {}
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

#[test]
fn rv64uc_compressed_instructions() {
    run_group("rv64uc-p-");
}

#[test]
fn rv64uf_single_precision() {
    run_group("rv64uf-p-");
}

#[test]
fn rv64ud_double_precision() {
    run_group("rv64ud-p-");
}

#[test]
fn rv64si_supervisor_mode() {
    run_group("rv64si-p-");
}

#[test]
fn rv64mi_machine_mode() {
    run_group("rv64mi-p-");
}
