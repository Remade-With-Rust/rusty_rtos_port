#![no_std]
#![no_main]
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]
//! The real Wi-Fi radio on the Kairos kernel, on a XIAO ESP32-S3.
//!
//! `xiao-s3-radio` proved the scheduler half of `esp-radio-rtos-driver` with
//! two tasks of its own; `esp-radio` was never in the binary. This cell links
//! it. Every task the radio blob creates, every semaphore and queue it
//! blocks on, and every software timer it arms runs on the Kairos kernel,
//! and its C heap is `rusty_rtos_alloc`. There is no `esp-rtos` and no
//! `esp-alloc` in the build.
//!
//! # What it checks
//!
//! Stage 1 needs no network: bring the controller up and SCAN. A scan is a
//! full round trip through the blob — its own tasks, its event queue, a
//! timer-driven channel hop and an interrupt per received beacon — so
//! "it found access points" is a claim about all of that, not about a
//! print. The cell passes only if at least one AP answers.
//!
//! Stage 2 runs only when the build is given a network
//! (`KAIROS_WIFI_SSID` / `KAIROS_WIFI_PASSWORD` at compile time, never
//! printed): associate, and report the AP's channel and signal.

extern crate alloc;

mod kernel;
mod libc_heap;

use rusty_rtos_port_esp_radio as adapter;

use core::ffi::c_void;
use core::future::Future;
use core::pin::pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll, Waker};

use esp_backtrace as _;
use esp_println::println;

use esp_radio::wifi::scan::{ScanConfig, ScanTypeConfig};
use esp_radio::wifi::WifiController;

esp_bootloader_esp_idf::esp_app_desc!();

/// One heap for the blob, the adapter's task stacks and Rust: `heap_4`.
#[global_allocator]
static ALLOC: libc_heap::Heap4Alloc = libc_heap::Heap4Alloc;

esp_radio_rtos_driver::register_scheduler_implementation!(
    static SCHEDULER: adapter::Scheduler = adapter::Scheduler
);
esp_radio_rtos_driver::register_semaphore_implementation!(adapter::Semaphore);
esp_radio_rtos_driver::register_queue_implementation!(adapter::Queue);
esp_radio_rtos_driver::register_timer_implementation!(adapter::Timer);
esp_radio_rtos_driver::register_wait_queue_implementation!(adapter::WaitQueue);

/// Drive a future to completion on THIS task.
///
/// No executor and no waker plumbing: between polls the task sleeps one tick
/// through the kernel, which is what hands the CPU to the radio's own tasks.
/// A waker would only shorten the wait; it cannot change the answer, and a
/// poll every millisecond is far inside every timeout the radio arms.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        kernel::sleep_ticks(1);
    }
}

#[esp_hal::main]
fn main() -> ! {
    let p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));

    println!();
    println!("=== Wi-Fi on the Kairos kernel, XIAO ESP32-S3 SILICON ===");
    println!("scheduler  rusty_rtos_kernel + rusty_rtos_port-xtensa (no esp-rtos)");
    println!("radio      esp-radio 1.0.0-beta.1, via esp-radio-rtos-driver 0.4.2");
    println!("heap       Kairos heap_4 for Rust AND the blob's malloc (no esp-alloc)");
    println!();

    #[cfg(feature = "driver-logs")]
    esp_println::logger::init_logger(log::LevelFilter::Info);

    println!(
        "WIFI heap heap_4 arena={} free={}",
        libc_heap::ARENA,
        libc_heap::free_bytes()
    );
    if !kernel::boot() {
        fail(format_args!("the kernel would not start"));
    }
    if !kernel::start_tick(p.SYSTIMER) {
        fail(format_args!("the tick would not start"));
    }
    println!("WIFI kernel_started tick=1kHz");
    let spin_start = kernel::now_us();
    while kernel::now_us() - spin_start < 20_000 {}
    let ps: u32;
    // SAFETY: reads PS.
    unsafe { core::arch::asm!("rsr.ps {0}", out(reg) ps, options(nostack)) };
    println!(
        "WIFI busy_20ms ticks={} ps={ps:#x}",
        kernel::ISR_TICKS.load(Ordering::Relaxed)
    );
    let before = kernel::ISR_TICKS.load(Ordering::Relaxed);
    kernel::sleep_ticks(50);
    println!(
        "WIFI sleep_check ticks_before={before} ticks_after={} (a 50-tick sleep)",
        kernel::ISR_TICKS.load(Ordering::Relaxed)
    );

    let t0 = kernel::now_us();
    let mut controller = match WifiController::new(p.WIFI, Default::default()) {
        Ok(c) => c,
        Err(e) => fail(format_args!("WifiController::new: {e:?}")),
    };
    println!("WIFI controller_up us={}", kernel::now_us() - t0);

    // ---------------------------------------------------- stage 1: scan --
    let t1 = kernel::now_us();
    let aps = match block_on(
        controller.scan_async(&ScanConfig::default().with_max(20).with_scan_type(
            ScanTypeConfig::Passive(esp_hal::time::Duration::from_millis(300)),
        )),
    ) {
        Ok(v) => v,
        Err(e) => fail(format_args!("scan: {e:?}")),
    };
    let scan_us = kernel::now_us() - t1;
    println!("WIFI scan_done aps={} us={scan_us}", aps.len());
    for ap in &aps {
        // Not the SSID: these are the neighbours' networks, and the claim
        // needs only that each one is a real, distinct beacon.
        println!(
            "WIFI ap ch={:>2} rssi={:>4} auth={:?} ssid_len={}",
            ap.channel,
            ap.signal_strength,
            ap.auth_method,
            ap.ssid.as_str().len()
        );
    }
    report_kernel();
    #[cfg(feature = "softap")]
    softap(&mut controller);
    if aps.is_empty() {
        fail(format_args!("the scan completed and found no access point"));
    }
    println!(
        "STAGE1: PASS -- {} access points through the blob on Kairos",
        aps.len()
    );

    // ------------------------------------------------- stage 2: associate --
    match (
        option_env!("KAIROS_WIFI_SSID"),
        option_env!("KAIROS_WIFI_PASSWORD"),
    ) {
        (Some(ssid), Some(password)) => associate(&mut controller, ssid, password),
        _ => println!("STAGE2: SKIPPED -- built without KAIROS_WIFI_SSID / KAIROS_WIFI_PASSWORD"),
    }

    println!();
    println!("RESULT: PASS");
    park();
}

fn associate(controller: &mut WifiController<'_>, ssid: &str, password: &str) {
    use esp_radio::wifi::sta::StationConfig;
    use esp_radio::wifi::{AuthenticationMethodConfig, Config};

    let (Ok(s), Ok(pw)) = (ssid.try_into(), password.try_into()) else {
        fail(format_args!(
            "the SSID or password does not fit the driver's types"
        ));
    };
    let conf = Config::Station(
        StationConfig::default()
            .with_ssid(s)
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw)),
    );
    if let Err(e) = controller.set_config(&conf) {
        fail(format_args!("set_config: {e:?}"));
    }
    let t = kernel::now_us();
    match block_on(controller.connect_async()) {
        Ok(info) => println!(
            "STAGE2: PASS -- associated in {} us, channel={} rssi={}",
            kernel::now_us() - t,
            info.channel,
            controller.rssi().unwrap_or(0)
        ),
        Err(e) => fail(format_args!("connect: {e:?}")),
    }
}

/// Become an access point and stay one. The proof is OUTSIDE the board: a
/// second radio (the workstation's) must list this SSID, which it can only do
/// if the blob's beacons left the antenna on Kairos's schedule.
#[cfg(feature = "softap")]
fn softap(controller: &mut WifiController<'_>) -> ! {
    use esp_radio::wifi::ap::AccessPointConfig;
    use esp_radio::wifi::Config;
    let Ok(ssid) = "kairos-xiao-s3".try_into() else {
        fail(format_args!("SSID"));
    };
    let conf = Config::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(ssid)
            .with_channel(6)
            .with_max_connections(4),
    );
    if let Err(e) = controller.set_config(&conf) {
        fail(format_args!("set_config(AccessPoint): {e:?}"));
    }
    println!("SOFTAP up ssid=kairos-xiao-s3 channel=6 open -- look for it from another radio");
    let mut n = 0u32;
    loop {
        kernel::sleep_ticks(5000);
        n += 5;
        println!("SOFTAP alive t={n}s");
        report_kernel();
    }
}

fn report_kernel() {
    let (entries, swaps, same, no_ctx, _, _) = kernel::switch_counts();
    println!(
        "WIFI kernel switch_entries={entries} swaps={swaps} declined_same={same} declined_no_ctx={no_ctx} radio_timers_fired={}",
        kernel::TIMERS_FIRED.load(Ordering::Relaxed)
    );
    println!("WIFI kernel {}", kernel::ready_report());
    use adapter::isr_stats as i;
    println!(
        "WIFI isr queue_sends={} queue_full={} sem_gives={} sem_refused={} yields={}",
        i::QUEUE_SENDS.load(Ordering::Relaxed),
        i::QUEUE_FULL.load(Ordering::Relaxed),
        i::SEM_GIVES.load(Ordering::Relaxed),
        i::SEM_REFUSED.load(Ordering::Relaxed),
        i::YIELDS.load(Ordering::Relaxed)
    );
    use libc_heap::stats as h;
    println!(
        "WIFI malloc calls={} nulls={} from_isr={} frees={}",
        h::CALLS.load(Ordering::Relaxed),
        h::NULLS.load(Ordering::Relaxed),
        h::FROM_ISR.load(Ordering::Relaxed),
        h::FREES.load(Ordering::Relaxed)
    );
    println!(
        "WIFI heap free={} min_ever_free={}",
        libc_heap::free_bytes(),
        libc_heap::minimum_ever_free()
    );
}

fn fail(why: core::fmt::Arguments<'_>) -> ! {
    report_kernel();
    println!("RESULT: FAIL -- {why}");
    park();
}

fn park() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// The registration macros name `c_void` in the shims they expand to.
#[allow(dead_code)]
type _CVoid = c_void;
