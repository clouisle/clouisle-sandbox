//! guest 内资源限制（cgroup v2）。

use std::fs;

/// 施加 pids.max：确保 cgroup v2 挂载于 `/sys/fs/cgroup` 且 `pids`
/// controller 已通过 `cgroup.subtree_control` 启用，创建 `clouisle`
/// 子 cgroup，写入 `pids.max`，并把 PID 1（agent，guest 内所有进程的祖先）
/// 移入——guest 内进程总数受限于该上限。`None` 不修改。
pub fn apply_pids_max(pids_max: Option<u32>) -> Result<(), String> {
    let Some(max) = pids_max else {
        return Ok(());
    };
    ensure_cgroup2()?;
    // 在 root cgroup 启用 pids controller（agent 是 PID 1 root，可写）。
    fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+pids")
        .map_err(|error| format!("enable pids controller: {error}"))?;
    fs::create_dir_all("/sys/fs/cgroup/clouisle")
        .map_err(|error| format!("create /sys/fs/cgroup/clouisle: {error}"))?;
    fs::write("/sys/fs/cgroup/clouisle/pids.max", max.to_string())
        .map_err(|error| format!("write pids.max: {error}"))?;
    // 移入自身；后续所有 fork/exec 继承该 cgroup。
    fs::write(
        "/sys/fs/cgroup/clouisle/cgroup.procs",
        std::process::id().to_string(),
    )
    .map_err(|error| format!("move agent into cgroup: {error}"))?;
    tracing::info!(pids_max = max, "applied guest pids.max");
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_cgroup2() -> Result<(), String> {
    use nix::errno::Errno;
    use nix::mount::{MsFlags, mount};

    // cgroup v2 判定：/proc/self/cgroup 有 "0::" 行。
    let proc_cgroup =
        fs::read_to_string("/proc/self/cgroup").map_err(|error| format!("read cgroup: {error}"))?;
    if proc_cgroup.lines().any(|line| line.starts_with("0::")) {
        return Ok(());
    }
    fs::create_dir_all("/sys/fs/cgroup")
        .map_err(|error| format!("create /sys/fs/cgroup: {error}"))?;
    match mount(
        Some("cgroup2"),
        "/sys/fs/cgroup",
        Some("cgroup2"),
        MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) | Err(Errno::EBUSY) => Ok(()),
        Err(error) => Err(format!("mount cgroup2: {error}")),
    }
}
