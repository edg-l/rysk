use crate::bus::DRAM_BASE;

/// How much memory a machine has when nothing says otherwise. Enough for a kernel and
/// the room it wants after itself, and small enough to allocate without thinking.
pub const DRAM_SIZE: u64 = 1024 * 1024 * 128;

#[derive(Debug, Clone)]
pub struct Dram {
    pub dram: Vec<u8>,
}

impl Dram {
    pub fn new(code: Vec<u8>) -> Dram {
        Self::with_size(code, DRAM_SIZE)
    }

    /// A machine with `size` bytes of memory. How much there is is a property of the
    /// machine rather than of the emulator, so it is asked for rather than assumed:
    /// a kernel image is tens of megabytes before it has run an instruction.
    pub fn with_size(code: Vec<u8>, size: u64) -> Dram {
        let mut dram = vec![0; size as usize];
        dram.splice(..code.len(), code);

        Self { dram }
    }

    /// How much memory there is, in bytes.
    pub fn size(&self) -> u64 {
        self.dram.len() as u64
    }

    /// Place `bytes` at `addr` and zero `zeroes` bytes after them, as loading an image
    /// does. Returns whether it fit.
    pub fn write(&mut self, addr: u64, bytes: &[u8], zeroes: u64) -> bool {
        let Some(start) = addr.checked_sub(DRAM_BASE) else {
            return false;
        };
        let Some(end) = (start as usize).checked_add(bytes.len() + zeroes as usize) else {
            return false;
        };
        if end > self.dram.len() {
            return false;
        }
        let start = start as usize;
        self.dram[start..start + bytes.len()].copy_from_slice(bytes);
        self.dram[start + bytes.len()..end].fill(0);
        true
    }

    /// Read `size` bits at `addr`.
    ///
    /// The width is a constant at almost every call site, so each arm folds to the one
    /// load it describes: a slice of known length has a single bounds check and turns
    /// into a single move, where a byte at a time had one of each per byte.
    #[inline]
    pub fn load(&self, addr: u64, size: u64) -> u64 {
        let index = (addr - DRAM_BASE) as usize;
        match size {
            8 => self.dram[index] as u64,
            16 => u16::from_le_bytes(self.bytes(index)) as u64,
            32 => u32::from_le_bytes(self.bytes(index)) as u64,
            64 => u64::from_le_bytes(self.bytes(index)),
            _ => unreachable!("load of {size} bits"),
        }
    }

    /// Write the low `size` bits of `value` at `addr`.
    #[inline]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) {
        let index = (addr - DRAM_BASE) as usize;
        match size {
            8 => self.dram[index] = value as u8,
            16 => self.put(index, (value as u16).to_le_bytes()),
            32 => self.put(index, (value as u32).to_le_bytes()),
            64 => self.put(index, value.to_le_bytes()),
            _ => unreachable!("store of {size} bits"),
        }
    }

    /// The `N` bytes at `index`.
    #[inline]
    fn bytes<const N: usize>(&self, index: usize) -> [u8; N] {
        self.dram[index..index + N].try_into().expect("N bytes")
    }

    /// Put `bytes` at `index`.
    #[inline]
    fn put<const N: usize>(&mut self, index: usize, bytes: [u8; N]) {
        self.dram[index..index + N].copy_from_slice(&bytes);
    }
}
