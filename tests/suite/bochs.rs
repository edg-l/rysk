//! The bochs display: what enumeration finds, what the registers answer, and what the
//! host sees once a guest has written a mode and some pixels.

use crate::common::*;
use rysk::{
    bochs::{self, Bochs, PAGE, Screen},
    device::{Line, Msi},
    pci::{self, Root},
};

/// Which device number the card is plugged into here, and the registers of a type 0
/// header the tests below reach it through.
const CARD: usize = 1;

fn config(device: usize) -> u64 {
    pci::ECAM + ((device as u64) << 15)
}

const VENDOR: i32 = 0x00;
const COMMAND: i32 = 0x04;
const CLASS: i32 = 0x08;
const BAR0: i32 = 0x10;
const BAR1: i32 = 0x14;
const BAR2: i32 = 0x18;
const SUBSYSTEM: i32 = 0x2c;
/// `command`'s memory space enable, without which neither window answers.
const MEMORY: u64 = 1 << 1;

/// Where the two windows are put. The framebuffer goes at the bottom of the 32-bit
/// window and the registers immediately after it, which is what software placing them
/// in the order it finds them would do.
const FB: u64 = pci::MMIO;
const REGS: u64 = pci::MMIO + bochs::VGAMEM;

/// Offsets inside the register window.
const DISPI: i32 = 0x500;
const QEXT_SIZE: i32 = 0x600;
const QEXT_BYTEORDER: i32 = 0x604;

const INDEX_ID: i32 = 0x0;
const INDEX_XRES: i32 = 0x1;
const INDEX_YRES: i32 = 0x2;
const INDEX_BPP: i32 = 0x3;
const INDEX_ENABLE: i32 = 0x4;
const INDEX_BANK: i32 = 0x5;
const INDEX_VIRT_WIDTH: i32 = 0x6;
const INDEX_VIRT_HEIGHT: i32 = 0x7;
const INDEX_X_OFFSET: i32 = 0x8;
const INDEX_Y_OFFSET: i32 = 0x9;
const INDEX_VIDEO_MEMORY_64K: i32 = 0xa;

/// Where a sixteen-bit register is in the window.
const fn reg(index: i32) -> i32 {
    DISPI + index * 2
}

/// A constant into a register, which is the two instructions an assembler's `li`
/// becomes for anything wider than an immediate.
fn li(rd: u32, value: u32) -> [u32; 2] {
    let high = value.wrapping_add(0x800) >> 12;
    let low = ((value & 0xfff) as i32) << 20 >> 20;
    [lui(rd, high), addiw(rd, rd, low)]
}

/// Write `value` into one of the sixteen-bit registers, the way the driver does.
fn set(index: i32, value: u32) -> Vec<u32> {
    let mut code = li(T5, value).to_vec();
    code.push(sh(T5, T2, reg(index)));
    code
}

/// The sequence `bochs_hw_setmode` writes: the card off, eight registers, the card on.
/// `drm/tiny/bochs.c`, which is the only thing that ever writes these in this order.
fn setmode(width: u32, height: u32) -> Vec<u32> {
    let virtual_height = (bochs::VGAMEM / (width as u64 * 4)) as u32;
    [
        set(INDEX_ENABLE, 0),
        set(INDEX_BPP, 32),
        set(INDEX_XRES, width),
        set(INDEX_YRES, height),
        set(INDEX_BANK, 0),
        set(INDEX_VIRT_WIDTH, width),
        set(INDEX_VIRT_HEIGHT, virtual_height),
        set(INDEX_X_OFFSET, 0),
        set(INDEX_Y_OFFSET, 0),
        // Enabled, and reached through a linear framebuffer, which is the only way
        // there is to reach one here.
        set(INDEX_ENABLE, 0x41),
    ]
    .concat()
}

/// A machine with a bochs display at device one, its two windows already placed and
/// turned on, and the host's side of the card handed back.
///
/// `t0` is the card's config space, `t1` the framebuffer, `t2` the registers, `t3` the
/// bit that turns the windows on and `t4` all ones.
fn card(code: &[u32]) -> (Program, Screen) {
    let root = Root::new(std::array::from_fn(|_| Line::default()), Msi::default());
    let display = Bochs::new(bochs::VGAMEM);
    let screen = display.screen();
    root.plug(CARD, Box::new(display));

    // Where software decided the windows go, and the write that makes them answer.
    let placed = [sw(T1, T0, BAR0), sw(T2, T0, BAR2), sw(T3, T0, COMMAND)];
    let program = prog(&[&placed[..], code].concat())
        .device(pci::ECAM, pci::ECAM_SIZE, Box::new(root.config()))
        .device(pci::MMIO, pci::MMIO_SIZE, Box::new(root.window(pci::MMIO)))
        .reg(T0, config(CARD))
        .reg(T1, FB)
        .reg(T2, REGS)
        .reg(T3, MEMORY)
        .reg(T4, u64::MAX);
    (program, screen)
}

#[test]
fn enumeration_finds_a_display_controller_at_revision_two() {
    let (program, _) = card(&[
        lwu(A0, T0, VENDOR),
        lwu(A1, T0, CLASS),
        lwu(A2, T0, SUBSYSTEM),
    ]);
    let machine = program.run();

    assert_eq!(machine.reg(A0), 0x1111_1234, "vendor 1234, device 1111");
    assert_eq!(
        machine.reg(A1),
        0x0380_0002,
        "display controller, other, revision two"
    );
    assert_eq!(machine.reg(A2), 0x1100_1af4, "the subsystem qemu reports");
}

#[test]
fn the_windows_are_a_prefetchable_framebuffer_and_registers_beside_it() {
    // Writing all ones and reading back reports each window's size in the bits that
    // stayed zero, and its kind in the low four.
    let (program, _) = card(&[
        sw(T4, T0, BAR0),
        sw(T4, T0, BAR1),
        sw(T4, T0, BAR2),
        lwu(A0, T0, BAR0),
        lwu(A1, T0, BAR1),
        lwu(A2, T0, BAR2),
    ]);
    let machine = program.run();

    assert_eq!(
        machine.reg(A0),
        (!(bochs::VGAMEM - 1) & 0xffff_ffff) | 0b1000,
        "sixteen mebibytes, memory, below four gibibytes, prefetchable"
    );
    assert_eq!(machine.reg(A1), 0, "there is no second window");
    assert_eq!(
        machine.reg(A2),
        0xffff_f000,
        "four kibibytes of registers, not prefetchable"
    );
}

#[test]
fn the_identity_register_says_this_is_a_bochs_display() {
    let (program, _) = card(&[lhu(A0, T2, reg(INDEX_ID))]);
    assert_eq!(program.run().reg(A0), 0xb0c5);
}

#[test]
fn the_identity_register_is_the_cards_to_say_and_not_softwares() {
    let (program, _) = card(&[&set(INDEX_ID, 0)[..], &[lhu(A0, T2, reg(INDEX_ID))]].concat());
    assert_eq!(program.run().reg(A0), 0xb0c5);
}

#[test]
fn the_card_reports_how_much_video_memory_it_has() {
    let (program, _) = card(&[lhu(A0, T2, reg(INDEX_VIDEO_MEMORY_64K))]);
    assert_eq!(
        program.run().reg(A0),
        bochs::VGAMEM / (64 * 1024),
        "in units of sixty-four kibibytes, which is what the register counts"
    );
}

#[test]
fn a_register_that_is_storage_reads_back_what_was_written() {
    let (program, _) = card(
        &[
            &set(INDEX_XRES, 1024)[..],
            &set(INDEX_YRES, 768)[..],
            &[lhu(A0, T2, reg(INDEX_XRES)), lhu(A1, T2, reg(INDEX_YRES))],
        ]
        .concat(),
    );
    let machine = program.run();

    assert_eq!(machine.reg(A0), 1024);
    assert_eq!(machine.reg(A1), 768);
}

#[test]
fn a_byte_written_to_half_a_register_leaves_the_other_half() {
    let (program, _) = card(
        &[
            &set(INDEX_XRES, 0x1234)[..],
            // The low byte of a register defined as sixteen bits.
            &li(T5, 0xff)[..],
            &[sb(T5, T2, reg(INDEX_XRES)), lhu(A0, T2, reg(INDEX_XRES))],
        ]
        .concat(),
    );
    assert_eq!(program.run().reg(A0), 0x12ff);
}

#[test]
fn the_window_reads_as_all_ones_where_the_card_answers_for_nothing() {
    // The first of these is where an EDID blob would be, and reading it as all ones is
    // exactly how the driver tells that this card has none. The second is the window a
    // VGA-compatible card would answer its ports in, which this one is not.
    let (program, _) = card(&[lwu(A0, T2, 0), lbu(A1, T2, 0x400), lhu(A2, T2, 0x516)]);
    let machine = program.run();

    assert_eq!(machine.reg(A0), 0xffff_ffff);
    assert_eq!(machine.reg(A1), 0xff);
    assert_eq!(machine.reg(A2), 0xffff);
}

#[test]
fn the_extended_registers_say_how_many_of_them_there_are() {
    let (program, _) = card(&[lwu(A0, T2, QEXT_SIZE)]);
    assert_eq!(program.run().reg(A0), 8);
}

#[test]
fn the_byte_order_is_little_until_a_driver_says_otherwise() {
    let (program, _) = card(
        &[
            &[lwu(A0, T2, QEXT_BYTEORDER)][..],
            &li(T5, 0xbebe_bebe)[..],
            &[sw(T5, T2, QEXT_BYTEORDER), lwu(A1, T2, QEXT_BYTEORDER)],
            // Anything that is not one of the two magic words says nothing at all.
            &li(T5, 0x1234_5678)[..],
            &[sw(T5, T2, QEXT_BYTEORDER), lwu(A2, T2, QEXT_BYTEORDER)],
            &li(T5, 0x1e1e_1e1e)[..],
            &[sw(T5, T2, QEXT_BYTEORDER), lwu(A3, T2, QEXT_BYTEORDER)],
        ]
        .concat(),
    );
    let machine = program.run();

    assert_eq!(machine.reg(A0), 0x1e1e_1e1e);
    assert_eq!(machine.reg(A1), 0xbebe_bebe);
    assert_eq!(machine.reg(A2), 0xbebe_bebe, "junk changes nothing");
    assert_eq!(machine.reg(A3), 0x1e1e_1e1e);
}

#[test]
fn the_byte_order_a_driver_asked_for_reaches_the_host() {
    let (program, screen) = card(
        &[
            &li(T5, 0xbebe_bebe)[..],
            &[sw(T5, T2, QEXT_BYTEORDER)],
            &setmode(1024, 768)[..],
        ]
        .concat(),
    );
    program.run();

    assert!(screen.mode().expect("a mode").big_endian);
}

#[test]
fn the_framebuffer_keeps_what_is_written_to_it() {
    let (program, screen) = card(&[
        sd(T4, T1, 0),
        // The last doubleword of video memory, reached from the other end of it.
        add(T6, T1, T5),
        sd(T3, T6, -8),
        ld(A0, T1, 0),
        ld(A1, T6, -8),
    ]);
    let machine = program.reg(T5, bochs::VGAMEM).run();

    assert_eq!(machine.reg(A0), u64::MAX);
    assert_eq!(machine.reg(A1), MEMORY);

    let pixels = unsafe { screen.vram().as_slice() };
    assert_eq!(&pixels[..8], &[0xff; 8], "and the host sees the same bytes");
}

#[test]
fn a_mode_is_only_a_mode_once_every_register_agrees() {
    let (program, screen) = card(&setmode(1024, 768));
    assert_eq!(screen.mode(), None, "nothing has been written yet");

    program.run();
    let mode = screen.mode().expect("a mode, now that all of them have");

    assert_eq!((mode.width, mode.height), (1024, 768));
    assert_eq!(mode.depth, 4, "thirty-two bits a pixel");
    assert_eq!(mode.stride, 1024 * 4);
    assert_eq!(mode.offset, 0);
    assert_eq!(mode.size, 1024 * 768 * 4);
    assert!(!mode.big_endian);
}

#[test]
fn a_card_that_was_never_enabled_is_showing_nothing() {
    // Every register of a mode except the one that says to show it.
    let mut code = setmode(1024, 768);
    code.truncate(code.len() - 3);
    let (program, screen) = card(&code);
    program.run();

    assert_eq!(screen.mode(), None);
}

#[test]
fn a_depth_the_card_cannot_show_is_no_mode() {
    let (program, screen) = card(&[&setmode(1024, 768)[..], &set(INDEX_BPP, 24)[..]].concat());
    program.run();

    assert_eq!(screen.mode(), None, "twenty-four bits a pixel is not one");
}

#[test]
fn a_picture_smaller_than_the_smallest_one_is_no_mode() {
    let (program, screen) = card(&[&setmode(1024, 768)[..], &set(INDEX_YRES, 32)[..]].concat());
    program.run();

    assert_eq!(screen.mode(), None);
}

#[test]
fn a_picture_that_does_not_fit_in_video_memory_is_no_mode() {
    // Sixteen mebibytes hold 4096 rows of 4096 bytes and not one more.
    let (program, screen) = card(&[&setmode(1024, 768)[..], &set(INDEX_YRES, 4097)[..]].concat());
    program.run();

    assert_eq!(screen.mode(), None);
}

#[test]
fn panning_past_the_end_of_video_memory_is_no_mode() {
    // The picture fits, and where this asks for it to start does not.
    let (program, screen) =
        card(&[&setmode(1024, 768)[..], &set(INDEX_Y_OFFSET, 4000)[..]].concat());
    program.run();

    assert_eq!(screen.mode(), None);
}

#[test]
fn a_picture_wider_than_the_screen_is_what_panning_moves_across() {
    let (program, screen) = card(
        &[
            &setmode(640, 480)[..],
            // A virtual width wider than the visible one, and a start part way into it.
            &set(INDEX_VIRT_WIDTH, 1024)[..],
            &set(INDEX_X_OFFSET, 16)[..],
            &set(INDEX_Y_OFFSET, 8)[..],
        ]
        .concat(),
    );
    program.run();
    let mode = screen.mode().expect("a mode");

    assert_eq!(mode.stride, 1024 * 4, "a row is the virtual width");
    assert_eq!((mode.width, mode.height), (640, 480), "the visible part");
    assert_eq!(mode.offset, 16 * 4 + 8 * 1024 * 4);
}

#[test]
fn a_virtual_width_narrower_than_the_screen_does_not_shorten_a_row() {
    let (program, screen) =
        card(&[&setmode(640, 480)[..], &set(INDEX_VIRT_WIDTH, 16)[..]].concat());
    program.run();

    assert_eq!(screen.mode().expect("a mode").stride, 640 * 4);
}

#[test]
fn a_write_to_the_framebuffer_marks_the_page_it_landed_in_and_no_other() {
    let (program, screen) = card(&[sd(T4, T1, 0), add(T6, T1, T5), sd(T4, T6, 0)]);
    program.reg(T5, 9 * PAGE).run();

    let dirty = screen.vram().take_dirty();
    assert_eq!(dirty.pages().collect::<Vec<_>>(), vec![0, 9]);
    assert!(dirty.touched(0, 8));
    assert!(dirty.touched(9 * PAGE, 8));
    assert!(!dirty.touched(PAGE, PAGE), "nothing was written here");
}

#[test]
fn taking_the_dirty_pages_clears_them() {
    let (program, screen) = card(&[sd(T4, T1, 0)]);
    program.run();

    assert!(screen.vram().take_dirty().any());
    assert!(
        !screen.vram().take_dirty().any(),
        "the second frame is told about the second frame's writes"
    );
}

#[test]
fn a_write_that_straddles_two_pages_marks_both() {
    // Four bytes either side of the boundary, which a guest may do because nothing
    // about a framebuffer says an access to it has to be aligned.
    let (program, screen) = card(&[add(T6, T1, T5), sd(T4, T6, 0)]);
    program.reg(T5, PAGE - 4).run();

    assert_eq!(
        screen.vram().take_dirty().pages().collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn reading_the_framebuffer_does_not_make_it_dirty() {
    let (program, screen) = card(&[ld(A0, T1, 0)]);
    program.run();

    assert!(!screen.vram().take_dirty().any());
}
