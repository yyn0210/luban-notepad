//! Disk engine performance benchmark with explicit budgets.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p luban-core --bench disk_engine_perf
//! ```
//!
//! The benchmark prints one JSON line per measurement and exits non-zero when a
//! measurement exceeds its budget. Environment:
//!
//! - `LUBAN_PERF_SCALE` (default `1.0`): scales the generated file sizes and the
//!   size-dependent budgets, for quick local checks.
//! - `LUBAN_PERF_BUDGET_FACTOR` (default `1.0`): multiplies every budget. The
//!   `--self-test` mode of `scripts/perf-nightly.sh` sets it very low to prove
//!   that an over-budget run fails.
//! - `LUBAN_PERF_DIR`: keeps the generated files in this directory instead of a
//!   temporary directory.

use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Duration, Instant};

use luban_core::disk_engine::DiskEngine;
use luban_core::search::{SearchEngine, SearchOptions};

const MB: u64 = 1024 * 1024;

/// Roadmap §5 targets, in milliseconds, before the CI factor.
const ROADMAP_ENGINE_OPEN_1GB_MS: u128 = 3_000;
const ROADMAP_ENGINE_FIND_1GB_MS: u128 = 5_000;

/// CI factors: shared runners are slower and use software rendering.
const ENGINE_BUDGET_FACTOR: f64 = 1.5;
/// The save measurement has no roadmap target; the budget is a broad regression
/// guard for a 1 GB atomic save.
const SAVE_REGRESSION_BUDGET_1GB_MS: u128 = 10_000;
/// Roadmap allows 100 MB RSS growth for a 3 GB file; 20% covers allocator noise.
const RSS_BUDGET_3GB_MB: u128 = 120;

const LINE: &[u8] = b"line 00000000 some text to make each line long enough to index\n";

struct Measurement {
    metric: &'static str,
    size_bytes: u64,
    measured: Duration,
    budget_ms: u128,
    roadmap_ms: Option<u128>,
    passed: bool,
    note: Option<String>,
}

impl Measurement {
    fn print(&self) {
        let measured_ms = self.measured.as_millis();
        let note = match &self.note {
            Some(note) => format!(",\"note\":{}", json_string(note)),
            None => String::new(),
        };
        let roadmap = match self.roadmap_ms {
            Some(ms) => format!(",\"roadmap_ms\":{ms}"),
            None => String::new(),
        };
        println!(
            "{{\"metric\":\"{}\",\"size_bytes\":{},\"measured_ms\":{},\"budget_ms\":{}{}{},\"passed\":{}}}",
            self.metric,
            self.size_bytes,
            measured_ms,
            self.budget_ms,
            roadmap,
            note,
            self.passed
        );
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
}

fn scaled(base: u64, scale: f64) -> u64 {
    ((base as f64 * scale) as u64).max(1)
}

fn scaled_ms(base: u128, scale: f64) -> u128 {
    ((base as f64 * scale).round() as u128).max(1)
}

fn work_dir() -> (Option<tempfile::TempDir>, PathBuf) {
    if let Ok(dir) = env::var("LUBAN_PERF_DIR") {
        let path = PathBuf::from(dir);
        fs::create_dir_all(&path).expect("create LUBAN_PERF_DIR");
        return (None, path);
    }
    let temp = tempfile::tempdir().expect("temporary benchmark directory");
    let path = temp.path().to_path_buf();
    (Some(temp), path)
}

/// Write roughly `bytes` bytes of plain text and return the time spent
/// generating the file. Generation is excluded from the measurements.
fn generate(path: &Path, bytes: u64) -> Duration {
    let started = Instant::now();
    let mut chunk = Vec::with_capacity(MB as usize + LINE.len());
    while chunk.len() < MB as usize {
        chunk.extend_from_slice(LINE);
    }
    let file = File::create(path).expect("create benchmark file");
    let mut writer = BufWriter::with_capacity(MB as usize, file);
    let mut written = 0u64;
    while written < bytes {
        let take = (bytes - written).min(chunk.len() as u64) as usize;
        writer
            .write_all(&chunk[..take])
            .expect("write benchmark file");
        written += take as u64;
    }
    writer.flush().expect("flush benchmark file");
    writer.get_ref().sync_all().expect("sync benchmark file");
    started.elapsed()
}

fn rss_kb() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse::<u64>()
                .ok();
        }
    }
    None
}

fn no_cancel() -> (AtomicBool, AtomicU64) {
    (AtomicBool::new(false), AtomicU64::new(0))
}

fn main() {
    let scale = env_f64("LUBAN_PERF_SCALE", 1.0).min(10.0);
    let factor = env_f64("LUBAN_PERF_BUDGET_FACTOR", 1.0);
    let (temp, dir) = work_dir();
    println!(
        "{{\"metric\":\"environment\",\"scale\":{scale},\"budget_factor\":{factor},\"dir\":{}}}",
        json_string(&dir.display().to_string())
    );

    let mut measurements = Vec::new();
    let one_gb = scaled(1024 * MB, scale);
    let three_gb = scaled(3 * 1024 * MB, scale);
    let open_budget =
        ((scaled_ms(ROADMAP_ENGINE_OPEN_1GB_MS, scale) as f64) * ENGINE_BUDGET_FACTOR * factor)
            .round() as u128;
    let find_budget =
        ((scaled_ms(ROADMAP_ENGINE_FIND_1GB_MS, scale) as f64) * ENGINE_BUDGET_FACTOR * factor)
            .round() as u128;
    let save_budget =
        ((scaled_ms(SAVE_REGRESSION_BUDGET_1GB_MS, scale) as f64) * factor).round() as u128;

    let text_path = dir.join("perf-text.txt");
    let generate_time = generate(&text_path, one_gb);
    println!(
        "{{\"metric\":\"generate\",\"size_bytes\":{one_gb},\"measured_ms\":{}}}",
        generate_time.as_millis()
    );

    // Open the file once for the open measurement.
    let (cancel, progress) = no_cancel();
    let started = Instant::now();
    let engine = DiskEngine::open(&text_path, &cancel, &progress).expect("open benchmark file");
    let open_time = started.elapsed();
    measurements.push(Measurement {
        metric: "engine_open_1gb",
        size_bytes: one_gb,
        measured: open_time,
        budget_ms: open_budget,
        roadmap_ms: Some(scaled_ms(ROADMAP_ENGINE_OPEN_1GB_MS, scale)),
        passed: open_time.as_millis() <= open_budget,
        note: (scale < 1.0).then(|| format!("scaled from 1 GiB by {scale}")),
    });

    // A full-file scan for a pattern that does not occur: the worst case for
    // the streaming find path.
    let search = SearchEngine::new("pattern-that-is-not-present", SearchOptions::default());
    let (cancel, progress) = no_cancel();
    let started = Instant::now();
    let hit = engine
        .find(&search, 0, true, &cancel, &progress)
        .expect("find");
    let find_time = started.elapsed();
    assert!(hit.is_none(), "the benchmark pattern must not occur");
    measurements.push(Measurement {
        metric: "engine_find_1gb",
        size_bytes: one_gb,
        measured: find_time,
        budget_ms: find_budget,
        roadmap_ms: Some(scaled_ms(ROADMAP_ENGINE_FIND_1GB_MS, scale)),
        passed: find_time.as_millis() <= find_budget,
        note: (scale < 1.0).then(|| format!("scaled from 1 GiB by {scale}")),
    });

    // Save: insert one character near the middle, then stream the whole file
    // through the atomic save path. The generated file is ASCII, so the middle
    // offset is always a character boundary.
    let mut engine = engine;
    let middle = engine.total_len() / 2;
    engine.apply_edit(middle, 0, "X").expect("edit before save");
    let target = dir.join("perf-save.txt");
    let (cancel, progress) = no_cancel();
    let started = Instant::now();
    let report = engine
        .snapshot()
        .write(&target, false, &progress, &cancel)
        .expect("save benchmark file");
    let save_time = started.elapsed();
    measurements.push(Measurement {
        metric: "engine_save_1gb",
        size_bytes: report.file_size(),
        measured: save_time,
        budget_ms: save_budget,
        roadmap_ms: None,
        passed: save_time.as_millis() <= save_budget,
        note: Some("no roadmap target; broad regression guard".to_string()),
    });
    drop(engine);
    drop(report);
    let _ = fs::remove_file(&text_path);
    let _ = fs::remove_file(&target);

    // Memory growth: process RSS before and after opening a 3 GB file.
    if scale >= 0.999 {
        let big_path = dir.join("perf-big.txt");
        let big_generate = generate(&big_path, three_gb);
        println!(
            "{{\"metric\":\"generate\",\"size_bytes\":{three_gb},\"measured_ms\":{}}}",
            big_generate.as_millis()
        );
        if let Some(before) = rss_kb() {
            let (cancel, progress) = no_cancel();
            let started = Instant::now();
            let engine = DiskEngine::open(&big_path, &cancel, &progress).expect("open big file");
            let elapsed = started.elapsed();
            let after = rss_kb().unwrap_or(before);
            let growth_mb = after.saturating_sub(before) as u128 / 1024;
            let budget = (RSS_BUDGET_3GB_MB as f64 * factor).round() as u128;
            measurements.push(Measurement {
                metric: "engine_rss_3gb",
                size_bytes: three_gb,
                measured: Duration::from_millis(elapsed.as_millis() as u64),
                budget_ms: budget,
                roadmap_ms: Some(100),
                passed: growth_mb <= budget,
                note: Some(format!(
                    "rss_growth_mb={growth_mb}; elapsed_ms={}",
                    elapsed.as_millis()
                )),
            });
            drop(engine);
        } else {
            println!(
                "{{\"metric\":\"engine_rss_3gb\",\"skipped\":true,\"reason\":\"VmRSS unavailable\"}}"
            );
        }
        let _ = fs::remove_file(&big_path);
    } else {
        println!(
            "{{\"metric\":\"engine_rss_3gb\",\"skipped\":true,\"reason\":\"LUBAN_PERF_SCALE < 1\"}}"
        );
    }

    let mut failed = 0;
    for measurement in &measurements {
        measurement.print();
        if !measurement.passed {
            failed += 1;
        }
    }
    println!(
        "{{\"metric\":\"summary\",\"measurements\":{},\"failed\":{failed}}}",
        measurements.len()
    );
    if let Some(temp) = temp {
        drop(temp);
    }
    if failed > 0 {
        eprintln!("performance budget exceeded: {failed} measurement(s)");
        std::process::exit(1);
    }
}
