//! Optional standalone benchmark support; absent from production builds.
use super::*;
use crossbeam_channel::bounded;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::Mutex;
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAGIC: &[u8; 8] = b"NFMSCAN1";
const HEADER: &str = "mode,dataset,cpus,run,seconds,nodes,unique_names,name_bytes,nodes_per_second";
const MEMORY_COLUMNS: &str = ",private_start_bytes,private_end_bytes,private_peak_bytes,private_end_delta_bytes,private_peak_delta_bytes,memory_samples";

// Local FFI keeps the optional harness compatible with older baseline manifests.
#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_faults: u32,
    peak_working_set: usize,
    working_set: usize,
    peak_paged_pool: usize,
    paged_pool: usize,
    peak_nonpaged_pool: usize,
    nonpaged_pool: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        size: u32,
    ) -> i32;
}

fn private_bytes() -> io::Result<usize> {
    let mut counters = ProcessMemoryCounters::default();
    counters.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    // SAFETY: counters has the PROCESS_MEMORY_COUNTERS_EX layout and size;
    // GetCurrentProcess returns a pseudo-handle valid for this call.
    if unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<ProcessMemoryCounters>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(counters.private_usage)
}

struct MemorySampler {
    start: usize,
    stop: Sender<()>,
    worker: Option<std::thread::JoinHandle<io::Result<(usize, usize)>>>,
}

impl MemorySampler {
    fn start() -> Result<Self> {
        let (stop, receiver) = bounded(1);
        let (ready, started) = bounded(1);
        let worker = std::thread::spawn(move || {
            let initial = private_bytes();
            let start = *initial.as_ref().unwrap_or(&0);
            let valid = initial.is_ok();
            let _ = ready.send(initial);
            if !valid {
                return Ok((0, 0));
            }
            let mut peak = start;
            let mut samples = 1;
            loop {
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        peak = peak.max(private_bytes()?);
                        samples += 1;
                    }
                    _ => return Ok((peak, samples)),
                }
            }
        });
        let mut sampler = Self {
            start: 0,
            stop,
            worker: Some(worker),
        };
        sampler.start = started.recv()??;
        Ok(sampler)
    }

    fn finish(mut self) -> Result<(usize, usize, usize, usize)> {
        // Caller still owns the final snapshot at this point.
        let end = private_bytes()?;
        let _ = self.stop.send(());
        let (peak, samples) = self
            .worker
            .take()
            .unwrap()
            .join()
            .map_err(|_| "memory sampler panicked")??;
        Ok((self.start, end, peak.max(end), samples + 1))
    }
}

impl Drop for MemorySampler {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Measurements {
    identity: Vec<String>,
    times: Vec<f64>,
    memory: Vec<[f64; 3]>,
}

fn parse_measurements(csv: &str) -> Result<Measurements> {
    let mut lines = csv.trim_start_matches('\u{feff}').lines();
    let header = lines.next().unwrap_or("");
    let with_memory = header == format!("{HEADER}{MEMORY_COLUMNS}");
    if header != HEADER && !with_memory {
        return Err("unsupported CSV header; generate a new baseline".into());
    }
    let mut identity = None;
    let mut times = Vec::new();
    let mut memory = Vec::new();
    for line in lines {
        let fields: Vec<_> = line.split(',').collect();
        if fields.len() != if with_memory { 15 } else { 9 }
            || !matches!(fields[0], "scan" | "replay")
        {
            return Err("invalid measurement row".into());
        }
        let key: Vec<_> = [0, 1, 2, 5, 6, 7]
            .iter()
            .map(|&i| fields[i].to_owned())
            .collect();
        if identity.as_ref().is_some_and(|old| old != &key) {
            return Err("inconsistent dataset or counts in CSV".into());
        }
        identity = Some(key);
        if fields[3].parse::<usize>()? != times.len() + 1 {
            return Err("incomplete or unordered runs".into());
        }
        let time = fields[4].parse::<f64>()?;
        if !time.is_finite() || time <= 0.0 {
            return Err("invalid elapsed time".into());
        }
        times.push(time);
        if with_memory {
            let start = fields[9].parse::<usize>()?;
            let end = fields[10].parse::<usize>()?;
            let peak = fields[11].parse::<usize>()?;
            if peak < start.max(end) || fields[14].parse::<usize>()? < 2 {
                return Err("invalid memory samples".into());
            }
            memory.push([start as f64, end as f64, peak as f64]);
        }
    }
    let identity = identity.ok_or("CSV has no measurements")?;
    times.sort_by(f64::total_cmp);
    Ok(Measurements {
        identity,
        times,
        memory,
    })
}

fn median(times: &[f64]) -> f64 {
    (times[(times.len() - 1) / 2] + times[times.len() / 2]) / 2.0
}

fn regression(baseline: &Measurements, current: &Measurements, limit: f64) -> Result<(f64, bool)> {
    if !limit.is_finite() || limit < 0.0 {
        return Err("threshold must be finite and nonnegative".into());
    }
    if baseline.identity != current.identity
        || baseline.times.len() != current.times.len()
        || baseline.memory.len() != current.memory.len()
    {
        return Err("mode, dataset, CPU count, result counts, and run count must match".into());
    }
    let change = (median(&current.times) / median(&baseline.times) - 1.0) * 100.0;
    Ok((change, change > limit))
}

fn compare(baseline: &str, current: &str, limit: f64) -> Result<()> {
    let baseline = parse_measurements(&std::fs::read_to_string(baseline)?)?;
    let current = parse_measurements(&std::fs::read_to_string(current)?)?;
    let (change, failed) = regression(&baseline, &current, limit)?;
    if !baseline.memory.is_empty() {
        for (index, label) in ["start", "end", "sampled peak"].iter().enumerate() {
            let mut before: Vec<_> = baseline.memory.iter().map(|m| m[index]).collect();
            let mut after: Vec<_> = current.memory.iter().map(|m| m[index]).collect();
            before.sort_by(f64::total_cmp);
            after.sort_by(f64::total_cmp);
            println!(
                "private {label}: baseline={:.2} MiB current={:.2} MiB delta={:+.2} MiB",
                median(&before) / 1048576.0,
                median(&after) / 1048576.0,
                (median(&after) - median(&before)) / 1048576.0
            );
        }
        println!("Memory run: timing is diagnostic only; no timing regression threshold applied.");
        return Ok(());
    }
    println!(
        "baseline median={:.6}s current median={:.6}s change={change:+.2}% threshold={limit:.2}%",
        median(&baseline.times),
        median(&current.times)
    );
    if failed {
        return Err("performance regression exceeds threshold".into());
    }
    println!("PASS: no regression above the configured threshold");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sampler_reads_private_commit_and_stops() {
        let sampler = MemorySampler::start().unwrap();
        let allocation = vec![42u8; 1024 * 1024];
        let (start, end, peak, samples) = sampler.finish().unwrap();
        std::hint::black_box(&allocation);
        assert!(start > 0 && end > 0);
        assert!(peak >= start.max(end));
        assert!(samples >= 2);
    }

    #[test]
    fn memory_csv_cannot_be_compared_with_unsampled_timing() {
        let plain = format!("{HEADER}\nreplay,abc,8,1,1.0,10,5,20,10\n");
        let memory = format!(
            "{HEADER}{MEMORY_COLUMNS}\nreplay,abc,8,1,1.0,10,5,20,10,100,150,200,50,100,3\n"
        );
        let parsed = parse_measurements(&memory).unwrap();
        assert_eq!(parsed.memory, vec![[100.0, 150.0, 200.0]]);
        assert!(regression(&parse_measurements(&plain).unwrap(), &parsed, 10.0).is_err());
    }

    #[test]
    fn comparisons_detect_regressions_and_reject_incompatible_data() {
        let csv =
            format!("{HEADER}\nreplay,abc,8,1,1.0,10,5,20,10\nreplay,abc,8,2,1.2,10,5,20,8\n");
        let base = parse_measurements(&csv).unwrap();
        assert!(!regression(&base, &base, 10.0).unwrap().1);
        let slower = Measurements {
            identity: base.identity.clone(),
            times: vec![1.2, 1.4],
            memory: Vec::new(),
        };
        assert!(regression(&base, &slower, 10.0).unwrap().1);
        let other = parse_measurements(&csv.replace("abc", "def")).unwrap();
        assert!(regression(&base, &other, 10.0).is_err());
        assert!(regression(&base, &base, f64::NAN).is_err());
        assert!(parse_measurements(&csv.replace("1.0", "NaN")).is_err());
        assert!(parse_measurements(HEADER).is_err());
    }

    #[test]
    fn replay_preserves_interning_and_snapshot_contents() {
        let names: Vec<_> = (0..1500).map(|i| format!("name-é-{}", i % 300)).collect();
        {
            let mut store = CompactUtf8FileStore::new();
            store.name_bytes = ChunkedStorage::new(16);
            for name in &names {
                store.add_node(-1, name);
            }
            store.complete_adding();
            let snapshot = store.snapshot();
            assert_eq!(snapshot.node_count, names.len());
            assert_eq!(snapshot.name_count, 300);
            for (index, expected) in names.iter().enumerate() {
                let node = snapshot.nodes[index];
                assert_eq!(node.name as usize, index % 300);
                let name = snapshot.names[node.name as usize];
                let mut heap = Vec::new();
                let bytes = snapshot.name_bytes.get_range(
                    name.offset as usize,
                    name.len as usize,
                    &mut [],
                    &mut heap,
                );
                assert_eq!(bytes, expected.as_bytes());
            }
        }
    }
}

struct Sink {
    latest: Mutex<Option<Arc<PublishedSnapshot>>>,
    done: Sender<ScanStatus>,
}

impl ScanEventSink for Sink {
    fn snapshot(&self, snapshot: Arc<PublishedSnapshot>) {
        *self.latest.lock().unwrap() = Some(snapshot);
    }
    fn complete(&self, status: ScanStatus) {
        let _ = self.done.send(status);
    }
}

fn scan_once(roots: &[PathBuf]) -> Result<(Duration, Arc<PublishedSnapshot>)> {
    let (done, receiver) = bounded(1);
    let sink = Arc::new(Sink {
        latest: Mutex::new(None),
        done,
    });
    let start = Instant::now();
    let _scan = start_scan(
        ScanOptions {
            roots: roots.to_vec(),
            max_depth: 0,
            directories_only: false,
            files_only: false,
        },
        Arc::clone(&sink),
    );
    let status = receiver.recv()?;
    let elapsed = start.elapsed();
    if status != ScanStatus::Completed {
        return Err(format!("scan ended with {status:?}").into());
    }
    let snapshot = sink
        .latest
        .lock()
        .unwrap()
        .take()
        .ok_or("missing final snapshot")?;
    Ok((elapsed, snapshot))
}

fn record(path: &str, roots: &[PathBuf]) -> Result<()> {
    // Refuse overwriting an existing recording.
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let (elapsed, snapshot) = scan_once(roots)?;
    let mut out = BufWriter::new(file);
    out.write_all(MAGIC)?;
    out.write_all(&(snapshot.node_count as u64).to_le_bytes())?;
    let mut heap = Vec::new();
    for index in 0..snapshot.node_count {
        let node = snapshot.nodes[index];
        let name = snapshot.names[node.name as usize];
        // get_range was public before copy_range_to, so the harness can also
        // compile against older baseline revisions. Recording is not timed.
        let bytes = snapshot.name_bytes.get_range(
            name.offset as usize,
            name.len as usize,
            &mut [],
            &mut heap,
        );
        out.write_all(&node.parent.to_le_bytes())?;
        out.write_all(&name.len.to_le_bytes())?;
        out.write_all(bytes)?;
    }
    out.flush()?;
    eprintln!(
        "Recorded {} nodes in {:.3}s (serialization excluded)",
        snapshot.node_count,
        elapsed.as_secs_f64()
    );
    Ok(())
}

fn read_u32(input: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn load(path: &str) -> Result<Vec<(i32, String)>> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut input = BufReader::new(file);
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err("invalid recording header".into());
    }
    let mut count = [0; 8];
    input.read_exact(&mut count)?;
    let count = usize::try_from(u64::from_le_bytes(count))?;
    if count as u64 > file_len.saturating_sub(16) / 8 {
        return Err("invalid recording count".into());
    }
    let mut records = Vec::new();
    for index in 0..count {
        let parent = read_u32(&mut input)? as i32;
        if parent < -1 || (parent >= 0 && parent as usize >= index) {
            return Err("invalid parent index".into());
        }
        let len = read_u32(&mut input)? as usize;
        if len > 16 * 1024 * 1024 {
            return Err("recorded name exceeds 16 MiB".into());
        }
        let mut bytes = vec![0; len];
        input.read_exact(&mut bytes)?;
        records.push((parent, String::from_utf8(bytes)?));
    }
    if input.read(&mut [0; 1])? != 0 {
        return Err("trailing recording data".into());
    }
    Ok(records)
}

fn replay(records: &[(i32, String)]) -> (Duration, Arc<PublishedSnapshot>) {
    let start = Instant::now();
    let mut store = CompactUtf8FileStore::new();
    for (parent, name) in records {
        store.add_node(*parent, name);
    }
    store.complete_adding();
    let snapshot = store.snapshot();
    let elapsed = start.elapsed();
    std::hint::black_box(&snapshot);
    (elapsed, snapshot)
}

/// Record, measure, or compare CSV measurements against a baseline.
pub fn run() -> Result<()> {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let memory = args.last().is_some_and(|arg| arg == "--memory");
    if memory {
        args.pop();
    }
    let usage = "usage: scan_bench record FILE ROOT... | replay FILE [RUNS] | scan ROOT [RUNS] | compare BASELINE.csv CURRENT.csv [MAX_REGRESSION_PERCENT]";
    if args.len() < 2 {
        return Err(usage.into());
    }
    if memory && !matches!(args[0].as_str(), "scan" | "replay") {
        return Err("--memory is only supported for replay or scan".into());
    }
    if args[0] == "compare" {
        if !(3..=4).contains(&args.len()) {
            return Err(usage.into());
        }
        let limit = args
            .get(3)
            .map(|s| s.parse::<f64>())
            .transpose()?
            .unwrap_or(10.0);
        return compare(&args[1], &args[2], limit);
    }
    if args[0] == "record" {
        if args.len() < 3 {
            return Err(usage.into());
        }
        let roots: Vec<_> = args[2..].iter().map(PathBuf::from).collect();
        for root in &roots {
            if !root.is_dir() {
                return Err(format!("not a directory: {}", root.display()).into());
            }
        }
        return record(&args[1], &roots);
    }
    if args.len() > 3 {
        return Err(usage.into());
    }
    let runs = args
        .get(2)
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(5);
    if runs == 0 {
        return Err("RUNS must be positive".into());
    }
    let records = match args[0].as_str() {
        "replay" => Some(load(&args[1])?),
        "scan" if PathBuf::from(&args[1]).is_dir() => None,
        _ => return Err(usage.into()),
    };
    let roots = [PathBuf::from(&args[1])];
    // Fixed checksum for dataset identity, independent of production hashing.
    // Done outside timing. A checksum is not a cryptographic authenticity check.
    let mut identity = 0xcbf29ce484222325u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            identity = (identity ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    };
    if let Some(records) = &records {
        for (parent, name) in records {
            feed(&parent.to_le_bytes());
            feed(&(name.len() as u64).to_le_bytes());
            feed(name.as_bytes());
        }
    } else {
        feed(
            std::fs::canonicalize(&roots[0])?
                .to_string_lossy()
                .as_bytes(),
        );
    }
    let cpus = std::thread::available_parallelism()?.get();
    let mut times = Vec::new();
    let mut expected = None;
    println!("{HEADER}{}", if memory { MEMORY_COLUMNS } else { "" });
    for run in 0..=runs {
        let sampler = if memory && run > 0 {
            Some(MemorySampler::start()?)
        } else {
            None
        };
        let (elapsed, snapshot) = if let Some(records) = &records {
            replay(records)
        } else {
            scan_once(&roots)?
        };
        let memory_usage = sampler.map(MemorySampler::finish).transpose()?;
        let counts = (
            snapshot.node_count,
            snapshot.name_count,
            snapshot.byte_count,
        );
        if let Some(expected) = expected {
            if counts != expected {
                return Err("counts changed between runs; dataset is not stable".into());
            }
        } else {
            expected = Some(counts);
        }
        if run == 0 {
            eprintln!("Warm-up complete: {counts:?}");
            continue;
        }
        let seconds = elapsed.as_secs_f64();
        let memory_csv = memory_usage
            .map(|(start, end, peak, samples)| {
                format!(
                    ",{start},{end},{peak},{},{},{samples}",
                    end as i128 - start as i128,
                    peak as i128 - start as i128
                )
            })
            .unwrap_or_default();
        println!(
            "{},{identity:016x},{cpus},{run},{seconds:.9},{},{},{},{:.0}{memory_csv}",
            args[0],
            counts.0,
            counts.1,
            counts.2,
            counts.0 as f64 / seconds
        );
        times.push(seconds);
    }
    times.sort_by(f64::total_cmp);
    let median = (times[(runs - 1) / 2] + times[runs / 2]) / 2.0;
    eprintln!(
        "median={median:.6}s min={:.6}s max={:.6}s",
        times[0],
        times[runs - 1]
    );
    Ok(())
}
