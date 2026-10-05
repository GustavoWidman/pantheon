//! Reproduce startup costs with complete ROOT records and an empty summary tree.
//! Fixture writes happen before the timer; startup includes lock acquisition,
//! JSON parsing, validation, incremental view replay, and frontier construction.
//!
//! cargo run --release --example memory_bench -- 10000 20000

use anyhow::{Result, ensure};
use pantheon::memory::{Memory, VIEW_BYTES};
use serde_json::json;
use std::{
    fs,
    io::{BufWriter, Write},
    time::Instant,
};

fn main() -> Result<()> {
    let mut counts = std::env::args()
        .skip(1)
        .map(|argument| argument.parse::<usize>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if counts.is_empty() {
        counts = vec![10_000, 20_000];
    }
    println!(
        "profile={} · five startup samples per fixture · no tree summaries",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("messages\tROOT bytes\tmin ms\tmedian ms\tmax ms");
    for count in counts {
        ensure!(count > 0, "message count must be positive");
        let directory = tempfile::tempdir()?;
        let main = directory.path().join("main");
        fs::create_dir(&main)?;
        let path = main.join("2026-01-01.jsonl");
        let text = "Benchmark original user message retained whole, without any existing summary. "
            .repeat(10);
        let mut output = BufWriter::new(fs::File::create(&path)?);
        for index in 0..count {
            serde_json::to_writer(
                &mut output,
                &json!({
                    "i": index, "kind": "user", "text": text,
                    "size": 6 + text.len(), "date": "2026-01-01T00:00:00+00:00"
                }),
            )?;
            output.write_all(b"\n")?;
        }
        output.flush()?;
        output.get_ref().sync_all()?;
        drop(output);
        let bytes = fs::metadata(&path)?.len();
        let mut times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let memory = Memory::open(directory.path(), VIEW_BYTES)?;
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            let stats = memory.stats();
            ensure!(
                stats["messages"] == count && stats["summaries"] == 0,
                "benchmark fixture unexpectedly changed"
            );
            ensure!(
                memory.ready_jobs(usize::MAX).len() == 1 && !memory.is_settled(),
                "benchmark should retain one unbuilt leaf frontier"
            );
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{count}\t{bytes}\t{:.3}\t{:.3}\t{:.3}",
            times[0], times[2], times[4]
        );
    }
    Ok(())
}
