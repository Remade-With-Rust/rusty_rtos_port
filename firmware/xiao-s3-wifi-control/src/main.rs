#![no_std]
#![no_main]
//! Null arm for `xiao-s3-wifi`: the identical scan, on esp-rtos + esp-alloc.

extern crate alloc;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use esp_backtrace as _;
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::scan::{ScanConfig, ScanTypeConfig};
use esp_radio::wifi::WifiController;

esp_bootloader_esp_idf::esp_app_desc!();

fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        // Preemptive scheduler: a short busy wait still lets radio tasks run.
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(1) {}
    }
}

#[esp_hal::main]
fn main() -> ! {
    let p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));
    #[cfg(feature = "driver-logs")]
    esp_println::logger::init_logger(log::LevelFilter::Info);
    esp_alloc::heap_allocator!(size: 160 * 1024);
    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    println!();
    println!("=== CONTROL: the same scan on esp-rtos + esp-alloc ===");
    let mut controller = WifiController::new(p.WIFI, Default::default()).unwrap();
    for (name, cfg) in [
        ("active-default", ScanConfig::default().with_max(20)),
        (
            "passive-300ms",
            ScanConfig::default()
                .with_max(20)
                .with_scan_type(ScanTypeConfig::Passive(Duration::from_millis(300))),
        ),
    ] {
        let t = Instant::now();
        let aps = block_on(controller.scan_async(&cfg)).unwrap();
        println!(
            "CONTROL {name} aps={} us={}",
            aps.len(),
            t.elapsed().as_micros()
        );
        for ap in &aps {
            println!(
                "CONTROL ap ch={:>2} rssi={:>4} ssid_len={}",
                ap.channel,
                ap.signal_strength,
                ap.ssid.as_str().len()
            );
        }
    }
    println!("CONTROL done");
    loop {}
}
