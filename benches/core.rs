use criterion::{criterion_group, criterion_main, Criterion};
use proxy_server::utils::percent_encoding::{decode_component, encode_component};
use proxy_server::utils::range::{format_range, parse_range_spec};
use std::hint::black_box;

fn parser_benchmarks(criterion: &mut Criterion) {
    criterion.bench_function("parse closed HTTP range", |bench| {
        bench.iter(|| parse_range_spec(black_box("bytes=1048576-2097151")).unwrap())
    });
    criterion.bench_function("format HTTP range", |bench| {
        bench.iter(|| format_range(black_box(1_048_576), black_box(2_097_151)))
    });
    criterion.bench_function("percent encode signed URL", |bench| {
        bench.iter(|| {
            encode_component(black_box(
                "https://media.example/video.mp4?token=secret&expires=2000000000",
            ))
        })
    });
    let encoded =
        encode_component("https://media.example/video.mp4?token=secret&expires=2000000000")
            .into_owned();
    criterion.bench_function("percent decode signed URL", |bench| {
        bench.iter(|| decode_component(black_box(&encoded)).unwrap())
    });
}

criterion_group!(benches, parser_benchmarks);
criterion_main!(benches);
