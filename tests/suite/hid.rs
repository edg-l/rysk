//! The keyboard and the mouse: what they say they are, and what comes out of them.

use rysk::{
    hid::Hid,
    usb::{Device, Setup, Speed, Stall},
};

/// A setup packet, since a test says what it wants rather than which byte goes where.
fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Setup {
    Setup {
        request_type,
        request,
        value,
        index,
        length,
    }
}

/// A request for a descriptor of `kind`, addressed to the device.
fn descriptor(kind: u8, index: u8, length: u16) -> Setup {
    setup(0x80, 6, ((kind as u16) << 8) | index as u16, 0, length)
}

/// The class requests, addressed to the interface.
const CLASS_IN: u8 = 0xa1;
const CLASS_OUT: u8 = 0x21;

/// The endpoint the reports come out of.
const REPORTS: u8 = 0x81;

#[test]
fn the_device_descriptor_says_a_full_speed_device_with_one_configuration() {
    let (mut keyboard, _) = Hid::keyboard();
    let bytes = keyboard.control(descriptor(1, 0, 18), &[]).expect("one");

    assert_eq!(keyboard.speed(), Speed::Full);
    assert_eq!(bytes.len(), 18);
    assert_eq!(bytes[1], 1, "a device descriptor");
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 0x0200, "usb 2.0");
    assert_eq!(
        bytes[4], 0,
        "a class said by the interface rather than here"
    );
    assert_eq!(bytes[7], 8, "eight bytes at a time on endpoint zero");
    assert_eq!(bytes[17], 1, "one configuration");
}

/// The whole of a configuration is one answer: the configuration, its interface, what
/// that interface says about its reports, and the endpoint they come out of.
#[test]
fn the_configuration_holds_an_interface_a_hid_descriptor_and_one_endpoint() {
    let (mut keyboard, _) = Hid::keyboard();
    let bytes = keyboard.control(descriptor(2, 0, 255), &[]).expect("one");
    let total = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;

    assert_eq!(bytes.len(), total, "as long as it says it is");
    assert_eq!(bytes[..2], [9, 2], "a configuration descriptor");
    assert_eq!(bytes[4], 1, "with one interface");
    assert_eq!(bytes[9..11], [9, 4], "which follows it");
    assert_eq!(bytes[14], 3, "of the human interface device class");
    assert_eq!(bytes[15], 1, "whose reports are the boot protocol's");
    assert_eq!(bytes[16], 1, "and which is a keyboard");
    assert_eq!(bytes[18..20], [9, 0x21], "then what it says about them");
    assert_eq!(bytes[27..29], [7, 5], "and the one endpoint");
    assert_eq!(bytes[29], REPORTS, "which carries them in");
    assert_eq!(bytes[30], 3, "as interrupt transfers");
}

/// A driver reads nine bytes of a configuration to find out how long the whole of it is,
/// and then asks for that much.
#[test]
fn a_request_for_less_than_there_is_answers_with_that_much() {
    let (mut keyboard, _) = Hid::keyboard();
    let head = keyboard.control(descriptor(2, 0, 9), &[]).expect("one");

    assert_eq!(head.len(), 9);
    assert!(u16::from_le_bytes([head[2], head[3]]) > 9, "there is more");
}

#[test]
fn the_report_descriptor_is_asked_for_through_the_interface() {
    let (mut keyboard, _) = Hid::keyboard();
    let through_interface = keyboard.control(setup(0x81, 6, 0x2200, 0, 255), &[]);
    let through_device = keyboard.control(descriptor(0x22, 0, 255), &[]);

    let report = through_interface.expect("a report descriptor");
    assert_eq!(report[..2], [0x05, 0x01], "a generic desktop usage page");
    assert_eq!(
        report.last(),
        Some(&0xc0),
        "ending the collection it opened"
    );
    assert_eq!(through_device, Err(Stall), "and not through the device");
}

/// A device with one speed has no device qualifier to describe the other, and says so
/// rather than answering with something.
#[test]
fn a_descriptor_the_device_does_not_have_is_refused() {
    let (mut mouse, _) = Hid::mouse();
    assert_eq!(mouse.control(descriptor(6, 0, 10), &[]), Err(Stall));
}

#[test]
fn the_strings_name_the_device() {
    let (mut mouse, _) = Hid::mouse();
    let languages = mouse.control(descriptor(3, 0, 255), &[]).expect("one");
    let product = mouse.control(descriptor(3, 2, 255), &[]).expect("one");

    let text: String = char::decode_utf16(
        product[2..]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
    )
    .map(|unit| unit.expect("a character"))
    .collect();

    assert_eq!(languages[2..4], [0x09, 0x04], "american english");
    assert_eq!(text, "mouse", "which a host prints after the manufacturer");
}

// --------------------------------------------------------------- what a request changes

#[test]
fn the_configuration_a_device_was_put_in_reads_back() {
    let (mut keyboard, _) = Hid::keyboard();
    assert_eq!(keyboard.control(setup(0x80, 8, 0, 0, 1), &[]), Ok(vec![0]));

    keyboard.control(setup(0, 9, 1, 0, 0), &[]).expect("one");

    assert_eq!(keyboard.control(setup(0x80, 8, 0, 0, 1), &[]), Ok(vec![1]));
}

#[test]
fn the_idle_a_host_asked_for_reads_back() {
    let (mut keyboard, _) = Hid::keyboard();
    keyboard
        .control(setup(CLASS_OUT, 0x0a, 24 << 8, 0, 0), &[])
        .expect("one");

    assert_eq!(
        keyboard.control(setup(CLASS_IN, 0x02, 0, 0, 1), &[]),
        Ok(vec![24]),
        "in units of four milliseconds, which is what the request counts"
    );
}

/// A device comes up in report protocol whatever its subclass says, and a host that
/// wants the other one asks.
#[test]
fn a_device_starts_in_report_protocol_and_can_be_put_in_the_other() {
    let (mut mouse, _) = Hid::mouse();
    let protocol = |device: &mut Hid| device.control(setup(CLASS_IN, 0x03, 0, 0, 1), &[]);

    assert_eq!(protocol(&mut mouse), Ok(vec![1]), "report protocol");

    mouse
        .control(setup(CLASS_OUT, 0x0b, 0, 0, 0), &[])
        .expect("one");
    assert_eq!(protocol(&mut mouse), Ok(vec![0]), "and then boot protocol");
}

/// Which matters for the mouse and for nothing else: the boot protocol has no room for
/// a wheel, and every real mouse has one.
#[test]
fn the_mouse_loses_its_wheel_in_boot_protocol() {
    let (mut mouse, pointer) = Hid::mouse();
    pointer.moved(1, 2, 3, 4);
    assert_eq!(mouse.read(REPORTS), Some(vec![1, 2, 3, 4]));

    mouse
        .control(setup(CLASS_OUT, 0x0b, 0, 0, 0), &[])
        .expect("one");
    pointer.moved(1, 2, 3, 4);

    assert_eq!(mouse.read(REPORTS), Some(vec![1, 2, 3]));
}

/// And for the keyboard it makes no difference at all, which is what the report
/// descriptor describing exactly the boot layout means.
#[test]
fn the_keyboard_reports_the_same_eight_bytes_in_either_protocol() {
    let (mut keyboard, keys) = Hid::keyboard();
    keys.holding(2, &[0x04]);
    let report = keyboard.read(REPORTS).expect("a report");

    keyboard
        .control(setup(CLASS_OUT, 0x0b, 0, 0, 0), &[])
        .expect("one");
    keys.holding(2, &[0x04]);

    assert_eq!(report.len(), 8);
    assert_eq!(keyboard.read(REPORTS), Some(report));
}

/// The lamps a keyboard has none of: a host writes them over the control endpoint,
/// because the boot subclass gives a device no endpoint going the other way.
#[test]
fn the_lamps_a_host_asked_for_read_back() {
    let (mut keyboard, _) = Hid::keyboard();
    keyboard
        .control(setup(CLASS_OUT, 0x09, 0x0200, 0, 1), &[0b010])
        .expect("one");

    assert_eq!(
        keyboard.control(setup(CLASS_IN, 0x01, 0x0200, 0, 1), &[]),
        Ok(vec![0b010]),
        "which is what reading the report back gives, there being nothing else"
    );
}

/// A mouse's descriptor describes no report going the other way, so it has nowhere to
/// put one and says so.
#[test]
fn a_device_with_no_lamps_refuses_a_report_about_them() {
    let (mut mouse, _) = Hid::mouse();

    assert_eq!(
        mouse.control(setup(CLASS_OUT, 0x09, 0x0200, 0, 1), &[1]),
        Err(Stall)
    );
    assert_eq!(
        mouse.control(setup(CLASS_IN, 0x01, 0x0200, 0, 1), &[]),
        Err(Stall)
    );
}

/// And the report a host asks for is the one it names: the input report is what the
/// endpoint would have sent, and the output report is the lamps.
#[test]
fn a_request_for_a_report_says_which_report_it_means() {
    let (mut keyboard, keys) = Hid::keyboard();
    keys.holding(0, &[0x04]);

    let input = keyboard.control(setup(CLASS_IN, 0x01, 0x0100, 0, 8), &[]);
    let feature = keyboard.control(setup(CLASS_IN, 0x01, 0x0300, 0, 8), &[]);

    assert_eq!(input, Ok(vec![0, 0, 4, 0, 0, 0, 0, 0]));
    assert_eq!(feature, Err(Stall), "and there are no feature reports");
}

// ------------------------------------------------------------------------- the reports

#[test]
fn an_endpoint_with_nothing_to_report_answers_nothing() {
    let (mut keyboard, _) = Hid::keyboard();
    assert_eq!(keyboard.read(REPORTS), None);
}

#[test]
fn a_key_struck_is_the_report_that_has_it_down_and_the_one_that_has_it_up() {
    let (mut keyboard, keys) = Hid::keyboard();
    keys.typed(0, 0x04);

    assert_eq!(keyboard.read(REPORTS), Some(vec![0, 0, 4, 0, 0, 0, 0, 0]));
    assert_eq!(keyboard.read(REPORTS), Some(vec![0; 8]));
    assert_eq!(keyboard.read(REPORTS), None, "and then nothing more");
}

/// Six keys is what the boot layout holds, and a frontend holding down more of them
/// than that gets the ones that fit.
#[test]
fn a_report_holds_six_keys_and_the_modifiers() {
    let (mut keyboard, keys) = Hid::keyboard();
    keys.holding(0b0000_0010, &[4, 5, 6, 7, 8, 9, 10]);

    assert_eq!(
        keyboard.read(REPORTS),
        Some(vec![2, 0, 4, 5, 6, 7, 8, 9]),
        "the left shift, and six of the seven keys"
    );
}

#[test]
fn an_endpoint_that_is_not_there_answers_nothing() {
    let (mut keyboard, keys) = Hid::keyboard();
    keys.typed(0, 0x04);

    assert_eq!(keyboard.read(0x82), None, "there is no second endpoint");
    assert_eq!(keyboard.read(0x01), None, "and none going out");
}

/// A guest that has stopped asking has a driver that is not running, and every keystroke
/// since then would arrive at once when it starts again.
#[test]
fn a_device_nobody_is_reading_drops_the_oldest_reports() {
    let (mut keyboard, keys) = Hid::keyboard();
    for key in 0..64u8 {
        keys.holding(0, &[key]);
    }

    let first = keyboard.read(REPORTS).expect("a report");
    assert_eq!(first[2], 64 - 16, "sixteen of them, ending at the newest");
}
