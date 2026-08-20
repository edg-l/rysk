//! The NVMe controller: its registers, the queues a driver builds for it, and the
//! blocks that go in and out of them.
//!
//! Driven the way a driver drives it rather than the way a guest instruction does, for
//! the same reason the xHCI chapter is: the registers are a dozen words and everything
//! else is queues in memory.

use std::sync::Arc;

use rysk::{
    bus::DRAM_BASE,
    device::{Device, Dma, Msi, Wires},
    disk::{BLOCK, Memory},
    dram::Dram,
    nvme::Nvme,
    pci::{Function, Root},
};

/// Where in guest memory this test puts each queue and the buffers it transfers through.
const ASQ: u64 = DRAM_BASE + 0x1_0000;
const ACQ: u64 = DRAM_BASE + 0x2_0000;
const IOSQ: u64 = DRAM_BASE + 0x3_0000;
const IOCQ: u64 = DRAM_BASE + 0x4_0000;
const BUFFER: u64 = DRAM_BASE + 0x5_0000;
const LIST: u64 = DRAM_BASE + 0x6_0000;

/// How long each queue is here. Short enough that a test can fill one.
const ENTRIES: u32 = 8;

/// How large a page the controller is told to read addresses as, which is the smallest
/// it takes and the one a guest uses.
const PAGE: u64 = 4096;

/// How many blocks the disk behind it has.
const BLOCKS: u64 = 64;

/// The registers.
const CAP: u64 = 0x00;
const VS: u64 = 0x08;
const INTMS: u64 = 0x0c;
const INTMC: u64 = 0x10;
const CC: u64 = 0x14;
const CSTS: u64 = 0x1c;
const AQA: u64 = 0x24;
const ASQ_REG: u64 = 0x28;
const ACQ_REG: u64 = 0x30;
const DOORBELLS: u64 = 0x1000;

/// Bits of them.
const CC_ENABLE: u32 = 1 << 0;
const CC_SHUTDOWN: u32 = 1 << 14;
const CSTS_READY: u32 = 1 << 0;
const CSTS_FATAL: u32 = 1 << 1;
const CSTS_SHUTDOWN_DONE: u32 = 0x2 << 2;

/// What a driver writes into the configuration register to turn the controller on:
/// four kibibyte pages, the command set that reads blocks, and the two entry sizes,
/// each as the power of two it is.
const CONFIGURATION: u32 = CC_ENABLE | (6 << 16) | (4 << 20);

/// The commands.
const DELETE_SQ: u8 = 0x00;
const CREATE_SQ: u8 = 0x01;
const GET_LOG_PAGE: u8 = 0x02;
const DELETE_CQ: u8 = 0x04;
const CREATE_CQ: u8 = 0x05;
const IDENTIFY: u8 = 0x06;
const SET_FEATURES: u8 = 0x09;
const GET_FEATURES: u8 = 0x0a;
const ASYNC_EVENT: u8 = 0x0c;
const FLUSH: u8 = 0x00;
const WRITE: u8 = 0x01;
const READ: u8 = 0x02;
const WRITE_ZEROES: u8 = 0x08;

/// The status codes a test looks for, as the field a completion carries them in.
const SUCCESS: u16 = 0;
const INVALID_OPCODE: u16 = 0x01;
const INVALID_FIELD: u16 = 0x02;
const INVALID_NAMESPACE: u16 = 0x0b;
const LBA_OUT_OF_RANGE: u16 = 0x80;
/// And the ones from the set that means something only to the command that answered.
const INVALID_QUEUE: u16 = 0x101;
const INVALID_QUEUE_SIZE: u16 = 0x102;
const INVALID_COMPLETION_QUEUE: u16 = 0x100;

/// The admin pair, and the one queue pair the tests below make.
const ADMIN: usize = 0;
const IO: usize = 1;

/// One answer, as the fields of it a test reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Answer {
    result: u32,
    head: u16,
    queue: u16,
    id: u16,
    status: u16,
}

/// One command, as the sixty-four bytes it is.
fn command(opcode: u8, id: u16, namespace: u32, data: (u64, u64), words: [u32; 6]) -> [u8; 64] {
    let mut bytes = [0u8; 64];
    bytes[0] = opcode;
    bytes[2..4].copy_from_slice(&id.to_le_bytes());
    bytes[4..8].copy_from_slice(&namespace.to_le_bytes());
    bytes[24..32].copy_from_slice(&data.0.to_le_bytes());
    bytes[32..40].copy_from_slice(&data.1.to_le_bytes());
    for (n, word) in words.into_iter().enumerate() {
        bytes[40 + n * 4..44 + n * 4].copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// A driver, and the controller it drives.
struct Host {
    memory: Arc<Dram>,
    nvme: Nvme,
    /// Where this driver has put each queue, and how far round each end of it has got.
    queues: [(u64, u64); 2],
    tail: [u32; 2],
    head: [u32; 2],
    phase: [bool; 2],
}

/// A controller with a disk of `BLOCKS` blocks behind it, before anything has been
/// written to it.
fn host() -> Host {
    let memory = Arc::new(Dram::with_size(Vec::new(), 1024 * 1024));
    let nvme = Nvme::new(Dma::new(memory.clone()), Box::new(Memory::new(BLOCKS)));
    Host {
        memory,
        nvme,
        queues: [(ASQ, ACQ), (IOSQ, IOCQ)],
        tail: [0; 2],
        head: [0; 2],
        phase: [true; 2],
    }
}

impl Host {
    fn read(&mut self, register: u64) -> u32 {
        self.nvme.load(0, register, 32).expect("a register") as u32
    }

    fn write(&mut self, register: u64, value: u32) {
        self.nvme
            .store(0, register, 32, value as u64)
            .expect("a register");
    }

    fn write64(&mut self, register: u64, value: u64) {
        self.write(register, value as u32);
        self.write(register + 4, (value >> 32) as u32);
    }

    fn poke(&self, at: u64, bytes: &[u8]) {
        assert!(self.memory.write(at, bytes, 0), "{at:#x} is not memory");
    }

    fn peek(&self, at: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        assert!(
            Dma::new(self.memory.clone()).read(at, &mut bytes),
            "{at:#x} is not memory"
        );
        bytes
    }

    /// Turn the controller on with the admin pair the registers describe.
    fn start(&mut self) {
        self.write(AQA, (ENTRIES - 1) | ((ENTRIES - 1) << 16));
        self.write64(ASQ_REG, ASQ);
        self.write64(ACQ_REG, ACQ);
        self.write(CC, CONFIGURATION);
        assert_ne!(self.read(CSTS) & CSTS_READY, 0, "the controller is ready");
    }

    /// Put a command on a queue and ring for it, and answer what came back.
    fn submit(&mut self, queue: usize, command: [u8; 64]) -> Vec<Answer> {
        let at = self.queues[queue].0 + self.tail[queue] as u64 * 64;
        self.poke(at, &command);
        self.tail[queue] = (self.tail[queue] + 1) % ENTRIES;
        let tail = self.tail[queue];
        self.write(DOORBELLS + queue as u64 * 8, tail);
        self.answers(queue)
    }

    /// Everything the controller has put on a completion queue since this was last
    /// asked, telling it afterwards how far this driver got.
    fn answers(&mut self, queue: usize) -> Vec<Answer> {
        let mut answers = Vec::new();
        loop {
            let at = self.queues[queue].1 + self.head[queue] as u64 * 16;
            let bytes = self.peek(at, 16);
            let status = u16::from_le_bytes(bytes[14..16].try_into().unwrap());
            if (status & 1 != 0) != self.phase[queue] {
                break;
            }
            answers.push(Answer {
                result: u32::from_le_bytes(bytes[..4].try_into().unwrap()),
                head: u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
                queue: u16::from_le_bytes(bytes[10..12].try_into().unwrap()),
                id: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
                status: status >> 1,
            });
            self.head[queue] = (self.head[queue] + 1) % ENTRIES;
            if self.head[queue] == 0 {
                self.phase[queue] = !self.phase[queue];
            }
            let head = self.head[queue];
            self.write(DOORBELLS + queue as u64 * 8 + 4, head);
        }
        answers
    }

    /// The one answer a command produced, which is what every command here but one
    /// produces.
    fn one(&mut self, queue: usize, command: [u8; 64]) -> Answer {
        let answers = self.submit(queue, command);
        assert_eq!(answers.len(), 1, "one command, one answer: {answers:?}");
        answers[0]
    }

    /// Make the one queue pair the tests use, which is the two commands a driver sends
    /// after it has identified the controller.
    fn make_queues(&mut self) {
        /// Its memory is one run, and an answer on it sends a message.
        const CONTIGUOUS: u32 = 1;
        const INTERRUPTS: u32 = 2;

        let queue = IO as u32 | ((ENTRIES - 1) << 16);
        let completion = self.one(
            ADMIN,
            command(
                CREATE_CQ,
                1,
                0,
                (IOCQ, 0),
                [
                    queue,
                    CONTIGUOUS | INTERRUPTS | ((IO as u32) << 16),
                    0,
                    0,
                    0,
                    0,
                ],
            ),
        );
        assert_eq!(completion.status, SUCCESS, "making a completion queue");
        let submission = self.one(
            ADMIN,
            command(
                CREATE_SQ,
                2,
                0,
                (IOSQ, 0),
                [queue, CONTIGUOUS | ((IO as u32) << 16), 0, 0, 0, 0],
            ),
        );
        assert_eq!(submission.status, SUCCESS, "and a submission queue");
    }

    /// What identify answers with, for one of the structures it has.
    fn identify(&mut self, structure: u32, namespace: u32) -> (Answer, Vec<u8>) {
        let answer = self.one(
            ADMIN,
            command(
                IDENTIFY,
                7,
                namespace,
                (BUFFER, 0),
                [structure, 0, 0, 0, 0, 0],
            ),
        );
        (answer, self.peek(BUFFER, 4096))
    }
}

// -------------------------------------------------------------------- what is on the bus

#[test]
fn enumeration_finds_a_non_volatile_memory_controller() {
    // No machine here: config space is driven directly, so these wires are counted by
    // nothing and nothing is asking.
    let wires = Wires::default();
    let root = Root::new(std::array::from_fn(|_| wires.line()), Msi::default());
    let memory = Arc::new(Dram::with_size(Vec::new(), 1024));
    root.plug(
        3,
        Box::new(Nvme::new(Dma::new(memory), Box::new(Memory::new(1)))),
    );
    let mut config = root.config();

    let class = config.load((3 << 15) + 0x08, 32).expect("config space");

    assert_eq!(
        class, 0x0108_0202,
        "mass storage, non-volatile memory, the NVMe interface, revision two"
    );
}

// --------------------------------------------------------------------------- registers

#[test]
fn the_capability_register_says_what_the_controller_takes() {
    let mut host = host();
    let low = host.read(CAP);
    let high = host.read(CAP + 4);

    assert_eq!(low & 0xffff, 1023, "a thousand and twenty-four entries");
    assert_eq!((low >> 16) & 1, 1, "each queue one run of memory");
    assert_ne!(
        (low >> 24) & 0xff,
        0,
        "and a time to wait for it to be ready"
    );
    assert_eq!(high & 0xf, 0, "doorbells four bytes apart");
    assert_eq!((high >> 5) & 1, 1, "the command set that reads blocks");
    assert_eq!((high >> 16) & 0xf, 0, "pages of four kibibytes at least");
}

#[test]
fn the_version_is_the_one_a_driver_reads_to_know_what_to_ask_for() {
    let mut host = host();
    assert_eq!(host.read(VS), 0x0001_0400, "1.4.0");
}

#[test]
fn a_controller_that_has_not_been_started_is_not_ready() {
    let mut host = host();
    assert_eq!(host.read(CSTS), 0);
}

#[test]
fn starting_it_makes_it_ready_and_stopping_it_forgets_the_queues() {
    let mut host = host();
    host.start();
    host.make_queues();

    host.write(CC, 0);
    assert_eq!(host.read(CSTS) & CSTS_READY, 0, "not ready any more");

    // The registers stay: they are how the controller is told where the admin pair is,
    // and a driver writes them before it turns the controller on rather than after.
    // What a driver does write again is the queue itself, since the answers still in it
    // carry the phase the controller is about to start writing.
    host.poke(ACQ, &vec![0u8; ENTRIES as usize * 16]);
    host.write(CC, CONFIGURATION);
    host.tail = [0; 2];
    host.head = [0; 2];
    host.phase = [true; 2];
    let answer = host.one(
        ADMIN,
        command(DELETE_SQ, 1, 0, (0, 0), [IO as u32, 0, 0, 0, 0, 0]),
    );

    assert_eq!(
        answer.status, INVALID_QUEUE,
        "and the queues it had are gone"
    );
}

/// A controller told to become ready with nowhere to put its admin queues cannot, and
/// says so in the one way a driver looks for.
#[test]
fn starting_it_with_no_admin_queue_is_a_fatal_error() {
    let mut host = host();
    host.write(AQA, (ENTRIES - 1) | ((ENTRIES - 1) << 16));
    host.write(CC, CONFIGURATION);

    assert_eq!(host.read(CSTS) & CSTS_READY, 0);
    assert_ne!(host.read(CSTS) & CSTS_FATAL, 0);
}

#[test]
fn shutting_it_down_finishes_at_once() {
    let mut host = host();
    host.start();
    host.write(CC, CONFIGURATION | CC_SHUTDOWN);

    assert_eq!(host.read(CSTS) & CSTS_SHUTDOWN_DONE, CSTS_SHUTDOWN_DONE);
}

/// Where the admin queues are is only software's to say while the controller is off:
/// moving them under a running controller would take its queues away mid-command.
#[test]
fn the_admin_queue_registers_are_not_writable_while_it_runs() {
    let mut host = host();
    host.start();

    host.write64(ASQ_REG, DRAM_BASE + 0x9_0000);

    assert_eq!(host.read(ASQ_REG), ASQ as u32);
}

/// A doorbell is not storage, and one rung before the controller is ready does nothing,
/// which is what stops a driver's leftovers running when it starts.
#[test]
fn a_doorbell_rung_before_the_controller_is_ready_does_nothing() {
    let mut host = host();
    host.write(AQA, (ENTRIES - 1) | ((ENTRIES - 1) << 16));
    host.write64(ASQ_REG, ASQ);
    host.write64(ACQ_REG, ACQ);
    host.poke(
        ASQ,
        &command(IDENTIFY, 1, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
    );

    host.write(DOORBELLS, 1);
    assert_eq!(host.read(DOORBELLS), 0, "and it reads back as nothing");

    host.write(CC, CONFIGURATION);
    assert!(host.answers(ADMIN).is_empty(), "nothing ran");
}

// --------------------------------------------------------------------------- identify

#[test]
fn identify_says_what_the_controller_is() {
    let mut host = host();
    host.start();
    let (answer, id) = host.identify(1, 0);

    assert_eq!(answer.status, SUCCESS);
    assert_eq!(&id[24..33], b"rysk nvme", "the model, padded with spaces");
    assert_eq!(id[33], b' ');
    assert_eq!(u32::from_le_bytes(id[80..84].try_into().unwrap()), 0x10400);
    assert_eq!(id[512], 0x66, "a command of sixty-four bytes");
    assert_eq!(id[513], 0x44, "and an answer of sixteen");
    assert_eq!(u32::from_le_bytes(id[516..520].try_into().unwrap()), 1);
    assert_ne!(id[77], 0, "and a largest transfer it will take");
}

#[test]
fn identify_says_how_many_blocks_the_namespace_has_and_how_big_they_are() {
    let mut host = host();
    host.start();
    let (answer, id) = host.identify(0, 1);

    assert_eq!(answer.status, SUCCESS);
    assert_eq!(u64::from_le_bytes(id[..8].try_into().unwrap()), BLOCKS);
    assert_eq!(
        id[25], 0,
        "one block format, counted one less than there is"
    );
    assert_eq!(id[128 + 2], BLOCK.trailing_zeros() as u8, "of 512 bytes");
}

#[test]
fn identify_lists_the_namespaces_there_are() {
    let mut host = host();
    host.start();
    let (answer, list) = host.identify(2, 0);

    assert_eq!(answer.status, SUCCESS);
    assert_eq!(u32::from_le_bytes(list[..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(list[4..8].try_into().unwrap()),
        0,
        "and the list ends there"
    );
}

/// What a namespace is, said before the driver knows which command set it belongs to.
#[test]
fn identify_says_what_a_namespace_is_without_naming_a_command_set() {
    let mut host = host();
    host.start();
    let (answer, id) = host.identify(8, 1);

    assert_eq!(answer.status, SUCCESS);
    assert_eq!(id[14] & 1, 1, "ready, since a file has nothing to prepare");
}

#[test]
fn identify_of_a_structure_the_controller_does_not_have_is_refused() {
    let mut host = host();
    host.start();
    assert_eq!(host.identify(0x1f, 0).0.status, INVALID_FIELD);
}

#[test]
fn identify_of_a_namespace_that_is_not_there_is_refused() {
    let mut host = host();
    host.start();
    assert_eq!(host.identify(0, 7).0.status, INVALID_NAMESPACE);
}

// ----------------------------------------------------------------------------- queues

#[test]
fn a_queue_pair_can_be_made_and_taken_away_again() {
    let mut host = host();
    host.start();
    host.make_queues();

    // A completion queue something still answers on cannot go.
    let held = host.one(
        ADMIN,
        command(DELETE_CQ, 3, 0, (0, 0), [IO as u32, 0, 0, 0, 0, 0]),
    );
    assert_eq!(held.status, INVALID_QUEUE);

    let submission = host.one(
        ADMIN,
        command(DELETE_SQ, 4, 0, (0, 0), [IO as u32, 0, 0, 0, 0, 0]),
    );
    let completion = host.one(
        ADMIN,
        command(DELETE_CQ, 5, 0, (0, 0), [IO as u32, 0, 0, 0, 0, 0]),
    );

    assert_eq!(submission.status, SUCCESS);
    assert_eq!(completion.status, SUCCESS, "and then it can");
}

#[test]
fn a_queue_the_controller_cannot_make_is_refused_by_name() {
    let mut host = host();
    host.start();
    let make = |host: &mut Host, queue: u32, entries: u32, flags: u32| {
        host.one(
            ADMIN,
            command(
                CREATE_CQ,
                1,
                0,
                (IOCQ, 0),
                [queue | (entries << 16), flags, 0, 0, 0, 0],
            ),
        )
        .status
    };

    assert_eq!(make(&mut host, 0, 7, 1), INVALID_QUEUE, "the admin queue");
    assert_eq!(make(&mut host, 99, 7, 1), INVALID_QUEUE, "one too far out");
    assert_eq!(make(&mut host, 1, 0, 1), INVALID_QUEUE_SIZE, "one entry");
    assert_eq!(
        make(&mut host, 1, 7, 0),
        INVALID_FIELD,
        "and one scattered over pages, which this controller cannot read"
    );
}

#[test]
fn a_submission_queue_needs_somewhere_to_put_its_answers() {
    let mut host = host();
    host.start();

    let answer = host.one(
        ADMIN,
        command(
            CREATE_SQ,
            1,
            0,
            (IOSQ, 0),
            [IO as u32 | ((ENTRIES - 1) << 16), 1 | (5 << 16), 0, 0, 0, 0],
        ),
    );

    assert_eq!(answer.status, INVALID_COMPLETION_QUEUE);
}

/// How many queue pairs software may have, which is the one feature this controller has.
#[test]
fn asking_how_many_queues_there_are_says_how_many_there_may_be() {
    let mut host = host();
    host.start();

    let asked = host.one(
        ADMIN,
        command(SET_FEATURES, 1, 0, (0, 0), [7, 0xffff_ffff, 0, 0, 0, 0]),
    );
    let read = host.one(
        ADMIN,
        command(GET_FEATURES, 2, 0, (0, 0), [7, 0, 0, 0, 0, 0]),
    );

    assert_eq!(asked.status, SUCCESS);
    assert_ne!(
        asked.result, 0,
        "some queues, counted one less than there are"
    );
    assert_eq!(
        read.result, asked.result,
        "and reading it back says the same"
    );
}

#[test]
fn a_feature_the_controller_does_not_have_is_refused() {
    let mut host = host();
    host.start();
    let answer = host.one(
        ADMIN,
        command(SET_FEATURES, 1, 0, (0, 0), [0x0c, 0, 0, 0, 0, 0]),
    );

    assert_eq!(answer.status, INVALID_FIELD);
}

#[test]
fn a_command_the_controller_does_not_have_is_refused_rather_than_ignored() {
    let mut host = host();
    host.start();
    assert_eq!(
        host.one(ADMIN, command(0x7f, 1, 0, (0, 0), [0; 6])).status,
        INVALID_OPCODE
    );
}

/// Every log page is empty: there are no errors, no temperature and no firmware slots.
#[test]
fn a_log_page_is_as_many_zeroes_as_were_asked_for() {
    let mut host = host();
    host.start();
    host.poke(BUFFER, &[0xff; 64]);

    let answer = host.one(
        ADMIN,
        command(
            GET_LOG_PAGE,
            1,
            0,
            (BUFFER, 0),
            [2 | (15 << 16), 0, 0, 0, 0, 0],
        ),
    );

    assert_eq!(answer.status, SUCCESS);
    assert_eq!(host.peek(BUFFER, 64), [0; 64], "sixteen words of nothing");
}

/// A command to be answered when something happens, on a controller where nothing ever
/// does: it is taken and never answered, which is what a driver expects.
#[test]
fn a_request_to_be_told_of_an_event_is_never_answered() {
    let mut host = host();
    host.start();

    let answers = host.submit(ADMIN, command(ASYNC_EVENT, 1, 0, (0, 0), [0; 6]));
    let after = host.one(
        ADMIN,
        command(IDENTIFY, 2, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
    );

    assert!(answers.is_empty(), "nothing came back");
    assert_eq!(after.status, SUCCESS, "and the queue carried on");
    assert_eq!(after.head, 2, "with both commands read");
}

// ------------------------------------------------------------------ reading and writing

#[test]
fn a_block_written_reads_back() {
    let mut host = host();
    host.start();
    host.make_queues();
    host.poke(BUFFER, &[0xa5; BLOCK as usize]);

    let written = host.one(IO, command(WRITE, 1, 1, (BUFFER, 0), [3, 0, 0, 0, 0, 0]));
    host.poke(BUFFER, &[0; BLOCK as usize]);
    let read = host.one(IO, command(READ, 2, 1, (BUFFER, 0), [3, 0, 0, 0, 0, 0]));

    assert_eq!(written.status, SUCCESS);
    assert_eq!(read.status, SUCCESS);
    assert_eq!(host.peek(BUFFER, BLOCK as usize), [0xa5; BLOCK as usize]);
}

/// The second pointer is where the rest of a transfer is when one page holds it, which
/// is what an eight-block read of a buffer that starts part way into a page needs.
#[test]
fn a_transfer_that_runs_past_a_page_carries_on_at_the_second_pointer() {
    let mut host = host();
    host.start();
    host.make_queues();
    let (first, second) = (BUFFER + PAGE - BLOCK, BUFFER + 2 * PAGE);
    host.poke(first, &[1; BLOCK as usize]);
    host.poke(second, &[2; BLOCK as usize]);

    // Two blocks: one fills the rest of the first page, the other is at the second.
    let written = host.one(
        IO,
        command(WRITE, 1, 1, (first, second), [0, 0, 1, 0, 0, 0]),
    );
    host.poke(first, &[0; BLOCK as usize]);
    host.poke(second, &[0; BLOCK as usize]);
    let read = host.one(IO, command(READ, 2, 1, (first, second), [0, 0, 1, 0, 0, 0]));

    assert_eq!(written.status, SUCCESS);
    assert_eq!(read.status, SUCCESS);
    assert_eq!(host.peek(first, BLOCK as usize), [1; BLOCK as usize]);
    assert_eq!(host.peek(second, BLOCK as usize), [2; BLOCK as usize]);
}

/// And a list of pages is where it is when one page does not, which is what every
/// transfer of more than two pages uses.
#[test]
fn a_transfer_of_several_pages_follows_the_list_of_them() {
    let mut host = host();
    host.start();
    host.make_queues();
    // Three pages of blocks, the first at the start of a page so the runs are whole
    // pages, and a list naming the second and third.
    let pages = [BUFFER, BUFFER + 0x2000, BUFFER + 0x4000];
    for (n, page) in pages.into_iter().enumerate() {
        host.poke(page, &vec![n as u8 + 1; PAGE as usize]);
    }
    host.poke(LIST, &pages[1].to_le_bytes());
    host.poke(LIST + 8, &pages[2].to_le_bytes());

    let blocks = (3 * PAGE / BLOCK) as u32;
    let written = host.one(
        IO,
        command(WRITE, 1, 1, (pages[0], LIST), [0, 0, blocks - 1, 0, 0, 0]),
    );
    for page in pages {
        host.poke(page, &vec![0; PAGE as usize]);
    }
    let read = host.one(
        IO,
        command(READ, 2, 1, (pages[0], LIST), [0, 0, blocks - 1, 0, 0, 0]),
    );

    assert_eq!(written.status, SUCCESS);
    assert_eq!(read.status, SUCCESS);
    for (n, page) in pages.into_iter().enumerate() {
        assert_eq!(
            host.peek(page, PAGE as usize),
            vec![n as u8 + 1; PAGE as usize],
            "page {n}"
        );
    }
}

#[test]
fn writing_zeroes_sends_none_of_them() {
    let mut host = host();
    host.start();
    host.make_queues();
    host.poke(BUFFER, &[0xff; 2 * BLOCK as usize]);
    host.one(IO, command(WRITE, 1, 1, (BUFFER, 0), [0, 0, 1, 0, 0, 0]));

    // No buffer at all: the command carries what to write in its name.
    let zeroed = host.one(IO, command(WRITE_ZEROES, 2, 1, (0, 0), [0, 0, 1, 0, 0, 0]));
    let read = host.one(IO, command(READ, 3, 1, (BUFFER, 0), [0, 0, 1, 0, 0, 0]));

    assert_eq!(zeroed.status, SUCCESS);
    assert_eq!(read.status, SUCCESS);
    assert_eq!(
        host.peek(BUFFER, 2 * BLOCK as usize),
        [0; 2 * BLOCK as usize]
    );
}

/// Every write is on the disk before it is answered, so a flush has nothing to do and
/// says so rather than refusing.
#[test]
fn a_flush_succeeds_with_nothing_to_do() {
    let mut host = host();
    host.start();
    host.make_queues();
    assert_eq!(
        host.one(IO, command(FLUSH, 1, 1, (0, 0), [0; 6])).status,
        SUCCESS
    );
}

#[test]
fn a_read_past_the_end_of_the_disk_is_refused() {
    let mut host = host();
    host.start();
    host.make_queues();

    let past = host.one(
        IO,
        command(READ, 1, 1, (BUFFER, 0), [BLOCKS as u32, 0, 0, 0, 0, 0]),
    );
    let over = host.one(
        IO,
        command(READ, 2, 1, (BUFFER, 0), [BLOCKS as u32 - 1, 0, 1, 0, 0, 0]),
    );
    let wraps = host.one(IO, command(READ, 3, 1, (BUFFER, 0), [!0, !0, 0, 0, 0, 0]));

    assert_eq!(past.status, LBA_OUT_OF_RANGE, "one block past the last");
    assert_eq!(over.status, LBA_OUT_OF_RANGE, "and one that runs off it");
    assert_eq!(wraps.status, LBA_OUT_OF_RANGE, "and one that wraps");
}

#[test]
fn a_command_about_a_namespace_that_is_not_there_is_refused() {
    let mut host = host();
    host.start();
    host.make_queues();

    let answer = host.one(IO, command(READ, 1, 9, (BUFFER, 0), [0; 6]));
    assert_eq!(answer.status, INVALID_NAMESPACE);
}

#[test]
fn a_storage_command_the_controller_does_not_have_is_refused() {
    let mut host = host();
    host.start();
    host.make_queues();
    assert_eq!(
        host.one(IO, command(0x7f, 1, 1, (0, 0), [0; 6])).status,
        INVALID_OPCODE
    );
}

// ------------------------------------------------------------------------ the queues

/// Every answer says how far round its submission queue the controller has read, which
/// is how software knows there is room to put another command in.
#[test]
fn an_answer_says_how_far_the_controller_has_read() {
    let mut host = host();
    host.start();

    let first = host.one(
        ADMIN,
        command(IDENTIFY, 1, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
    );
    let second = host.one(
        ADMIN,
        command(IDENTIFY, 2, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
    );

    assert_eq!(first.head, 1);
    assert_eq!(second.head, 2);
    assert_eq!(first.queue, ADMIN as u16, "and which queue it came from");
    assert_eq!(second.id, 2, "and which command it answers");
}

/// The phase bit is the whole of how software tells an answer it has not seen from one
/// it has: the queue wraps and the bit turns over.
#[test]
fn a_completion_queue_wraps_and_turns_its_phase_bit_over() {
    let mut host = host();
    host.start();
    let phase = host.phase[ADMIN];

    for id in 0..ENTRIES as u16 + 2 {
        let answer = host.one(
            ADMIN,
            command(IDENTIFY, id, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
        );
        assert_eq!(answer.status, SUCCESS);
    }

    assert_ne!(host.phase[ADMIN], phase, "it went round");
}

/// A queue with nowhere left to put an answer is a controller that cannot go on, which
/// is what the fatal bit is for: dropping the answer would leave a command nothing ever
/// completes.
#[test]
fn a_completion_queue_nobody_is_reading_is_a_fatal_error() {
    let mut host = host();
    host.start();

    // A queue of eight entries holds seven answers, since one place has to stay empty
    // for the two ends to be told apart. The eighth is the one with nowhere to go, and
    // it takes two rings to send eight commands for the same reason.
    for entry in 0..ENTRIES {
        host.poke(
            ASQ + entry as u64 * 64,
            &command(IDENTIFY, 1, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
        );
    }
    host.write(DOORBELLS, ENTRIES - 1);
    assert_eq!(host.read(CSTS) & CSTS_FATAL, 0, "seven of them fit");

    host.write(DOORBELLS, 0);

    assert_ne!(host.read(CSTS) & CSTS_FATAL, 0, "and the eighth does not");
}

// ------------------------------------------------------------------------ the interrupt

#[test]
fn an_answer_raises_the_wire_and_taking_it_lowers_it() {
    let mut host = host();
    host.start();
    assert!(!host.nvme.asserted(false).pin, "nothing has happened yet");

    host.poke(
        ASQ,
        &command(IDENTIFY, 1, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
    );
    host.write(DOORBELLS, 1);
    assert!(host.nvme.asserted(false).pin, "an answer is waiting");

    host.answers(ADMIN);
    assert!(!host.nvme.asserted(false).pin, "and it has been taken");
}

#[test]
fn an_answer_sends_the_message_its_queue_was_given() {
    let mut host = host();
    host.start();
    host.make_queues();
    host.nvme.asserted(true);

    host.submit(IO, command(FLUSH, 1, 1, (0, 0), [0; 6]));
    let sent = host.nvme.asserted(true);

    assert_eq!(
        sent.messages,
        1 << IO,
        "the vector that queue was made with"
    );
    assert!(
        !sent.pin,
        "and a function sending messages does not pull its pin"
    );
}

/// The mask is what a driver uses to stop an interrupt while it is handling one, and it
/// means something only where the interrupt is a wire.
#[test]
fn a_masked_vector_raises_nothing() {
    let mut host = host();
    host.start();
    host.write(INTMS, 1);

    host.poke(
        ASQ,
        &command(IDENTIFY, 1, 0, (BUFFER, 0), [1, 0, 0, 0, 0, 0]),
    );
    host.write(DOORBELLS, 1);
    assert!(!host.nvme.asserted(false).pin, "the admin vector is masked");
    assert_eq!(host.read(INTMS), 1, "and reading it back says so");

    host.write(INTMC, 1);
    assert!(
        host.nvme.asserted(false).pin,
        "and unmasking it lets it out"
    );
}

/// A queue made without them says nothing when an answer lands on it, which is what a
/// driver polling one asks for.
#[test]
fn a_queue_made_without_interrupts_raises_nothing() {
    let mut host = host();
    host.start();
    let queue = IO as u32 | ((ENTRIES - 1) << 16);
    host.one(
        ADMIN,
        command(CREATE_CQ, 1, 0, (IOCQ, 0), [queue, 1, 0, 0, 0, 0]),
    );
    host.one(
        ADMIN,
        command(
            CREATE_SQ,
            2,
            0,
            (IOSQ, 0),
            [queue, 1 | ((IO as u32) << 16), 0, 0, 0, 0],
        ),
    );
    host.nvme.asserted(true);

    host.submit(IO, command(FLUSH, 1, 1, (0, 0), [0; 6]));

    assert_eq!(host.nvme.asserted(true).messages, 0);
}

/// The controller and its disk have to be able to cross a thread boundary, since the
/// harts are not on the thread a frontend is.
#[test]
fn a_controller_and_its_disk_can_be_handed_to_another_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<Nvme>();
    assert_send::<Box<dyn rysk::disk::Disk>>();
}
