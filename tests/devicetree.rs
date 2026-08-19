//! The device tree rysk builds, read back.
//!
//! A serialiser checked by its own parser is only half an argument, so what this
//! asserts is the things a consumer will actually look for: that the header describes
//! the blob it is in, that every node closes, and that the addresses in the tree are
//! the ones the bus decodes.

use rysk::{bus::DRAM_BASE, clint, dram::DRAM_SIZE, machine, pci, plic, uart};
use std::collections::HashMap;

/// Every property in the tree, by its path, which is enough to check what a driver
/// would look up without pretending to be a driver.
fn parse(blob: &[u8]) -> HashMap<String, Vec<u8>> {
    let word = |at: usize| u32::from_be_bytes(blob[at..at + 4].try_into().unwrap());
    assert_eq!(word(0), 0xd00d_feed, "magic");
    assert_eq!(
        word(4) as usize,
        blob.len(),
        "the header describes the blob"
    );
    assert_eq!(word(20), 17, "version");
    assert_eq!(word(24), 16, "last compatible version");

    let structure = word(8) as usize;
    let strings = word(12) as usize;
    let name = |at: u32| {
        let from = strings + at as usize;
        let len = blob[from..].iter().position(|&b| b == 0).unwrap();
        String::from_utf8(blob[from..from + len].to_vec()).unwrap()
    };

    let mut props = HashMap::new();
    let mut path: Vec<String> = Vec::new();
    let mut at = structure;
    loop {
        let token = word(at);
        at += 4;
        match token {
            1 => {
                let len = blob[at..].iter().position(|&b| b == 0).unwrap();
                path.push(String::from_utf8(blob[at..at + len].to_vec()).unwrap());
                at = (at + len + 4) & !3;
            }
            2 => {
                path.pop().expect("a node closed that was never opened");
            }
            3 => {
                let (len, offset) = (word(at) as usize, word(at + 4));
                at += 8;
                let key = format!("{}/{}", path.join("/"), name(offset));
                props.insert(key, blob[at..at + len].to_vec());
                at = (at + len + 3) & !3;
            }
            4 => {}
            9 => break,
            other => panic!("token {other} at {}", at - 4),
        }
    }
    assert!(path.is_empty(), "a node was left open: {path:?}");
    props
}

fn cells(value: &[u8]) -> Vec<u32> {
    value
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

fn text(value: &[u8]) -> String {
    String::from_utf8(value[..value.len() - 1].to_vec()).unwrap()
}

#[test]
fn the_tree_describes_the_machine_the_bus_decodes() {
    let blob = machine::describe("rv64imac", DRAM_SIZE, 1, &machine::Boot::default());
    let tree = parse(&blob);

    let reg = |path: &str| cells(&tree[path]);
    assert_eq!(
        reg("/memory@80000000/reg"),
        [
            (DRAM_BASE >> 32) as u32,
            DRAM_BASE as u32,
            (DRAM_SIZE >> 32) as u32,
            DRAM_SIZE as u32
        ],
        "the memory the dram actually is"
    );
    for (path, base, size) in [
        ("/soc/serial@10000000/reg", uart::BASE, uart::SIZE),
        ("/soc/plic@c000000/reg", plic::BASE, plic::SIZE),
        ("/soc/clint@2000000/reg", clint::BASE, clint::SIZE),
    ] {
        assert_eq!(
            reg(path),
            [
                (base >> 32) as u32,
                base as u32,
                (size >> 32) as u32,
                size as u32
            ],
            "{path} is where the bus put it"
        );
    }

    assert_eq!(
        cells(&tree["/cpus/timebase-frequency"]),
        [clint::FREQUENCY as u32],
        "the rate mtime counts at, which every timeout is computed from"
    );
    assert_eq!(text(&tree["/cpus/cpu@0/riscv,isa"]), "rv64imac");
    assert_eq!(text(&tree["/cpus/cpu@0/mmu-type"]), "riscv,sv39");
    assert_eq!(
        text(&tree["/chosen/stdout-path"]),
        "/soc/serial@10000000",
        "and it points at a node that exists"
    );
    assert!(tree.contains_key("/soc/serial@10000000/compatible"));
}

#[test]
fn a_device_names_the_controller_its_line_runs_to() {
    let tree = parse(&machine::describe(
        "rv64imac",
        DRAM_SIZE,
        1,
        &machine::Boot::default(),
    ));
    let intc = cells(&tree["/cpus/cpu@0/interrupt-controller/phandle"])[0];
    let plic = cells(&tree["/soc/plic@c000000/phandle"])[0];

    assert_eq!(
        cells(&tree["/soc/serial@10000000/interrupt-parent"]),
        [plic],
        "the serial port reports to the plic"
    );
    assert_eq!(
        cells(&tree["/soc/plic@c000000/interrupts-extended"]),
        [intc, 11, intc, 9],
        "and the plic to the hart, at machine and supervisor level"
    );
    assert_eq!(
        cells(&tree["/soc/clint@2000000/interrupts-extended"]),
        [intc, 3, intc, 7],
        "while the clint drives the software and timer causes directly"
    );
    let source = cells(&tree["/soc/serial@10000000/interrupts"])[0];
    assert!(
        source <= cells(&tree["/soc/plic@c000000/riscv,ndev"])[0],
        "a source the controller says it has"
    );
}

#[test]
fn what_a_loader_hands_over_reaches_the_tree() {
    let options = machine::Boot {
        bootargs: Some("console=ttyS0 rdinit=/bin/sh".into()),
        initrd: Some((0x8700_0000, 0x8710_0000)),
    };
    let tree = parse(&machine::describe("rv64imac", DRAM_SIZE, 1, &options));
    assert_eq!(
        text(&tree["/chosen/bootargs"]),
        "console=ttyS0 rdinit=/bin/sh"
    );
    assert_eq!(cells(&tree["/chosen/linux,initrd-start"]), [0, 0x8700_0000]);
    assert_eq!(cells(&tree["/chosen/linux,initrd-end"]), [0, 0x8710_0000]);

    // And none of it appears when there is none of it to say.
    let bare = parse(&machine::describe(
        "rv64imac",
        DRAM_SIZE,
        1,
        &machine::Boot::default(),
    ));
    assert!(!bare.contains_key("/chosen/bootargs"));
    assert!(!bare.contains_key("/chosen/linux,initrd-start"));
}

#[test]
fn every_hart_is_described_and_named_by_the_number_it_reports() {
    let tree = parse(&machine::describe(
        "rv64imac",
        DRAM_SIZE,
        4,
        &machine::Boot::default(),
    ));

    let intc: Vec<u32> = (0..4)
        .map(|hart| cells(&tree[&format!("/cpus/cpu@{hart}/interrupt-controller/phandle")])[0])
        .collect();
    for hart in 0..4u32 {
        assert_eq!(
            cells(&tree[&format!("/cpus/cpu@{hart}/reg")]),
            [hart],
            "the node is named by the mhartid it reports"
        );
    }
    let plic = cells(&tree["/soc/plic@c000000/phandle"])[0];
    let mut phandles = intc.clone();
    phandles.push(plic);
    phandles.sort_unstable();
    phandles.dedup();
    assert_eq!(
        phandles.len(),
        5,
        "every controller has a name of its own, and none of them is zero"
    );
    assert!(phandles[0] > 0);

    // Both controllers reach every hart, and the order they are listed in is what
    // numbers the plic's contexts, so it is not free to vary.
    assert_eq!(
        cells(&tree["/soc/plic@c000000/interrupts-extended"]),
        intc.iter()
            .flat_map(|&hart| [hart, 11, hart, 9])
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        cells(&tree["/soc/clint@2000000/interrupts-extended"]),
        intc.iter()
            .flat_map(|&hart| [hart, 3, hart, 7])
            .collect::<Vec<_>>(),
    );
}

#[test]
fn the_tree_says_where_the_root_complex_is_and_where_its_interrupts_go() {
    let tree = parse(&machine::describe(
        "rv64imac",
        DRAM_SIZE,
        2,
        &machine::Boot::default(),
    ));
    let node = "/soc/pci@30000000";

    assert_eq!(
        text(&tree[&format!("{node}/compatible")]),
        "pci-host-ecam-generic",
        "the binding a kernel already has a driver for"
    );
    assert_eq!(
        cells(&tree[&format!("{node}/reg")]),
        [
            (pci::ECAM >> 32) as u32,
            pci::ECAM as u32,
            (pci::ECAM_SIZE >> 32) as u32,
            pci::ECAM_SIZE as u32
        ],
        "config space, which is the only part of it that is placed rather than found"
    );
    assert_eq!(
        cells(&tree[&format!("{node}/bus-range")]),
        [0, 0xff],
        "as many buses as config space has room for"
    );
    assert_eq!(cells(&tree[&format!("{node}/#address-cells")]), [3]);
    assert_eq!(cells(&tree[&format!("{node}/#interrupt-cells")]), [1]);

    // Each window as the space it is in, where it starts down there, where it starts
    // up here, and how big it is.
    let ranges = cells(&tree[&format!("{node}/ranges")]);
    assert_eq!(ranges.len(), 21, "three windows of seven cells each");
    let window = |n: usize| {
        let w = &ranges[n * 7..n * 7 + 7];
        (
            w[0] >> 24,
            ((w[3] as u64) << 32) | w[4] as u64,
            ((w[5] as u64) << 32) | w[6] as u64,
        )
    };
    assert_eq!(
        window(0),
        (0x01, pci::PIO, pci::PIO_SIZE),
        "the port window"
    );
    assert_eq!(
        window(1),
        (0x02, pci::MMIO, pci::MMIO_SIZE),
        "the 32-bit window, which ends where dram begins"
    );
    assert_eq!(
        window(1).1 + window(1).2,
        DRAM_BASE,
        "and really does end there"
    );
    assert_eq!(
        window(2),
        (0x03, pci::MMIO64, pci::MMIO64_SIZE),
        "the 64-bit window"
    );

    // The map has to say exactly what the swizzle computes, or an interrupt is
    // delivered as a different one than the tree promised.
    assert_eq!(
        cells(&tree[&format!("{node}/interrupt-map-mask")]),
        [0x1800, 0, 0, 7]
    );
    let map = cells(&tree[&format!("{node}/interrupt-map")]);
    assert_eq!(
        map.len(),
        16 * 6,
        "four devices of four pins, six cells each"
    );
    let plic = cells(&tree["/soc/plic@c000000/phandle"])[0];
    let ndev = cells(&tree["/soc/plic@c000000/riscv,ndev"])[0];
    for entry in map.chunks(6) {
        let (device, pin) = ((entry[0] >> 11) as usize, entry[3] as u8);
        assert_eq!(entry[4], plic, "every pin runs to the platform controller");
        assert_eq!(
            entry[5] as usize,
            32 + pci::swizzle(device, pin),
            "device {device} pin {pin} lands where the swizzle puts it"
        );
        assert!(
            entry[5] <= ndev,
            "on a source the controller says it answers for"
        );
    }
}
