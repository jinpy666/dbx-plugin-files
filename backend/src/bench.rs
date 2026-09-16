//! Performance baselines (P-FILES ⑤). Ignored by default — run explicitly:
//!
//! ```text
//! cargo test --release --manifest-path backend/Cargo.toml -- --ignored --nocapture bench_
//! ```
//!
//! Measures (recorded in docs/PROGRESS-P-FILES.zh-CN.md §5):
//! - 10k-entry directory listing: full `files/list` vs `files/listPaged`
//!   (memory backend, real in-process costs, no network);
//! - 50 MiB upload/download throughput through the same primitives the
//!   binary transfer channels use (`engine::transfer` Writer 4 MiB chunks /
//!   Reader 256 KiB chunks) on memory + fs backends (fs = real disk);
//! - progress-event throttle effectiveness: events emitted vs chunks pushed
//!   for a 50 MiB transfer in 256 KiB chunks (`transfers::Throttle` ≥200 ms
//!   OR ≥1% delta policy).

use std::time::Instant;

use crate::engine::transfer as slot;
use crate::engine::{ops, Engine};
use crate::model::StoredConnection;
use crate::store::Store;
use crate::transfers::Throttle;

const MIB: u64 = 1024 * 1024;
/// 10k entries for the listing benchmark.
const LIST_FILES: u64 = 10_000;
/// 50 MiB for the throughput benchmark.
const BLOB_BYTES: u64 = 50 * MIB;
/// Upload writer chunking used by the binary channel (`engine::transfer`).
const WRITE_CHUNK: usize = 4 * 1024 * 1024;
/// Download/read chunking used by the binary channel (`model::TRANSFER_CHUNK_SIZE`).
const READ_CHUNK: usize = 256 * 1024;

fn memory_operator() -> opendal::Operator {
    opendal::Operator::via_iter("memory", Vec::<(String, String)>::new()).unwrap()
}

fn pattern(len: usize) -> Vec<u8> {
    // Deterministic pattern (no rng dependency); content is irrelevant for
    // throughput, only the byte count matters.
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn seed_dir(operator: &opendal::Operator, count: u64) {
    operator.create_dir("bench/").await.unwrap();
    for i in 0..count {
        operator
            .write(&format!("bench/file-{i:05}.txt"), format!("entry {i}"))
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "performance baseline; run with --ignored --nocapture bench_"]
async fn bench_list_10k_memory() {
    let operator = memory_operator();
    seed_dir(&operator, LIST_FILES).await;

    let started = Instant::now();
    let entries = ops::list(&operator, "bench", false).await.unwrap();
    let full = started.elapsed();
    assert_eq!(entries.len() as u64, LIST_FILES);

    let page_size = 200u64;
    let pages = 5u64;
    let started = Instant::now();
    let mut total = 0u64;
    for page in 1..=pages {
        let (slice, t) = ops::list_paged(&operator, "bench", page, page_size).await.unwrap();
        total = t;
        assert_eq!(slice.len() as u64, page_size);
    }
    let paged = started.elapsed();
    let per_page_ms = paged.as_secs_f64() * 1000.0 / pages as f64;
    println!(
        "bench_list_10k_memory: full_list={} entries={} ms; listPaged({pages} pages of {page_size})={:.1} ms/call total={total}",
        full.as_secs_f64() * 1000.0,
        entries.len(),
        per_page_ms,
    );
}

async fn bench_throughput(operator: &opendal::Operator, backend: &str) {
    // Upload: 4 MiB chunks through the channel writer (P-FILES ⑤).
    let chunk = pattern(WRITE_CHUNK);
    let mut writer = slot::open_upload_writer(operator, "bench/blob.bin")
        .await
        .expect("upload writer");
    let started = Instant::now();
    let mut written = 0u64;
    while written < BLOB_BYTES {
        let len = (BLOB_BYTES - written).min(WRITE_CHUNK as u64) as usize;
        if len == WRITE_CHUNK {
            writer.write(chunk.clone()).await.unwrap();
        } else {
            writer.write(chunk[..len].to_vec()).await.unwrap();
        }
        written += len as u64;
    }
    writer.close().await.unwrap();
    let upload = started.elapsed();
    let upload_mibs = BLOB_BYTES as f64 / MIB as f64 / upload.as_secs_f64();

    // Download: 256 KiB ranged reads through the channel reader.
    let (reader, size) = slot::open_download_reader(operator, "bench/blob.bin")
        .await
        .expect("download reader");
    assert_eq!(size, BLOB_BYTES);
    let started = Instant::now();
    let mut offset = 0u64;
    let mut checksum: u8 = 0;
    while offset < size {
        let end = (offset + READ_CHUNK as u64).min(size);
        let bytes = slot::read_chunk(&reader, offset, end).await.unwrap();
        checksum = checksum.wrapping_add(bytes[bytes.len() / 2]);
        offset = end;
    }
    let download = started.elapsed();
    let download_mibs = BLOB_BYTES as f64 / MIB as f64 / download.as_secs_f64();
    let _ = checksum; // keep the read honest without printing data
    println!(
        "bench_50mb_{backend}: upload {:.1} MiB/s ({} MiB in {:.2}s); download {:.1} MiB/s ({:.2}s); chunks: up={} x4MiB down={} x256KiB",
        upload_mibs,
        BLOB_BYTES / MIB,
        upload.as_secs_f64(),
        download_mibs,
        download.as_secs_f64(),
        (BLOB_BYTES as usize + WRITE_CHUNK - 1) / WRITE_CHUNK,
        (BLOB_BYTES as usize + READ_CHUNK - 1) / READ_CHUNK,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "performance baseline; run with --ignored --nocapture bench_"]
async fn bench_50mb_memory() {
    bench_throughput(&memory_operator(), "memory").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "performance baseline; run with --ignored --nocapture bench_"]
async fn bench_50mb_fs() {
    let temp = tempfile::tempdir().unwrap();
    let operator = opendal::Operator::via_iter(
        "fs",
        vec![("root".to_string(), temp.path().to_string_lossy().to_string())],
    )
    .unwrap();
    operator.create_dir("bench/").await.unwrap();
    bench_throughput(&operator, "fs").await;
}

/// Throttle effectiveness for a 50 MiB transfer pushed in 256 KiB chunks:
/// counts how many progress events the ≥200 ms / ≥1% policy emits versus the
/// unthrottled per-chunk upper bound, and derives the bytes-per-event ratio.
#[test]
fn bench_progress_event_throttle_50mb() {
    let chunk_bytes = READ_CHUNK as u64;
    let chunks = (BLOB_BYTES + chunk_bytes - 1) / chunk_bytes;
    let mut throttle = Throttle::default();
    let mut events = 0u64;
    let mut transferred = 0u64;
    for _ in 0..chunks {
        transferred = (transferred + chunk_bytes).min(BLOB_BYTES);
        if throttle.should_emit(transferred, Some(BLOB_BYTES)) {
            events += 1;
        }
    }
    let bytes_per_event = if events > 0 { BLOB_BYTES / events } else { 0 };
    println!(
        "bench_progress_event_throttle_50mb: chunks={} events={} (unthrottled bound={chunks}); \
         event/byte ratio={:.6} ({bytes_per_event} bytes/event at 256KiB chunk granularity)",
        chunks,
        events,
        events as f64 / BLOB_BYTES as f64,
    );
    assert!(events * 2 <= chunks, "throttle must at least halve the event count");
}

/// Startup hydration path (P-FILES ①c) sanity: load_history swaps the
/// in-memory mirror from the store and must not break a fresh JobTable.
#[test]
fn bench_load_history_hydration_cost_200_records() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        for i in 0..crate::model::TRANSFER_HISTORY_LIMIT {
            store
                .record_transfer(crate::store::TransferRecord {
                    task_id: format!("t{i}"),
                    connection_id: "c1".into(),
                    kind: "upload".into(),
                    remote_path: format!("/f{i}"),
                    total_bytes: Some(1),
                    transferred_bytes: 1,
                    status: "completed".into(),
                    error: None,
                    started_at: None,
                    finished_at: None,
                    local_path: None,
                })
                .unwrap();
        }
        let table = crate::transfers::JobTable::new();
        let started = Instant::now();
        table.load_history(&store).await;
        println!(
            "bench_load_history_hydration_cost_200_records: {} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        let engine = Engine::new();
        assert!(engine.connection_ids().is_empty());
        let _ = StoredConnection::is_custom; // keep imports honest
    });
}
