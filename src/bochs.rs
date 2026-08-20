//! The bochs display: a linear framebuffer, and the eleven registers that say what
//! shape the picture in it is.
//!
//! This is the smallest display a stock Linux binds to. `drm/tiny/bochs.c` matches it
//! on vendor and device alone and drives it with nothing but sixteen-bit writes to the
//! registers below, which is why it works on a machine with no port instructions where
//! a VGA-compatible card does not: there is no legacy port window to reach, and the
//! card does not claim to be VGA-compatible in the first place. Its class is
//! "display controller, other".
//!
//! Two halves face two ways. `Bochs` is a function on the PCI bus: the guest finds it
//! by enumeration, is given somewhere to put its two windows, and writes a mode into
//! one and pixels into the other. `Screen` is the same card seen from the host: what
//! the mode currently is, the bytes of the framebuffer, and which pages of them have
//! changed since the last time anyone looked. Neither knows anything about the other's
//! side, and the card underneath them is one thing behind an `Arc`.
//!
//! The registers are the Bochs VBE interface as QEMU's `bochs-display` implements it,
//! which is the definition the driver was written against: there is no standards
//! document for them. Where a comment below cites an offset it is citing that model.
//!
//! Below the registers sits the block a monitor would answer with, which `edid` builds
//! and this card only places: a card with nothing plugged into it still has to say what
//! it can show, and a driver reading no block falls back to a list it invented.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering::Relaxed},
};

use crate::{
    device::{Field, Value, field},
    edid,
    pci::{Bar, Function, Header},
    shared::Bytes,
    trap::Exception,
};

/// How much video memory the card has. QEMU's default, and the largest a driver
/// asks about: the size is reported in units of 64 KiB through a sixteen-bit
/// register, so anything is expressible, and the framebuffer window is this big.
pub const VGAMEM: u64 = 16 * 1024 * 1024;

/// The identity `drm/tiny/bochs.c` matches on. It checks only these two, and the
/// subsystem pair below only decides which name it files the card under.
const VENDOR: u16 = 0x1234;
const DEVICE: u16 = 0x1111;
const SUBSYSTEM_VENDOR: u16 = 0x1af4;
const SUBSYSTEM: u16 = 0x1100;

/// Display controller, other, no programming interface, revision two. The revision
/// matters: the driver looks for the extended registers at `QEXT` only from two on.
const CLASS: u32 = 0x0380_0002;

/// The window the registers answer in, and how big it is.
const MMIO: u64 = 0x1000;

/// What the monitor on the other end of this card says it is. There is no other end,
/// so the card is what has to say: a driver reads the block at the bottom of the
/// register window and takes its modes from there rather than guessing at a list.
pub const MONITOR: edid::Monitor = edid::Monitor {
    vendor: *b"RSK",
    name: "rysk display",
    width: 1280,
    height: 800,
};

/// Where that block is, which is the bottom of the register window. A driver reads it
/// one 128-byte block at a time and stops at the first offset past this that the
/// window would not answer for.
const EDID_END: u64 = edid::SIZE as u64;

/// Where the sixteen-bit registers are in that window, and how many there are. Ten of
/// them are storage; the eleventh reports how much video memory there is and is not.
const DISPI: u64 = 0x500;
const REGISTERS: usize = 10;
const DISPI_END: u64 = DISPI + (REGISTERS as u64 + 1) * 2;

const INDEX_ID: usize = 0x0;
const INDEX_XRES: usize = 0x1;
const INDEX_YRES: usize = 0x2;
const INDEX_BPP: usize = 0x3;
const INDEX_ENABLE: usize = 0x4;
const INDEX_VIRT_WIDTH: usize = 0x6;
const INDEX_X_OFFSET: usize = 0x8;
const INDEX_Y_OFFSET: usize = 0x9;
const INDEX_VIDEO_MEMORY_64K: usize = 0xa;

/// What the identity register answers. The driver accepts anything whose top twelve
/// bits are `VBE_DISPI_ID0`, and reports the whole of it.
const ID5: u16 = 0xB0C5;

/// The bit of `ENABLE` that says the mode written into the other registers is the one
/// the card should be showing. The rest of that register describes a way of reaching
/// the framebuffer that does not exist here, where it is always a window on the bus.
const ENABLED: u16 = 0x01;

/// The extended registers, which are QEMU's rather than Bochs': how many bytes of them
/// there are, and which end the bytes of a pixel are in.
const QEXT: u64 = 0x600;
const QEXT_SIZE: u32 = 8;
const QEXT_END: u64 = QEXT + QEXT_SIZE as u64;
const QEXT_REG_SIZE: u64 = QEXT;
const QEXT_REG_BYTEORDER: u64 = QEXT + 4;
const QEXT_REG_SIZE_END: u64 = QEXT_REG_SIZE + 4;
const QEXT_LITTLE_ENDIAN: u32 = 0x1e1e_1e1e;
const QEXT_BIG_ENDIAN: u32 = 0xbebe_bebe;

/// Which window each of the two lives in.
const FRAMEBUFFER: usize = 0;
const REGISTERS_BAR: usize = 2;

/// The unit dirtiness is tracked in, which is the page a host would have mapped the
/// framebuffer in were it presenting it by mapping rather than by copying.
pub const PAGE: u64 = 4096;

/// What a driver has written into the sixteen-bit registers, and which way round it
/// wants the bytes of a pixel.
#[derive(Debug, Default)]
struct Vbe {
    regs: [u16; REGISTERS],
    big_endian: bool,
}

/// The card: its video memory and its registers, shared by the two sides that face it.
#[derive(Debug)]
struct Card {
    vram: Vram,
    vbe: Mutex<Vbe>,
    /// What the monitor answers when asked what it is. Generated once, since nothing
    /// about it changes and nothing writes to it.
    edid: [u8; edid::SIZE],
}

/// The framebuffer: bytes any hart may write and the host may read, and a bit per page
/// saying which of them have been written since anyone last asked.
///
/// The dirty bits are what keep a frame from costing a whole screen. A host presenting
/// this has to copy what changed, and without them the only safe answer to "what
/// changed" is "everything", which at four bytes a pixel is megabytes a frame.
#[derive(Debug)]
pub struct Vram {
    bytes: Bytes,
    dirty: Box<[AtomicU64]>,
}

impl Vram {
    fn new(size: u64) -> Self {
        let pages = size.div_ceil(PAGE);
        Self {
            bytes: Bytes::new(Vec::new(), size),
            dirty: (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// How many bytes of video memory there are.
    pub fn size(&self) -> u64 {
        self.bytes.len()
    }

    /// Every byte of it, for a host that is about to present some of them.
    ///
    /// # Safety
    ///
    /// The bytes are shared with every running hart, so what the reference says is only
    /// meaningful to a caller that knows what else may be writing them. For a frontend
    /// that is the whole bargain: it may see a frame torn halfway through being drawn,
    /// which is what a real card's scanout engine sees too, and the next frame fixes it.
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { self.bytes.as_slice() }
    }

    /// Whether all `size` bits at `offset` are inside the framebuffer.
    fn contains(&self, offset: u64, size: u64) -> bool {
        match offset.checked_add(size / 8) {
            Some(end) => end <= self.size(),
            None => false,
        }
    }

    fn load(&self, offset: u64, size: u64) -> Result<u64, Exception> {
        if !self.contains(offset, size) {
            return Err(Exception::LoadAccessFault(offset));
        }
        Ok(self.bytes.load(offset as usize, size))
    }

    fn store(&self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        if !self.contains(offset, size) {
            return Err(Exception::StoreAmoAccessFault(offset));
        }
        self.bytes.store(offset as usize, size, value);
        self.soil(offset, size / 8);
        Ok(())
    }

    /// Say that the `len` bytes at `offset` have been written. An access is eight bytes
    /// at most and so touches one page or two, but both are counted rather than assumed.
    ///
    /// The load before the store is what makes a blit cheap: a run of stores down one
    /// page finds the bit already set every time after the first, and pays a load
    /// instead of a read-modify-write.
    ///
    /// Relaxed, and deliberately imprecise by one frame. A hart that finds the bit
    /// already set does not order its store against anything, so a host taking the bits
    /// at that moment may present the page without the byte just written to it. Closing
    /// that would put a StoreLoad barrier on every store to video memory, which is the
    /// whole cost of a blit, to buy a pixel arriving one frame earlier: the next store
    /// to that page sets the bit again, and the frame after shows it. A card whose
    /// scanout raced a write shows the same thing.
    fn soil(&self, offset: u64, len: u64) {
        let last = (offset + len.max(1) - 1) / PAGE;
        for page in offset / PAGE..=last {
            let (word, bit) = ((page / 64) as usize, 1u64 << (page % 64));
            if let Some(word) = self.dirty.get(word)
                && word.load(Relaxed) & bit == 0
            {
                word.fetch_or(bit, Relaxed);
            }
        }
    }

    /// Whether anything has been written since the last `take_dirty`, without taking
    /// it.
    ///
    /// This is what a host asks between frames. Presenting one is a picture laid out,
    /// converted and handed to a renderer; asking whether there is a picture to present
    /// is one bit per page. A machine being driven through its serial port draws
    /// nothing at all, and a window open on it should cost what it is showing.
    pub fn drawn(&self) -> bool {
        self.dirty.iter().any(|word| word.load(Relaxed) != 0)
    }

    /// Which pages have been written since this was last asked, clearing them as it
    /// goes so that the next caller is told about the next frame's writes and not this
    /// one's.
    pub fn take_dirty(&self) -> Dirty {
        Dirty(
            self.dirty
                .iter()
                .map(|word| word.swap(0, Relaxed))
                .collect(),
        )
    }
}

/// Which pages of video memory had been written when it was asked, as one bit each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dirty(Vec<u64>);

impl Dirty {
    /// Whether anything at all was written.
    pub fn any(&self) -> bool {
        self.0.iter().any(|word| *word != 0)
    }

    /// Whether any of the `len` bytes at `offset` were, which is the question a host
    /// asks once per scanline to find the band of the picture worth copying.
    pub fn touched(&self, offset: u64, len: u64) -> bool {
        if len == 0 {
            return false;
        }
        (offset / PAGE..=(offset + len - 1) / PAGE).any(|page| {
            let (word, bit) = ((page / 64) as usize, 1u64 << (page % 64));
            self.0.get(word).is_some_and(|word| word & bit != 0)
        })
    }

    /// Every page that was written, by page number.
    pub fn pages(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().enumerate().flat_map(|(word, bits)| {
            (0..64)
                .filter(move |bit| bits & (1 << bit) != 0)
                .map(move |bit| (word * 64 + bit) as u64)
        })
    }
}

/// The shape of the picture the registers currently describe, if they describe one.
///
/// Worked out rather than stored, because the registers are written one at a time and
/// a mode is only a mode once all of them agree: a driver setting a mode turns the card
/// off, writes eight registers, and turns it back on, and every state in between is one
/// of these being `None` or being a shape nothing should be shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// What one pixel of it is made of, which is the whole of what a host needs to
    /// read one out.
    pub format: Format,
    /// How many bytes one row occupies, which is the virtual width rather than the
    /// visible one: a driver pans by making the picture wider than the screen.
    pub stride: u32,
    /// Where in video memory the top left pixel is.
    pub offset: u64,
    /// How many bytes the whole picture occupies from there.
    pub size: u64,
}

/// What the bytes of one pixel are, of the two arrangements this card can be driven in.
///
/// The depth register picks between them and nothing else does, so a format is a depth
/// plus whatever else that depth needs said about it. At thirty-two bits that is which
/// end of the pixel its bytes start at, which the driver sets through the extended
/// registers to match the format of its own framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Four bytes: blue, green, red and one the card ignores, or the reverse of that.
    Xrgb8888 { big_endian: bool },
    /// Two bytes: five bits of red, six of green and five of blue, in one sixteen-bit
    /// word of the host's own order.
    ///
    /// The byte order register says nothing about this one. A driver asking for the
    /// other order at this depth is asking for something the card has no arrangement
    /// for, and QEMU's model answers the same way: native order only.
    R5g6b5,
}

impl Format {
    /// How many bytes one pixel of it takes.
    pub fn bytes(self) -> u32 {
        match self {
            Self::Xrgb8888 { .. } => 4,
            Self::R5g6b5 => 2,
        }
    }
}

impl Vbe {
    /// What one of the sixteen-bit registers reads as. Two of them are not storage:
    /// the identity is fixed, and the size of video memory is the card's to say.
    fn read(&self, index: usize, vgamem: u64) -> u16 {
        match index {
            INDEX_ID => ID5,
            INDEX_VIDEO_MEMORY_64K => (vgamem / (64 * 1024)) as u16,
            _ => self.regs.get(index).copied().unwrap_or(u16::MAX),
        }
    }

    /// Write one of them. Nothing is validated here, because nothing can be: the
    /// registers are written one at a time and any single one of them may disagree
    /// with the rest until the last has landed. Whether they add up to a mode is
    /// `mode` below, asked once they have all been written.
    fn write(&mut self, index: usize, value: u16) {
        if let Some(reg) = self.regs.get_mut(index) {
            *reg = value;
        }
    }

    /// The mode these registers describe, or nothing if they do not describe one.
    fn mode(&self, vgamem: u64) -> Option<Mode> {
        if self.regs[INDEX_ENABLE] & ENABLED == 0 {
            return None;
        }
        let format = match self.regs[INDEX_BPP] {
            16 => Format::R5g6b5,
            32 => Format::Xrgb8888 {
                big_endian: self.big_endian,
            },
            _ => return None,
        };
        let depth = format.bytes();
        let width = self.regs[INDEX_XRES] as u32;
        let height = self.regs[INDEX_YRES] as u32;
        // A virtual width narrower than the visible one would make a row shorter than
        // the picture, so the visible width is the floor.
        let stride = (self.regs[INDEX_VIRT_WIDTH] as u32).max(width) * depth;
        let size = stride as u64 * height as u64;
        let offset = self.regs[INDEX_X_OFFSET] as u64 * depth as u64
            + self.regs[INDEX_Y_OFFSET] as u64 * stride as u64;
        if width < 64 || height < 64 || offset + size > vgamem {
            return None;
        }
        Some(Mode {
            width,
            height,
            format,
            stride,
            offset,
            size,
        })
    }
}

/// The card as a function on the PCI bus.
#[derive(Debug)]
pub struct Bochs(Arc<Card>);

impl Bochs {
    /// A card with `vgamem` bytes of video memory.
    pub fn new(vgamem: u64) -> Self {
        assert!(
            vgamem.is_power_of_two() && vgamem.is_multiple_of(64 * 1024),
            "video memory has to be a whole number of the units its size is reported in"
        );
        Self(Arc::new(Card {
            vram: Vram::new(vgamem),
            vbe: Mutex::new(Vbe::default()),
            edid: edid::generate(&MONITOR),
        }))
    }

    /// The same card, seen from the host.
    pub fn screen(&self) -> Screen {
        Screen(self.0.clone())
    }

    /// A byte of the register window. Everything the model does not answer for reads as
    /// all ones, which is what an access to a window with nothing behind it gives.
    ///
    /// The registers arrive already held, so that every byte of one access sees the
    /// same ones: a read of a sixteen-bit register that took the two halves separately
    /// could be torn in half by a write between them.
    fn register(&self, vbe: &Vbe, offset: u64) -> u8 {
        match offset {
            ..EDID_END => self.0.edid[offset as usize],
            DISPI..DISPI_END => {
                let at = offset - DISPI;
                vbe.read((at / 2) as usize, self.0.vram.size())
                    .to_le_bytes()[(at % 2) as usize]
            }
            QEXT_REG_SIZE..QEXT_REG_SIZE_END => {
                QEXT_SIZE.to_le_bytes()[(offset - QEXT_REG_SIZE) as usize]
            }
            QEXT_REG_BYTEORDER..QEXT_END => {
                let order = match vbe.big_endian {
                    true => QEXT_BIG_ENDIAN,
                    false => QEXT_LITTLE_ENDIAN,
                };
                order.to_le_bytes()[(offset - QEXT_REG_BYTEORDER) as usize]
            }
            _ => u8::MAX,
        }
    }
}

impl Function for Bochs {
    fn describe(&self) -> Vec<Field> {
        let vbe = self.0.vbe.lock().unwrap_or_else(|held| held.into_inner());
        let mut fields = vec![
            field("video memory", Value::Count(self.0.vram.size())),
            field(
                "byte order",
                Value::Text(match vbe.big_endian {
                    true => "big endian".to_owned(),
                    false => "little endian".to_owned(),
                }),
            ),
        ];
        match vbe.mode(self.0.vram.size()) {
            None => fields.push(field("showing", Value::Text("nothing".to_owned()))),
            Some(mode) => fields.extend([
                field("width", Value::Count(u64::from(mode.width))),
                field("height", Value::Count(u64::from(mode.height))),
                field("format", Value::Text(format!("{:?}", mode.format))),
                field("stride", Value::Count(u64::from(mode.stride))),
                field("offset", Value::Bits(mode.offset)),
            ]),
        }
        fields
    }

    fn header(&self) -> Header {
        Header {
            vendor: VENDOR,
            device: DEVICE,
            class: CLASS,
            subsystem_vendor: SUBSYSTEM_VENDOR,
            subsystem: SUBSYSTEM,
            bars: [
                // The framebuffer, which is prefetchable because reading it twice
                // reads the same bytes and nothing about reading it is an action.
                Bar::Memory {
                    size: self.0.vram.size(),
                    prefetchable: true,
                    wide: false,
                },
                Bar::None,
                Bar::Memory {
                    size: MMIO,
                    prefetchable: false,
                    wide: false,
                },
                Bar::None,
                Bar::None,
                Bar::None,
            ],
            // The card never interrupts: a driver that has drawn a frame has drawn it,
            // and there is no scanout to be told about.
            pin: 0,
            msix: None,
            express: true,
        }
    }

    fn load(&mut self, bar: usize, offset: u64, size: u64) -> Result<u64, Exception> {
        match bar {
            FRAMEBUFFER => self.0.vram.load(offset, size),
            // The registers are sixteen bits wide and the extended ones thirty-two, so
            // an access of any width is served a byte at a time out of whichever it
            // lands in. A driver reads them at their own width; anything else is a
            // program looking, and it gets a consistent answer either way.
            REGISTERS_BAR => {
                let vbe = self.0.vbe.lock().unwrap();
                Ok((0..size / 8).fold(0, |value, byte| {
                    value | (self.register(&vbe, offset + byte) as u64) << (byte * 8)
                }))
            }
            _ => Err(Exception::LoadAccessFault(offset)),
        }
    }

    fn store(&mut self, bar: usize, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        match bar {
            FRAMEBUFFER => self.0.vram.store(offset, size, value),
            REGISTERS_BAR => {
                let mut vbe = self.0.vbe.lock().unwrap();
                match offset {
                    // A write narrower than a register keeps the rest of it, which is
                    // what a sixteen-bit register reached by an eight-bit write means.
                    DISPI..DISPI_END => {
                        for byte in 0..size / 8 {
                            let at = offset + byte - DISPI;
                            let (index, half) = ((at / 2) as usize, (at % 2) as usize);
                            let mut bytes = vbe.regs.get(index).copied().unwrap_or(0).to_le_bytes();
                            bytes[half] = (value >> (byte * 8)) as u8;
                            vbe.write(index, u16::from_le_bytes(bytes));
                        }
                    }
                    // The byte order is set by writing one of two magic words, and by
                    // nothing else: a partial write of one is not one.
                    QEXT_REG_BYTEORDER if size == 32 => match value as u32 {
                        QEXT_BIG_ENDIAN => vbe.big_endian = true,
                        QEXT_LITTLE_ENDIAN => vbe.big_endian = false,
                        _ => {}
                    },
                    _ => {}
                }
                Ok(())
            }
            _ => Err(Exception::StoreAmoAccessFault(offset)),
        }
    }
}

/// The card seen from the host: what to draw, and what has changed since last time.
#[derive(Debug, Clone)]
pub struct Screen(Arc<Card>);

impl Screen {
    /// The shape of the picture, if the registers currently describe one. A driver
    /// part way through setting a mode has none, and so does one that has not started.
    pub fn mode(&self) -> Option<Mode> {
        self.0.vbe.lock().unwrap().mode(self.0.vram.size())
    }

    /// The video memory itself, which is where the picture is.
    pub fn vram(&self) -> &Vram {
        &self.0.vram
    }
}
