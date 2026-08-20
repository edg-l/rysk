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

use eframe::egui::{Color32, Grid, RichText, ScrollArea, Ui};

use crate::{
    debug::{Address, Csr, Line, Registers, Session, Stop, Symbols, Window},
    device::{Report, Value},
    inst::REG_NAMES,
    machine::State,
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

/// The bar above everything: what the machine is doing, and what to ask of it.
pub fn controls(ui: &mut Ui, view: &View, state: State, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let running = state == State::Running;
        let over = state == State::Halted;

        if ui
            .add_enabled(
                !over,
                eframe::egui::Button::new(match running {
                    true => "⏸ pause",
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
            .add_enabled(!running && !over, eframe::egui::Button::new("⏭ step"))
            .clicked()
        {
            actions.push(Action::Step);
        }
        if ui
            .add_enabled(!running && !over, eframe::egui::Button::new("⤓ to trap"))
            .clicked()
        {
            actions.push(Action::StepToTrap);
        }

        ui.separator();
        ui.label(
            RichText::new(match state {
                State::Running => "running",
                State::Paused => "paused",
                State::Halted => "halted",
            })
            .color(match state {
                State::Running => Color32::from_rgb(0x6d, 0xc0, 0x6d),
                State::Paused => Color32::from_rgb(0xd0, 0xb0, 0x50),
                State::Halted => Color32::from_rgb(0xd0, 0x70, 0x70),
            }),
        );

        if view.harts > 1 {
            ui.separator();
            for hart in 0..view.harts {
                if ui
                    .selectable_label(hart == view.hart, format!("hart {hart}"))
                    .clicked()
                {
                    actions.push(Action::Hart(hart));
                }
            }
        }

        if let Some(stop) = &view.stop {
            ui.separator();
            ui.label(describe(stop));
        }
    });
}

/// Why the machine last stopped, in a few words.
fn describe(stop: &Stop) -> String {
    match stop {
        Stop::Halted(halt) => format!("halted: {halt}"),
        Stop::Paused => "paused".to_owned(),
        Stop::Breakpoint { pc, .. } => format!("breakpoint at {pc:#x}"),
        Stop::Watchpoint { at, was, now, .. } => {
            format!("{:#x}: {was:#x} became {now:#x}", at.addr())
        }
        Stop::Stepped { retired, .. } => format!("stepped {retired}"),
        Stop::Trapped { taken, .. } => format!("trapped: {}", taken.trap),
    }
}

/// The integer registers, four to a row, with what moved since the last stop picked
/// out. `x0` is there because leaving a hole where it should be reads worse than the
/// zero it always holds.
pub fn registers(ui: &mut Ui, view: &View) {
    let Some(regs) = &view.registers else {
        return;
    };
    Grid::new("registers")
        .num_columns(8)
        .striped(true)
        .show(ui, |ui| {
            for (which, value) in regs.x.iter().enumerate() {
                ui.label(RichText::new(REG_NAMES[which]).monospace().weak());
                ui.label(number(format!("{value:#x}"), view.changed[which]));
                if (which + 1) % 4 == 0 {
                    ui.end_row();
                }
            }
        });
}

/// A value, picked out if it just moved.
fn number(text: String, changed: bool) -> RichText {
    let text = RichText::new(text).monospace();
    match changed {
        true => text.color(Color32::from_rgb(0xf0, 0xc0, 0x60)),
        false => text,
    }
}

/// The control registers a person reads, named, and only the ones holding something: a
/// machine that has never been in supervisor mode has nothing to say about `stvec`, and
/// twenty rows of zero are twenty rows to look past.
pub fn csrs(ui: &mut Ui, view: &View) {
    Grid::new("csrs")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            let mut shown = 0;
            for (index, csr) in view.csrs.iter().enumerate() {
                if csr.value == 0 && !view.csrs_changed.get(index).copied().unwrap_or(false) {
                    continue;
                }
                ui.label(RichText::new(csr.name).monospace().weak());
                ui.label(number(
                    format!("{:#x}", csr.value),
                    view.csrs_changed.get(index).copied().unwrap_or(false),
                ));
                shown += 1;
                if shown % 2 == 0 {
                    ui.end_row();
                }
            }
        });
    let (pending, enabled) = view.interrupts;
    ui.label(
        RichText::new(format!("mip {pending:#x}  mie {enabled:#x}"))
            .monospace()
            .weak(),
    );
}

/// The disassembly, with `pc` picked out and a breakpoint on any line that is clicked.
///
/// The instruction prints itself, which is the disassembler this project already had.
/// What is added beside it is the address resolved through the symbols and, for a
/// branch or a jump, where it goes: `Display for Inst` gives an offset, because an
/// instruction does not know where it is.
pub fn code(ui: &mut Ui, view: &View, symbols: &Symbols, actions: &mut Vec<Action>) {
    let pc = view.registers.as_ref().map(|regs| regs.pc);
    if view.code.is_empty() {
        // Which is not a failure worth hiding: a hart that jumped somewhere there is no
        // memory is exactly the case a person opened this panel for.
        ui.label(
            RichText::new(match pc {
                Some(pc) => format!("nothing to read at {pc:#x}"),
                None => "nothing read yet".to_owned(),
            })
            .weak(),
        );
        return;
    }
    ScrollArea::vertical()
        .id_salt("code")
        .max_height(320.0)
        .show(ui, |ui| {
            for line in &view.code {
                let here = Some(line.at) == pc;
                let stops = view.breakpoints.contains(&line.at);
                ui.horizontal(|ui| {
                    // The breakpoint dot is the control: clicking it is what sets one,
                    // so a person never has to type an address they can see.
                    let mark = match (stops, here) {
                        (true, _) => "●",
                        (false, true) => "▶",
                        (false, false) => " ",
                    };
                    if ui
                        .small_button(RichText::new(mark).monospace().color(match stops {
                            true => Color32::from_rgb(0xd0, 0x60, 0x60),
                            false => Color32::GRAY,
                        }))
                        .on_hover_text("break here")
                        .clicked()
                    {
                        actions.push(Action::ToggleBreakpoint(line.at));
                    }
                    let text = RichText::new(format!(
                        "{:#010x}  {:<28}{}",
                        line.at,
                        match (&line.inst, line.length) {
                            (Some(inst), _) => inst.to_string(),
                            // What is there but decodes to nothing, at the width it
                            // actually occupies: a halfword shown as a word would be
                            // showing two bytes that were never read.
                            (None, 2) => format!(".half {:#06x}", line.encoding as u16),
                            (None, _) => format!(".word {:#010x}", line.encoding),
                        },
                        match line.target {
                            Some(target) => place(symbols, target),
                            None => String::new(),
                        }
                    ))
                    .monospace();
                    ui.label(match here {
                        true => text.color(Color32::from_rgb(0x80, 0xd0, 0xf0)),
                        false => text,
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
/// A device's registers are not read here and never will be: on this bus a read is
/// often an action, and a panel that polled one would consume the guest's input by
/// showing it. So a device's window reads as nothing, and what a device is doing is the
/// panel below.
pub fn memory(ui: &mut Ui, view: &View, at: &mut String, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label("at");
        let box_ = ui.text_edit_singleline(at);
        if box_.lost_focus()
            && ui.input(|input| input.key_pressed(eframe::egui::Key::Enter))
            && let Some(addr) = parse(at)
        {
            actions.push(Action::Goto(addr));
        }
        if ui.small_button("pc").clicked()
            && let Some(regs) = &view.registers
        {
            *at = format!("{:#x}", regs.pc);
            actions.push(Action::Goto(regs.pc));
        }
        if ui.small_button("sp").clicked()
            && let Some(regs) = &view.registers
        {
            *at = format!("{:#x}", regs.x[2]);
            actions.push(Action::Goto(regs.x[2]));
        }
    });
    let Some(window) = &view.memory else {
        return;
    };
    if window.bytes.iter().all(Option::is_none) {
        ui.label(
            RichText::new(format!(
                "nothing answers at {:#x}: no memory there, a page this hart cannot \
                 reach, or a device, which is never read",
                view.at
            ))
            .weak(),
        );
        return;
    }
    ScrollArea::vertical()
        .id_salt("memory")
        .max_height(280.0)
        .show(ui, |ui| {
            for (row, bytes) in window.bytes.chunks(ROW).enumerate() {
                let addr = view.at.wrapping_add((row * ROW) as u64);
                let hex: String = bytes
                    .iter()
                    .map(|byte| match byte {
                        Some(byte) => format!("{byte:02x} "),
                        None => "?? ".to_owned(),
                    })
                    .collect();
                let text: String = bytes
                    .iter()
                    .map(|byte| match byte {
                        Some(byte) if byte.is_ascii_graphic() || *byte == b' ' => *byte as char,
                        _ => '.',
                    })
                    .collect();
                ui.label(RichText::new(format!("{addr:#010x}  {hex} {text}")).monospace());
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

/// What is on the bus and what each of it is doing. Asked rather than read: this is
/// `Device::describe`, which is a question by construction.
pub fn devices(ui: &mut Ui, view: &View) {
    for report in &view.devices {
        ui.collapsing(
            format!(
                "{} at {:#x}..{:#x}",
                report.name,
                report.base,
                report.base + report.size
            ),
            |ui| fields(ui, report),
        );
    }
}

fn fields(ui: &mut Ui, report: &Report) {
    Grid::new(("device", report.base, report.name))
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for (name, value) in &report.fields {
                ui.label(RichText::new(name.as_ref()).weak());
                ui.label(RichText::new(show(value)).monospace());
                ui.end_row();
            }
        });
    // A root complex is a bus of its own, so what is plugged into it goes under it.
    for behind in &report.behind {
        ui.collapsing(behind.name, |ui| fields(ui, behind));
    }
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
        ui.label(RichText::new("none taken").weak());
        return;
    }
    ui.label(
        RichText::new(match view.taken as usize > view.traps.len() {
            true => format!("{} taken, the last {} kept", view.taken, view.traps.len()),
            false => format!("{} taken", view.taken),
        })
        .weak(),
    );
    ScrollArea::vertical()
        .id_salt("traps")
        .max_height(200.0)
        .show(ui, |ui| {
            for taken in &view.traps {
                ui.label(
                    RichText::new(format!(
                        "{:>6}  {}  {} to {}, handler {}",
                        taken.seq,
                        place(symbols, taken.pc),
                        taken.from,
                        taken.to,
                        place(symbols, taken.handler),
                    ))
                    .monospace(),
                );
            }
        });
}
