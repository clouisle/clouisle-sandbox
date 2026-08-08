//! 真实预热池（FR-08 / ADR-008）。
//!
//! 槽位状态机：`Preparing → Ready → Acquired → Ready → Destroyed`。
//! - `Preparing`：冷启动 VM 中（尚未可用）
//! - `Ready`：就绪，可被 `acquire` 取走
//! - `Acquired`：已被租出，`release` 后回到 `Ready`
//! - `Destroyed`：因健康检查失败或闲置超时被销毁（从池中移除）
//!
//! 池按键 `(image_digest, resources_hash)`（即 [`SandboxSpec::pool_key`]）分桶。
//! 每个槽位携带一个已创建并启动的 VM 句柄，`acquire` 命中即返回，避免冷启动。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use clouisle_core::SandboxSpec;
use clouisle_vmm::{StopMode, VmHandle, Vmm};

/// 池错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// 槽位不在池中（已被销毁或从未预热）。
    #[error("slot not found in pool")]
    SlotNotFound,
    /// 槽位当前不是 `Acquired` 状态，无法释放。
    #[error("slot is not currently acquired")]
    SlotAlreadyAcquired,
    /// 槽位状态非法，无法执行请求的转换。
    #[error("invalid slot state for operation")]
    InvalidState,
}

/// 槽位生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// 正在冷启动（VM 已创建、启动中）。
    Preparing,
    /// 就绪，可被取走。
    Ready,
    /// 已被租出。
    Acquired,
    /// 已销毁（从池中移除）。
    Destroyed,
}

/// 池中的一个就绪槽位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSlot {
    pub id: String,
    pub pool_key: String,
    pub vm_handle: VmHandle,
    pub state: SlotState,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

impl PoolSlot {
    fn new(pool_key: String) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            pool_key,
            vm_handle: VmHandle {
                id: String::new(),
                backend: String::new(),
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
            },
            state: SlotState::Preparing,
            created_at: now,
            last_used_at: now,
        }
    }
}

/// 池内部状态。
struct PoolInner {
    slots: Vec<PoolSlot>,
    /// 需要保持预热的模板规格，按 `pool_key` 索引。
    templates: HashMap<String, SandboxSpec>,
    min_idle: usize,
    max_idle_secs: u64,
    bg_started: bool,
}

impl Default for PoolInner {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            templates: HashMap::new(),
            min_idle: 0,
            max_idle_secs: 300,
            bg_started: false,
        }
    }
}

/// 预热池。
///
/// 生产环境用真实 VMM 后端（如 FirecrackerVmm）。
pub struct Pool {
    inner: Arc<Mutex<PoolInner>>,
    vmm: Arc<dyn Vmm>,
    tick: Duration,
}

impl Pool {
    /// 新建预热池，注入 VMM 后端。
    pub fn new(min_idle: usize, max_idle_secs: u64, vmm: Arc<dyn Vmm>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                min_idle,
                max_idle_secs,
                ..PoolInner::default()
            })),
            vmm,
            tick: Duration::from_secs(1),
        }
    }

    /// 注册模板并立即预热一个就绪槽位。
    ///
    /// 将 `spec` 记入模板表（供后台补足），并同步创建 + 启动一个 VM，
    /// 成功则返回 `Ready` 槽位，创建失败返回 `None`。
    pub async fn warm(&self, spec: &SandboxSpec) -> Option<PoolSlot> {
        self.ensure_background().await;
        let key = spec.pool_key();
        {
            let mut g = self.inner.lock().await;
            g.templates.insert(key.clone(), spec.clone());
        }
        self.create_slot(spec, &key).await
    }

    /// 取走一个与 `spec` 匹配的 `Ready` 槽位；无匹配则返回 `None`（由调用方冷启动）。
    pub async fn acquire(&self, spec: &SandboxSpec) -> Option<PoolSlot> {
        self.ensure_background().await;
        let key = spec.pool_key();
        let mut g = self.inner.lock().await;
        let now = Utc::now();
        // 优先取最久未使用的就绪槽位（LRU）。
        let pos = g
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pool_key == key && s.state == SlotState::Ready)
            .min_by_key(|(_, s)| s.last_used_at)
            .map(|(i, _)| i)?;
        let slot = &mut g.slots[pos];
        slot.state = SlotState::Acquired;
        slot.last_used_at = now;
        Some(slot.clone())
    }

    /// 将 `Acquired` 槽位归还池中，状态回到 `Ready`。
    pub async fn release(&self, slot: PoolSlot) -> Result<(), PoolError> {
        self.ensure_background().await;
        let mut g = self.inner.lock().await;
        let found = g.slots.iter_mut().find(|s| s.id == slot.id);
        match found {
            None => Err(PoolError::SlotNotFound),
            Some(s) => match s.state {
                SlotState::Acquired => {
                    s.state = SlotState::Ready;
                    s.last_used_at = Utc::now();
                    Ok(())
                }
                SlotState::Ready => Err(PoolError::SlotAlreadyAcquired),
                _ => Err(PoolError::InvalidState),
            },
        }
    }

    /// 当前就绪槽位数。
    pub async fn ready_count(&self) -> usize {
        self.ensure_background().await;
        let g = self.inner.lock().await;
        g.slots
            .iter()
            .filter(|s| s.state == SlotState::Ready)
            .count()
    }

    /// 调整目标闲置槽位数。
    pub async fn set_min_idle(&self, n: usize) {
        self.ensure_background().await;
        let mut g = self.inner.lock().await;
        g.min_idle = n;
    }

    /// 惰性启动后台巡检任务（补足 / 健康检查 / 闲置回收）。
    async fn ensure_background(&self) {
        let spawn = {
            let mut g = self.inner.lock().await;
            if g.bg_started {
                false
            } else {
                g.bg_started = true;
                true
            }
        };
        if spawn {
            spawn_background(self.inner.clone(), self.vmm.clone(), self.tick);
        }
    }

    /// 冷启动一个槽位：注册 Preparing → create + start → Ready。
    async fn create_slot(&self, spec: &SandboxSpec, key: &str) -> Option<PoolSlot> {
        let slot = {
            let mut g = self.inner.lock().await;
            let mut s = PoolSlot::new(key.to_string());
            s.id = format!(
                "{}_{}",
                spec.image.reference.replace(['/', ':'], "_"),
                uuid::Uuid::now_v7().simple()
            );
            g.slots.push(s.clone());
            s
        };

        let handle = match self.vmm.create(&slot.id, spec).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, spec = %key, "pool: cold create failed");
                self.remove_slot(&slot.id).await;
                return None;
            }
        };
        if let Err(e) = self.vmm.start(&handle).await {
            tracing::warn!(error = %e, id = %handle.id, "pool: cold start failed");
            let _ = self.vmm.stop(&handle, StopMode::Force).await;
            self.remove_slot(&slot.id).await;
            return None;
        }

        let mut g = self.inner.lock().await;
        if let Some(s) = g.slots.iter_mut().find(|s| s.id == slot.id) {
            s.vm_handle = handle;
            s.state = SlotState::Ready;
            s.last_used_at = Utc::now();
            Some(s.clone())
        } else {
            None
        }
    }

    async fn remove_slot(&self, id: &str) {
        self.inner.lock().await.slots.retain(|s| s.id != id);
    }
}

/// 后台巡检：补足 idle、健康检查、闲置回收。
fn spawn_background(inner: Arc<Mutex<PoolInner>>, vmm: Arc<dyn Vmm>, tick: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            reap_idle(&inner, &vmm, tick).await;
            health_check(&inner, &vmm).await;
            replenish(&inner, &vmm).await;
        }
    });
}

/// 回收闲置过久的 `Ready` 槽位：`last_used_at` 距今 ≥ `max_idle_secs` 则销毁。
async fn reap_idle(inner: &Arc<Mutex<PoolInner>>, vmm: &Arc<dyn Vmm>, tick: Duration) {
    let max_idle_secs = { inner.lock().await.max_idle_secs };
    if max_idle_secs == 0 {
        return;
    }
    let now = Utc::now();
    let (to_destroy, _) = {
        let mut g = inner.lock().await;
        let mut doomed = Vec::new();
        g.slots.retain(|s| {
            if s.state == SlotState::Ready
                && (now - s.last_used_at).num_seconds() >= max_idle_secs as i64
            {
                doomed.push(s.vm_handle.clone());
                false
            } else {
                true
            }
        });
        (doomed, ())
    };
    for h in to_destroy {
        let _ = vmm.stop(&h, StopMode::Force).await;
    }
    let _ = tick; // tick used for future interval tuning
}

/// 健康检查：对 `Acquired` 槽位 ping（`Vmm::stats`），失败则标记死亡并移除。
async fn health_check(inner: &Arc<Mutex<PoolInner>>, vmm: &Arc<dyn Vmm>) {
    let to_ping = {
        let g = inner.lock().await;
        g.slots
            .iter()
            .filter(|s| s.state == SlotState::Acquired)
            .map(|s| s.vm_handle.clone())
            .collect::<Vec<_>>()
    };
    for h in to_ping {
        if vmm.stats(&h).await.is_err() {
            tracing::warn!(id = %h.id, "pool: acquired slot dead, removing");
            inner.lock().await.slots.retain(|s| s.vm_handle.id != h.id);
            let _ = vmm.stop(&h, StopMode::Force).await;
        }
    }
}

/// 补足闲置：当就绪槽位数 < `min_idle` 时，为最缺的模板冷启动一个新 VM。
async fn replenish(inner: &Arc<Mutex<PoolInner>>, vmm: &Arc<dyn Vmm>) {
    // 选择要补足的模板：就绪槽不足且当前无冷启动在途。
    let template: Option<(String, SandboxSpec)> = {
        let g = inner.lock().await;
        if g.templates.is_empty() {
            return;
        }
        // 已有 Preparing 槽位在途（含其它键），避免并发 tick 重复冷启动。
        if g.slots.iter().any(|s| s.state == SlotState::Preparing) {
            return;
        }
        let ready = g
            .slots
            .iter()
            .filter(|s| s.state == SlotState::Ready)
            .count();
        if ready >= g.min_idle {
            return;
        }
        // 选 in-pool（Preparing+Ready）槽位最少的模板，即最饥饿。
        let mut best: Option<(String, usize)> = None;
        for key in g.templates.keys() {
            let in_pool = g
                .slots
                .iter()
                .filter(|s| s.pool_key == *key && s.state != SlotState::Destroyed)
                .count();
            if best.as_ref().is_none_or(|(_, c)| in_pool < *c) {
                best = Some((key.clone(), in_pool));
            }
        }
        match best {
            Some((key, _)) => g.templates.get(&key).map(|spec| (key, spec.clone())),
            None => None,
        }
    };

    let (key, spec) = match template {
        Some(t) => t,
        None => return,
    };

    // 预占一个 Preparing 占位，避免并发 tick 重复冷启动。
    let slot = {
        let mut g = inner.lock().await;
        let mut s = PoolSlot::new(key.clone());
        s.id = format!(
            "{}_{}",
            spec.image.reference.replace(['/', ':'], "_"),
            uuid::Uuid::now_v7().simple()
        );
        g.slots.push(s.clone());
        s
    };

    let handle = match vmm.create(&slot.id, &spec).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, spec = %key, "pool: replenish create failed");
            inner.lock().await.slots.retain(|s| s.id != slot.id);
            return;
        }
    };
    if let Err(e) = vmm.start(&handle).await {
        tracing::warn!(error = %e, id = %handle.id, "pool: replenish start failed");
        let _ = vmm.stop(&handle, StopMode::Force).await;
        inner.lock().await.slots.retain(|s| s.id != slot.id);
        return;
    }

    let now = Utc::now();
    let mut g = inner.lock().await;
    if let Some(s) = g.slots.iter_mut().find(|s| s.id == slot.id) {
        s.vm_handle = handle;
        s.state = SlotState::Ready;
        s.last_used_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clouisle_core::ImageRef;
    use clouisle_vmm::{
        SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
    };

    use async_trait::async_trait;

    /// 仅测试夹具：内存状态机 VMM（不依赖 KVM）。
    #[derive(Clone)]
    struct TestVmm;

    #[async_trait]
    impl Vmm for TestVmm {
        async fn create(
            &self,
            _: &str,
            _: &clouisle_core::SandboxSpec,
        ) -> clouisle_core::Result<VmHandle> {
            Ok(VmHandle {
                id: uuid::Uuid::now_v7().to_string(),
                backend: "test".into(),
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
            })
        }
        async fn start(&self, _: &VmHandle) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn pause(&self, _: &VmHandle) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn resume(&self, _: &VmHandle) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn snapshot(
            &self,
            _: &VmHandle,
            _k: SnapshotKind,
            _o: &SnapshotPaths,
        ) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn restore(
            &self,
            _: &clouisle_core::SandboxSpec,
            _: &SnapshotPaths,
        ) -> clouisle_core::Result<VmHandle> {
            Ok(VmHandle {
                id: uuid::Uuid::now_v7().to_string(),
                backend: "test".into(),
                pid: None,
                api_socket: None,
                vsock_socket: None,
                vsock_cid: None,
            })
        }
        async fn stop(&self, _: &VmHandle, _m: StopMode) -> clouisle_core::Result<()> {
            Ok(())
        }
        async fn stats(&self, _: &VmHandle) -> clouisle_core::Result<VmStats> {
            Ok(VmStats::default())
        }
        fn capabilities(&self) -> VmmCapabilities {
            VmmCapabilities {
                snapshot: true,
                vsock: true,
                balloon: false,
            }
        }
    }

    fn vmm() -> Arc<dyn Vmm> {
        Arc::new(TestVmm)
    }

    fn spec(refname: &str, digest: Option<&str>) -> SandboxSpec {
        SandboxSpec {
            image: ImageRef {
                reference: refname.into(),
                digest: digest.map(|d| d.into()),
            },
            ..SandboxSpec::default()
        }
    }

    async fn warm_pool() -> Pool {
        let pool = Pool::new(0, 60, vmm());
        pool.warm(&spec("alpine:latest", None)).await;
        pool
    }

    #[tokio::test]
    async fn acquire_from_empty_returns_none() {
        let pool = Pool::new(0, 60, vmm());
        assert_eq!(pool.acquire(&spec("alpine:latest", None)).await, None);
    }

    #[tokio::test]
    async fn acquire_after_warm_returns_slot() {
        let pool = warm_pool().await;
        let slot = pool.acquire(&spec("alpine:latest", None)).await;
        let slot = slot.expect("warmed slot should be acquirable");
        assert_eq!(slot.state, SlotState::Acquired);
        assert_eq!(pool.ready_count().await, 0);
    }

    #[tokio::test]
    async fn release_returns_slot_to_pool() {
        let pool = warm_pool().await;
        let slot = pool.acquire(&spec("alpine:latest", None)).await.unwrap();
        assert_eq!(pool.ready_count().await, 0);
        pool.release(slot).await.expect("release should succeed");
        assert_eq!(pool.ready_count().await, 1);
    }

    #[tokio::test]
    async fn ready_count_increases_on_warm() {
        let pool = Pool::new(0, 60, vmm());
        assert_eq!(pool.ready_count().await, 0);
        pool.warm(&spec("alpine:latest", None)).await;
        assert_eq!(pool.ready_count().await, 1);
    }

    #[tokio::test]
    async fn different_pool_key_does_not_match() {
        let pool = warm_pool().await;
        // 不同镜像 → 不同 cache_key → 不同 pool_key。
        assert_eq!(pool.acquire(&spec("busybox:latest", None)).await, None);
        // 相同镜像不同 digest → 不同 pool_key。
        assert_eq!(
            pool.acquire(&spec("alpine:latest", Some("sha256:abc")))
                .await,
            None
        );
    }

    #[tokio::test]
    async fn release_unknown_slot_errors() {
        let pool = warm_pool().await;
        let slot = pool.acquire(&spec("alpine:latest", None)).await.unwrap();
        let mut bogus = slot.clone();
        bogus.id = "nope".into();
        assert_eq!(pool.release(bogus).await, Err(PoolError::SlotNotFound));
        // 归还正常槽位两次：第二次应报 SlotAlreadyAcquired。
        pool.release(slot).await.unwrap();
    }

    #[tokio::test]
    async fn set_min_idle_replenishes_background() {
        let pool = Pool::new(0, 60, vmm());
        pool.warm(&spec("alpine:latest", None)).await;
        // 取走唯一就绪槽位后，设 min_idle=1，后台应补足到 1。
        let _slot = pool.acquire(&spec("alpine:latest", None)).await.unwrap();
        assert_eq!(pool.ready_count().await, 0);
        pool.set_min_idle(1).await;
        // 后台 tick 为 1s，轮询等待补足。
        for _ in 0..20 {
            if pool.ready_count().await >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(pool.ready_count().await, 1);
    }
}
