//! 单机资源核算与准入控制（FR-03/FR-04）。
//!
//! 使用 `tokio::sync::Semaphore` 实现 RAII 资源预留。
//! 每个资源维度独立 Semaphore，admit 时 acquire 所有维度，
//! Reservation 持有 permits，drop 时自动归还。

use std::sync::{Arc, Mutex};

use clouisle_core::error::{ClouisleError, Result};
use clouisle_core::{Resources, SandboxSpec};
use tokio::sync::Semaphore;

/// 资源池：原子检查 + 基于 Semaphore 的 RAII 预留。
#[derive(Debug, Clone)]
pub struct ResourcePool {
    vcpu_sem: Arc<Semaphore>,
    mem_sem: Arc<Semaphore>,
    disk_sem: Arc<Semaphore>,
    max_sandboxes: Arc<Semaphore>,
    tracking: Arc<Mutex<Allocated>>,
    capacity: Resources,
}

#[derive(Debug, Default)]
struct Allocated {
    vcpu: u16,
    memory_mb: u32,
    disk_mb: u32,
    count: usize,
}

impl ResourcePool {
    pub fn new(capacity: Resources, max_sandboxes: usize) -> Self {
        Self {
            vcpu_sem: Arc::new(Semaphore::new(capacity.vcpu as usize)),
            mem_sem: Arc::new(Semaphore::new(capacity.memory_mb as usize)),
            disk_sem: Arc::new(Semaphore::new(capacity.disk_mb as usize)),
            max_sandboxes: Arc::new(Semaphore::new(max_sandboxes)),
            tracking: Arc::new(Mutex::new(Allocated::default())),
            capacity,
        }
    }

    /// 尝试预留资源。返回 RAII `Reservation`，drop 时自动归还。
    ///
    /// 使用 `try_acquire_many_owned` 实现 fail-fast：任一资源不足立即返回 `ResourceExhausted`。
    pub async fn admit(&self, spec: &SandboxSpec) -> Result<Reservation> {
        let r = &spec.resources;

        // 逐个 try_acquire；任一失败则释放已取得的 permit 并返回错误
        let sb_perm = match self.max_sandboxes.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                return Err(ClouisleError::resource_exhausted(
                    "sandbox count limit reached",
                ));
            }
        };
        let vcpu_perm = match self.vcpu_sem.clone().try_acquire_many_owned(r.vcpu as u32) {
            Ok(p) => p,
            Err(_) => {
                drop(sb_perm);
                return Err(ClouisleError::resource_exhausted(format!(
                    "insufficient vcpu: need {}",
                    r.vcpu
                )));
            }
        };
        let mem_perm = match self.mem_sem.clone().try_acquire_many_owned(r.memory_mb) {
            Ok(p) => p,
            Err(_) => {
                drop(sb_perm);
                drop(vcpu_perm);
                return Err(ClouisleError::resource_exhausted(format!(
                    "insufficient memory: need {} MB",
                    r.memory_mb
                )));
            }
        };
        let disk_perm = match self.disk_sem.clone().try_acquire_many_owned(r.disk_mb) {
            Ok(p) => p,
            Err(_) => {
                drop(sb_perm);
                drop(vcpu_perm);
                drop(mem_perm);
                return Err(ClouisleError::resource_exhausted(format!(
                    "insufficient disk: need {} MB",
                    r.disk_mb
                )));
            }
        };

        let mut t = self
            .tracking
            .lock()
            .expect("resource tracking lock poisoned");
        t.vcpu += r.vcpu;
        t.memory_mb += r.memory_mb;
        t.disk_mb += r.disk_mb;
        t.count += 1;

        Ok(Reservation {
            _sandbox: sb_perm,
            _vcpu: vcpu_perm,
            _mem: mem_perm,
            _disk: disk_perm,
            tracking: self.tracking.clone(),
            resources: r.clone(),
        })
    }

    /// 当前可用资源。
    pub async fn available(&self) -> Resources {
        Resources {
            vcpu: self.vcpu_sem.available_permits() as u16,
            memory_mb: self.mem_sem.available_permits() as u32,
            disk_mb: self.disk_sem.available_permits() as u32,
            bandwidth_mbps: None,
            iops: None,
            pids_max: None,
        }
    }

    /// 当前已预留资源。
    pub async fn reserved(&self) -> Resources {
        let t = self
            .tracking
            .lock()
            .expect("resource tracking lock poisoned");
        Resources {
            vcpu: t.vcpu,
            memory_mb: t.memory_mb,
            disk_mb: t.disk_mb,
            bandwidth_mbps: None,
            iops: None,
            pids_max: None,
        }
    }

    /// 总容量。
    pub fn capacity(&self) -> Resources {
        self.capacity.clone()
    }
}

/// RAII 预留：持有 Semaphore permit，drop 时自动归还并更新 tracking。
#[must_use]
pub struct Reservation {
    _sandbox: tokio::sync::OwnedSemaphorePermit,
    _vcpu: tokio::sync::OwnedSemaphorePermit,
    _mem: tokio::sync::OwnedSemaphorePermit,
    _disk: tokio::sync::OwnedSemaphorePermit,
    tracking: Arc<Mutex<Allocated>>,
    resources: Resources,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut tracking = self
            .tracking
            .lock()
            .expect("resource tracking lock poisoned");
        tracking.vcpu = tracking.vcpu.saturating_sub(self.resources.vcpu);
        tracking.memory_mb = tracking.memory_mb.saturating_sub(self.resources.memory_mb);
        tracking.disk_mb = tracking.disk_mb.saturating_sub(self.resources.disk_mb);
        tracking.count = tracking.count.saturating_sub(1);
    }
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("resources", &self.resources)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clouisle_core::ImageRef;
    use clouisle_core::error::ErrorKind;

    fn spec(vcpu: u16, mem: u32, disk: u32) -> SandboxSpec {
        SandboxSpec {
            image: ImageRef::new("x:y"),
            resources: Resources {
                vcpu,
                memory_mb: mem,
                disk_mb: disk,
                ..Resources::default()
            },
            ..SandboxSpec::default()
        }
    }

    fn capacity() -> Resources {
        Resources {
            vcpu: 50,
            memory_mb: 1024 * 16,
            disk_mb: 1024 * 8,
            ..Resources::default()
        }
    }

    #[tokio::test]
    async fn no_oversubscription_under_concurrency() {
        let pool = ResourcePool::new(capacity(), 200);
        let n = 100;
        let mut handles = Vec::new();
        for _ in 0..n {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                pool.admit(&spec(1, 256, 512)).await.ok()
            }));
        }
        // 收集所有 reservation 并持有，直到计数完成
        let mut reservations: Vec<Reservation> = Vec::new();
        let mut ok = 0;
        for h in handles {
            if let Some(r) = h.await.unwrap() {
                ok += 1;
                reservations.push(r);
            }
        }
        // 各维度容量边界：
        // vcpu 50/1=50, mem 16384/256=64, disk 8192/512=16 → 磁盘是瓶颈，最多 16
        assert_eq!(ok, 16, "expected 16 accepted (disk-bound), got {ok}");
        assert_eq!(reservations.len(), 16);
    }

    #[tokio::test]
    async fn reservation_released_on_drop() {
        let pool = ResourcePool::new(capacity(), 200);
        let s = spec(2, 512, 1024);
        {
            let _r = pool.admit(&s).await.unwrap();
            let avail = pool.available().await;
            assert_eq!(avail.vcpu, 48, "vcpu should decrease");
        }
        let avail = pool.available().await;
        assert_eq!(avail.vcpu, 50, "vcpu should be restored");
    }

    #[tokio::test]
    async fn resource_exhaustion() {
        let pool = ResourcePool::new(capacity(), 200);
        let mut admits = Vec::new();
        for _ in 0..50 {
            admits.push(pool.admit(&spec(1, 64, 64)).await.unwrap());
        }
        let err = pool.admit(&spec(1, 64, 64)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ResourceExhausted);

        admits.drain(40..).for_each(drop);
        assert!(pool.admit(&spec(1, 64, 64)).await.is_ok());
    }

    #[tokio::test]
    async fn sandbox_count_cap() {
        let pool = ResourcePool::new(capacity(), 3);
        let mut admits = Vec::new();
        for _ in 0..3 {
            admits.push(pool.admit(&spec(1, 64, 64)).await.unwrap());
        }
        let err = pool.admit(&spec(1, 64, 64)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ResourceExhausted);
        drop(admits);
    }

    #[tokio::test]
    async fn available_reflects_reserved() {
        let pool = ResourcePool::new(capacity(), 200);
        assert_eq!(pool.available().await.vcpu, 50);
        let _r = pool.admit(&spec(10, 1024, 2048)).await.unwrap();
        assert_eq!(pool.available().await.vcpu, 40);
    }

    #[tokio::test]
    async fn restored_reservations_remain_held() {
        let pool = ResourcePool::new(capacity(), 200);
        let running = [spec(4, 1024, 2048), spec(2, 512, 1024)];
        let _reservations = [
            pool.admit(&running[0]).await.unwrap(),
            pool.admit(&running[1]).await.unwrap(),
        ];
        let avail = pool.available().await;
        assert_eq!(avail.vcpu, 44, "held reservations should consume 6 vcpu");
    }

    #[tokio::test]
    async fn drop_releases_permits_and_tracking() {
        let pool = ResourcePool::new(capacity(), 200);
        let reservation = pool.admit(&spec(4, 1024, 2048)).await.unwrap();
        assert_eq!(pool.reserved().await.vcpu, 4);
        drop(reservation);
        assert_eq!(pool.available().await.vcpu, 50);
        assert_eq!(pool.reserved().await.vcpu, 0);
    }

    #[tokio::test]
    async fn large_resource_need_rejected() {
        let pool = ResourcePool::new(capacity(), 200);
        let err = pool.admit(&spec(100, 64, 64)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ResourceExhausted);
    }
}
