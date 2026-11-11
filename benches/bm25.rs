use clap::Parser;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use haku::{Args, BM25, IO};
use iwe_corpus::{doc_key, generate_corpus, hub_key};
use std::{
    fs::read_to_string,
    hint::black_box,
    iter::once,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::tempdir;
use tokio::runtime::Runtime;

#[path = "vendor/iwe/corpus.rs"]
mod iwe_corpus;

fn bench(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let mut group = c.benchmark_group("bm25/add");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    for n_docs in [5_000, 10_000, 20_000] {
        let corpus_dir = tempdir().unwrap();
        generate_corpus(corpus_dir.path(), n_docs, 42);

        let docs: Vec<String> = (1..=n_docs)
            .map(doc_key)
            .chain(once(hub_key().to_owned()))
            .map(|key| read_to_string(corpus_dir.path().join(format!("{key}.md"))).unwrap())
            .collect();

        drop(corpus_dir);

        group.throughput(Throughput::Elements(docs.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n_docs), &docs, |b, docs| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;

                for _ in 0..iters {
                    let dir = tempdir().unwrap();
                    let mut args = Args::parse_from(["bm25-bench"]);
                    args.index_dir = dir.path().join("index");
                    args.cache_dir = dir.path().join("cache");
                    args.docs_dir = dir.path().join("docs");
                    args.wal_dir = dir.path().join("wal");
                    let io = Arc::new(IO::new(&args));
                    let index = runtime.block_on(BM25::open(io.clone(), &args)).unwrap();

                    let started = Instant::now();
                    runtime.block_on(async {
                        for doc in docs {
                            index.add(black_box(doc)).await.unwrap();
                        }
                    });
                    elapsed += started.elapsed();

                    drop(index);
                    drop(io);
                    drop(dir);
                }

                elapsed
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
