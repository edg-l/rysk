//! Just enough ELF64 to load a little-endian RISC-V image: where its bytes belong,
//! where to start, and what its symbols are called.

use std::collections::HashMap;

/// A loadable image: the segments to place in memory, the entry point, and the symbol
/// table, which is how a test corpus's `tohost` is found.
#[derive(Debug, Clone)]
pub struct Image {
    pub entry: u64,
    pub segments: Vec<Segment>,
    pub symbols: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub addr: u64,
    pub bytes: Vec<u8>,
    /// Bytes past `bytes` that the image wants zeroed, which is how `.bss` arrives.
    pub zeroes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotAnElf,
    Not64BitLittleEndianRiscv,
    Truncated,
    SegmentOutsideDram(u64),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnElf => write!(f, "not an elf file"),
            Self::Not64BitLittleEndianRiscv => write!(f, "not a 64-bit little-endian riscv elf"),
            Self::Truncated => write!(f, "elf file is truncated"),
            Self::SegmentOutsideDram(addr) => {
                write!(f, "segment at {addr:#x} does not fit in dram")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Whether `bytes` starts with the ELF magic, so a caller can tell an image from a
/// flat binary without committing to parsing it.
pub fn is_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, Error> {
    let slice = bytes.get(at..at + 2).ok_or(Error::Truncated)?;
    Ok(u16::from_le_bytes(slice.try_into().unwrap()))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, Error> {
    let slice = bytes.get(at..at + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, Error> {
    let slice = bytes.get(at..at + 8).ok_or(Error::Truncated)?;
    Ok(u64::from_le_bytes(slice.try_into().unwrap()))
}

const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;

pub fn parse(bytes: &[u8]) -> Result<Image, Error> {
    if !is_elf(bytes) {
        return Err(Error::NotAnElf);
    }
    // e_ident: 64-bit, two's complement little-endian, and e_machine says RISC-V.
    let class = *bytes.get(4).ok_or(Error::Truncated)?;
    let data = *bytes.get(5).ok_or(Error::Truncated)?;
    if class != 2 || data != 1 || u16_at(bytes, 18)? != 243 {
        return Err(Error::Not64BitLittleEndianRiscv);
    }

    let entry = u64_at(bytes, 24)?;
    let phoff = u64_at(bytes, 32)? as usize;
    let shoff = u64_at(bytes, 40)? as usize;
    let phentsize = u16_at(bytes, 54)? as usize;
    let phnum = u16_at(bytes, 56)? as usize;
    let shentsize = u16_at(bytes, 58)? as usize;
    let shnum = u16_at(bytes, 60)? as usize;

    let mut segments = Vec::new();
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32_at(bytes, ph)? != PT_LOAD {
            continue;
        }
        let offset = u64_at(bytes, ph + 8)? as usize;
        // Load at the physical address: the corpus links its virtual addresses to the
        // same place, and there is no translation to resolve them with anyway.
        let addr = u64_at(bytes, ph + 16)?;
        let filesz = u64_at(bytes, ph + 32)? as usize;
        let memsz = u64_at(bytes, ph + 40)?;
        let chunk = bytes
            .get(offset..offset + filesz)
            .ok_or(Error::Truncated)?
            .to_vec();
        segments.push(Segment {
            addr,
            bytes: chunk,
            zeroes: memsz.saturating_sub(filesz as u64),
        });
    }

    let mut symbols = HashMap::new();
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if u32_at(bytes, sh + 4)? != SHT_SYMTAB {
            continue;
        }
        let offset = u64_at(bytes, sh + 24)? as usize;
        let size = u64_at(bytes, sh + 32)? as usize;
        let entsize = u64_at(bytes, sh + 56)? as usize;
        // sh_link names the string table holding the symbol names.
        let strtab = shoff + u32_at(bytes, sh + 40)? as usize * shentsize;
        let strings = u64_at(bytes, strtab + 24)? as usize;

        for sym in (0..size).step_by(entsize.max(1)) {
            let at = offset + sym;
            let name = u32_at(bytes, at)? as usize;
            let value = u64_at(bytes, at + 8)?;
            let start = strings + name;
            let end = bytes[start..]
                .iter()
                .position(|&b| b == 0)
                .ok_or(Error::Truncated)?;
            if end > 0 {
                let name = String::from_utf8_lossy(&bytes[start..start + end]).into_owned();
                symbols.insert(name, value);
            }
        }
    }

    Ok(Image {
        entry,
        segments,
        symbols,
    })
}
