//! 启动时延分解基准（ADR-002 / PERF-001）。
//!
//! 用 `BootTrace` 计时点到 SLO 的映射：
//! - POOL_ALLOC: 从 warm pool 取就绪沙盒
//! - COLD_START: 完整冷启动

use std::time::Instant;

use clouisle_core::timing::{BootTrace, SloKind};
use clouisle_vmm::Vmm;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_slo_kind_str(c: &mut Criterion) {
    c.bench_function("slo_kind_as_str", |b| {
        b.iter(|| {
            black_box(SloKind::ColdStart.as_str());
            black_box(SloKind::PoolAlloc.targets_p95_ms());
        })
    });
}

fn bench_boot_trace(c: &mut Criterion) {
    c.bench_function("boot_trace_breakdown", |b| {
        b.iter(|| {
            let mut t = BootTrace::new("sbx-bench");
            t.mark_request();
            for _ in 0..100 {
                t.mark_scratch();
                t.mark_spawned();
                t.mark_configured();
            }
            black_box(t.total_ms());
        })
    });
}

fn bench_mock_create(c: &mut Criterion) {
    // FirecrackerVmm 下模拟 100 次冷启动的平均时延
    c.bench_function("mock_cold_start_approx", |b| {
        b.iter_custom(|iters| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let vmm = clouisle_vmm::FirecrackerVmm::new(clouisle_vmm::FirecrackerConfig::default());
            let spec = clouisle_core::SandboxSpec::default();
            let start = Instant::now();
            rt.block_on(async {
                for _ in 0..iters {
                    let h = vmm.create(&spec).await.unwrap();
                    vmm.start(&h).await.unwrap();
                }
            });
            start.elapsed()
        })
    });
}

criterion_group!(benches, bench_slo_kind_str, bench_boot_trace, bench_mock_create);
criterion_main!(benches);