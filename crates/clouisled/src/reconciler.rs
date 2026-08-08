//! Reconciler：漂移收敛。
//!
//! 对比「DB 中属于本节点的沙盒」与「本机实际 VMM 进程」，处理三种漂移：
//! 1. DB 有、进程无 → 标记 error
//! 2. 进程有、DB 无 → 孤儿，杀掉
//! 3. 状态不符 → 以本机实际为准回写 DB

use std::sync::Arc;

use tokio::sync::RwLock;

use clouisle_core::{Sandbox, SandboxStatus};
use clouisle_store::Store;
use clouisle_vmm::Vmm;

/// 漂移检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftReport {
    pub node_id: String,
    /// DB 有、本机无 → 标记 error 的沙盒
    pub marked_error: Vec<String>,
    /// 孤儿进程数（本机有、DB 无）
    pub orphans_killed: usize,
    /// 状态已修正的沙盒
    pub status_corrected: Vec<String>,
}

/// Reconciler。
pub struct Reconciler {
    node_id: String,
    vmm: Arc<dyn Vmm>,
    /// 模拟的本机实际运行 ID 集合（真实实现从 /proc 扫描 firecracker 进程）
    live_sandboxes: Arc<RwLock<std::collections::HashSet<String>>>,
}

impl Reconciler {
    pub fn new(node_id: String, vmm: Arc<dyn Vmm>) -> Self {
        Self {
            node_id,
            vmm,
            live_sandboxes: Arc::new(RwLock::new(std::collections::HashSet::new())),
        }
    }

    /// 登记一个实际运行的沙盒（真实实现：reaper 上报）。
    pub async fn register_live(&self, sandbox_id: String) {
        self.live_sandboxes.write().await.insert(sandbox_id);
    }

    /// 注销一个已死沙盒。
    pub async fn unregister_live(&self, sandbox_id: &str) {
        self.live_sandboxes.write().await.remove(sandbox_id);
    }

    /// 执行一轮收敛（幂等）。
    pub async fn reconcile(&self, store: &Arc<dyn Store>) -> DriftReport {
        let mut report = DriftReport {
            node_id: self.node_id.clone(),
            marked_error: Vec::new(),
            orphans_killed: 0,
            status_corrected: Vec::new(),
        };

        let all = store.list_sandboxes(None).await.unwrap_or_default();
        let live = self.live_sandboxes.read().await.clone();

        for sb in &all {
            // 只处理本节点 + 活动状态
            if sb.node_id.as_deref() != Some(self.node_id.as_str()) {
                continue;
            }
            if !sb.status.is_active() {
                continue;
            }

            if live.contains(&sb.id.to_string()) {
                // 进程活着：状态可能漂移（DB running 但进程 Starting 等）→ 修正为 Running
                if sb.status != SandboxStatus::Running {
                    store
                        .update_sandbox_status(&sb.id, &SandboxStatus::Running)
                        .await
                        .ok();
                    report.status_corrected.push(sb.id.clone());
                }
            } else {
                // 进程已死但 DB 还是活动状态 → 标记 error
                store
                    .update_sandbox_status(&sb.id, &SandboxStatus::Error)
                    .await
                    .ok();
                report.marked_error.push(sb.id.clone());
            }
        }

        // 孤儿：本机有、DB 无 → 视为孤儿（真实实现按 firecracker 命名规则匹配）
        let db_ids: std::collections::HashSet<String> =
            all.iter().map(|s| s.id.clone()).collect();
        for id in &live {
            if !db_ids.contains(id) {
                // 杀掉该 VMM
                let _ = self.kill_orphan(id).await;
                report.orphans_killed += 1;
            }
        }

        report
    }

    async fn kill_orphan(&self, sandbox_id: &str) -> clouisle_core::Result<()> {
        // 用 MockVmm 无法按 id 索引 handle；此处在真实后端按进程 pid 收拾。
        // 记录即可。
        tracing::warn!(sandbox_id, "killing orphan VMM");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clouisle_store::InMemoryStore;
    use clouisle_vmm::{
        SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
    };
    use async_trait::async_trait;

    #[derive(Clone)]
    struct TestVmm;

    #[async_trait]
    impl Vmm for TestVmm {
        async fn create(&self, _: &clouisle_core::SandboxSpec) -> clouisle_core::Result<VmHandle> {
            Ok(VmHandle {
                id: uuid::Uuid::now_v7().to_string(),
                backend: "test".into(), pid: None, api_socket: None, vsock_socket: None,
            })
        }
        async fn start(&self, _: &VmHandle) -> clouisle_core::Result<()> { Ok(()) }
        async fn pause(&self, _: &VmHandle) -> clouisle_core::Result<()> { Ok(()) }
        async fn resume(&self, _: &VmHandle) -> clouisle_core::Result<()> { Ok(()) }
        async fn snapshot(&self, _: &VmHandle, _k: SnapshotKind, _o: &SnapshotPaths) -> clouisle_core::Result<()> { Ok(()) }
        async fn restore(&self, _: &clouisle_core::SandboxSpec, _: &SnapshotPaths) -> clouisle_core::Result<VmHandle> {
            Ok(VmHandle {
                id: uuid::Uuid::now_v7().to_string(),
                backend: "test".into(), pid: None, api_socket: None, vsock_socket: None,
            })
        }
        async fn stop(&self, _: &VmHandle, _m: StopMode) -> clouisle_core::Result<()> { Ok(()) }
        async fn stats(&self, _: &VmHandle) -> clouisle_core::Result<VmStats> { Ok(VmStats::default()) }
        fn capabilities(&self) -> VmmCapabilities {
            VmmCapabilities { snapshot: true, vsock: true, balloon: false }
        }
    }
    use clouisle_core::{Sandbox, SandboxSpec};

    #[tokio::test]
    async fn dead_sandbox_marked_error() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let mut sb = Sandbox::new("sbx-1".into(), SandboxSpec::default());
        sb.node_id = Some("node-1".into());
        sb.status = SandboxStatus::Running;
        store.create_sandbox(&sb).await.unwrap();

        let r = Reconciler::new("node-1".into(), Arc::new(TestVmm));
        let report = r.reconcile(&store).await;
        assert!(report.marked_error.contains(&"sbx-1".to_string()));

        let got = store.get_sandbox("sbx-1").await.unwrap();
        assert_eq!(got.status, SandboxStatus::Error);
    }

    #[tokio::test]
    async fn live_sandbox_corrected_to_running() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let mut sb = Sandbox::new("sbx-1".into(), SandboxSpec::default());
        sb.node_id = Some("node-1".into());
        sb.status = SandboxStatus::Starting;
        store.create_sandbox(&sb).await.unwrap();

        let r = Reconciler::new("node-1".into(), Arc::new(TestVmm));
        r.register_live("sbx-1".into()).await;
        let report = r.reconcile(&store).await;
        assert!(report.status_corrected.contains(&"sbx-1".to_string()));

        let got = store.get_sandbox("sbx-1").await.unwrap();
        assert_eq!(got.status, SandboxStatus::Running);
    }

    #[tokio::test]
    async fn orphan_process_counted_as_killed() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let r = Reconciler::new("node-1".into(), Arc::new(TestVmm));
        r.register_live("orphan-1".into()).await;
        let report = r.reconcile(&store).await;
        assert_eq!(report.orphans_killed, 1);
    }

    #[tokio::test]
    async fn other_nodes_ignored() {
        let store: Arc<dyn Store> = Arc::new(InMemoryStore::new());
        let mut sb = Sandbox::new("sbx-other".into(), SandboxSpec::default());
        sb.node_id = Some("node-2".into());
        sb.status = SandboxStatus::Running;
        store.create_sandbox(&sb).await.unwrap();

        let r = Reconciler::new("node-1".into(), Arc::new(TestVmm));
        let report = r.reconcile(&store).await;
        assert!(report.marked_error.is_empty());
    }
}