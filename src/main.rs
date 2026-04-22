#![allow(unsafe_op_in_unsafe_fn)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ptr;
use std::time::Instant;
use libc::{mmap, munmap, MAP_ANONYMOUS, MAP_HUGETLB, MAP_PRIVATE, PROT_READ, PROT_WRITE};

const PAGE_SIZE: usize = 4096;
const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;
const CACHE_LINE_SIZE: usize = 64;
const TSC_GHZ: f64 = 4.291920;
const SAFETY_BUFFER_MB: usize = 4096;
const SCORE_NOISE_FLOOR: f64 = 2.0;
const MIN_ALLOC_MB: usize = 64;

#[derive(Debug, Clone)]
struct BenchMode {
    name:            &'static str,
    seq_passes:      usize,
    random_accesses: usize,
    warmup_passes:   usize,
}

impl BenchMode {
    fn fast() -> Self {
        Self { name: "FAST", seq_passes: 2, random_accesses: 500, warmup_passes: 1 }
    }
    fn balanced() -> Self {
        Self { name: "BALANCED", seq_passes: 5, random_accesses: 2000, warmup_passes: 2 }
    }
    fn precise() -> Self {
        Self { name: "PRECISE", seq_passes: 20, random_accesses: 10000, warmup_passes: 3 }
    }
}

#[derive(Debug, Clone)]
struct RegionStats {
    paddr:           usize,
    seq_read_bw:     f64,
    seq_write_bw:    f64,
    rand_latency_ns: f64,
    score:           f64,
}

#[derive(Debug, Clone)]
struct ContiguousBlock {
    start_paddr:  usize,
    end_paddr:    usize,
    size_mb:      usize,
    avg_score:    f64,
    avg_read_bw:  f64,
    avg_write_bw: f64,
    avg_latency:  f64,
    min_score:    f64,
}

fn read_meminfo_mb(key: &str) -> usize {
    let content = std::fs::read_to_string("/proc/meminfo")
        .expect("Cannot read /proc/meminfo");
    for line in content.lines() {
        if line.starts_with(key) {
            let kb: usize = line.split_whitespace()
                .nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            return kb / 1024;
        }
    }
    0
}

fn reserve_huge_pages(count: usize) {
    let path = "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages";
    // Release any leftover reservation from a previous crashed run
    let _ = std::fs::write(path, "0");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let current: usize = std::fs::read_to_string(path)
        .unwrap_or_default().trim().parse().unwrap_or(0);
    if current >= count { return; }
    let _ = std::fs::write(path, count.to_string());
    let actual: usize = std::fs::read_to_string(path)
        .unwrap_or_default().trim().parse().unwrap_or(0);
    if actual < count {
        println!("  Warning: requested {} huge pages, got {}.", count, actual);
    }
}

fn release_huge_pages() {
    let path = "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages";
    let _ = std::fs::write(path, "0");
}

fn virtual_to_physical(file: &mut File, vaddr: usize) -> Option<usize> {
    let pagemap_offset = (vaddr / PAGE_SIZE) * 8;
    file.seek(SeekFrom::Start(pagemap_offset as u64)).ok()?;
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf).ok()?;
    let entry = u64::from_le_bytes(buf);
    if entry & (1 << 63) == 0 { return None; }
    let pfn = entry & 0x007FFFFF_FFFFFFFF;
    Some((pfn as usize) * PAGE_SIZE + (vaddr % PAGE_SIZE))
}

unsafe fn bench_seq_read(base: *const u8, size: usize,
                          passes: usize, warmup: usize) -> f64 {
    for _ in 0..warmup {
        let mut s = 0u64;
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            s = s.wrapping_add(std::ptr::read_volatile(base.add(i)) as u64);
        }
        std::hint::black_box(s);
    }
    let t = Instant::now();
    let mut s = 0u64;
    for _ in 0..passes {
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            s = s.wrapping_add(std::ptr::read_volatile(base.add(i)) as u64);
        }
    }
    std::hint::black_box(s);
    let elapsed = t.elapsed().as_secs_f64();
    if elapsed == 0.0 { return 0.0; }
    (size * passes) as f64 / elapsed / (1024.0 * 1024.0)
}

unsafe fn bench_seq_write(base: *mut u8, size: usize,
                           passes: usize, warmup: usize) -> f64 {
    for _ in 0..warmup {
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            std::ptr::write_volatile(base.add(i), 0xAA);
        }
    }
    let t = Instant::now();
    for _ in 0..passes {
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            std::ptr::write_volatile(base.add(i), 0xBB);
        }
    }
    let elapsed = t.elapsed().as_secs_f64();
    if elapsed == 0.0 { return 0.0; }
    (size * passes) as f64 / elapsed / (1024.0 * 1024.0)
}

unsafe fn bench_random_latency(base: *mut u8, size: usize, accesses: usize) -> f64 {
    if accesses == 0 { return 0.0; }

    let n_nodes = size / CACHE_LINE_SIZE;

    // Build a single-cycle pointer chase using Sattolo's algorithm.
    // Each cache line's first usize stores the byte offset of the next line.
    // Sattolo (j < i, not j <= i) guarantees exactly one cycle, so every
    // access hits a unique cache line — no short cycles inflating cache hits.
    let mut rng: u64 = 0xdeadbeef12345678;
    let lcg = |r: &mut u64| -> u64 {
        *r = r.wrapping_mul(6364136223846793005)
              .wrapping_add(1442695040888963407);
        *r
    };
    for i in 0..n_nodes {
        *(base.add(i * CACHE_LINE_SIZE) as *mut usize) = i * CACHE_LINE_SIZE;
    }
    for i in (1..n_nodes).rev() {
        let j = (lcg(&mut rng) as usize) % i;
        let pa = base.add(i * CACHE_LINE_SIZE) as *mut usize;
        let pb = base.add(j * CACHE_LINE_SIZE) as *mut usize;
        let tmp = *pa;
        *pa = *pb;
        *pb = tmp;
    }

    for i in (0..size).step_by(CACHE_LINE_SIZE) {
        std::arch::x86_64::_mm_clflush(base.add(i));
    }
    std::arch::x86_64::_mm_mfence();

    std::arch::x86_64::_mm_lfence();
    let start = std::arch::x86_64::_rdtsc();
    std::arch::x86_64::_mm_lfence();

    let mut off: usize = 0;
    for _ in 0..accesses {
        off = std::ptr::read_volatile(base.add(off) as *const usize);
    }

    std::arch::x86_64::_mm_lfence();
    let end = std::arch::x86_64::_rdtsc();
    std::hint::black_box(off);

    (end - start) as f64 / accesses as f64 / TSC_GHZ
}

/// Squared deviation scoring — NaN safe
fn compute_score(read_bw: f64, write_bw: f64, latency_ns: f64,
                 max_read: f64, max_write: f64, min_latency: f64) -> f64 {
    if max_read == 0.0 || max_write == 0.0 || latency_ns == 0.0 {
        return 0.0;
    }
    let rs = (read_bw  / max_read).min(1.0).powi(2);
    let ws = (write_bw / max_write).min(1.0).powi(2);
    let ls = (min_latency / latency_ns).min(1.0).powi(2);
    let score = (rs * 0.40 + ws * 0.30 + ls * 0.30) * 100.0;
    if score.is_nan() || score.is_infinite() { 0.0 } else { score }
}

/// Build contiguous physical blocks from address-sorted stats
fn find_contiguous_blocks(stats_by_paddr: &[RegionStats]) -> Vec<ContiguousBlock> {
    let mut blocks: Vec<ContiguousBlock> = Vec::new();
    if stats_by_paddr.is_empty() { return blocks; }

    let make_block = |start: usize, end: usize, stats: &[RegionStats]| -> ContiguousBlock {
        let run = &stats[start..=end];
        let n = run.len() as f64;
        ContiguousBlock {
            start_paddr:  run.first().unwrap().paddr,
            end_paddr:    run.last().unwrap().paddr + HUGE_PAGE_SIZE,
            size_mb:      run.len() * 2,
            avg_score:    run.iter().map(|s| s.score).sum::<f64>() / n,
            avg_read_bw:  run.iter().map(|s| s.seq_read_bw).sum::<f64>() / n,
            avg_write_bw: run.iter().map(|s| s.seq_write_bw).sum::<f64>() / n,
            avg_latency:  run.iter().map(|s| s.rand_latency_ns).sum::<f64>() / n,
            min_score:    run.iter().map(|s| s.score).fold(f64::MAX, f64::min),
        }
    };

    let mut run_start = 0usize;
    for i in 1..stats_by_paddr.len() {
        if stats_by_paddr[i].paddr != stats_by_paddr[i-1].paddr + HUGE_PAGE_SIZE {
            blocks.push(make_block(run_start, i-1, stats_by_paddr));
            run_start = i;
        }
    }
    blocks.push(make_block(run_start, stats_by_paddr.len()-1, stats_by_paddr));
    blocks
}

fn parse_args() -> (BenchMode, bool, Option<usize>) {
    let args: Vec<String> = std::env::args().collect();
    let mut mode = BenchMode::fast();
    let mut verbose = false;
    let mut limit_mb: Option<usize> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--fast"     | "-f" => mode = BenchMode::fast(),
            "--balanced" | "-b" => mode = BenchMode::balanced(),
            "--precise"  | "-p" => mode = BenchMode::precise(),
            "--verbose"  | "-v" => verbose = true,
            "--limit"    | "-l" => {
                i += 1;
                if i < args.len() { limit_mb = args[i].parse().ok(); }
            }
            "--help" | "-h" => {
                println!("dram-mapper — Physical RAM Region Benchmarker");
                println!();
                println!("USAGE:  sudo ./dram-mapper [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!("  -f, --fast          Fast mode (default, ~2 min full map)");
                println!("  -b, --balanced      Balanced mode (~10 min full map)");
                println!("  -p, --precise       Precise mode (~40 min full map)");
                println!("  -v, --verbose       Show all regions in table");
                println!("  -l, --limit <MB>    Cap allocation at MB");
                println!("                      (default: MemAvailable - 4GB)");
                println!("  -h, --help          This help");
                println!();
                println!("EXAMPLES:");
                println!("  sudo ./dram-mapper --fast             # full map, fast");
                println!("  sudo ./dram-mapper --balanced         # full map, accurate");
                println!("  sudo ./dram-mapper --fast --limit 8192 # test 8GB only");
                std::process::exit(0);
            }
            _ => eprintln!("Unknown argument: {} (try --help)", args[i]),
        }
        i += 1;
    }
    (mode, verbose, limit_mb)
}

fn flush_stdout() {
    use std::io::Write;
    std::io::stdout().flush().ok();
}

struct HugePageGuard;

impl Drop for HugePageGuard {
    fn drop(&mut self) {
        release_huge_pages();
    }
}

fn main() {
    let (mode, verbose, limit_mb) = parse_args();

    println!("╔══════════════════════════════════════════════════╗");
    println!("║         DRAM Physical Region Mapper v2           ║");
    println!("║   Ryzen 9 9950X3D / DDR5 @ 3600 MT/s            ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    // Safety: release huge pages on Ctrl+C or panic
    let _huge_page_guard = HugePageGuard;
    ctrlc::set_handler(|| {
        release_huge_pages();
        eprintln!("\nInterrupted — huge pages released.");
        std::process::exit(1);
    }).expect("Failed to set Ctrl+C handler");

    let mem_available_mb = read_meminfo_mb("MemAvailable:");
    let mem_total_mb     = read_meminfo_mb("MemTotal:");

    println!("Memory:   {} MB total, {} MB available", mem_total_mb, mem_available_mb);

    let safe_mb = if let Some(lim) = limit_mb {
        lim.min(mem_available_mb.saturating_sub(SAFETY_BUFFER_MB))
    } else {
        mem_available_mb.saturating_sub(SAFETY_BUFFER_MB)
    };
    let safe_mb    = (safe_mb / 2) * 2;
    if safe_mb < MIN_ALLOC_MB {
        eprintln!("Error: only {} MB usable, need at least {} MB.", safe_mb, MIN_ALLOC_MB);
        std::process::exit(1);
    }
    let num_pages  = safe_mb / 2;
    let alloc_size = num_pages * HUGE_PAGE_SIZE;

    println!("Safety:   {} MB reserved for OS", SAFETY_BUFFER_MB);
    println!("Mapping:  {} MB ({} × 2MB huge pages)", safe_mb, num_pages);
    println!("Mode:     {} | passes={} random={} warmup={}",
        mode.name, mode.seq_passes, mode.random_accesses, mode.warmup_passes);
    println!();

    print!("Reserving huge pages... ");
    flush_stdout();
    reserve_huge_pages(num_pages);
    println!("done");

    print!("Allocating {} MB... ", safe_mb);
    flush_stdout();
    let ptr = unsafe {
        mmap(ptr::null_mut(), alloc_size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB,
            -1, 0)
    };
    if ptr == libc::MAP_FAILED {
        eprintln!("Failed. Check huge page reservation.");
        eprintln!("Run: sudo sh -c 'echo {} > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages'",
            num_pages);
        release_huge_pages();
        std::process::exit(1);
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, alloc_size) };
    for i in (0..alloc_size).step_by(HUGE_PAGE_SIZE) { slice[i] = 1; }
    println!("done");

    print!("Resolving physical addresses... ");
    flush_stdout();
    let base_vaddr = ptr as usize;
    let mut pagemap = File::open("/proc/self/pagemap")
        .expect("Cannot open pagemap — run as root");
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for i in 0..num_pages {
        let vaddr = base_vaddr + i * HUGE_PAGE_SIZE;
        if let Some(paddr) = virtual_to_physical(&mut pagemap, vaddr) {
            regions.push((vaddr, paddr));
        }
    }
    println!("{} regions", regions.len());
    println!("Physical range: 0x{:016x} – 0x{:016x}",
        regions.iter().map(|r| r.1).min().unwrap(),
        regions.iter().map(|r| r.1).max().unwrap());
    println!();

    // Benchmark every region
    println!("Benchmarking {} regions...", regions.len());
    let bench_start = Instant::now();
    let mut stats: Vec<RegionStats> = Vec::new();

    for (idx, &(vaddr, paddr)) in regions.iter().enumerate() {
        if idx % 64 == 0 {
            let pct     = idx * 100 / regions.len();
            let elapsed = bench_start.elapsed().as_secs_f64();
            let eta     = if idx > 0 {
                elapsed / idx as f64 * (regions.len() - idx) as f64
            } else { 0.0 };
            print!("\r  [{:3}%] {}/{} | {:.0}s elapsed | ETA {:.0}s    ",
                pct, idx, regions.len(), elapsed, eta);
            flush_stdout();
        }

        let base_ptr = vaddr as *mut u8;
        let read_bw  = unsafe { bench_seq_read(base_ptr as *const u8,
            HUGE_PAGE_SIZE, mode.seq_passes, mode.warmup_passes) };
        let write_bw = unsafe { bench_seq_write(base_ptr,
            HUGE_PAGE_SIZE, mode.seq_passes, mode.warmup_passes) };
        let latency  = unsafe { bench_random_latency(base_ptr,
            HUGE_PAGE_SIZE, mode.random_accesses) };

        stats.push(RegionStats {
            paddr,
            seq_read_bw:     read_bw,
            seq_write_bw:    write_bw,
            rand_latency_ns: latency,
            score:           0.0,
        });
    }
    println!("\r  [100%] {}/{} | {:.1}s total                          ",
        regions.len(), regions.len(), bench_start.elapsed().as_secs_f64());

    // Compute scores — NaN safe
    let max_read    = stats.iter().map(|s| s.seq_read_bw)
        .filter(|v| v.is_finite()).fold(0.0_f64, f64::max);
    let max_write   = stats.iter().map(|s| s.seq_write_bw)
        .filter(|v| v.is_finite()).fold(0.0_f64, f64::max);
    let min_latency = stats.iter().map(|s| s.rand_latency_ns)
        .filter(|v| v.is_finite() && *v > 0.0).fold(f64::MAX, f64::min);

    for s in &mut stats {
        s.score = compute_score(s.seq_read_bw, s.seq_write_bw, s.rand_latency_ns,
            max_read, max_write, min_latency);
    }

    stats.sort_by_key(|s| s.paddr);

    let mut by_score = stats.clone();
    by_score.sort_by(|a, b| b.score.total_cmp(&a.score));

    // Summary statistics — filter non-finite values
    let finite_stats: Vec<&RegionStats> = stats.iter()
        .filter(|s| s.score.is_finite() && s.score > 0.0)
        .collect();
    let n = finite_stats.len() as f64;

    let avg_read    = finite_stats.iter().map(|s| s.seq_read_bw).sum::<f64>()     / n;
    let avg_write   = finite_stats.iter().map(|s| s.seq_write_bw).sum::<f64>()    / n;
    let avg_latency = finite_stats.iter().map(|s| s.rand_latency_ns).sum::<f64>() / n;

    let mut scores_sorted: Vec<f64> = finite_stats.iter().map(|s| s.score).collect();
    scores_sorted.sort_by(|a, b| a.total_cmp(b));

    let p10 = scores_sorted[(scores_sorted.len() / 10).max(0)];
    let p50 = scores_sorted[scores_sorted.len() / 2];
    let p90 = scores_sorted[(scores_sorted.len() * 9 / 10).min(scores_sorted.len() - 1)];

    println!();
    println!("══════════════════════════════════════════════════════════════════════");
    println!(" RESULTS — Ranked by Performance Score");
    println!("══════════════════════════════════════════════════════════════════════");
    println!();
    println!("  Regions benchmarked: {}", stats.len());
    println!("  Best  read:  {:8.1} MB/s  |  Best  write:  {:8.1} MB/s  |  Best  lat:  {:6.2} ns",
        max_read, max_write, min_latency);
    println!("  Avg   read:  {:8.1} MB/s  |  Avg   write:  {:8.1} MB/s  |  Avg   lat:  {:6.2} ns",
        avg_read, avg_write, avg_latency);
    println!("  Score  p10: {:.1}  |  p50: {:.1}  |  p90: {:.1}", p10, p50, p90);
    println!();

    let show = if verbose { by_score.len() } else { 20.min(by_score.len()) };

    println!("  {:>5}  {:>18}  {:>10}  {:>10}  {:>11}  {:>6}",
        "Rank", "Physical Addr", "Read MB/s", "Write MB/s", "Latency ns", "Score");
    println!("  {}", "-".repeat(72));

    for (rank, s) in by_score.iter().take(show).enumerate() {
        let star = if rank < 10 { "★" } else { " " };
        println!("  {:>4}{} 0x{:016x}  {:>10.1}  {:>10.1}  {:>11.2}  {:>6.1}",
            rank + 1, star, s.paddr,
            s.seq_read_bw, s.seq_write_bw, s.rand_latency_ns, s.score);
    }
    if !verbose && by_score.len() > 20 {
        println!("  ... {} more hidden (--verbose to show all)", by_score.len() - 20);
    }

    println!();
    println!("  Worst 5 regions:");
    println!("  {:>5}  {:>18}  {:>10}  {:>10}  {:>11}  {:>6}",
        "Rank", "Physical Addr", "Read MB/s", "Write MB/s", "Latency ns", "Score");
    println!("  {}", "-".repeat(72));
    for (i, s) in by_score.iter().rev().take(5).enumerate() {
        println!("  {:>4}  0x{:016x}  {:>10.1}  {:>10.1}  {:>11.2}  {:>6.1}",
            by_score.len() - i, s.paddr,
            s.seq_read_bw, s.seq_write_bw, s.rand_latency_ns, s.score);
    }

    // Contiguous block analysis
    let mut blocks = find_contiguous_blocks(&stats);

    // Sort: prefer larger blocks unless score difference exceeds noise floor
    blocks.sort_by(|a, b| {
        let score_diff = (b.avg_score - a.avg_score).abs();
        if score_diff < SCORE_NOISE_FLOOR {
            b.size_mb.cmp(&a.size_mb)
        } else {
            b.avg_score.total_cmp(&a.avg_score)
        }
    });

    let useful_blocks: Vec<&ContiguousBlock> = blocks.iter()
        .filter(|b| b.size_mb >= 256)
        .collect();

    println!();
    println!("══════════════════════════════════════════════════════════════════════");
    println!(" CONTIGUOUS BLOCK ANALYSIS");
    println!("══════════════════════════════════════════════════════════════════════");
    println!();
    println!("  Total contiguous runs found: {}", blocks.len());
    println!("  Runs ≥ 256MB:                {}", useful_blocks.len());
    println!();

    if useful_blocks.is_empty() {
        println!("  No contiguous blocks ≥ 256MB found.");
        println!("  Try running without --limit, or reboot to reduce fragmentation.");
    } else {
        println!("  {:>5}  {:>16}  {:>16}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
            "Rank", "Phys Start", "Phys End", "Size MB",
            "Rd MB/s", "Wr MB/s", "Lat ns", "Score");
        println!("  {}", "-".repeat(85));

        for (i, b) in useful_blocks.iter().take(10).enumerate() {
            let star = if i == 0 { "★" } else { " " };
            println!("  {:>4}{} 0x{:014x}  0x{:014x}  {:>8}  {:>8.0}  {:>8.0}  {:>8.2}  {:>8.1}",
                i + 1, star,
                b.start_paddr, b.end_paddr,
                b.size_mb,
                b.avg_read_bw, b.avg_write_bw,
                b.avg_latency, b.avg_score);
        }
    }

    // LLM Placement Recommendations
    println!();
    println!("══════════════════════════════════════════════════════════════════════");
    println!(" LLM PLACEMENT RECOMMENDATIONS");
    println!("══════════════════════════════════════════════════════════════════════");
    println!();

    if useful_blocks.is_empty() {
        println!("  Cannot make placement recommendation — no suitable block found.");
        println!("  Run without --limit to map the full available memory.");
    } else {
        let best = useful_blocks[0];

        println!("  Selected block:");
        println!("  Physical range:  0x{:016x} – 0x{:016x}",
            best.start_paddr, best.end_paddr);
        println!("  Size:            {} MB", best.size_mb);
        println!("  Performance:     {:.0} MB/s read  |  {:.0} MB/s write  |  {:.2} ns latency",
            best.avg_read_bw, best.avg_write_bw, best.avg_latency);
        println!("  Score:           {:.1} avg  |  {:.1} worst region in block",
            best.avg_score, best.min_score);
        println!();

        // Model fit analysis
        let mb = best.size_mb;
        println!("  Model fit analysis ({} MB available):", mb);
        let models: &[(&str, usize, &str)] = &[
            ("70B (Q4_K_M)", 40000, "~40 GB"),
            ("34B (Q4_K_M)", 20000, "~20 GB"),
            ("13B (Q4_K_M)",  8000, "~8 GB"),
            ("7B  (Q4_K_M)",  4000, "~4 GB"),
            ("3B  (Q4_K_M)",  2000, "~2 GB"),
            ("1B  (Q4_K_M)",   800, "~0.8 GB"),
        ];
        for (name, required_mb, size_str) in models {
            let fits = mb >= *required_mb;
            let marker = if fits { "✓" } else { "✗" };
            println!("  {}  {}  ({}){}",
                marker, name, size_str,
                if fits { "" } else { " — insufficient space" });
        }
        println!();

        // Component layout
        // KV cache: 15% — latency critical, placed first
        // Weights:  70% — bandwidth critical
        // Embeddings: remainder — sparse access
        let kv_mb     = ((mb as f64 * 0.15) as usize / 2) * 2;
        let wt_mb     = ((mb as f64 * 0.70) as usize / 2) * 2;
        let emb_mb    = mb.saturating_sub(kv_mb + wt_mb);
        let kv_start  = best.start_paddr;
        let wt_start  = kv_start + kv_mb * 1024 * 1024;
        let emb_start = wt_start + wt_mb * 1024 * 1024;

        println!("  Recommended component layout:");
        println!();
        println!("  ┌─ KV Cache       {:6} MB  →  0x{:016x}", kv_mb, kv_start);
        println!("  │   Latency critical. Placed at block start (lowest latency).");
        println!("  │");
        println!("  ├─ Model Weights  {:6} MB  →  0x{:016x}", wt_mb, wt_start);
        println!("  │   Bandwidth critical. Sequential reads during every forward pass.");
        println!("  │");
        if emb_mb > 0 {
            println!("  └─ Embeddings     {:6} MB  →  0x{:016x}", emb_mb, emb_start);
            println!("      Sparse random access. Least latency sensitive component.");
        }
        println!();

        println!("  ── Allocator constants ──────────────────────────────────────────");
        println!("  BLOCK_BASE    = 0x{:016x}", best.start_paddr);
        println!("  BLOCK_SIZE_MB = {}", mb);
        println!("  KV_OFFSET     = 0x{:016x}  ({} MB)", 0usize, kv_mb);
        println!("  WT_OFFSET     = 0x{:016x}  ({} MB)",
            kv_mb * 1024 * 1024, wt_mb);
        if emb_mb > 0 {
            println!("  EMB_OFFSET    = 0x{:016x}  ({} MB)",
                (kv_mb + wt_mb) * 1024 * 1024, emb_mb);
        }
        println!("  ─────────────────────────────────────────────────────────────────");
    }

    // Cleanup — always release memory and huge pages
    unsafe { munmap(ptr, alloc_size) };

    print!("Releasing huge pages... ");
    flush_stdout();
    release_huge_pages();
    println!("done");

    println!();
    println!("Done. Memory released cleanly.");
}