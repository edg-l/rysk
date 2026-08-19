//! The device tree rysk hands its guest, built rather than loaded.
//!
//! Firmware and a kernel both learn what the machine is from this: how many harts,
//! what the memory is, where each device sits and which interrupt it drives. Building
//! it from the same description that builds the bus is what stops the two disagreeing,
//! which is a class of bug that presents as a driver reading a device that is not
//! there.
//!
//! Devicetree Specification 0.4, chapter 5.

/// The tokens that make up the structure block.
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;

const MAGIC: u32 = 0xd00d_feed;
const VERSION: u32 = 17;
const LAST_COMPATIBLE: u32 = 16;
const HEADER_SIZE: usize = 40;

/// A tree under construction. Everything in the format is big-endian and padded to
/// four bytes, so both are done here rather than remembered at every call.
#[derive(Debug, Default)]
pub struct Fdt {
    structure: Vec<u8>,
    strings: Vec<u8>,
    depth: usize,
}

impl Fdt {
    pub fn new() -> Self {
        Self::default()
    }

    fn token(&mut self, token: u32) {
        self.structure.extend_from_slice(&token.to_be_bytes());
    }

    /// Property names are pooled, since a tree repeats a handful of them constantly.
    fn intern(&mut self, name: &str) -> u32 {
        let wanted = name.as_bytes();
        let mut at = 0;
        while at < self.strings.len() {
            let end = at + self.strings[at..].iter().position(|&b| b == 0).unwrap();
            if &self.strings[at..end] == wanted {
                return at as u32;
            }
            at = end + 1;
        }
        let offset = self.strings.len() as u32;
        self.strings.extend_from_slice(wanted);
        self.strings.push(0);
        offset
    }

    fn pad(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    pub fn begin_node(&mut self, name: &str) {
        self.token(BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
        self.depth += 1;
    }

    pub fn end_node(&mut self) {
        self.token(END_NODE);
        self.depth -= 1;
    }

    pub fn prop(&mut self, name: &str, value: &[u8]) {
        let offset = self.intern(name);
        self.token(PROP);
        self.token(value.len() as u32);
        self.token(offset);
        self.structure.extend_from_slice(value);
        self.pad();
    }

    /// A property that is true by existing and carries nothing, like `ranges` on a bus
    /// that does not translate.
    pub fn flag(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    pub fn string(&mut self, name: &str, value: &str) {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.prop(name, &bytes);
    }

    /// Several strings in one property, which is how `compatible` lists the drivers
    /// that will do, most specific first.
    pub fn strings(&mut self, name: &str, values: &[&str]) {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(0);
        }
        self.prop(name, &bytes);
    }

    /// A property of 32-bit cells, which is what every number in a tree is made of. A
    /// 64-bit value is two of them, high half first, because a tree says how many
    /// cells an address takes rather than how wide one is.
    pub fn cells(&mut self, name: &str, cells: &[u32]) {
        let mut bytes = Vec::with_capacity(cells.len() * 4);
        for cell in cells {
            bytes.extend_from_slice(&cell.to_be_bytes());
        }
        self.prop(name, &bytes);
    }

    /// An address and a size, each as the two cells the root says they take.
    pub fn reg(&mut self, base: u64, size: u64) {
        self.cells(
            "reg",
            &[
                (base >> 32) as u32,
                base as u32,
                (size >> 32) as u32,
                size as u32,
            ],
        );
    }

    /// Serialise the tree. `reserved` is the memory a guest must not use, which is
    /// where the tree itself ends up.
    pub fn finish(mut self, reserved: &[(u64, u64)]) -> Vec<u8> {
        assert_eq!(self.depth, 0, "a node was left open");
        self.token(END);

        let mut reservations = Vec::new();
        for (base, size) in reserved.iter().chain(&[(0, 0)]) {
            reservations.extend_from_slice(&base.to_be_bytes());
            reservations.extend_from_slice(&size.to_be_bytes());
        }

        let struct_at = HEADER_SIZE + reservations.len();
        let strings_at = struct_at + self.structure.len();
        let total = strings_at + self.strings.len();

        let mut blob = Vec::with_capacity(total);
        for word in [
            MAGIC,
            total as u32,
            struct_at as u32,
            strings_at as u32,
            HEADER_SIZE as u32,
            VERSION,
            LAST_COMPATIBLE,
            0, // the hart that is booting
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            blob.extend_from_slice(&word.to_be_bytes());
        }
        blob.extend_from_slice(&reservations);
        blob.extend_from_slice(&self.structure);
        blob.extend_from_slice(&self.strings);
        blob
    }
}
