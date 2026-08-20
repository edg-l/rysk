//! What the window shows about the machine beside the guest's picture.
//!
//! Every value here comes from `debug::Session` and none of it is read out of the
//! machine directly, which is what milestone 3 was for. What this file adds is only the
//! last step: turning those values into rows a person reads.
//!
//! **Nothing here holds the machine.** The panels draw from a `View`, which is a copy
//! taken while the machine was stopped, and they emit `Action`s rather than acting.
//! That is what lets a frame be drawn while the harts are running, when the session is
//! locked by the thread running them and there is nothing new to read: the panels show
//! the last exact values and say that is what they are, rather than showing values torn
//! out from under a hart.

use eframe::egui::{
    Button, Frame, Grid, Margin, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2,
};

use crate::{
    debug::{Address, Csr, Line, Registers, Session, Stop, Symbols, Window},
    device::{Report, Value},
    inst::REG_NAMES,
    machine::State,
    theme,
    trap::Taken,
};

/// How many instructions the disassembly shows.
const CODE: usize = 48;

/// How many bytes the memory panel shows, and how many go on a row.
const MEMORY: usize = 16 * 24;
const ROW: usize = 16;

/// How far into a symbol `pc` may be for the disassembly to start at the symbol rather
/// than at `pc` itself. Far enough to hold a short function whole, near enough that a
/// long one does not push what is executing off the end of the panel.
const CONTEXT: u64 = 192;

/// What the window asks of the machine. Panels emit these rather than doing anything,
/// since a panel is drawn whether or not the session can be reached this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Run,
    Pause,
    Step,
    StepToTrap,
    ToggleBreakpoint(u64),
    /// Look at another hart, which changes what every panel is about.
    Hart(usize),
    /// Point the memory panel somewhere.
    Goto(u64),
    /// Point the disassembly somewhere, and stop it following `pc`.
    Show(u64),
    /// And put it back on `pc`.
    Follow,
}

/// Everything the panels show, read in one go while the machine was stopped.
///
/// It is kept between frames rather than re-read every frame, because most frames
/// cannot re-read it: while the harts run, the thread running them holds the session
/// and this is the last exact answer there was.
#[derive(Debug, Default)]
pub struct View {
    pub hart: usize,
    pub harts: usize,
    /// Whether what is in here was read at this stop or an earlier one.
    pub current: bool,
    pub stop: Option<Stop>,
    pub registers: Option<Registers>,
    /// Which integer registers moved at the last read, which is what a person is
    /// looking for after a step.
    changed: [bool; 32],
    pub csrs: Vec<Csr>,
    csrs_changed: Vec<bool>,
    pub code: Vec<Line>,
    pub memory: Option<Window>,
    pub devices: Vec<Report>,
    pub traps: Vec<Taken>,
    pub taken: u64,
    pub breakpoints: Vec<u64>,
    /// What is pending on this hart and what it has enabled, which together are why it
    /// is or is not in a handler.
    pub interrupts: (u64, u64),
    /// Where the memory panel is looking.
    pub at: u64,
    /// And the disassembly, when it is not following `pc`.
    pub showing: Option<u64>,
}

impl View {
    /// Read the machine, which is only possible while it is stopped.
    ///
    /// What moved since the last read is worked out here rather than drawn from
    /// storage, so a step highlights exactly the registers that step wrote.
    pub fn read(&mut self, session: &mut Session, hart: usize) {
        let was = self.registers.take();
        let registers = session.registers(hart);
        self.changed = std::array::from_fn(|which| {
            was.as_ref()
                .is_some_and(|was| was.x[which] != registers.x[which])
        });

        let csrs = session.csrs(hart);
        self.csrs_changed = csrs
            .iter()
            .enumerate()
            .map(|(index, csr)| {
                self.csrs
                    .get(index)
                    .is_some_and(|before| before.value != csr.value)
            })
            .collect();
        self.csrs = csrs;

        // Where the disassembly starts. Following `pc` means starting at the symbol it
        // is in rather than at `pc`, so that what led here is on the screen too; an
        // instruction stream of more than one length cannot be walked backwards, and
        // the symbol is the only place it is safe to start from.
        let from = match self.showing {
            Some(at) => at,
            None => match session.symbols().nearest(registers.pc) {
                Some((_, offset)) if offset <= CONTEXT => registers.pc - offset,
                _ => registers.pc,
            },
        };
        self.code = session.disassemble(hart, Address::Virtual(from), CODE);
        self.memory = Some(session.memory(hart, Address::Virtual(self.at), MEMORY));
        self.devices = session.devices();
        self.traps = session.traps(hart);
        self.taken = session.traps_taken(hart);
        self.breakpoints = session.breakpoints().collect();
        self.interrupts = session.interrupts(hart);
        self.stop = session.stop().cloned();
        self.registers = Some(registers);
        self.harts = session.harts();
        self.hart = hart;
        self.current = true;
    }
}

/// An address, and what the image calls it. The whole reason symbols are here: a
/// kernel that stopped somewhere is placed by the name of what it stopped in, not by
/// the number.
pub fn place(symbols: &Symbols, addr: u64) -> String {
    match symbols.nearest(addr) {
        Some((name, 0)) => format!("{addr:#x} <{name}>"),
        Some((name, offset)) => format!("{addr:#x} <{name}+{offset:#x}>"),
        None => format!("{addr:#x}"),
    }
}

/// The fascia: what to ask of the machine, and a lamp saying what it is doing.
///
/// A board says what it is doing with a lamp rather than a word, so this does too, and
/// the word beside it is legend. The transport is on the left where a hand goes first,
/// and what happened last is on the right, which is the thing read rather than pressed.
pub fn controls(ui: &mut Ui, view: &View, state: State, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let running = state == State::Running;
        let over = state == State::Halted;

        if ui
            .add_enabled(
                !over,
                Button::new(match running {
                    true => "hold",
                    false => "▶ run",
                }),
            )
            .clicked()
        {
            actions.push(match running {
                true => Action::Pause,
                false => Action::Run,
            });
        }
        // Stepping a running machine would be stepping it somewhere it has already
        // gone, so it is offered only once it has stopped.
        if ui
            .add_enabled(!running && !over, Button::new("step"))
            .on_hover_text("one instruction")
            .clicked()
        {
            actions.push(Action::Step);
        }
        if ui
            .add_enabled(!running && !over, Button::new("to trap"))
            .on_hover_text("run until this hart enters a handler")
            .clicked()
        {
            actions.push(Action::StepToTrap);
        }

        ui.add_space(6.0);
        lamp(ui, state);

        if view.harts > 1 {
            ui.add_space(6.0);
            ui.label(theme::legend("hart"));
            for hart in 0..view.harts {
                if ui
                    .selectable_label(hart == view.hart, theme::data(hart.to_string()))
                    .clicked()
                {
                    actions.push(Action::Hart(hart));
                }
            }
        }

        if let Some(stop) = &view.stop {
            ui.add_space(6.0);
            ui.label(theme::faint(describe(stop)));
        }
    });
}

/// The status lamp, painted rather than written: a board has one, and a filled dot is
/// read at a glance where a word has to be.
fn lamp(ui: &mut Ui, state: State) {
    let (colour, name) = match state {
        State::Running => (theme::LIVE, "running"),
        State::Paused => (theme::GOLD, "held"),
        State::Halted => (theme::FAULT, "halted"),
    };
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    let centre = rect.center();
    // A lit lamp has a halo on a dark board, which is also what tells the eye it is lit
    // rather than printed.
    ui.painter()
        .circle_filled(centre, 6.0, colour.gamma_multiply(0.20));
    ui.painter().circle_filled(centre, 3.5, colour);
    ui.label(theme::legend(name));
}

/// Why the machine last stopped, in a few words.
fn describe(stop: &Stop) -> String {
    match stop {
        Stop::Halted(halt) => format!("halted: {halt}"),
        Stop::Paused => "held".to_owned(),
        Stop::Breakpoint { pc, .. } => format!("breakpoint {pc:#x}"),
        Stop::Watchpoint { at, was, now, .. } => {
            format!("{:#x}: {was:#x} → {now:#x}", at.addr())
        }
        Stop::Stepped { retired, .. } => match retired {
            1 => "stepped".to_owned(),
            n => format!("stepped {n}"),
        },
        Stop::Trapped { taken, .. } => format!("trap: {}", taken.trap),
    }
}

/// The integer registers, four to a row.
///
/// The name is legend and the value is the answer, so they are not the same weight: a
/// register file is read by scanning the right-hand column of each pair, and whatever
/// moved since the machine last stopped is the only thing in gold.
pub fn registers(ui: &mut Ui, view: &View) {
    let Some(regs) = &view.registers else {
        return;
    };
    Grid::new("registers")
        .num_columns(8)
        .spacing([8.0, 2.0])
        .show(ui, |ui| {
            for (which, held) in regs.x.iter().enumerate() {
                ui.label(theme::legend(REG_NAMES[which]));
                // Two thirds of a register file is usually zero, and a column of zeroes
                // is what a person is scanning past rather than at.
                ui.label(match (*held, view.changed[which]) {
                    (_, true) => theme::value(format!("{held:#x}"), true),
                    (0, false) => theme::faint("0"),
                    _ => theme::value(format!("{held:#x}"), false),
                });
                if (which + 1) % 4 == 0 {
                    ui.end_row();
                }
            }
        });
}

/// The control registers a person reads, and only the ones holding something: a machine
/// that has never been in supervisor mode has nothing to say about `stvec`, and twenty
/// rows of zero are twenty rows to look past.
pub fn csrs(ui: &mut Ui, view: &View) {
    Grid::new("csrs")
        .num_columns(4)
        .spacing([8.0, 2.0])
        .show(ui, |ui| {
            let mut shown = 0;
            for (index, csr) in view.csrs.iter().enumerate() {
                let moved = view.csrs_changed.get(index).copied().unwrap_or(false);
                if csr.value == 0 && !moved {
                    continue;
                }
                ui.label(theme::legend(csr.name));
                ui.label(theme::value(format!("{:#x}", csr.value), moved));
                shown += 1;
                if shown % 2 == 0 {
                    ui.end_row();
                }
            }
        });
    let (pending, enabled) = view.interrupts;
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(theme::legend("pending"));
        ui.label(theme::faint(format!("{pending:#x}")));
        ui.label(theme::legend("enabled"));
        ui.label(theme::faint(format!("{enabled:#x}")));
    });
}

/// The disassembly, with the instruction about to execute under the probe and a
/// breakpoint on any line that is clicked.
///
/// The instruction prints itself, which is the disassembler this project already had.
/// What is added beside it is the address resolved through the symbols and, for a branch
/// or a jump, where it goes: `Display for Inst` gives an offset, because an instruction
/// does not know where it is.
pub fn code(ui: &mut Ui, view: &View, symbols: &Symbols, actions: &mut Vec<Action>) {
    let pc = view.registers.as_ref().map(|regs| regs.pc);
    if view.code.is_empty() {
        // Not a failure worth hiding: a hart that jumped somewhere there is no memory is
        // exactly the case a person opened this panel for.
        ui.label(theme::legend(match pc {
            Some(pc) => format!("nothing to read at {pc:#x}"),
            None => "nothing read yet".to_owned(),
        }));
        return;
    }
    ScrollArea::vertical()
        .id_salt("code")
        .max_height(250.0)
        .show(ui, |ui| {
            for line in &view.code {
                let here = Some(line.at) == pc;
                let stops = view.breakpoints.contains(&line.at);
                // The probe is a band behind the row rather than a colour on it, so the
                // eye finds the current instruction without reading anything. A frame,
                // because painting it afterwards paints it over the text.
                let band = match here {
                    true => Frame::new()
                        .fill(theme::PROBE.gamma_multiply(0.13))
                        .inner_margin(Margin::symmetric(2, 0)),
                    false => Frame::new().inner_margin(Margin::symmetric(2, 0)),
                };
                band.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // The dot is the control: clicking it sets a breakpoint, so an
                        // address that can be seen never has to be typed.
                        let (rect, hit) =
                            ui.allocate_exact_size(Vec2::new(10.0, 12.0), Sense::click());
                        if hit.hovered() {
                            ui.painter().circle_filled(rect.center(), 4.0, theme::TRACE);
                        }
                        if stops {
                            ui.painter().circle_filled(
                                rect.center(),
                                5.0,
                                theme::FAULT.gamma_multiply(0.25),
                            );
                            ui.painter().circle_filled(rect.center(), 3.0, theme::FAULT);
                        }
                        if hit.clicked() {
                            actions.push(Action::ToggleBreakpoint(line.at));
                        }

                        ui.label(theme::faint(format!("{:>10x}", line.at)));
                        let text = match (&line.inst, line.length) {
                            (Some(inst), _) => inst.to_string(),
                            // What is there but decodes to nothing, at the width it actually
                            // occupies: a halfword shown as a word would be showing two
                            // bytes that were never read.
                            (None, 2) => format!(".half {:#06x}", line.encoding as u16),
                            (None, _) => format!(".word {:#010x}", line.encoding),
                        };
                        let body = RichText::new(text).monospace().color(match here {
                            true => theme::PROBE,
                            false => theme::SILK,
                        });
                        ui.label(body);
                        if let Some(target) = line.target {
                            ui.label(theme::faint(place(symbols, target)));
                        }
                        // The band is the width of the panel rather than of the text,
                        // so the row a hart is on reads as a row.
                        ui.allocate_space(Vec2::new(ui.available_width(), 0.0));
                    });
                });
            }
        });
    if view.showing.is_some() && ui.button("follow pc").clicked() {
        actions.push(Action::Follow);
    }
}

/// Memory as hex and as text, sixteen bytes to a row, with a byte nothing answered for
/// shown as one nothing answered for rather than as a zero.
///
/// A device's registers are not read here and never will be: on this bus a read is often
/// an action, and a panel that polled one would consume the guest's input by showing it.
/// So a device's window reads as nothing, and what a device is doing is the panel below.
pub fn memory(ui: &mut Ui, view: &View, at: &mut String, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(theme::legend("at"));
        let box_ = ui.add(
            TextEdit::singleline(at)
                .desired_width(120.0)
                .font(eframe::egui::TextStyle::Monospace),
        );
        if box_.lost_focus()
            && ui.input(|input| input.key_pressed(eframe::egui::Key::Enter))
            && let Some(addr) = parse(at)
        {
            actions.push(Action::Goto(addr));
        }
        // The two addresses a person actually wants, rather than making them read one
        // off the panel above and type it back in.
        for (name, of) in [("pc", 0usize), ("sp", 2usize)] {
            if ui.small_button(name).clicked()
                && let Some(regs) = &view.registers
            {
                let addr = match of {
                    0 => regs.pc,
                    which => regs.x[which],
                };
                *at = format!("{addr:#x}");
                actions.push(Action::Goto(addr));
            }
        }
    });
    let Some(window) = &view.memory else {
        return;
    };
    if window.bytes.iter().all(Option::is_none) {
        ui.label(theme::legend(format!(
            "nothing answers at {:#x} — no memory, an unreachable page, or a device",
            view.at
        )));
        return;
    }
    ScrollArea::vertical()
        .id_salt("memory")
        .max_height(260.0)
        .show(ui, |ui| {
            for (row, bytes) in window.bytes.chunks(ROW).enumerate() {
                let addr = view.at.wrapping_add((row * ROW) as u64);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(theme::faint(format!("{addr:>10x}")));
                    let mut hex = String::with_capacity(ROW * 3);
                    let mut text = String::with_capacity(ROW);
                    let mut missing = false;
                    for byte in bytes {
                        match byte {
                            Some(byte) => {
                                hex.push_str(&format!("{byte:02x} "));
                                text.push(match byte.is_ascii_graphic() || *byte == b' ' {
                                    true => *byte as char,
                                    false => '·',
                                });
                            }
                            None => {
                                hex.push_str("·· ");
                                text.push(' ');
                                missing = true;
                            }
                        }
                    }
                    ui.label(RichText::new(hex).monospace().color(match missing {
                        true => theme::SILK_DIM,
                        false => theme::SILK,
                    }));
                    ui.label(theme::faint(text));
                });
            }
        });
}
/// An address a person typed, in hex with or without the prefix, or in decimal.
fn parse(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text
            .parse()
            .ok()
            .or_else(|| u64::from_str_radix(text, 16).ok()),
    }
}

/// The bus, drawn as the board it is.
///
/// This is the one panel that is a picture rather than a table, and it earns it: what a
/// person wants from a memory map is *where things sit*, and a list of names in the
/// order a `Vec` happens to hold them does not say that. So the devices are laid out in
/// address order down a rail, each with the pad it answers at, its base, and how much
/// space it takes.
///
/// The order is the address order and is exact; the *spacing* is not, and deliberately
/// so. This address space runs from a four-kilobyte serial port to a sixteen-gibibyte
/// PCI window, so anything drawn to scale would be one device and a lot of nothing, and
/// a log scale is a picture people read as linear. Position carries the order, the text
/// carries the number, and neither pretends to be the other.
pub fn devices(ui: &mut Ui, view: &View) {
    if view.devices.is_empty() {
        ui.label(theme::legend("nothing on the bus"));
        return;
    }
    for report in &view.devices {
        component(ui, report, 0);
    }
}

/// One thing on the bus, and whatever is plugged into it.
fn component(ui: &mut Ui, report: &Report, depth: usize) {
    let id = ui.make_persistent_id(("device", report.base, report.name, depth));
    let mut open = ui.data_mut(|data| data.get_temp::<bool>(id).unwrap_or(false));

    let row = ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 12.0);
        // The pad it answers at. Filled when it is the thing itself, hollow for what is
        // plugged into it, which is how a board distinguishes a part from a header.
        let (pad, hit) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), Sense::click());
        let painter = ui.painter();
        if depth == 0 {
            painter.rect_filled(pad, 1.0, theme::GOLD.gamma_multiply(0.75));
        } else {
            painter.rect_stroke(
                pad,
                1.0,
                Stroke::new(1.0, theme::GOLD.gamma_multiply(0.6)),
                eframe::egui::StrokeKind::Inside,
            );
        }

        let name = ui.label(theme::heading(report.name));
        if report.size > 0 {
            ui.label(theme::faint(format!("{:#x}", report.base)));
            ui.label(theme::legend(span(report.size)));
        }
        if hit.clicked() || name.clicked() {
            open = !open;
        }
        hit.union(name)
    });
    // The whole row is the control, so a name is as clickable as its pad.
    if row
        .inner
        .union(row.response.interact(Sense::click()))
        .clicked()
    {
        open = !open;
    }
    ui.data_mut(|data| data.insert_temp(id, open));

    if open && !report.fields.is_empty() {
        ui.indent(id, |ui| fields(ui, report));
    }
    for behind in &report.behind {
        component(ui, behind, depth + 1);
    }
}

/// How much address space something takes, in the units a person thinks in. A device
/// tree says `0x10000`; nobody reads that as sixty-four kilobytes without stopping.
fn span(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [(1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K"), (1, "B")];
    for (size, suffix) in UNITS {
        if bytes >= size {
            let whole = bytes / size;
            return match bytes % size {
                0 => format!("{whole}{suffix}"),
                _ => format!("{:.1}{suffix}", bytes as f64 / size as f64),
            };
        }
    }
    "0".to_owned()
}

/// What one device has to say about itself.
fn fields(ui: &mut Ui, report: &Report) {
    Grid::new(("fields", report.base, report.name))
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            for (name, held) in &report.fields {
                ui.label(theme::legend(name.as_ref()));
                ui.label(match held {
                    // A flag reads as a lamp rather than as the word `true`, which is
                    // what the rest of the window says about a state.
                    Value::Flag(true) => theme::value("yes".to_owned(), true),
                    Value::Flag(false) => theme::faint("no"),
                    other => theme::data(show(other)),
                });
                ui.end_row();
            }
        });
}

/// A device's value, shown the way the device said it means something: a count is a
/// number and a register is its bits.
fn show(value: &Value) -> String {
    match value {
        Value::Count(count) => count.to_string(),
        Value::Bits(bits) => format!("{bits:#x}"),
        Value::Flag(flag) => match flag {
            true => "yes".to_owned(),
            false => "no".to_owned(),
        },
        Value::Text(text) => text.clone(),
    }
}

/// The traps this hart took, newest first, which on a guest that is not where it should
/// be is usually the whole answer.
pub fn traps(ui: &mut Ui, view: &View, symbols: &Symbols) {
    if view.traps.is_empty() {
        ui.label(theme::legend("none taken"));
        return;
    }
    ui.label(theme::legend(
        match view.taken as usize > view.traps.len() {
            true => format!("{} taken · last {} kept", view.taken, view.traps.len()),
            false => format!("{} taken", view.taken),
        },
    ));
    ScrollArea::vertical()
        .id_salt("traps")
        .max_height(150.0)
        .show(ui, |ui| {
            for taken in &view.traps {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(theme::faint(format!("{:>5}", taken.seq)));
                    ui.label(theme::data(place(symbols, taken.pc)));
                    // The pair of modes is the thing neither `mcause` nor `mepc`
                    // records, and the usual reason a handler never runs.
                    ui.label(theme::legend(format!("{} → {}", taken.from, taken.to)));
                    ui.label(theme::faint(place(symbols, taken.handler)));
                });
            }
        });
}
