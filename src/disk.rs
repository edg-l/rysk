//! What a block device is made of, and the two things one can be here.
//!
//! A controller on the bus decides how a guest asks for blocks; where the blocks
//! actually are is a separate question, and this is it. That is the same split the
//! serial port makes between the 16550 and the `Write` it sends bytes to, and it is what
//! lets a test hold a disk in memory while a machine holds one in a file.

use std::{
    fmt, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

/// How many bytes a block is. Five hundred and twelve, because that is what every
/// partition table, filesystem and bootloader assumes when nothing tells it otherwise,
/// and because a disk that reports something else has to be sure the image on it agrees.
pub const BLOCK: u64 = 512;

/// Somewhere to keep blocks.
///
/// An access is whole blocks at a time, which is what the controllers above this ask
/// for and what saves every implementation the same bounds arithmetic. A read or a write
/// that runs off the end answers false rather than being clamped, since a controller
/// that asked for one has been given a wrong address and needs to say so.
pub trait Disk: fmt::Debug + Send {
    /// How many blocks there are.
    fn blocks(&self) -> u64;

    /// Fill `into`, which is a whole number of blocks, starting at `block`.
    fn read(&mut self, block: u64, into: &mut [u8]) -> bool;

    /// And put `from` there.
    fn write(&mut self, block: u64, from: &[u8]) -> bool;

    /// Make sure everything written has reached wherever it is being kept. A disk with
    /// nowhere further to push has nothing to do.
    fn flush(&mut self) {}
}

/// Whether all of `len` bytes from `block` are inside a disk of `blocks` blocks, and
/// where they start.
fn within(blocks: u64, block: u64, len: usize) -> Option<u64> {
    let len = len as u64;
    if !len.is_multiple_of(BLOCK) {
        return None;
    }
    let end = block.checked_add(len / BLOCK)?;
    (end <= blocks).then(|| block * BLOCK)
}

/// A disk kept in memory, which is what a test has and what a machine asked for one with
/// no image gets: it starts zeroed and nothing survives the run.
#[derive(Debug)]
pub struct Memory(Vec<u8>);

impl Memory {
    /// A disk of `blocks` blocks, all zero.
    pub fn new(blocks: u64) -> Self {
        Self(vec![0; (blocks * BLOCK) as usize])
    }

    /// The bytes it holds, for a test that wants to see what was written without asking
    /// the disk for it back.
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Disk for Memory {
    fn blocks(&self) -> u64 {
        self.0.len() as u64 / BLOCK
    }

    fn read(&mut self, block: u64, into: &mut [u8]) -> bool {
        match within(self.blocks(), block, into.len()) {
            Some(at) => {
                into.copy_from_slice(&self.0[at as usize..at as usize + into.len()]);
                true
            }
            None => false,
        }
    }

    fn write(&mut self, block: u64, from: &[u8]) -> bool {
        match within(self.blocks(), block, from.len()) {
            Some(at) => {
                self.0[at as usize..at as usize + from.len()].copy_from_slice(from);
                true
            }
            None => false,
        }
    }
}

/// A disk kept in a file, which is what a machine given an image has.
///
/// The file is the disk: it is opened as it is and never grown, so how many blocks there
/// are is how long it was. A file that is not a whole number of blocks has the tail that
/// does not fill one left out of the disk, since a block is the smallest thing anything
/// above here can ask for.
#[derive(Debug)]
pub struct File {
    file: fs::File,
    blocks: u64,
}

impl File {
    /// Open `path` as a disk. It is opened for writing, because a filesystem mounted
    /// from it will write to it.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
        let blocks = file.metadata()?.len() / BLOCK;
        Ok(Self { file, blocks })
    }

    fn at(&mut self, block: u64, len: usize) -> Option<()> {
        let at = within(self.blocks, block, len)?;
        self.file.seek(SeekFrom::Start(at)).ok()?;
        Some(())
    }
}

impl Disk for File {
    fn blocks(&self) -> u64 {
        self.blocks
    }

    fn read(&mut self, block: u64, into: &mut [u8]) -> bool {
        self.at(block, into.len()).is_some() && self.file.read_exact(into).is_ok()
    }

    fn write(&mut self, block: u64, from: &[u8]) -> bool {
        self.at(block, from.len()).is_some() && self.file.write_all(from).is_ok()
    }

    fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disk_keeps_what_was_written_to_it() {
        let mut disk = Memory::new(4);
        assert!(disk.write(1, &[0xab; BLOCK as usize]));

        let mut block = [0u8; BLOCK as usize];
        assert!(disk.read(1, &mut block));
        assert_eq!(block, [0xab; BLOCK as usize]);

        assert!(disk.read(0, &mut block));
        assert_eq!(block, [0; BLOCK as usize], "and nothing else moved");
    }

    #[test]
    fn an_access_past_the_end_does_not_happen() {
        let mut disk = Memory::new(4);
        let mut block = [0u8; BLOCK as usize];

        assert!(!disk.read(4, &mut block), "one block past the last");
        assert!(!disk.write(u64::MAX, &block), "and an address that wraps");
        assert!(!disk.read(3, &mut [0u8; 2 * BLOCK as usize]));
    }

    /// An access is whole blocks, which is what everything above a disk deals in.
    #[test]
    fn an_access_of_part_of_a_block_does_not_happen() {
        let mut disk = Memory::new(4);
        assert!(!disk.read(0, &mut [0u8; 100]));
        assert!(!disk.write(0, &[0u8; 100]));
    }
}
