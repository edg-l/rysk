//! The block of bytes a monitor answers with when asked what it is: which modes it
//! can show, which one it would rather show, and what it is called.
//!
//! A display controller has none of this. What it has is a channel to whatever is
//! plugged into it, and on a card with nothing plugged in the emulator is the one that
//! has to say. So this generates a monitor rather than modelling one, and what it
//! generates is what QEMU's `hw/display/edid-generate.c` does, because the parser on
//! the other end is `drm_edid.c` and that is the blob it has been read against for
//! years. Reproducing it is what makes a mode list from rysk the same mode list.
//!
//! VESA E-EDID 1.4 defines the base block, and CTA-861 the extension after it. Neither
//! is freely redistributable, so where a field below needs explaining the explanation
//! is here rather than a section number.

/// One block, which is what every offset here is inside and what a reader asks for at
/// a time.
pub const BLOCK: usize = 128;

/// The whole thing: the base block, and one CTA extension after it listing the
/// short video descriptors a base block has nowhere to put.
pub const SIZE: usize = 2 * BLOCK;

/// What the monitor says it is.
#[derive(Debug, Clone, Copy)]
pub struct Monitor {
    /// Three letters, which is a PnP identifier: five bits each, packed big-endian.
    pub vendor: [u8; 3],
    /// Its name, of which thirteen bytes are stored and twelve are kept.
    pub name: &'static str,
    /// The mode it would rather be driven in, which is what a driver takes as its
    /// preferred timing and what a console ends up sized by.
    pub width: u32,
    pub height: u32,
}

/// The refresh the timings below are computed for, in hundredths of a hertz.
const REFRESH: u64 = 75_000;

/// How large the picture is said to be, in dots per inch, which is what turns a
/// resolution into the millimetres the block reports.
const DPI: u32 = 100;

/// When the monitor says it was made. A block has to name a year, and a card that has
/// always existed has none to name, so this is the one QEMU's generator picks.
const YEAR: u8 = (2014u32 - 1990) as u8;

/// Where the four descriptor slots of the base block start, and how long one is.
const DESCRIPTORS: usize = 54;
const DESCRIPTOR: usize = 18;
const SLOTS: usize = 4;

/// The byte counting how many extension blocks follow the base one, and the checksum
/// after it, which every block ends with.
const EXTENSIONS: usize = 126;
const CHECKSUM: usize = 127;

/// Where the first standard timing goes, and where they stop: eight of them, two bytes
/// each, and a mode too large or too oddly shaped to be described in two bytes has to
/// go somewhere else.
const STANDARD: usize = 38;
const STANDARD_END: usize = 54;

/// A mode the block can advertise, and the three places one can be advertised from.
///
/// Which place a mode ends up in is not its own choice: the established bitmap holds a
/// fixed set of modes and nothing else, a standard timing is two bytes and so can only
/// describe a mode of one of four aspect ratios and under 2048 wide, and everything
/// left over goes in a descriptor of extra timings or, if the extension can name it,
/// in the extension.
struct Mode {
    width: u32,
    height: u32,
    /// The bit of the established timing bitmap this mode is, if it is one of them.
    established: Option<(usize, u32)>,
    /// The bit of the extra-timings descriptor it is, if it is one of those.
    extra: Option<(usize, u32)>,
    /// The number CTA-861 gives it, if it has one, which is how the extension names a
    /// mode in one byte.
    video: Option<u8>,
}

const fn mode(width: u32, height: u32) -> Mode {
    Mode {
        width,
        height,
        established: None,
        extra: None,
        video: None,
    }
}

const fn established(width: u32, height: u32, byte: usize, bit: u32) -> Mode {
    Mode {
        established: Some((byte, bit)),
        ..mode(width, height)
    }
}

const fn extra(width: u32, height: u32, byte: usize, bit: u32) -> Mode {
    Mode {
        extra: Some((byte, bit)),
        ..mode(width, height)
    }
}

const fn video(mut mode: Mode, number: u8) -> Mode {
    mode.video = Some(number);
    mode
}

/// Every mode advertised, in the order they are offered a place. The order is what
/// decides which of them get the eight standard timings, so it is not free to vary:
/// the widest go first, and the ones that have somewhere else to go come last.
const MODES: [Mode; 22] = [
    video(mode(5120, 2160), 125),
    video(mode(4096, 2160), 101),
    video(mode(3840, 2160), 96),
    video(mode(2560, 1080), 89),
    mode(2048, 1152),
    video(mode(1920, 1080), 31),
    video(mode(3840, 2160), 97),
    extra(1920, 1200, 10, 0),
    extra(1600, 1200, 9, 2),
    extra(1680, 1050, 9, 5),
    extra(1440, 900, 8, 5),
    extra(1280, 1024, 7, 1),
    extra(1280, 960, 7, 3),
    extra(1280, 768, 7, 6),
    extra(1920, 1440, 11, 5),
    extra(1856, 1392, 10, 3),
    extra(1792, 1344, 10, 5),
    extra(1440, 1050, 8, 1),
    extra(1360, 768, 8, 7),
    established(1024, 768, 36, 3),
    established(800, 600, 35, 0),
    established(640, 480, 35, 5),
];

/// The blanking a picture is surrounded by, and how fast the pixels of it arrive.
///
/// Nothing here scans out, so none of this is measured: it is a plausible set of
/// numbers for the resolution, which is what a mode line has to carry to be a mode line
/// at all. What matters is that a parser can compute a refresh rate from it that lands
/// in the range the block also advertises.
struct Timings {
    front: (u32, u32),
    sync: (u32, u32),
    blank: (u32, u32),
    /// In units of ten kilohertz, which is what two bytes of a timing descriptor hold.
    clock: u64,
}

impl Timings {
    fn new(width: u32, height: u32) -> Self {
        let blank = (width * 35 / 100, height * 35 / 1000);
        Self {
            front: (width * 25 / 100, height * 5 / 1000),
            sync: (width * 3 / 100, height * 5 / 1000),
            blank,
            clock: REFRESH * (width + blank.0) as u64 * (height + blank.1) as u64 / 10_000_000,
        }
    }
}

/// How many millimetres `pixels` of picture are at `DPI`.
fn millimetres(pixels: u32) -> u32 {
    pixels * 254 / 10 / DPI
}

/// The sum of a block, negated, which is what makes the whole block sum to zero. A
/// block that already does keeps its zero rather than being given one.
fn checksum(block: &mut [u8]) {
    let sum = block[..CHECKSUM]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    block[CHECKSUM] = sum.wrapping_neg();
}

/// The first five bytes of every descriptor that is not a timing: three zeroes where a
/// timing would have put a pixel clock it could not be, then what kind it is.
fn descriptor(block: &mut [u8], at: usize, kind: u8) {
    block[at..at + 5].fill(0);
    block[at + 3] = kind;
}

/// A descriptor holding a line of text: thirteen bytes, space padded, ended by a
/// newline if there is room for one.
fn text(block: &mut [u8], at: usize, kind: u8, line: &str) {
    descriptor(block, at, kind);
    block[at + 5..at + DESCRIPTOR].fill(b' ');
    let bytes = line.as_bytes();
    let len = bytes.len().min(12);
    block[at + 5..at + 5 + len].copy_from_slice(&bytes[..len]);
    block[at + 5 + len] = b'\n';
}

/// The preferred timing, which is the one descriptor that is a mode rather than a note
/// about one. Twelve bits each for the resolutions and the blanking, ten for the
/// porches, and the high bits of every one of them gathered into two bytes at the end,
/// which is what makes this eighteen bytes rather than twenty-six.
fn timing(block: &mut [u8], at: usize, timings: &Timings, width: u32, height: u32) {
    let (x, y) = (width, height);
    let (front, sync, blank) = (&timings.front, &timings.sync, &timings.blank);
    let (mm_x, mm_y) = (millimetres(x), millimetres(y));
    let desc = &mut block[at..at + DESCRIPTOR];
    desc[0..2].copy_from_slice(&(timings.clock as u16).to_le_bytes());
    desc[2] = x as u8;
    desc[3] = blank.0 as u8;
    desc[4] = (((x & 0xf00) >> 4) | ((blank.0 & 0xf00) >> 8)) as u8;
    desc[5] = y as u8;
    desc[6] = blank.1 as u8;
    desc[7] = (((y & 0xf00) >> 4) | ((blank.1 & 0xf00) >> 8)) as u8;
    desc[8] = front.0 as u8;
    desc[9] = sync.0 as u8;
    desc[10] = (((front.1 & 0xf) << 4) | (sync.1 & 0xf)) as u8;
    desc[11] = (((front.0 & 0x300) >> 2)
        | ((sync.0 & 0x300) >> 4)
        | ((front.1 & 0x30) >> 2)
        | ((sync.1 & 0x30) >> 4)) as u8;
    desc[12] = mm_x as u8;
    desc[13] = mm_y as u8;
    desc[14] = (((mm_x & 0xf00) >> 4) | ((mm_y & 0xf00) >> 8)) as u8;
    // Digital separate sync, both polarities positive, which is what a mode with no
    // real cable behind it may as well say.
    desc[17] = 0x18;
}

/// The eight coordinates that say what this monitor's red, green, blue and white are,
/// each ten bits, with the low two of all eight packed into the two bytes before them.
fn chromaticity(block: &mut [u8]) {
    /// sRGB, as thousandths, in the order the block holds them.
    const POINTS: [(u32, u32); 4] = [(640, 330), (300, 600), (150, 60), (3127, 3290)];
    /// The denominator each pair above is over, so that the fixed point below is
    /// exact rather than a float rounded twice.
    const SCALE: [u32; 4] = [1000, 1000, 1000, 10000];

    let ten_bit = |value: u32, scale: u32| (value * 1024 + scale / 2) / scale;
    let mut low = [0u8; 2];
    for (n, ((x, y), scale)) in POINTS.into_iter().zip(SCALE).enumerate() {
        let (x, y) = (ten_bit(x, scale), ten_bit(y, scale));
        low[n / 2] |= (((x & 3) << 6) >> ((n % 2) * 4)) as u8;
        low[n / 2] |= (((y & 3) << 4) >> ((n % 2) * 4)) as u8;
        block[27 + n * 2] = (x >> 2) as u8;
        block[28 + n * 2] = (y >> 2) as u8;
    }
    block[25..27].copy_from_slice(&low);
}

/// A standard timing, which is a width in units of eight pixels above 256, an aspect
/// ratio, and a refresh above 60. Nothing that cannot be said that way fits.
fn standard(width: u32, height: u32) -> Option<[u8; 2]> {
    let aspect = match (width * 10 == height * 16, width * 3 == height * 4) {
        (true, _) => 0,
        (_, true) => 1,
        _ if width * 4 == height * 5 => 2,
        _ if width * 9 == height * 16 => 3,
        _ => return None,
    };
    let byte = (width / 8).checked_sub(31)?;
    (byte <= 255).then_some([byte as u8, (aspect << 6) as u8])
}

/// The extension block's own header: what kind of extension it is, which revision,
/// where its descriptors start, and a collection of short video descriptors naming the
/// modes the base block had nowhere to put.
///
/// The fourth byte counts the video descriptors in its low bits, and the third says
/// where the block's own descriptors begin, which is after them.
fn extension(block: &mut [u8]) {
    const CTA: u8 = 0x02;
    const REVISION: u8 = 0x03;
    /// A collection of short video descriptors, and how many of them, in one byte:
    /// the kind in the top three bits and the count in the low five.
    const VIDEO_BLOCK: u8 = 0x40;

    let dta = &mut block[BLOCK..];
    dta[0] = CTA;
    dta[1] = REVISION;
    dta[2] = 5;
    dta[3] = 0;
    dta[4] = VIDEO_BLOCK;
    for number in MODES.iter().filter_map(|mode| mode.video) {
        let at = dta[2] as usize;
        dta[at] = number;
        dta[2] += 1;
        dta[4] += 1;
    }
}

/// Every descriptor slot, in the order they are filled: the four of the base block,
/// then whatever the extension has room for after its video descriptors.
fn slots(descriptors: usize) -> impl Iterator<Item = usize> {
    let after = BLOCK + descriptors;
    let extension = (after..)
        .step_by(DESCRIPTOR)
        .take_while(|at| at + DESCRIPTOR <= BLOCK + CHECKSUM);
    (0..SLOTS)
        .map(|slot| DESCRIPTORS + slot * DESCRIPTOR)
        .chain(extension)
}

/// The block `monitor` would answer with.
pub fn generate(monitor: &Monitor) -> [u8; SIZE] {
    /// What every block starts with, and the only thing a reader checks before
    /// believing the rest of it.
    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    /// The descriptor kinds used here: extra standard timings, the range of what the
    /// monitor will accept, its name, and a slot holding nothing.
    const EXTRA_TIMINGS: u8 = 0xf7;
    const RANGES: u8 = 0xfd;
    const NAME: u8 = 0xfc;
    const UNUSED: u8 = 0x10;

    let (width, height) = (monitor.width, monitor.height);
    let timings = Timings::new(width, height);
    assert!(
        width < 4096 && height < 4096 && timings.clock < 65536,
        "a preferred timing descriptor holds twelve bits of resolution and sixteen of clock"
    );

    let mut block = [0u8; SIZE];
    block[..8].copy_from_slice(&HEADER);
    let vendor = monitor.vendor.iter().fold(0u16, |id, letter| {
        (id << 5) | ((letter - b'@') & 0x1f) as u16
    });
    block[8..10].copy_from_slice(&vendor.to_be_bytes());
    // A product code and a serial number, which nothing distinguishes this card by.
    block[10..12].copy_from_slice(&0x1234u16.to_le_bytes());
    // When it was made, as a week and a year counted from 1990.
    block[16] = 42;
    block[17] = YEAR;
    // Version 1.4 of the structure.
    block[18] = 1;
    block[19] = 4;
    // A digital input of eight bits a channel, speaking DisplayPort.
    block[20] = 0xa5;
    // How large the picture is, in centimetres.
    block[21] = (millimetres(width) / 10) as u8;
    block[22] = (millimetres(height) / 10) as u8;
    // Gamma of 2.2, stored as a hundred times that less a hundred.
    block[23] = 220 - 100;
    // The colours are sRGB, and the first descriptor is the preferred timing.
    block[24] = 0x06;
    chromaticity(&mut block);

    block[EXTENSIONS] = 1;
    extension(&mut block);

    let mut slots = slots(block[BLOCK + 2] as usize);
    let mut next = || {
        slots
            .next()
            .expect("a descriptor slot for every descriptor")
    };
    timing(&mut block, next(), &timings, width, height);

    // The extra timings have to be placed before the modes are, since which modes end
    // up in it is decided by how many of them the standard timings could hold.
    let timings_at = next();
    descriptor(&mut block, timings_at, EXTRA_TIMINGS);
    block[timings_at + 5] = 10;
    fill(&mut block, timings_at);

    let ranges = next();
    descriptor(&mut block, ranges, RANGES);
    // Fifty to a hundred and twenty-five hertz vertically, thirty to a hundred and
    // sixty kilohertz horizontally, and up to 2550 MHz of pixels.
    block[ranges + 5] = 50;
    block[ranges + 6] = 125;
    block[ranges + 7] = 30;
    block[ranges + 8] = 160;
    block[ranges + 9] = 255;
    // No timing formula beyond what is written here.
    block[ranges + 10] = 0x01;
    block[ranges + 11] = b'\n';
    block[ranges + 12..ranges + DESCRIPTOR].fill(b' ');

    text(&mut block, next(), NAME, monitor.name);
    for slot in slots {
        descriptor(&mut block, slot, UNUSED);
    }

    let (base, extension) = block.split_at_mut(BLOCK);
    checksum(base);
    checksum(extension);
    block
}

/// Put every mode somewhere: the established bitmap if it has a bit there, a standard
/// timing while any are left, and the extra-timings descriptor at `extra` otherwise.
///
/// A mode with nowhere left to go is still named by the extension, which is where the
/// widest of them are only ever named: the modes come first that have nothing else.
fn fill(block: &mut [u8], extra: usize) {
    let mut at = STANDARD;
    for mode in &MODES {
        if let Some((byte, bit)) = mode.established {
            block[byte] |= 1 << bit;
        } else if at < STANDARD_END {
            if let Some(timing) = standard(mode.width, mode.height) {
                block[at..at + 2].copy_from_slice(&timing);
                at += 2;
            }
        } else if let Some((byte, bit)) = mode.extra {
            block[extra + byte] |= 1 << bit;
        }
    }
    // A slot with no mode in it says so, rather than describing a mode of no width.
    while at < STANDARD_END {
        block[at..at + 2].copy_from_slice(&[0x01, 0x01]);
        at += 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity QEMU's own generator is called with by `hw/display/bochs-display.c`,
    /// which is what `QEMU` below is a block of.
    const QEMU: Monitor = Monitor {
        vendor: *b"RHT",
        name: "QEMU Monitor",
        width: 1280,
        height: 800,
    };

    /// Every byte `qemu_edid_generate` writes for that monitor into the 256 bytes
    /// `bochs-display` gives it, as hexadecimal.
    ///
    /// This is the ground truth the generator above is held against, the way
    /// `tests/encodings.s` is the one the assembler is held against: it was produced by
    /// compiling `hw/display/edid-generate.c` and running it, so agreeing with it cannot
    /// be arranged by changing this file. What it buys is that a driver reading rysk's
    /// block walks the same parser down the same branches it walks on QEMU, including
    /// the extension block, which is where the modes too wide to describe in two bytes
    /// are named.
    const REFERENCE: &str = concat!(
        "00ffffffffffff004914341200000000",
        "2a180104a520147806ee91a3544c9926",
        "0f5054210800e1c0d1c0d100a940b300",
        "950081808140ea2900c051201c304026",
        "444045cb10000018000000f7000a0040",
        "82002820000000000000000000fd0032",
        "7d1ea0ff010a202020202020000000fc",
        "0051454d55204d6f6e69746f720a013a",
        "02030b00467d6560591f610000001000",
        "00000000000000000000000000000000",
        "10000000000000000000000000000000",
        "00001000000000000000000000000000",
        "00000000100000000000000000000000",
        "00000000000010000000000000000000",
        "00000000000000001000000000000000",
        "0000000000000000000000000000002f",
    );

    /// The bytes `REFERENCE` spells.
    fn reference() -> Vec<u8> {
        REFERENCE
            .as_bytes()
            .chunks(2)
            .map(|byte| u8::from_str_radix(std::str::from_utf8(byte).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn the_generator_agrees_with_the_one_it_was_written_from() {
        assert_eq!(generate(&QEMU).as_slice(), reference());
    }

    #[test]
    fn a_block_starts_with_the_header_a_reader_checks_for() {
        let block = generate(&QEMU);
        assert_eq!(
            &block[..8],
            &[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]
        );
        assert_eq!((block[18], block[19]), (1, 4), "version 1.4");
    }

    /// Both blocks have to sum to zero or a parser throws the whole thing away, and the
    /// base block has to say the extension is there or the parser never reads it.
    #[test]
    fn both_blocks_sum_to_zero_and_the_first_says_the_second_is_there() {
        let block = generate(&QEMU);
        let sum = |bytes: &[u8]| bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        assert_eq!(sum(&block[..BLOCK]), 0);
        assert_eq!(sum(&block[BLOCK..]), 0);
        assert_eq!(block[EXTENSIONS], 1);
    }

    /// The first descriptor is the preferred timing, and it is the mode the monitor was
    /// asked for: this is what decides the resolution a console comes up in.
    #[test]
    fn the_first_descriptor_is_the_mode_the_monitor_prefers() {
        let block = generate(&Monitor {
            width: 1024,
            height: 768,
            ..QEMU
        });
        let desc = &block[DESCRIPTORS..DESCRIPTORS + DESCRIPTOR];
        assert_ne!(&desc[..2], &[0, 0], "a timing rather than a note about one");
        let width = desc[2] as u32 | ((desc[4] as u32 & 0xf0) << 4);
        let height = desc[5] as u32 | ((desc[7] as u32 & 0xf0) << 4);
        assert_eq!((width, height), (1024, 768));
        assert_eq!(block[24] & 0x02, 0x02, "and the block says it is preferred");
    }

    /// The name is thirteen bytes whatever it says, so a shorter one is padded and a
    /// longer one is cut rather than running into the descriptor after it.
    #[test]
    fn a_name_is_padded_to_the_thirteen_bytes_a_descriptor_holds() {
        let named = |name| {
            let block = generate(&Monitor { name, ..QEMU });
            let at = DESCRIPTORS + 3 * DESCRIPTOR;
            assert_eq!(block[at + 3], 0xfc, "the descriptor holding a name");
            block[at + 5..at + DESCRIPTOR].to_vec()
        };
        assert_eq!(named("ab"), b"ab\n          ");
        assert_eq!(named("a much longer name"), b"a much longe\n");
    }

    /// A monitor the preferred timing descriptor cannot describe is a machine built
    /// wrong rather than a block that quietly describes something else.
    #[test]
    #[should_panic(expected = "twelve bits of resolution")]
    fn a_mode_too_large_for_a_timing_descriptor_is_refused() {
        generate(&Monitor {
            width: 7680,
            height: 4320,
            ..QEMU
        });
    }
}
