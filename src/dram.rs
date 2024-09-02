use crate::bus::DRAM_BASE;

pub const DRAM_SIZE: u64 = 1024 * 1024 * 128; // 128MiB

#[derive(Debug, Clone)]
pub struct Dram {
    dram: Vec<u8>,
}

impl Dram {
    pub fn new(code: Vec<u8>) -> Dram {
        let mut dram = vec![0; DRAM_SIZE as usize];
        dram.splice(..code.len(), code);

        Self { dram }
    }

    #[inline]
    pub fn load(&self, addr: u64, size: u64) -> Result<u64, ()> {
        match size {
            8 => Ok(self.load8(addr)),
            16 => Ok(self.load16(addr)),
            32 => Ok(self.load32(addr)),
            64 => Ok(self.load64(addr)),
            _ => Err(()),
        }
    }

    #[inline]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), ()> {
        match size {
            8 => {
                self.store8(addr, value);
                Ok(())
            }
            16 => {
                self.store16(addr, value);
                Ok(())
            }
            32 => {
                self.store32(addr, value);
                Ok(())
            }
            64 => {
                self.store64(addr, value);
                Ok(())
            }
            _ => Err(()),
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
            | ((self.dram[index + 5] as u64) << 38)
            | ((self.dram[index + 6] as u64) << 46)
            | ((self.dram[index + 7] as u64) << 54)
    }

    #[inline]
    fn store64(&mut self, addr: u64, value: u64) {
        let index = (addr - DRAM_BASE) as usize;
        self.dram[index] = (value & 0xff) as u8;
        self.dram[index + 1] = ((value >> 8) & 0xff) as u8;
        self.dram[index + 2] = ((value >> 16) & 0xff) as u8;
        self.dram[index + 3] = ((value >> 24) & 0xff) as u8;
        self.dram[index + 4] = ((value >> 32) & 0xff) as u8;
        self.dram[index + 5] = ((value >> 38) & 0xff) as u8;
        self.dram[index + 6] = ((value >> 46) & 0xff) as u8;
        self.dram[index + 7] = ((value >> 54) & 0xff) as u8;
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
