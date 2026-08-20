//! What the window is made of: a palette, a type scale, and the few helpers that keep
//! every panel spelling them the same way.
//!
//! The machine this emulates is a development board, which is what the window is drawn
//! as. That is not decoration — it is where the vocabulary comes from and every part of
//! it carries something. A board is dark green soldermask with a white legend printed on
//! it and gold on the pads that are meant to be touched, so: the ground has a green cast
//! rather than being one more neutral near-black, a label is printed legend and recedes
//! the way legend does, and gold marks the one thing a person is looking for. There is a
//! probe colour for where the hart is and a fault colour for where it went wrong, and
//! nothing else is coloured at all.
//!
//! The discipline that matters: **a label is not the answer.** A panel of hexadecimal is
//! read by scanning values, so the values are bright and in one width, and the names
//! beside them are small, upper case and dim. Colour is spent on what moved.

use eframe::egui::{
    Color32, Context, CornerRadius, FontFamily, FontId, RichText, Stroke, Style, TextStyle, Theme,
    ThemePreference, Vec2, Visuals,
};

// ------------------------------------------------------------------ the palette

/// Soldermask, the ground everything sits on. Dark with a green cast rather than a
/// neutral black, which is what a board is and what keeps this from being one more
/// dark editor.
pub const MASK: Color32 = Color32::from_rgb(0x0d, 0x15, 0x12);

/// The fascia: the strip the controls are on, raised out of the mask.
pub const RAISED: Color32 = Color32::from_rgb(0x13, 0x1d, 0x19);

/// And where data sits, sunk below it, so a table of values reads as inset rather than
/// as more surface.
pub const SUNK: Color32 = Color32::from_rgb(0x08, 0x0e, 0x0c);

/// Silkscreen: the legend printed on a board. Warm white, never pure, because printed
/// ink is not.
pub const SILK: Color32 = Color32::from_rgb(0xdc, 0xdd, 0xd6);

/// The same ink at the weight a label deserves, which is less than its value.
pub const SILK_DIM: Color32 = Color32::from_rgb(0x6b, 0x7a, 0x74);

/// Immersion gold, the finish on a pad worth touching. Spent on one thing: what moved
/// since the machine last stopped.
pub const GOLD: Color32 = Color32::from_rgb(0xd2, 0xa8, 0x3e);

/// A probe clipped to a test point. Where the hart is about to execute.
pub const PROBE: Color32 = Color32::from_rgb(0x5f, 0xb8, 0xc9);

/// A fault lamp: halted, a breakpoint set, a byte nothing answered for.
pub const FAULT: Color32 = Color32::from_rgb(0xc9, 0x54, 0x40);

/// A trace between two pads, which is what a rule between two things is.
pub const TRACE: Color32 = Color32::from_rgb(0x24, 0x33, 0x2d);

/// A machine that is running, which is the one state that is neither a fault nor a
/// thing to look at. Muted on purpose: running is the ordinary case and should not be
/// the brightest thing on the fascia.
pub const LIVE: Color32 = Color32::from_rgb(0x7c, 0xa8, 0x86);

// -------------------------------------------------------------------- the scale

/// Data. One width, because it is all hexadecimal and a column that does not line up
/// cannot be scanned.
pub const DATA: f32 = 12.0;

/// Legend. Small, because a label is read once and its value is read every time.
pub const LEGEND: f32 = 10.5;

/// What a control says.
pub const CONTROL: f32 = 12.0;

/// Dress the context: the palette, the scale, and spacing tight enough that a panel
/// holds a useful number of rows.
pub fn install(ctx: &Context) {
    let mut style = Style {
        text_styles: [
            (TextStyle::Small, FontId::proportional(LEGEND)),
            (TextStyle::Body, FontId::proportional(CONTROL)),
            (TextStyle::Button, FontId::proportional(CONTROL)),
            (TextStyle::Heading, FontId::proportional(13.0)),
            (TextStyle::Monospace, FontId::monospace(DATA)),
        ]
        .into(),
        ..Default::default()
    };

    // Tighter than the default, which is laid out for forms rather than for tables.
    style.spacing.item_spacing = Vec2::new(6.0, 3.0);
    style.spacing.button_padding = Vec2::new(8.0, 3.0);
    style.spacing.indent = 14.0;
    style.spacing.interact_size.y = 20.0;

    let mut visuals = Visuals::dark();
    visuals.panel_fill = MASK;
    visuals.window_fill = RAISED;
    visuals.extreme_bg_color = SUNK;
    visuals.faint_bg_color = Color32::from_rgb(0x11, 0x19, 0x16);
    visuals.window_stroke = Stroke::new(1.0, TRACE);
    visuals.weak_text_color = Some(SILK_DIM);
    visuals.warn_fg_color = GOLD;
    visuals.error_fg_color = FAULT;
    visuals.hyperlink_color = PROBE;

    // A board has no rounded corners on anything that matters, and a debugger is a
    // dense grid: square is both truer and tighter.
    let square = CornerRadius::same(2);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = square;
        widget.expansion = 0.0;
    }

    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, SILK);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, TRACE);
    visuals.widgets.noninteractive.bg_fill = MASK;
    visuals.widgets.noninteractive.weak_bg_fill = MASK;

    // A control is a pad: dark until it is worth touching, gold-edged under the
    // pointer, and lit when it is doing something.
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, SILK);
    visuals.widgets.inactive.bg_fill = RAISED;
    visuals.widgets.inactive.weak_bg_fill = RAISED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, TRACE);

    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, SILK);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x1a, 0x26, 0x21);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x1a, 0x26, 0x21);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, GOLD);

    visuals.widgets.active.fg_stroke = Stroke::new(1.0, MASK);
    visuals.widgets.active.bg_fill = GOLD;
    visuals.widgets.active.weak_bg_fill = GOLD;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, GOLD);

    visuals.widgets.open.fg_stroke = Stroke::new(1.0, SILK);
    visuals.widgets.open.bg_fill = RAISED;
    visuals.widgets.open.weak_bg_fill = RAISED;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, TRACE);

    visuals.selection.bg_fill = PROBE.gamma_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0, PROBE);

    style.visuals = visuals;
    // Both, and then pinned to the dark one. A board has one finish: what the host's
    // own light-or-dark setting says about it is not a question this window has an
    // answer to, and half-answering it would leave the panels lit and the picture not.
    ctx.set_style_of(Theme::Dark, style.clone());
    ctx.set_style_of(Theme::Light, style);
    ctx.set_theme(ThemePreference::Dark);
}

// ------------------------------------------------------------------- the voices

/// A printed label. Upper case and small, the way a legend is silkscreened next to the
/// thing it names, and dim because it is not the answer.
pub fn legend(text: impl AsRef<str>) -> RichText {
    RichText::new(text.as_ref().to_uppercase())
        .size(LEGEND)
        .color(SILK_DIM)
}

/// A value: one width, bright, and gold if it moved since the machine last stopped.
pub fn value(text: impl Into<String>, moved: bool) -> RichText {
    let text = RichText::new(text.into()).monospace();
    match moved {
        true => text.color(GOLD),
        false => text.color(SILK),
    }
}

/// Data that is not a value being watched: an address, a range, a name.
pub fn data(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).monospace().color(SILK)
}

/// The same, said quietly.
pub fn faint(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).monospace().color(SILK_DIM)
}

/// A heading over a section of the legend.
pub fn heading(text: impl AsRef<str>) -> RichText {
    RichText::new(text.as_ref().to_uppercase())
        .size(LEGEND)
        .color(SILK)
        .family(FontFamily::Proportional)
}
