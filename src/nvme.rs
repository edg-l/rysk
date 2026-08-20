//! An NVMe controller: a dozen registers, and pairs of queues in guest memory that
//! everything else happens through.
//!
//! Almost nothing here is a register. Software builds a submission queue and a
//! completion queue in its own memory, tells the controller where they are, and then
//! rings a doorbell to say it has put a command in one; the controller reads the
//! command, does it, and writes an answer into the other. There are two queue pairs'
//! worth of registers and the rest is memory, which is what makes this a small device
//! next to the controller before it.
//!
//! A command finishes inside the doorbell write that announced it, for the same reason a
//! transfer does in `xhci`: there is no medium underneath with a latency to model, and a
//! controller that answers immediately is one that is very fast. Nothing here is ever
//! outstanding, so nothing here needs a `poll`.
//!
//! NVM Express Base Specification 2.0 defines the registers, the queues and the
//! commands, and the NVM Command Set Specification the reads and writes. Neither is
//! redistributable, so where a field needs explaining the explanation is here;
//! `include/linux/nvme.h` is the same layout as a driver reads it.

use crate::{
    device::{Dma, Field, Value, field as says},
    disk::{BLOCK, Disk},
    pci::{Asserted, Bar, Function, Header, MsiX},
    trap::Exception,
};

/// The identity this reports. Nothing binds on it: `nvme` matches the class, so this is
/// only what `lspci` prints, and it is the one QEMU's own controller reports so that the
/// two machines name the same device.
const VENDOR: u16 = 0x1b36;
const DEVICE: u16 = 0x0010;
/// Mass storage, non-volatile memory, the NVMe programming interface, revision two.
const CLASS: u32 = 0x0108_0202;

/// The one window: the registers, the doorbells above them, and the table of messages
/// and the array of the ones that could not be sent above that.
pub const WINDOW: u64 = 0x4000;
const DOORBELLS: u64 = 0x1000;
const MSIX_TABLE: u64 = 0x2000;
const MSIX_PENDING: u64 = 0x3000;

/// How many queue pairs there are, counting the admin pair as the first. One per hart
/// and one to spare, which is what a driver asks for.
const QUEUES: usize = 16;
/// The admin pair, which is the one the controller is told about through registers
/// rather than through a command.
const ADMIN: usize = 0;

/// The largest queue this controller will take, as a number of entries. Reported one
/// less than it is, since a queue of no entries is not a queue.
const MAX_ENTRIES: u32 = 1024;

/// How long each kind of queue entry is. Both are fixed by the specification and
/// software confirms them in the configuration register, as powers of two.
const COMMAND: u64 = 64;
const COMPLETION: u64 = 16;

/// The largest transfer one command may ask for, as the power of two pages the
/// capability reports. Thirty-two pages is 128 KiB, which is enough that one command
/// carries a whole readahead and few enough that the list of pages fits in one page.
const MAX_TRANSFER_PAGES: u8 = 5;

/// The registers.
const CAP: u64 = 0x00;
const VS: u64 = 0x08;
const INTMS: u64 = 0x0c;
const INTMC: u64 = 0x10;
const CC: u64 = 0x14;
const CSTS: u64 = 0x1c;
const AQA: u64 = 0x24;
const ASQ: u64 = 0x28;
const ACQ: u64 = 0x30;

/// Version 1.4 of the specification, which is what says which commands a driver may
/// assume are there.
const VERSION: u32 = 0x0001_0400;

/// Bits of `CC`: on, how large a page the controller should read addresses as, how it
/// is being shut down, and how long the entries of each queue are.
const CC_ENABLE: u32 = 1 << 0;
const CC_PAGE: u32 = 0xf << 7;
const CC_SHUTDOWN: u32 = 0x3 << 14;

/// Bits of `CSTS`: ready, a fatal error, and how far a shutdown has got.
const CSTS_READY: u32 = 1 << 0;
const CSTS_FATAL: u32 = 1 << 1;
const CSTS_SHUTDOWN: u32 = 0x3 << 2;
const CSTS_SHUTDOWN_DONE: u32 = 0x2 << 2;

/// The admin commands. NVM Express Base Specification 2.0, figure 138.
const DELETE_SQ: u8 = 0x00;
const CREATE_SQ: u8 = 0x01;
const GET_LOG_PAGE: u8 = 0x02;
const DELETE_CQ: u8 = 0x04;
const CREATE_CQ: u8 = 0x05;
const IDENTIFY: u8 = 0x06;
const ABORT: u8 = 0x08;
const SET_FEATURES: u8 = 0x09;
const GET_FEATURES: u8 = 0x0a;
const ASYNC_EVENT: u8 = 0x0c;
const KEEP_ALIVE: u8 = 0x18;

/// And the ones for the command set that reads and writes blocks.
const FLUSH: u8 = 0x00;
const WRITE: u8 = 0x01;
const READ: u8 = 0x02;
const WRITE_ZEROES: u8 = 0x08;

/// Which structure an identify command is asking for.
const CNS_NAMESPACE: u32 = 0x00;
const CNS_CONTROLLER: u32 = 0x01;
const CNS_NAMESPACES: u32 = 0x02;
const CNS_DESCRIPTORS: u32 = 0x03;
const CNS_NAMESPACE_INDEPENDENT: u32 = 0x08;

/// How long every structure identify answers with is.
const IDENTIFY_SIZE: usize = 4096;

/// The one feature this controller has that means anything: how many queue pairs
/// software would like. NVM Express Base Specification 2.0, figure 316.
const FEATURE_QUEUES: u32 = 0x07;

/// The only namespace there is. A controller with one disk behind it has one.
const NAMESPACE: u32 = 1;

/// A status, as the field a completion carries it in: the code, and which set of codes
/// it is from. The phase bit shares the field and is put in when the entry is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status(u16);

/// A code from the generic set, which is the one every command shares.
const fn generic(code: u8) -> Status {
    Status((code as u16) << 1)
}

/// And one from the set that means something only for the command that answered it.
const fn specific(code: u8) -> Status {
    Status(((code as u16) << 1) | (1 << 9))
}

const SUCCESS: Status = generic(0x00);
const INVALID_OPCODE: Status = generic(0x01);
const INVALID_FIELD: Status = generic(0x02);
const DATA_TRANSFER_ERROR: Status = generic(0x04);
const INVALID_NAMESPACE: Status = generic(0x0b);
const LBA_OUT_OF_RANGE: Status = generic(0x80);
const INVALID_QUEUE: Status = specific(0x01);
const INVALID_QUEUE_SIZE: Status = specific(0x02);
const INVALID_COMPLETION_QUEUE: Status = specific(0x00);

/// One submission queue: where it is, how long, and how far the controller and software
/// have each got round it.
#[derive(Debug, Clone, Copy)]
struct Submission {
    base: u64,
    entries: u32,
    /// Where the controller has read up to, which it publishes in every completion so
    /// software knows how much room there is.
    head: u32,
    /// Which completion queue its answers go on.
    completion: usize,
}

/// And one completion queue, which the controller writes and software reads.
#[derive(Debug, Clone, Copy)]
struct Completion {
    base: u64,
    entries: u32,
    /// Where software has read up to, which it says by ringing the doorbell.
    head: u32,
    /// And where the controller has written up to.
    tail: u32,
    /// Which turn round the queue this is, since there is no count between the two ends
    /// and the bit is what tells an entry from the one it overwrote.
    phase: bool,
    /// Which message an entry posted here sends, and whether it sends one at all.
    vector: u16,
    interrupts: bool,
}

/// A command, as the fields of it anything here reads.
#[derive(Debug, Clone, Copy, Default)]
struct Request {
    opcode: u8,
    id: u16,
    namespace: u32,
    /// Where the data is, or where the list of where it is, is.
    prp1: u64,
    prp2: u64,
    /// The six words whose meaning is the command's own.
    words: [u32; 6],
}

/// The controller.
#[derive(Debug)]
pub struct Nvme {
    dma: Dma,
    disk: Box<dyn Disk>,
    cc: u32,
    csts: u32,
    /// Which messages software has masked, which means something only for a machine
    /// where an interrupt is a wire.
    masked: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    submissions: [Option<Submission>; QUEUES],
    completions: [Option<Completion>; QUEUES],
    asserted: Asserted,
}

impl Nvme {
    /// A controller with `disk` behind it.
    pub fn new(dma: Dma, disk: Box<dyn Disk>) -> Self {
        let mut nvme = Self {
            dma,
            disk,
            cc: 0,
            csts: 0,
            masked: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            submissions: [None; QUEUES],
            completions: [None; QUEUES],
            asserted: Asserted::default(),
        };
        nvme.reset();
        nvme
    }

    /// Everything a controller forgets when it is turned off, which is every queue and
    /// where each one was. What it keeps is the registers software wrote before turning
    /// it on, which is how it was told where the admin pair is.
    fn reset(&mut self) {
        self.csts = 0;
        self.submissions = [None; QUEUES];
        self.completions = [None; QUEUES];
        self.signal();
    }

    fn ready(&self) -> bool {
        self.csts & CSTS_READY != 0
    }

    /// How large a page software said addresses should be read as.
    /// NVM Express Base Specification 2.0, 3.1.3.5.
    fn page(&self) -> u64 {
        1 << (12 + ((self.cc & CC_PAGE) >> 7))
    }

    // ------------------------------------------------------------------- the registers

    fn capabilities(&self) -> u64 {
        /// How long software should wait for the controller to become ready, in units
        /// of half a second. It is ready inside the write that asks it to be, so this
        /// is the smallest thing that is not "no time at all".
        const TIMEOUT: u64 = 1;
        /// The command set that reads and writes blocks, which is the only one here.
        const NVM_COMMAND_SET: u64 = 1 << 37;
        /// Queues have to be one run of memory each, which is what this model reads.
        const CONTIGUOUS: u64 = 1 << 16;

        (MAX_ENTRIES as u64 - 1) | CONTIGUOUS | (TIMEOUT << 24) | NVM_COMMAND_SET
    }

    fn read(&mut self, register: u64) -> u32 {
        match register {
            CAP => self.capabilities() as u32,
            CAP_HIGH => (self.capabilities() >> 32) as u32,
            VS => VERSION,
            // Both of these read as which messages are masked; they differ in what
            // writing one does. NVM Express Base Specification 2.0, 3.1.3.3.
            INTMS | INTMC => self.masked,
            CC => self.cc,
            CSTS => self.csts,
            AQA => self.aqa,
            ASQ => self.asq as u32,
            ASQ_HIGH => (self.asq >> 32) as u32,
            ACQ => self.acq as u32,
            ACQ_HIGH => (self.acq >> 32) as u32,
            _ => 0,
        }
    }

    fn write(&mut self, register: u64, value: u32) {
        match register {
            // Writing a one masks a message; writing a one to the other register
            // unmasks it. Neither register is written to.
            INTMS => {
                self.masked |= value;
                self.signal();
            }
            INTMC => {
                self.masked &= !value;
                self.signal();
            }
            CC => self.configure(value),
            AQA if !self.ready() => self.aqa = value,
            ASQ if !self.ready() => self.asq = (self.asq & !0xffff_ffff) | value as u64,
            ASQ_HIGH if !self.ready() => {
                self.asq = (self.asq & 0xffff_ffff) | ((value as u64) << 32)
            }
            ACQ if !self.ready() => self.acq = (self.acq & !0xffff_ffff) | value as u64,
            ACQ_HIGH if !self.ready() => {
                self.acq = (self.acq & 0xffff_ffff) | ((value as u64) << 32)
            }
            // A fatal error is cleared by turning the controller off and on again, and
            // the rest of the status is the controller's.
            _ => {}
        }
    }

    /// What a write to the configuration register does: turn the controller on, turn it
    /// off, or shut it down. NVM Express Base Specification 2.0, 3.1.3.5.
    fn configure(&mut self, value: u32) {
        let was = self.cc & CC_ENABLE != 0;
        self.cc = value;
        match (was, value & CC_ENABLE != 0) {
            (false, true) => {
                // The admin queues are the two the registers describe, and they are
                // what the controller becomes ready with. A pair it cannot make is a
                // controller that cannot start, which is what the fatal bit says.
                let submission = self.aqa & 0xfff;
                let completion = (self.aqa >> 16) & 0xfff;
                self.completions[ADMIN] = Some(Completion {
                    base: self.acq,
                    entries: completion + 1,
                    head: 0,
                    tail: 0,
                    phase: true,
                    vector: 0,
                    interrupts: true,
                });
                self.submissions[ADMIN] = Some(Submission {
                    base: self.asq,
                    entries: submission + 1,
                    head: 0,
                    completion: ADMIN,
                });
                self.csts = match self.asq == 0 || self.acq == 0 {
                    true => CSTS_FATAL,
                    false => CSTS_READY,
                };
            }
            (true, false) => self.reset(),
            _ => {}
        }
        // A shutdown is finished the moment it is asked for: there is nothing in flight
        // to finish and nothing cached to write out.
        if value & CC_SHUTDOWN != 0 {
            self.csts = (self.csts & !CSTS_SHUTDOWN) | CSTS_SHUTDOWN_DONE;
        }
    }

    // -------------------------------------------------------------------- the doorbells

    /// Which queue a doorbell belongs to, and whether it is the one software rings to
    /// say it has added a command or the one it rings to say it has taken an answer.
    /// The two alternate, four bytes apart, which is the stride the capability reports.
    fn doorbell(offset: u64) -> (usize, bool) {
        let index = offset / 4;
        ((index / 2) as usize, index % 2 == 1)
    }

    fn ring(&mut self, queue: usize, completion: bool, value: u32) {
        if !self.ready() || queue >= QUEUES {
            return;
        }
        if completion {
            if let Some(queue) = &mut self.completions[queue] {
                queue.head = value % queue.entries.max(1);
            }
            self.signal();
            return;
        }
        let Some(mut submission) = self.submissions[queue] else {
            return;
        };
        let tail = value % submission.entries.max(1);
        // Every command between where the controller had got to and where software says
        // it has got to. A queue whose entries each added another would never end; a
        // driver does not write one, and a guest that did would otherwise hold the hart
        // that rang for it forever.
        for _ in 0..MAX_ENTRIES {
            if submission.head == tail {
                break;
            }
            let at = submission.base + submission.head as u64 * COMMAND;
            submission.head = (submission.head + 1) % submission.entries;
            self.submissions[queue] = Some(submission);
            let Some(request) = self.request(at) else {
                break;
            };
            if let Some((status, result)) = self.run(queue, request) {
                self.complete(submission.completion, queue, request.id, status, result);
            }
        }
        self.submissions[queue] = Some(submission);
    }

    /// The command at `at`, as the fields anything here reads.
    fn request(&self, at: u64) -> Option<Request> {
        let word = |offset: u64| self.dma.load(at + offset, 32).map(|word| word as u32);
        let first = word(0)?;
        Some(Request {
            opcode: first as u8,
            id: (first >> 16) as u16,
            namespace: word(4)?,
            prp1: self.dma.load(at + 24, 64)?,
            prp2: self.dma.load(at + 32, 64)?,
            words: [
                word(40)?,
                word(44)?,
                word(48)?,
                word(52)?,
                word(56)?,
                word(60)?,
            ],
        })
    }

    /// Put an answer on a completion queue and say so.
    fn complete(&mut self, cq: usize, sq: usize, id: u16, status: Status, result: u32) {
        let head = self.submissions[sq].map_or(0, |queue| queue.head);
        let Some(queue) = self.completions.get_mut(cq).and_then(Option::as_mut) else {
            return;
        };
        let next = (queue.tail + 1) % queue.entries;
        if next == queue.head {
            // Software has not taken the answers it has, and there is nowhere to put
            // this one. There is nothing to do about that: dropping the answer leaves a
            // command nothing ever completes, so the controller says it has failed in
            // the one way a driver resets it over.
            // NVM Express Base Specification 2.0, 3.3.3.
            self.csts |= CSTS_FATAL;
            return;
        }
        let at = queue.base + queue.tail as u64 * COMPLETION;
        let phase = queue.phase;
        queue.tail = next;
        if queue.tail == 0 {
            queue.phase = !queue.phase;
        }
        self.dma.store(at, 32, result as u64);
        self.dma.store(at + 4, 32, 0);
        self.dma
            .store(at + 8, 32, (head as u64 & 0xffff) | ((sq as u64) << 16));
        self.dma.store(
            at + 12,
            32,
            (id as u64) | (((status.0 | phase as u16) as u64) << 16),
        );
        self.signal();
        if let Some(queue) = self.completions[cq]
            && queue.interrupts
            && self.masked & (1 << queue.vector.min(31)) == 0
        {
            self.asserted.messages |= 1 << queue.vector.min(63);
        }
    }

    /// Work out what this controller is asserting on its wire, which is that some queue
    /// has an answer on it that software has not taken.
    fn signal(&mut self) {
        self.asserted.pin = self.completions.iter().flatten().any(|queue| {
            queue.interrupts
                && queue.head != queue.tail
                && self.masked & (1 << queue.vector.min(31)) == 0
        });
    }

    // --------------------------------------------------------------------- the commands

    /// Carry out one command. Nothing means it has been taken and will be answered
    /// later, which of the commands here is only ever the one that asks to be told when
    /// something happens.
    fn run(&mut self, queue: usize, request: Request) -> Option<(Status, u32)> {
        match queue {
            ADMIN => self.admin(request),
            _ => Some((self.storage(request), 0)),
        }
    }

    fn admin(&mut self, request: Request) -> Option<(Status, u32)> {
        Some(match request.opcode {
            CREATE_CQ => (self.create_completion(request), 0),
            CREATE_SQ => (self.create_submission(request), 0),
            DELETE_CQ => (self.delete(request, true), 0),
            DELETE_SQ => (self.delete(request, false), 0),
            IDENTIFY => (self.identify(request), 0),
            // Every log page this controller has is empty: there are no errors to
            // report, no temperature to read and no firmware to have slots for. A page
            // of zeroes is what a controller with nothing to say answers with.
            GET_LOG_PAGE => {
                let dwords = ((request.words[0] >> 16) & 0xfff) as usize
                    | ((request.words[1] as usize & 0xffff) << 16);
                (self.put(&request, &vec![0u8; (dwords + 1) * 4]), 0)
            }
            SET_FEATURES | GET_FEATURES => self.feature(request),
            // A command to be answered when something happens, and nothing here ever
            // happens: no temperature, no failed disk and no namespace appearing. So it
            // is taken and never answered, which is what a controller with nothing to
            // report does with one.
            ASYNC_EVENT => return None,
            // Nothing here is ever outstanding long enough to abort, so the command
            // that was to be aborted has already finished.
            ABORT => (SUCCESS, 1),
            KEEP_ALIVE => (SUCCESS, 0),
            _ => (INVALID_OPCODE, 0),
        })
    }

    /// How many queue pairs there are for software to ask for, and which feature
    /// answers are storage. NVM Express Base Specification 2.0, 5.27.
    fn feature(&mut self, request: Request) -> (Status, u32) {
        match request.words[0] & 0xff {
            // Software asks for a number of queue pairs and is told what it may have,
            // both counted one less than they are. Asking and reading back give the
            // same answer, since what it may have does not depend on what it asked for.
            FEATURE_QUEUES => {
                let queues = QUEUES as u32 - 2;
                (SUCCESS, queues | (queues << 16))
            }
            // A feature this controller does not have is one it says it does not have,
            // rather than one it accepts and forgets.
            _ => (INVALID_FIELD, 0),
        }
    }

    fn create_completion(&mut self, request: Request) -> Status {
        /// Bits of the second word: whether the queue is one run of memory, and whether
        /// an entry posted to it sends a message.
        const CONTIGUOUS: u32 = 1 << 0;
        const INTERRUPTS: u32 = 1 << 1;

        let queue = (request.words[0] & 0xffff) as usize;
        let entries = (request.words[0] >> 16) + 1;
        let vector = (request.words[1] >> 16) as u16;
        if queue == ADMIN || queue >= QUEUES || self.completions[queue].is_some() {
            return INVALID_QUEUE;
        }
        if !(2..=MAX_ENTRIES).contains(&entries) {
            return INVALID_QUEUE_SIZE;
        }
        // A queue scattered over pages is one this controller cannot read: the capability
        // says so, and a driver that asked anyway is asking for something not offered.
        if request.words[1] & CONTIGUOUS == 0 || vector as usize >= QUEUES {
            return INVALID_FIELD;
        }
        self.completions[queue] = Some(Completion {
            base: request.prp1,
            entries,
            head: 0,
            tail: 0,
            phase: true,
            vector,
            interrupts: request.words[1] & INTERRUPTS != 0,
        });
        SUCCESS
    }

    fn create_submission(&mut self, request: Request) -> Status {
        const CONTIGUOUS: u32 = 1 << 0;

        let queue = (request.words[0] & 0xffff) as usize;
        let entries = (request.words[0] >> 16) + 1;
        let completion = (request.words[1] >> 16) as usize;
        if queue == ADMIN || queue >= QUEUES || self.submissions[queue].is_some() {
            return INVALID_QUEUE;
        }
        if !(2..=MAX_ENTRIES).contains(&entries) {
            return INVALID_QUEUE_SIZE;
        }
        // The queue its answers go on has to be one that exists, or there would be
        // nowhere to put them.
        if completion >= QUEUES || self.completions[completion].is_none() {
            return INVALID_COMPLETION_QUEUE;
        }
        if request.words[1] & CONTIGUOUS == 0 {
            return INVALID_FIELD;
        }
        self.submissions[queue] = Some(Submission {
            base: request.prp1,
            entries,
            head: 0,
            completion,
        });
        SUCCESS
    }

    fn delete(&mut self, request: Request, completion: bool) -> Status {
        let queue = (request.words[0] & 0xffff) as usize;
        if queue == ADMIN || queue >= QUEUES {
            return INVALID_QUEUE;
        }
        match completion {
            // A completion queue with a submission queue still pointing at it is one
            // that still has answers to carry.
            true => {
                if self
                    .submissions
                    .iter()
                    .flatten()
                    .any(|sq| sq.completion == queue)
                {
                    return INVALID_QUEUE;
                }
                if self.completions[queue].take().is_none() {
                    return INVALID_QUEUE;
                }
            }
            false => {
                if self.submissions[queue].take().is_none() {
                    return INVALID_QUEUE;
                }
            }
        }
        self.signal();
        SUCCESS
    }

    /// What the controller and its one namespace are.
    fn identify(&mut self, request: Request) -> Status {
        let structure = match request.words[0] & 0xff {
            CNS_CONTROLLER => self.controller(),
            CNS_NAMESPACE => match request.namespace {
                NAMESPACE => self.namespace(),
                _ => return INVALID_NAMESPACE,
            },
            CNS_NAMESPACE_INDEPENDENT => match request.namespace {
                NAMESPACE => independent(),
                _ => return INVALID_NAMESPACE,
            },
            // Every namespace there is, by number, ending at the first zero.
            CNS_NAMESPACES => {
                let mut list = vec![0u8; IDENTIFY_SIZE];
                list[..4].copy_from_slice(&NAMESPACE.to_le_bytes());
                list
            }
            // The names a namespace has besides its number, of which this one has none:
            // what is behind it is whichever file the machine was started with, and
            // nothing about that is a name the namespace carries. An empty list is what
            // says so, and a driver reads until the first descriptor of no length.
            CNS_DESCRIPTORS => vec![0u8; IDENTIFY_SIZE],
            _ => return INVALID_FIELD,
        };
        self.put(&request, &structure)
    }

    /// The four kibibytes that say what the controller is.
    fn controller(&self) -> Vec<u8> {
        let mut id = vec![0u8; IDENTIFY_SIZE];
        let mut put = |at: usize, bytes: &[u8]| id[at..at + bytes.len()].copy_from_slice(bytes);
        put(0, &VENDOR.to_le_bytes());
        put(2, &VENDOR.to_le_bytes());
        // A serial number, a model and a firmware revision, each padded with spaces
        // rather than with zeroes, which is what the specification asks of a string.
        put(4, &field::<20>("rysk0"));
        put(24, &field::<40>("rysk nvme"));
        put(64, &field::<8>("1"));
        // The largest transfer, as the power of two pages it is.
        put(77, &[MAX_TRANSFER_PAGES]);
        put(78, &1u16.to_le_bytes());
        put(80, &VERSION.to_le_bytes());
        // An I/O controller rather than one that only administers.
        put(111, &[1]);
        // How long a queue entry is, as the powers of two that are the least and the
        // most this controller takes: both are fixed, so both halves are the same.
        put(512, &[0x66, 0x44]);
        // How many commands may be outstanding at once, and how many namespaces there
        // are.
        put(514, &(MAX_ENTRIES as u16).to_le_bytes());
        put(516, &NAMESPACE.to_le_bytes());
        // Which optional commands it has: writing zeroes without sending them, which is
        // what makes discarding a block cheap.
        put(520, &(1u16 << 3).to_le_bytes());
        // And that a write is on the disk when it is answered, so a flush has nothing
        // to do and there is no volatile cache to describe.
        put(525, &[0]);
        id
    }

    /// And the four kibibytes that say what the one namespace is.
    fn namespace(&self) -> Vec<u8> {
        let mut id = vec![0u8; IDENTIFY_SIZE];
        let blocks = self.disk.blocks();
        let mut put = |at: usize, bytes: &[u8]| id[at..at + bytes.len()].copy_from_slice(bytes);
        // How many blocks there are, how many are available, and how many are used.
        // All three are the same: nothing here is thin.
        put(0, &blocks.to_le_bytes());
        put(8, &blocks.to_le_bytes());
        put(16, &blocks.to_le_bytes());
        // One block format, which is format zero, and no metadata anywhere.
        put(25, &[0]);
        put(26, &[0]);
        // What a block is, as the power of two it is, in the first format.
        put(128, &[0, 0, BLOCK.trailing_zeros() as u8, 0]);
        id
    }

    // ---------------------------------------------------------------- reading and writing

    /// A command from the set that reads and writes blocks.
    fn storage(&mut self, request: Request) -> Status {
        if request.namespace != NAMESPACE {
            return INVALID_NAMESPACE;
        }
        match request.opcode {
            // Every write is on the disk before it is answered, so there is nothing
            // waiting for a flush to push.
            FLUSH => {
                self.disk.flush();
                SUCCESS
            }
            READ | WRITE | WRITE_ZEROES => self.blocks(request),
            _ => INVALID_OPCODE,
        }
    }

    fn blocks(&mut self, request: Request) -> Status {
        let block = ((request.words[1] as u64) << 32) | request.words[0] as u64;
        let count = (request.words[2] & 0xffff) as u64 + 1;
        let Some(end) = block.checked_add(count) else {
            return LBA_OUT_OF_RANGE;
        };
        if end > self.disk.blocks() {
            return LBA_OUT_OF_RANGE;
        }
        let len = (count * BLOCK) as usize;
        if count > (1 << MAX_TRANSFER_PAGES) * self.page() / BLOCK {
            return INVALID_FIELD;
        }
        match request.opcode {
            READ => {
                let mut bytes = vec![0u8; len];
                if !self.disk.read(block, &mut bytes) {
                    return DATA_TRANSFER_ERROR;
                }
                self.put(&request, &bytes)
            }
            WRITE => {
                let Some(bytes) = self.take(&request, len) else {
                    return DATA_TRANSFER_ERROR;
                };
                match self.disk.write(block, &bytes) {
                    true => SUCCESS,
                    false => DATA_TRANSFER_ERROR,
                }
            }
            // Zeroes the guest did not have to send, which is the whole point of the
            // command: a filesystem discarding a megabyte sends one command.
            _ => match self.disk.write(block, &vec![0u8; len]) {
                true => SUCCESS,
                false => DATA_TRANSFER_ERROR,
            },
        }
    }

    /// Where the bytes of a transfer are, as the runs of memory the command describes.
    ///
    /// The first pointer starts anywhere and runs to the end of its page. What follows
    /// is a second pointer if one page is enough for the rest, and otherwise a list of
    /// pages whose own last entry is the next list when one list is not enough.
    /// NVM Express Base Specification 2.0, 4.1.1.
    fn regions(&self, request: &Request, len: usize) -> Option<Vec<(u64, usize)>> {
        let page = self.page();
        let mut regions = Vec::new();
        let mut left = len as u64;
        let first = (page - (request.prp1 & (page - 1))).min(left);
        regions.push((request.prp1, first as usize));
        left -= first;
        if left == 0 {
            return Some(regions);
        }
        if left <= page {
            regions.push((request.prp2, left as usize));
            return Some(regions);
        }
        let (mut list, mut index) = (request.prp2, 0);
        let entries = page / 8;
        while left > 0 {
            let entry = self.dma.load(list + index * 8, 64)?;
            // The last entry of a list that does not reach the end of the transfer is
            // where the list carries on rather than a page of it.
            if index == entries - 1 && left > page {
                list = entry;
                index = 0;
                continue;
            }
            let take = left.min(page);
            regions.push((entry, take as usize));
            left -= take;
            index += 1;
        }
        Some(regions)
    }

    /// Put `bytes` where the command said to, and say how it went.
    fn put(&mut self, request: &Request, bytes: &[u8]) -> Status {
        let Some(regions) = self.regions(request, bytes.len()) else {
            return DATA_TRANSFER_ERROR;
        };
        let mut left = bytes;
        for (at, len) in regions {
            let (head, tail) = left.split_at(len.min(left.len()));
            if !self.dma.write(at, head) {
                return DATA_TRANSFER_ERROR;
            }
            left = tail;
        }
        SUCCESS
    }

    /// And take `len` bytes from where it said they are.
    fn take(&mut self, request: &Request, len: usize) -> Option<Vec<u8>> {
        let regions = self.regions(request, len)?;
        let mut bytes = Vec::with_capacity(len);
        for (at, run) in regions {
            let mut chunk = vec![0u8; run];
            if !self.dma.read(at, &mut chunk) {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        bytes.truncate(len);
        Some(bytes)
    }
}

/// The registers whose second half is a register of its own, since every access here is
/// thirty-two bits wide and the addresses these hold are sixty-four.
const CAP_HIGH: u64 = CAP + 4;
const ASQ_HIGH: u64 = ASQ + 4;
const ACQ_HIGH: u64 = ACQ + 4;

/// A fixed-width string field, which is padded with spaces rather than ended by a zero.
fn field<const N: usize>(text: &str) -> [u8; N] {
    let mut bytes = [b' '; N];
    let len = text.len().min(N);
    bytes[..len].copy_from_slice(&text.as_bytes()[..len]);
    bytes
}

/// What a namespace is, said without reference to which command set it belongs to,
/// which is what a driver asks before it knows what kind of namespace this is.
fn independent() -> Vec<u8> {
    let mut id = vec![0u8; IDENTIFY_SIZE];
    // Ready: there is nothing for a namespace backed by a file to prepare.
    id[14] = 1;
    id
}

impl Function for Nvme {
    fn describe(&self) -> Vec<Field> {
        let live = |queues: usize| Value::Count(queues as u64);
        vec![
            says("blocks", Value::Count(self.disk.blocks())),
            says("cc", Value::Bits(u64::from(self.cc))),
            says("csts", Value::Bits(u64::from(self.csts))),
            says("admin queue attributes", Value::Bits(u64::from(self.aqa))),
            says("admin submission queue", Value::Bits(self.asq)),
            says("admin completion queue", Value::Bits(self.acq)),
            says(
                "submission queues",
                live(self.submissions.iter().flatten().count()),
            ),
            says(
                "completion queues",
                live(self.completions.iter().flatten().count()),
            ),
            says("messages masked", Value::Bits(u64::from(self.masked))),
        ]
    }

    fn header(&self) -> Header {
        Header {
            vendor: VENDOR,
            device: DEVICE,
            class: CLASS,
            subsystem_vendor: VENDOR,
            subsystem: DEVICE,
            bars: [
                Bar::Memory {
                    size: WINDOW,
                    prefetchable: false,
                    wide: false,
                },
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
            ],
            pin: 1,
            msix: Some(MsiX {
                vectors: QUEUES,
                table: (0, MSIX_TABLE),
                pending: (0, MSIX_PENDING),
            }),
            express: true,
        }
    }

    fn load(&mut self, bar: usize, offset: u64, size: u64) -> Result<u64, Exception> {
        if bar != 0 {
            return Err(Exception::LoadAccessFault(offset));
        }
        // A doorbell is not storage: what it does is done when it is written, and
        // reading one back says nothing was left waiting.
        let word = match offset < DOORBELLS {
            true => self.read(offset & !3),
            false => 0,
        };
        let shift = (offset & 3) * 8;
        Ok(((word as u64) >> shift) & (u64::MAX >> (64 - size.min(32))))
    }

    fn store(&mut self, bar: usize, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        if bar != 0 {
            return Err(Exception::StoreAmoAccessFault(offset));
        }
        let register = offset & !3;
        let shift = (offset & 3) * 8;
        let mask = ((u64::MAX >> (64 - size.min(32))) << shift) as u32;
        if offset >= DOORBELLS {
            let (queue, completion) = Self::doorbell(register - DOORBELLS);
            self.ring(queue, completion, (value << shift) as u32 & mask);
            return Ok(());
        }
        // A write narrower than a register keeps the rest of it, which every one of
        // these is defined as: there is no register here a byte is the whole of.
        let word = (self.read(register) & !mask) | ((value << shift) as u32 & mask);
        self.write(register, word);
        Ok(())
    }

    fn asserted(&mut self, messaging: bool) -> Asserted {
        let mut taken = self.asserted;
        self.asserted.messages = 0;
        // A function sending messages does not pull its wire.
        if messaging {
            taken.pin = false;
        }
        taken
    }
}
