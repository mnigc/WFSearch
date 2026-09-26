//! End-to-end query benchmarks over a synthetic 1,000,000-entry volume.
//! Measures the full engine path: parse -> parallel scan -> merge -> slice.

use criterion::{criterion_group, criterion_main, Criterion};
use std::time::Instant;
use wfs_core::{Engine, JournalPos, Query, SearchOptions, SortKind, VolumeIndex, VolumePhase};

const N: usize = 1_000_000;

fn synthetic_name(i: usize) -> String {
    if i.is_multiple_of(4) {
        format!("report_{i:07}.docx")
    } else if i % 4 == 1 {
        format!("项目文件夹_{i:07}.txt")
    } else if i % 4 == 2 {
        format!("IMG_2026{i:05}.jpg")
    } else {
        format!("notes-{i:07}.md")
    }
}

fn engine_with(n: usize) -> Engine {
    let mut idx = VolumeIndex::new('C');
    for i in 0..n {
        let frn = 100 + i as u64;
        idx.insert(frn, 5, &synthetic_name(i), i % 16 == 0);
    }
    let e = Engine::new();
    e.init_volume('C', idx, Some(JournalPos::default()), VolumePhase::Ready);
    e
}

fn bench_scan(c: &mut Criterion) {
    let build_t0 = Instant::now();
    let engine = engine_with(N);
    let build_elapsed = build_t0.elapsed();
    println!(
        "\n[synth] built {N} entries in {:.2}s\n",
        build_elapsed.as_secs_f32()
    );

    let mut group = c.benchmark_group("scan-1M");
    group.sample_size(30);

    let cases: &[(&str, &str)] = &[
        ("substr-ascii-common", "report"),
        ("substr-ascii-rare", "report_0099"),
        ("substr-cjk", "文件夹"),
        ("wildcard", "*.docx"),
        ("wildcard-cjk", "项目文件夹_000*"),
        ("multi-term", "report 2026"),
        ("path-term", r"文件夹_0000\0"),
        ("match-all", ""),
    ];

    for (label, q) in cases {
        let query = Query::parse(q);
        group.bench_function(*label, |b| {
            b.iter(|| {
                let opts = SearchOptions {
                    limit: 100,
                    offset: 0,
                    sort: SortKind::None,
                    ..Default::default()
                };
                let out = engine.search(&query, &opts);
                let hits = &out.hits;
                (out.total_matched, hits.len())
            })
        });
    }

    let sorted: Query = Query::parse("report");
    group.bench_function("sort-name", |b| {
        b.iter(|| {
            let opts = SearchOptions {
                limit: 100,
                offset: 0,
                sort: SortKind::Name,
                ..Default::default()
            };
            engine.search(&sorted, &opts).total_matched
        })
    });

    group.bench_function("sort-path", |b| {
        b.iter(|| {
            let opts = SearchOptions {
                limit: 100,
                offset: 0,
                sort: SortKind::Path,
                ..Default::default()
            };
            engine.search(&sorted, &opts).total_matched
        })
    });

    let all: Query = Query::parse("report");
    group.bench_function("match-path-flag", |b| {
        b.iter(|| {
            let opts = SearchOptions {
                limit: 100,
                offset: 0,
                sort: SortKind::None,
                match_path: true,
            };
            engine.search(&all, &opts).total_matched
        })
    });

    group.finish();
}

/// The plan called for an `ArcSwap` index so queries never take a lock. That
/// only pays off if publishing an update is cheap: every batch would have to
/// clone the whole `VolumeIndex` — node array *and* both hash maps — so this
/// measures that clone against the cost of the read lock it would replace.
fn bench_publish_cost(c: &mut Criterion) {
    let engine = engine_with(N);
    let mut group = c.benchmark_group("index-publish");
    group.sample_size(10);

    group.bench_function("clone-1m-volume-index", |b| {
        b.iter(|| engine.snapshot_data().len())
    });
    group.bench_function("rwlock-read-1m", |b| {
        b.iter(|| engine.status().volumes.len())
    });

    group.finish();
}

criterion_group!(benches, bench_scan, bench_publish_cost);
criterion_main!(benches);
