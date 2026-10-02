//! **An unprivileged Kairos task tries four bad accesses on a Cortex-M33,
//! and the MPU refuses every one.** The K8 kill test: "the privileged /
//! unprivileged demo on M33 refuses a bad access".
//!
//! # The privilege boundary
//!
//! | | privileged | unprivileged |
//! |---|---|---|
//! | who | the kernel, `PendSV`, `SysTick`, the fault handlers, `IDLE`, `Tmr Svc`, the judge task | the two application tasks, `good` and `rogue` |
//! | memory | everything (`MPU_CTRL.PRIVDEFENA`) | code read-only (region 0), and its OWN stack and data (region 1) -- nothing else |
//! | the kernel | called directly, interrupts masked | through `svc #0` only ([`sys`]) |
//!
//! Region 1 is reprogrammed on every switch, in [`pick_next`], together with
//! `CONTROL.nPRIV`: whoever the kernel chooses decides what the next thread
//! may touch. That is FreeRTOS's `portRESTORE_CONTEXT` for its MPU ports,
//! cut down to the part this cell claims.
//!
//! # What `rogue` tries
//!
//! | lap | access | refused by |
//! |---|---|---|
//! | 5 | write another task's data (`good`'s lap counter) | MemManage, `DACCVIOL` |
//! | 10 | write privileged data (a canary beside the kernel) | MemManage, `DACCVIOL` |
//! | 15 | read the kernel itself | MemManage, `DACCVIOL` |
//! | 20 | write `MPU_CTRL = 0`, switching the MPU off | BusFault: the SCS is privileged-only |
//!
//! A refusal is logged, the faulting instruction is stepped over, and the
//! task carries on -- so the cell can show four refusals from one task and
//! that the system kept scheduling through them. `good` meanwhile counts to
//! [`GOOD_LAPS`] in its own memory, and must end with exactly that count.
//!
//! # Poisoned
//!
//! `--features poison` leaves the MPU off and changes nothing else. The
//! writes then land: `good`'s count is overwritten and the canary changes,
//! and the cell must FAIL.
//!
//! # NOT claimed
//!
//! The kernel-call shim exposes two calls, not FreeRTOS's ~80 MPU wrappers;
//! there is no `PSPLIM` stack guard, no TrustZone split (everything runs
//! Secure), and no access-control lists. That is the `rusty_rtos_mpu`
//! package, which comes after kernel 1.0.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cortex_m_rt::{entry, exception, ExceptionFrame};
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::TaskHandle;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel, Stall};
use rusty_rtos_port_cortex_m::{
    init_stack, pend_switch, set_scheduler, start_first_task, start_tick, CortexMPort,
    CURRENT_SP_SLOT,
};

/// Idle at 0, the judge at 1, the two application tasks at 2, the timer
/// daemon at 3.
#[derive(Debug, Clone, Copy, Default)]
pub struct M33Config;

impl Config for M33Config {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 4;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 3;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

#[derive(Debug, Default)]
struct NoTrace;
impl Trace for NoTrace {
    const WANTS_NAMES: bool = false;
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

const TASKS: usize = 6;
const QUEUES: usize = 4;
const SLOTS: usize = 16;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    M33Config,
    CortexMPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, TIMERS, lists_for(M33Config::MAX_PRIORITIES, QUEUES, GROUPS)) },
    { lists_for(M33Config::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
    { <M33Config as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
>;

/// The kernel. Privileged memory: no MPU region grants it to a task.
struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: reached only by privileged code -- `with_kernel` (interrupts
// masked) from privileged tasks, and `in_handler` from exceptions that no
// task can be inside: PendSV and SysTick are lowest priority, and SVCall is
// raised synchronously by an UNPRIVILEGED task, which never holds the
// kernel. One core.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

/// Borrow the kernel with interrupts masked. Privileged tasks only: an
/// unprivileged `cpsid i` is a no-op, which is exactly why those use [`sys`].
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    cortex_m::interrupt::free(|_| {
        // SAFETY: interrupts are masked and there is one core.
        let slot = unsafe { &mut *KERNEL.0.get() };
        slot.as_mut().map(f)
    })
}

/// Borrow the kernel from an exception handler. See [`KernelCell`].
fn in_handler<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    // SAFETY: as `KernelCell`'s `Sync`.
    let slot = unsafe { &mut *KERNEL.0.get() };
    slot.as_mut().map(f)
}

// ------------------------------------------------------------------ MPU --

/// The ARMv8-M (PMSAv8) MPU, Secure instance.
mod mpu {
    use core::ptr::{read_volatile, write_volatile};

    const TYPE: *const u32 = 0xE000_ED90 as *const u32;
    pub const CTRL: *mut u32 = 0xE000_ED94 as *mut u32;
    const RNR: *mut u32 = 0xE000_ED98 as *mut u32;
    const RBAR: *mut u32 = 0xE000_ED9C as *mut u32;
    const RLAR: *mut u32 = 0xE000_EDA0 as *mut u32;
    const MAIR0: *mut u32 = 0xE000_EDC0 as *mut u32;

    /// `CTRL`: the MPU on, with the default map as a background region for
    /// PRIVILEGED accesses only. An unprivileged access outside every
    /// enabled region faults.
    const ENABLE: u32 = 1;
    const PRIVDEFENA: u32 = 1 << 2;
    /// `RBAR.AP`: read-only at any privilege / read-write at any privilege.
    const AP_RO_ANY: u32 = 0b11 << 1;
    const AP_RW_ANY: u32 = 0b01 << 1;
    /// `RBAR.XN`: never execute.
    const XN: u32 = 1;
    /// `RLAR.EN`, with `AttrIndx` 0.
    const EN: u32 = 1;

    /// Region 0 is the code; region 1 is the running task's own memory.
    const TASK_REGION: u32 = 1;

    /// How many regions this core implements; zero means no MPU at all.
    pub fn regions() -> u32 {
        // SAFETY: a read of an implemented, side-effect-free SCS register.
        (unsafe { read_volatile(TYPE) } >> 8) & 0xFF
    }

    fn barrier() {
        // SAFETY: barriers; no memory, no fault.
        unsafe { core::arch::asm!("dsb", "isb", options(nostack, preserves_flags)) };
    }

    /// Program region 0 over the code and, unless poisoned, switch on.
    pub fn init(code_base: u32, code_end: u32) {
        // SAFETY: privileged writes to the MPU, made before any task runs.
        unsafe {
            write_volatile(MAIR0, 0xFF); // Attr0: normal memory, write-back
            write_volatile(RNR, 0);
            write_volatile(RBAR, (code_base & !31) | AP_RO_ANY);
            write_volatile(RLAR, ((code_end - 1) & !31) | EN);
            if cfg!(not(feature = "poison")) {
                write_volatile(CTRL, PRIVDEFENA | ENABLE);
            }
        }
        barrier();
    }

    /// Give the next thread `[base, base + len)`, read-write, never
    /// executable. `base` and `len` are multiples of 32.
    pub fn grant(base: u32, len: u32) {
        // SAFETY: privileged, from PendSV, where no thread is running.
        unsafe {
            write_volatile(RNR, TASK_REGION);
            write_volatile(RBAR, base | AP_RW_ANY | XN);
            write_volatile(RLAR, ((base + len - 1) & !31) | EN);
        }
        barrier();
    }

    /// Take region 1 away: the next thread is privileged and needs none.
    pub fn revoke() {
        // SAFETY: as `grant`.
        unsafe {
            write_volatile(RNR, TASK_REGION);
            write_volatile(RLAR, 0);
        }
        barrier();
    }

    /// Whether the MPU is still on.
    pub fn enabled() -> bool {
        // SAFETY: a privileged read of `CTRL`.
        unsafe { read_volatile(CTRL) & ENABLE != 0 }
    }
}

/// Set or clear `CONTROL.nPRIV`, which governs THREAD mode on the next
/// exception return. Called from `PendSV`; handler mode itself is always
/// privileged.
fn set_thread_unprivileged(unprivileged: bool) {
    let mut control: u32;
    // SAFETY: a register read.
    unsafe { core::arch::asm!("mrs {0}, CONTROL", out(reg) control, options(nomem, nostack)) };
    control = (control & !1) | u32::from(unprivileged);
    // SAFETY: privileged write of nPRIV; the ISB the architecture asks for.
    unsafe {
        core::arch::asm!("msr CONTROL, {0}", "isb", in(reg) control, options(nostack));
    }
}

/// Whether thread mode is currently unprivileged -- i.e. whether the
/// exception being handled was raised by an application task.
fn thread_unprivileged() -> bool {
    let control: u32;
    // SAFETY: a register read.
    unsafe { core::arch::asm!("mrs {0}, CONTROL", out(reg) control, options(nomem, nostack)) };
    control & 1 != 0
}

// ----------------------------------------------------- task memory -------

const STACK_WORDS: usize = 256;

/// Everything an application task may touch, as one MPU region: 32-byte
/// aligned and a multiple of 32 bytes long, which is PMSAv8's granule.
#[repr(C, align(32))]
struct TaskMem {
    stack: [usize; STACK_WORDS],
    laps: u32,
    done: u32,
    /// `rogue` only: what its read of the kernel returned.
    read_back: u32,
    _pad: [u32; 5],
}

impl TaskMem {
    const fn new() -> Self {
        TaskMem { stack: [0; STACK_WORDS], laps: 0, done: 0, read_back: 0, _pad: [0; 5] }
    }
}

const _: () = assert!(core::mem::size_of::<TaskMem>() % 32 == 0);

static mut MEM_GOOD: TaskMem = TaskMem::new();
static mut MEM_ROGUE: TaskMem = TaskMem::new();

/// Privileged data the rogue will aim at. Not the kernel itself, so that the
/// poisoned arm fails deterministically instead of crashing.
const CANARY_VALUE: u32 = 0xC0FF_EE11;
static mut CANARY: u32 = CANARY_VALUE;

const PRIV_STACK_WORDS: usize = 512;
static mut STACK_IDLE: [usize; PRIV_STACK_WORDS] = [0; PRIV_STACK_WORDS];
static mut STACK_TMR: [usize; PRIV_STACK_WORDS] = [0; PRIV_STACK_WORDS];
static mut STACK_JUDGE: [usize; PRIV_STACK_WORDS] = [0; PRIV_STACK_WORDS];

static SLOTS_SP: [AtomicUsize; TASKS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// Which application task each kernel slot holds: 0 none (privileged),
/// [`APP_GOOD`] or [`APP_ROGUE`].
static APP_OF: [AtomicU32; TASKS] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
const APP_GOOD: u32 = 1;
const APP_ROGUE: u32 = 2;
/// The application task on the CPU now, for the fault handlers.
static RUNNING_APP: AtomicU32 = AtomicU32::new(0);

fn mem_of(app: u32) -> Option<(u32, u32)> {
    let len = core::mem::size_of::<TaskMem>() as u32;
    // The second poison: the MPU stays ON, but a task's region is sized
    // over all of RAM -- the mistake of a region that is too big rather
    // than an MPU that is off. It must fail too, or the PASS says nothing
    // about the per-task region.
    if cfg!(feature = "poison-region") && app != 0 {
        return Some((0x3800_0000, 0x0020_0000));
    }
    match app {
        APP_GOOD => Some((addr_of!(MEM_GOOD) as u32, len)),
        APP_ROGUE => Some((addr_of!(MEM_ROGUE) as u32, len)),
        _ => None,
    }
}

// ------------------------------------------------------------ the switch --

static SWITCHES: AtomicU32 = AtomicU32::new(0);
static UNPRIVILEGED_SWITCHES: AtomicU32 = AtomicU32::new(0);

/// `PendSV`'s choice, and the privilege boundary with it: the incoming
/// task's region and `nPRIV` are set before the exception returns into it.
extern "C" fn pick_next() {
    let next = in_handler(|k| {
        k.switch_context();
        k.current()
    });
    let Some(handle) = next else { return };
    let i = handle.index() as usize;
    let Some(slot) = SLOTS_SP.get(i) else { return };
    CURRENT_SP_SLOT.store(core::ptr::from_ref(slot) as usize, Ordering::Relaxed);
    SWITCHES.fetch_add(1, Ordering::Relaxed);

    let app = APP_OF.get(i).map_or(0, |a| a.load(Ordering::Relaxed));
    match mem_of(app) {
        Some((base, len)) => {
            mpu::grant(base, len);
            set_thread_unprivileged(true);
            UNPRIVILEGED_SWITCHES.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            mpu::revoke();
            set_thread_unprivileged(false);
        }
    }
    RUNNING_APP.store(app, Ordering::Relaxed);
}

// ---------------------------------------------------- the kernel-call shim --

const SYS_DELAY: u32 = 0;
const SYS_TICKS: u32 = 1;
static SVC_CALLS: AtomicU32 = AtomicU32::new(0);

/// A kernel call from an unprivileged task: `svc #0`, operation in `r0`,
/// argument in `r1`, answer back in `r0`.
#[inline(always)]
fn sys(op: u32, arg: u32) -> u32 {
    let answer: u32;
    // SAFETY: `svc` enters the SVCall handler, which reads r0/r1 from the
    // stacked frame and writes r0 back; nothing else is touched.
    unsafe {
        core::arch::asm!("svc #0", inout("r0") op => answer, in("r1") arg, options(nostack));
    }
    answer
}

#[exception]
fn SVCall() {
    // The caller is a thread on the process stack, so its frame is there:
    // r0, r1, r2, r3, r12, lr, pc, xPSR.
    let frame = cortex_m::register::psp::read() as *mut u32;
    // SAFETY: the hardware stacked eight words at PSP on entry.
    let (op, arg) = unsafe { (read_volatile(frame), read_volatile(frame.add(1))) };
    let answer = match op {
        SYS_DELAY => {
            let _ = in_handler(|k| k.delay(u64::from(arg)));
            pend_switch();
            0
        }
        SYS_TICKS => in_handler(|k| k.tick_count()).unwrap_or(0) as u32,
        _ => u32::MAX,
    };
    // SAFETY: as above; the task reads this as `sys`'s answer.
    unsafe { write_volatile(frame, answer) };
    SVC_CALLS.fetch_add(1, Ordering::Relaxed);
}

// ------------------------------------------------------------ refusals ----

const CFSR: *mut u32 = 0xE000_ED28 as *mut u32;
const MMFAR: *const u32 = 0xE000_ED34 as *const u32;
const BFAR: *const u32 = 0xE000_ED38 as *const u32;
const SHCSR: *mut u32 = 0xE000_ED24 as *mut u32;
const MMARVALID: u32 = 1 << 7;
const DACCVIOL: u32 = 1 << 1;
const BFARVALID: u32 = 1 << 15;
const PRECISERR: u32 = 1 << 9;

const KIND_MEM: u32 = 1;
const KIND_BUS: u32 = 2;
const LOG: usize = 8;
static FAULTS: AtomicU32 = AtomicU32::new(0);
static FAULT_KIND: [AtomicU32; LOG] = [const { AtomicU32::new(0) }; LOG];
static FAULT_ADDR: [AtomicU32; LOG] = [const { AtomicU32::new(0) }; LOG];
static FAULT_APP: [AtomicU32; LOG] = [const { AtomicU32::new(0) }; LOG];

/// Refuse the access: log it, and step the task over the instruction.
///
/// Only a fault raised by an UNPRIVILEGED thread is refusable. Anything
/// else is a defect in privileged code, and the cell stops on it.
fn refuse(kind: u32) {
    if !thread_unprivileged() {
        fatal("a fault in PRIVILEGED code -- a defect, not a refusal");
    }
    // SAFETY: privileged reads and a write-one-to-clear of the fault
    // status registers.
    let cfsr = unsafe { read_volatile(CFSR) };
    let addr = match kind {
        KIND_MEM if cfsr & MMARVALID != 0 && cfsr & DACCVIOL != 0 => {
            // SAFETY: valid by MMARVALID.
            unsafe { read_volatile(MMFAR) }
        }
        KIND_BUS if cfsr & BFARVALID != 0 && cfsr & PRECISERR != 0 => {
            // SAFETY: valid by BFARVALID.
            unsafe { read_volatile(BFAR) }
        }
        // An instruction fetch, or an imprecise bus error: the stacked PC
        // is not the access, so stepping over it would skip the wrong
        // instruction.
        _ => fatal("a fault this cell cannot step over"),
    };
    // SAFETY: as above.
    unsafe { write_volatile(CFSR, cfsr) };

    // Step over the faulting load or store. Thumb-2: a halfword whose top
    // five bits are 0b11101, 0b11110 or 0b11111 starts a 32-bit instruction.
    let frame = cortex_m::register::psp::read() as *mut u32;
    // SAFETY: the faulting thread's frame, stacked at PSP on entry; its PC
    // points into the code region, which privileged code may read.
    unsafe {
        let pc = read_volatile(frame.add(6));
        let first = read_volatile(pc as *const u16);
        let len = if first >> 11 >= 0b11101 { 4 } else { 2 };
        write_volatile(frame.add(6), pc + len);
    }

    let n = FAULTS.fetch_add(1, Ordering::Relaxed) as usize;
    if n < LOG {
        FAULT_KIND[n].store(kind, Ordering::Relaxed);
        FAULT_ADDR[n].store(addr, Ordering::Relaxed);
        FAULT_APP[n].store(RUNNING_APP.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

#[exception]
fn MemoryManagement() {
    refuse(KIND_MEM);
}

#[exception]
fn BusFault() {
    refuse(KIND_BUS);
}

#[exception]
unsafe fn HardFault(ef: &ExceptionFrame) -> ! {
    hprintln!("HARDFAULT pc={:#010x} lr={:#010x}", ef.pc(), ef.lr());
    fatal("hard fault");
}

fn fatal(why: &str) -> ! {
    hprintln!("RESULT: FAIL -- {}", why);
    debug::exit(debug::EXIT_FAILURE);
    loop {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------- tasks --

const GOOD_LAPS: u32 = 50;
const ROGUE_LAPS: u32 = 25;
const DEADLINE_TICKS: u64 = 2_000;

/// Unprivileged. Counts to [`GOOD_LAPS`] in its own memory, one tick a lap.
extern "C" fn task_good(mem: usize) -> ! {
    let m = mem as *mut TaskMem;
    loop {
        // SAFETY: `m` is this task's own `TaskMem`, which region 1 grants it.
        unsafe {
            let laps = read_volatile(addr_of!((*m).laps));
            if laps >= GOOD_LAPS {
                write_volatile(addr_of_mut!((*m).done), 1);
            } else {
                write_volatile(addr_of_mut!((*m).laps), laps.wrapping_add(1));
            }
        }
        sys(SYS_DELAY, 1);
    }
}

/// Unprivileged. Writes its own memory every lap (legal), and on four laps
/// reaches for something that is not its own.
extern "C" fn task_rogue(mem: usize) -> ! {
    let m = mem as *mut TaskMem;
    // Addresses only: forming them touches nothing.
    // SAFETY: `addr_of_mut!` on a `static mut` creates no reference.
    let good_laps = unsafe { addr_of_mut!(MEM_GOOD.laps) };
    // SAFETY: as above.
    let canary = addr_of_mut!(CANARY);
    let kernel = KERNEL.0.get().cast::<u32>();
    let mut lap = 0u32;
    loop {
        lap = lap.wrapping_add(1);
        // SAFETY: every access below is a single aligned load or store of a
        // `u32`. The legal ones are to this task's own `TaskMem`; the four
        // illegal ones are the experiment, and the MPU (or the SCS's
        // privilege check) refuses them before they happen.
        unsafe {
            if lap <= ROGUE_LAPS {
                write_volatile(addr_of_mut!((*m).laps), lap);
            }
            match lap {
                5 => write_volatile(good_laps, 0xDEAD),
                10 => write_volatile(canary, 0x0BAD),
                15 => write_volatile(addr_of_mut!((*m).read_back), read_volatile(kernel)),
                20 => write_volatile(mpu::CTRL, 0),
                ROGUE_LAPS => write_volatile(addr_of_mut!((*m).done), 1),
                _ => {}
            }
        }
        sys(SYS_DELAY, 1);
    }
}

/// Privileged. Waits for both application tasks, then judges.
extern "C" fn task_judge(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(5));
        pend_switch();
        // SAFETY: privileged reads of words the tasks write whole.
        let both = unsafe {
            read_volatile(addr_of!(MEM_GOOD.done)) != 0 && read_volatile(addr_of!(MEM_ROGUE.done)) != 0
        };
        let late = with_kernel(|k| k.tick_count()).unwrap_or(0) >= DEADLINE_TICKS;
        if both || late {
            finish(late);
        }
    }
}

extern "C" fn task_idle(_: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// `Tmr Svc` exists because `start_scheduler` makes it; it sleeps.
extern "C" fn task_timer(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(DEADLINE_TICKS * 4));
        pend_switch();
    }
}

#[exception]
fn SysTick() {
    let want = in_handler(|k| k.increment_tick()).unwrap_or(false);
    rusty_rtos_port_cortex_m::tick(&PORT, want);
}

static PORT: CortexMPort = CortexMPort::new();

// --------------------------------------------------------------- verdict --

fn finish(late: bool) -> ! {
    // SAFETY: privileged reads.
    let (good_laps, good_done, rogue_laps, rogue_done, canary) = unsafe {
        (
            read_volatile(addr_of!(MEM_GOOD.laps)),
            read_volatile(addr_of!(MEM_GOOD.done)),
            read_volatile(addr_of!(MEM_ROGUE.laps)),
            read_volatile(addr_of!(MEM_ROGUE.done)),
            read_volatile(addr_of!(CANARY)),
        )
    };
    let ticks = with_kernel(|k| k.tick_count()).unwrap_or(0);
    let (stalls, why) = with_kernel(|k| (k.stalls(), k.first_stall())).unwrap_or((0, Stall::None));
    let faults = FAULTS.load(Ordering::SeqCst);

    // SAFETY: addresses only.
    let want: [(u32, u32); 4] = unsafe {
        [
            (KIND_MEM, addr_of!(MEM_GOOD.laps) as u32),
            (KIND_MEM, addr_of!(CANARY) as u32),
            (KIND_MEM, KERNEL.0.get() as u32),
            (KIND_BUS, mpu::CTRL as u32),
        ]
    };

    hprintln!();
    hprintln!("ticks                 {}", ticks);
    hprintln!("switches              {}   into an unprivileged task: {}",
        SWITCHES.load(Ordering::SeqCst), UNPRIVILEGED_SWITCHES.load(Ordering::SeqCst));
    hprintln!("kernel calls by svc   {}", SVC_CALLS.load(Ordering::SeqCst));
    hprintln!("good                  laps={} (want {}) done={}", good_laps, GOOD_LAPS, good_done);
    hprintln!("rogue                 laps={} (want {}) done={}", rogue_laps, ROGUE_LAPS, rogue_done);
    hprintln!("canary                {:#010x} (want {:#010x})", canary, CANARY_VALUE);
    hprintln!("MPU enabled at end    {}", mpu::enabled());
    hprintln!("scheduler stalls      {}   first: {:?}", stalls, why);
    hprintln!("refusals              {}", faults);
    for i in 0..(faults as usize).min(LOG) {
        let kind = FAULT_KIND[i].load(Ordering::SeqCst);
        hprintln!(
            "  {} {:<9} at {:#010x}  by {}",
            i,
            if kind == KIND_MEM { "MemManage" } else { "BusFault" },
            FAULT_ADDR[i].load(Ordering::SeqCst),
            match FAULT_APP[i].load(Ordering::SeqCst) {
                APP_GOOD => "good",
                APP_ROGUE => "rogue",
                _ => "?",
            }
        );
    }

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            hprintln!("      ok    {}", what);
        } else {
            failed += 1;
            hprintln!("      FAIL  {}", what);
        }
    };
    check(mpu::regions() >= 2, "the core implements an MPU with at least two regions");
    check(!late, "both application tasks finished before the deadline");
    check(
        good_laps == GOOD_LAPS && good_done == 1,
        "`good` counted to exactly 50 in its own memory: nobody else wrote it",
    );
    check(canary == CANARY_VALUE, "the privileged canary is unchanged");
    check(
        rogue_laps == ROGUE_LAPS && rogue_done == 1,
        "`rogue` kept running after every refusal: refused, not crashed",
    );
    let logged = (0..LOG).map(|i| {
        (FAULT_KIND[i].load(Ordering::SeqCst), FAULT_ADDR[i].load(Ordering::SeqCst), FAULT_APP[i].load(Ordering::SeqCst))
    });
    let exact = faults == 4
        && logged.zip(want.iter()).all(|((k, a, app), &(wk, wa))| k == wk && a == wa && app == APP_ROGUE);
    check(
        exact,
        "exactly four refusals, all `rogue`'s: another task's data, privileged data, the kernel, MPU_CTRL",
    );
    check(mpu::enabled(), "the MPU is still on: the attempt to switch it off was refused");
    check(stalls == 0, "the scheduler never failed to choose a task (Law 3)");
    check(
        UNPRIVILEGED_SWITCHES.load(Ordering::SeqCst) > 0 && SVC_CALLS.load(Ordering::SeqCst) > 0,
        "application tasks ran unprivileged and reached the kernel only by svc",
    );

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- four bad accesses from an unprivileged task, four refusals;");
        hprintln!("        the kernel, the canary and the other task untouched.");
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} check(s) failed", failed);
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}

// ------------------------------------------------------------------ boot --

fn arm_task(handle: TaskHandle, top: *mut usize, entry: extern "C" fn(usize) -> !, arg: usize, app: u32) -> bool {
    let i = handle.index() as usize;
    let (Some(slot), Some(kind)) = (SLOTS_SP.get(i), APP_OF.get(i)) else {
        return false;
    };
    // SAFETY: every caller passes one past the end of a stack of at least
    // 256 words that no other task uses.
    let sp = unsafe { init_stack(top, entry, arg) };
    slot.store(sp, Ordering::SeqCst);
    kind.store(app, Ordering::SeqCst);
    true
}

fn top_of(stack: *mut usize, words: usize) -> *mut usize {
    stack.wrapping_add(words)
}

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== MPU refusal on Cortex-M33 (mps2-an505, QEMU) ===");
    hprintln!("two unprivileged Kairos tasks, each confined to its own memory;");
    hprintln!("one of them tries four accesses it has no right to.");
    if cfg!(feature = "poison") {
        hprintln!("POISONED: the MPU is left OFF. This run must FAIL.");
    }
    if cfg!(feature = "poison-region") {
        hprintln!("POISONED: each task's region covers ALL of RAM. This run must FAIL.");
    }
    hprintln!("MPU regions implemented: {}", mpu::regions());

    let mut kernel = match K::new(CortexMPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => fatal("the kernel refused the geometry"),
    };
    let Ok(good) = kernel.create_task("good", 2) else { fatal("create good") };
    let Ok(rogue) = kernel.create_task("rogue", 2) else { fatal("create rogue") };
    let Ok(judge) = kernel.create_task("judge", 1) else { fatal("create judge") };
    let Ok(started) = kernel.start_scheduler() else { fatal("start") };

    // SAFETY: no task runs yet; addresses of statics, no references.
    let armed = unsafe {
        let good_mem = addr_of_mut!(MEM_GOOD);
        let rogue_mem = addr_of_mut!(MEM_ROGUE);
        arm_task(good, top_of(addr_of_mut!((*good_mem).stack).cast(), STACK_WORDS), task_good, good_mem as usize, APP_GOOD)
            && arm_task(rogue, top_of(addr_of_mut!((*rogue_mem).stack).cast(), STACK_WORDS), task_rogue, rogue_mem as usize, APP_ROGUE)
            && arm_task(judge, top_of(addr_of_mut!(STACK_JUDGE).cast(), PRIV_STACK_WORDS), task_judge, 0, 0)
            && arm_task(started.idle, top_of(addr_of_mut!(STACK_IDLE).cast(), PRIV_STACK_WORDS), task_idle, 0, 0)
            && arm_task(started.timer, top_of(addr_of_mut!(STACK_TMR).cast(), PRIV_STACK_WORDS), task_timer, 0, 0)
    };
    if !armed {
        fatal("a task handle fell outside the slot table");
    }

    // The first task runs straight out of `start_first_task`, privileged,
    // with no PendSV in between to set its region -- so it must be a
    // privileged task. The timer daemon outranks everything and is.
    let first = kernel.current().index() as usize;
    if APP_OF.get(first).map_or(1, |a| a.load(Ordering::SeqCst)) != 0 {
        fatal("the first task would start unconfined");
    }
    if let Some(slot) = SLOTS_SP.get(first) {
        CURRENT_SP_SLOT.store(core::ptr::from_ref(slot) as usize, Ordering::SeqCst);
    }

    cortex_m::interrupt::free(|_| {
        // SAFETY: nothing else holds the kernel yet.
        unsafe { *KERNEL.0.get() = Some(kernel) };
    });

    // MemManage and BusFault on, so a refusal is handled rather than
    // escalated to HardFault.
    // SAFETY: a privileged read-modify-write of SHCSR before any task runs.
    unsafe { write_volatile(SHCSR, read_volatile(SHCSR) | (1 << 16) | (1 << 17)) };
    mpu::init(0x1000_0000, 0x1040_0000);

    set_scheduler(pick_next);
    start_tick(25_000);
    // SAFETY: every task has a stack, a scheduler is installed, and
    // CURRENT_SP_SLOT names the first task's slot.
    unsafe { start_first_task() }
}
