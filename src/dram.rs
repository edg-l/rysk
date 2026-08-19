use crate::bus::DRAM_BASE;

pub const DRAM_SIZE: u64 = 1024 * 1024 * 128; // 128MiB

#[derive(Debug, Clone)]
pub struct Dram {
    pub dram: Vec<u8>,
}

impl Dram {
    pub fn new(code: Vec<u8>) -> Dram {
        let mut dram = vec![0; DRAM_SIZE as usize];
        dram.splice(..code.len(), code);

        Self { dram }
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

    #[inline]
    pub fn load(&self, addr: u64, size: u64) -> u64 {
        match size {
            8 => self.load8(addr),
            16 => self.load16(addr),
            32 => self.load32(addr),
            64 => self.load64(addr),
            _ => unreachable!("load of {size} bits"),
        }
    }

    #[inline]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) {
        match size {
            8 => self.store8(addr, value),
            16 => self.store16(addr, value),
            32 => self.store32(addr, value),
            64 => self.store64(addr, value),
            _ => unreachable!("store of {size} bits"),
        }
    }

    #[inline]
    fn load64(&self, addr: u64) -> u64 {
        let index = (addr - DRAM_BASE) as usize;
        (self.dram[index] as u64)
            | ((self.dram[index + 1] as u64) << 8)
            | ((self.dram[index + 2] as u64) << 16)
            | ((self.dram[index + 3] as u64) << 24)
            | ((self.dram[index + 4] as u64) << 32)
            | ((self.dram[index + 5] as u64) << 40)
            | ((self.dram[index + 6] as u64) << 48)
            | ((self.dram[index + 7] as u64) << 56)
    }

    #[inline]
    fn store64(&mut self, addr: u64, value: u64) {
        let index = (addr - DRAM_BASE) as usize;
        self.dram[index] = (value & 0xff) as u8;
        self.dram[index + 1] = ((value >> 8) & 0xff) as u8;
        self.dram[index + 2] = ((value >> 16) & 0xff) as u8;
        self.dram[index + 3] = ((value >> 24) & 0xff) as u8;
        self.dram[index + 4] = ((value >> 32) & 0xff) as u8;
        self.dram[index + 5] = ((value >> 40) & 0xff) as u8;
        self.dram[index + 6] = ((value >> 48) & 0xff) as u8;
        self.dram[index + 7] = ((value >> 56) & 0xff) as u8;
    }

    #[inline]
    fn load32(&self, addr: u64) -> u64 {
        let index = (addr - DRAM_BASE) as usize;
        (self.dram[index] as u64)
            | ((self.dram[index + 1] as u64) << 8)
            | ((self.dram[index + 2] as u64) << 16)
            | ((self.dram[index + 3] as u64) << 24)
    }

    #[inline]
    fn store32(&mut self, addr: u64, value: u64) {
        let index = (addr - DRAM_BASE) as usize;
        self.dram[index] = (value & 0xff) as u8;
        self.dram[index + 1] = ((value >> 8) & 0xff) as u8;
        self.dram[index + 2] = ((value >> 16) & 0xff) as u8;
        self.dram[index + 3] = ((value >> 24) & 0xff) as u8;
    }

    #[inline]
    fn load16(&self, addr: u64) -> u64 {
        let index = (addr - DRAM_BASE) as usize;
        (self.dram[index] as u64) | ((self.dram[index + 1] as u64) << 8)
    }

    #[inline]
    fn store16(&mut self, addr: u64, value: u64) {
        let index = (addr - DRAM_BASE) as usize;
        self.dram[index] = (value & 0xff) as u8;
        self.dram[index + 1] = ((value >> 8) & 0xff) as u8;
    }

    #[inline]
    fn load8(&self, addr: u64) -> u64 {
        let index = (addr - DRAM_BASE) as usize;
        self.dram[index] as u64
    }

    #[inline]
    fn store8(&mut self, addr: u64, value: u64) {
        let index = (addr - DRAM_BASE) as usize;
        self.dram[index] = (value & 0xff) as u8;
    }
}
