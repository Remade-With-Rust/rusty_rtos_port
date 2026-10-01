# `xiao-s3-wifi`: the real Wi-Fi radio on the Kairos kernel

**Status: Stage 1 PASSES on a XIAO ESP32-S3 (2026-10-01).** `esp-radio`
1.0.0-beta.1 runs on the Kairos kernel. Its driver task, event queue,
semaphores and timers, and its interrupt-side sends, all go through
`rusty_rtos_port-esp-radio`. The heap is Kairos `heap_4`. There is no
`esp-rtos` and no `esp-alloc` in the binary.

```
WIFI controller_up us=44436
WIFI scan_done aps=14 us=3919492          (passive, 300 ms x 13 channels)
WIFI kernel switch_entries=20231 swaps=12172 declined_no_ctx=0 radio_timers_fired=13
WIFI isr queue_sends=116 queue_full=0 yields=111
WIFI malloc calls=126 nulls=0 from_isr=2 frees=62
WIFI heap free=148152 min_ever_free=146000  (of a 192 KiB heap_4 arena)
STAGE1: PASS
```

## Measuring this run: the control arm

`../xiao-s3-wifi-control` runs the same scan on the same board with
Espressif's own `esp-rtos` and `esp-alloc`. It found **15** access points
where this cell first found **0**. That proved the board, the antenna and
the air were fine, and that the zero was ours. A transmit test was not
enough: the soft-AP mode (`--features softap`) was heard by the
workstation's radio at -33 dBm, so transmit worked all along.

## Four defects the real radio found, none visible before it

| # | defect | how it showed | fix |
|---|---|---|---|
| 1 | **The cell had no tick.** `PeriodicTimer::start` without `listen()` counts but never interrupts. `xiao-s3-radio` passed anyway, because its workers never needed time to pass. | a 50-tick sleep never returned; 0 ticks after 20 ms of spinning with `PS.INTLEVEL = 0` | `timer.listen()`, in both cells |
| 2 | **`main` level with the timer daemon.** `start_scheduler` created `Tmr Svc` at `main`'s priority and the kernel made it `current` while the CPU was on `main`. | the boot line read `current=2`; the first switch saved `main`'s registers as the daemon's | `main` strictly above `TIMER_TASK_PRIORITY` |
| 3 | **The radio timer table was too small.** 16 slots; `ieee80211_hostap_attach` needs more in soft-AP mode. | the adapter halted, naming it | 64 slots |
| 4 | **The heap starved the blob.** `rusty_alloc`'s small-metal profile costs about a page per size class touched, whatever the bytes asked for. | 29 of 88 blob mallocs returned NULL (96 B each) with the 196,608-byte region at `free=0`, so the scan list could not take a record | Kairos `heap_4` for Rust AND the blob, interrupt-masked like `esp-alloc` (`src/libc_heap.rs`) |

Two smaller adapter changes came from the same work. Neither was the cause
of the zero, but both are real:
- **The queue ring is now guarded by the interrupt mask.** It had been
  guarded by a kernel mutex that the radio's interrupt cannot take.
- **The interrupt-side send now reserves its slot** through the `empty`
  semaphore, like any other sender.

`IDLE` and `Tmr Svc` now have stacks (`register_kernel_task`), and the
daemon services the radio's timers.

## Stage 2 (associate) and Stage 3 (MQTT, in the umbrella)

Both need a 2.4 GHz WPA2 network, given at build time and never printed:

```sh
KAIROS_WIFI_SSID=... KAIROS_WIFI_PASSWORD=... cargo build --release
espflash flash -p COM4 --monitor target/xtensa-esp32s3-none-elf/release/xiao-s3-wifi
```

Stage 3 lives at `tools/vertical/firmware/xiao-s3-mqtt-wifi` in the
umbrella. It holds an MQTT session over `rusty_rtos_tcp` over this link and
needs one more variable: `KAIROS_MQTT_BROKER=a.b.c.d:port` (the default is
the DHCP gateway on 1883).

## Diagnostics left in

- `--features driver-logs`: the blob's own log lines.
- `kernel::ISR_BEAT`: a once-a-second heartbeat from the tick interrupt. It
  prints the interrupted PC, which is how a spinning task is found.
- `adapter::isr_stats` and `libc_heap::stats`: the counters above.

All are off by default, and no number is taken with logs on.
