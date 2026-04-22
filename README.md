# dram-mapper

  
  <img width="1398" height="1336" alt="Screenshot From 2026-04-22 12-07-38" src="https://github.com/user-attachments/assets/adb6079b-21fc-4cfc-8031-c1284cd0dfc0" />
                                     

  # Build
  
  `cd ~/Documents/dram-mapper`  
  `cargo build`  

  # Run (requires sudo for /proc/self/pagemap and huge pages)
  `sudo ./target/debug/dram-mapper --fast             # ~2 min, full map`  
  `sudo ./target/debug/dram-mapper --balanced          # ~10 min, more accurate`  
  `sudo ./target/debug/dram-mapper --precise           # ~40 min, most accurate`  
  `sudo ./target/debug/dram-mapper --fast --limit 8192 # test 8GB only`  
  `sudo ./target/debug/dram-mapper --fast --verbose    # show all regions`  

  If huge pages get stuck (low available memory on next run):  
  `echo 0 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages`  
  `sleep 2 && cat /proc/meminfo | grep MemAvailable`  


  # Scores  
  The scores are useful for picking a runtime-discovered physical block for an LLM inference allocator, diagnosing memory-subsystem asymmetries, and A/B testing BIOS changes.

# Porting to Other x86_64 CPUs

This tool is compiled with Ryzen 9 9950X3D constants. To run it on a different x86_64 CPU you must update one value; the rest is cosmetic.

## 1. Required: update `TSC_GHZ` (src/main.rs:12)

The `_rdtsc` instruction ticks at your CPU's **invariant TSC frequency**, which equals the **base (non-turbo) clock** stamped on the CPU's spec sheet. Every latency in nanoseconds is derived from this value, so a wrong number scales every latency and every score.

Find your exact TSC frequency:

```bash
sudo dmesg | grep -i "tsc:"
```

You should see a line like:

```
tsc: Refined TSC clocksource calibration: 4291.919 MHz
```

Divide the MHz value by 1000 and put it in `main.rs`:

```rust
const TSC_GHZ: f64 = 4.291919;   // was 4.291920 for 9950X3D
```

If `dmesg` is empty (common on systemd systems), try:

```bash
journalctl -k | grep -i "tsc:"
```

Fallback if neither works — use the CPU's base clock from `lscpu`:

```bash
lscpu | grep "CPU max MHz\|BogoMIPS"
```

For most modern CPUs the TSC rate equals the base clock (not the boost clock). Common values:

| CPU                       | TSC_GHZ |
|---------------------------|---------|
| Ryzen 9 9950X3D           | 4.29    |
| Ryzen 9 7950X / 7950X3D   | 4.50    |
| Ryzen 7 7800X3D           | 4.20    |
| Ryzen 5 7600X             | 4.70    |
| Intel i9-14900K (P-core)  | 3.20    |
| Intel i9-13900K (P-core)  | 3.00    |
| Intel i7-13700K (P-core)  | 3.40    |
| Intel i5-13600K (P-core)  | 3.50    |

Always prefer the `dmesg` value when available — it's measured, not nominal.

## 2. Optional: update the banner (src/main.rs:296)

Purely cosmetic. Edit the string to match your hardware:

```rust
println!("║   Ryzen 9 9950X3D / DDR5 @ 3600 MT/s            ║");
```

## 3. What does NOT need to change

These constants are universal on x86_64 and should be left alone:

- `PAGE_SIZE = 4096` — fixed by the x86_64 ABI
- `HUGE_PAGE_SIZE = 2 MB` — standard 2 MB huge page size
- `CACHE_LINE_SIZE = 64` — every x86_64 CPU since ~2005
- `SAFETY_BUFFER_MB = 4096` — leaves 4 GB for the OS; increase on systems with <16 GB RAM if you hit OOM

## 4. What will NOT work at all

- **Non-Linux OSes** (macOS, Windows, BSD) — the tool uses `/proc/meminfo`, `/proc/self/pagemap`, `/sys/kernel/mm/hugepages/...`, and `MAP_HUGETLB`. None of these exist outside Linux.
- **32-bit x86 (i686)** — won't compile; uses x86_64-only intrinsics.
- **ARM / aarch64** — won't compile. Use `src/main-arm.rs` instead.
- **Running without root** — `/proc/self/pagemap` returns zero PFNs to unprivileged users since kernel 4.0.

## 5. Sanity check after porting

After updating `TSC_GHZ` and rebuilding, run once with `--fast --limit 1024` and check that:

- `Best lat` is between **70–150 ns** (healthy DDR4/DDR5 range)
- `Best read` is between **8–20 GB/s** single-threaded
- Score p10/p50/p90 are within ~10 of each other (tight distribution)

If latencies look 30–50% too high or too low, your `TSC_GHZ` is wrong — try the `dmesg` value again.

## 6. Long-term fix: runtime TSC calibration

To make the binary truly portable, replace the `TSC_GHZ` constant with a calibration function run at startup:

```rust
fn calibrate_tsc_ghz() -> f64 {
    use std::time::Instant;
    let t0 = Instant::now();
    let c0 = unsafe { std::arch::x86_64::_rdtsc() };
    std::thread::sleep(std::time::Duration::from_millis(100));
    let c1 = unsafe { std::arch::x86_64::_rdtsc() };
    (c1 - c0) as f64 / t0.elapsed().as_nanos() as f64
}
```

Call it once in `main()` and pass the result through to `bench_random_latency`. Accurate to ~0.01% after 100 ms.

  
