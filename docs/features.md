# 功能设计详解

每个功能按 **设计 / 配置 / 验证** 三部分说明。验证列给出可复现的命令或实测结论。

---

## 1. 沙盒生命周期

### 设计
- `POST /api/v1/sandboxes` 接收 `SandboxSpec`（image/resources/network/mounts/secrets/env/ttl/restart_policy 等扁平展开）。
- 创建流程：校验 → 认证 → 资源准入 → 预热快照认领（命中 restore 0.2s）→ VMM create → 网络 → agent Hello → `Running`。
- `GET/POST /api/v1/sandboxes/{id}/recover`：对非 running 沙盒重跑 provision（幂等）。
- TTL 到期强制销毁；`restart_policy`（never/on_failure/always）有界自动恢复（≤3 次）。
- 删除：VMM 强制停止 + 网络/资源/快照/副本全清理，返回 204。

### 配置
| 参数 | 说明 | 默认 |
|---|---|---|
| `ttl_secs` | 租期（秒），到期销毁；null 永不过期 | null |
| `start_timeout_secs` | agent 就绪超时 | 10 |
| `restart_policy` | `never`/`on_failure`/`always` | never |
| `sync` | true=同步等待 running；false=202 异步 | true |
| `node_selector` | 多节点调度标签 | 空 |

### 验证
```bash
curl -X POST localhost:8080/api/v1/sandboxes -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"docker.io/library/alpine:latest"},"ttl_secs":3600}'
# 201 → running；删除后 204，二次删除 404；TTL 到期自动销毁
```

---

## 2. 命令执行

### 设计
- `POST .../exec`：同步执行，返回 `{exec_id, exit_code, stdout, stderr, duration_ms, timed_out, stdout_truncated, stderr_truncated}`。
- `POST .../exec/stream`：SSE 增量输出（stdout/stderr/exit 事件）。
- 交互式进程（E2B Process Connect）：`ProcessStart` 支持 PTY（openpty + devpts + winsize），`Stdin/StdinEof/Signal/Resize/Update` 控制帧按进程 id 路由。
- 超时：`timeout_ms` 到期 SIGKILL 进程组；guest 无残留孤儿。
- 输出上限 1 MiB 截断（`*_truncated=true`）。
- 历史：`GET .../exec`（列表，limit 生效）、`GET .../exec/{exec_id}`。

### 配置
| 参数 | 说明 |
|---|---|
| `argv` | 命令 + 参数（必填，非空） |
| `env` | 环境变量（仅本次进程） |
| `cwd` | 工作目录（绝对路径） |
| `timeout_ms` | 超时（0 拒绝；默认 30000） |
| `stream` | 同步端点是否流式 |

### 验证
```bash
curl -X POST localhost:8080/api/v1/sandboxes/$ID/exec -H "Authorization: Bearer $KEY" \
  -d '{"argv":["sh","-c","echo out; echo err >&2; exit 7"],"timeout_ms":10000}'
# → exit_code 7, stdout "out\n", stderr "err\n"（分流正确）
curl -N localhost:8080/api/v1/sandboxes/$ID/exec/stream -H "Authorization: Bearer $KEY" \
  -d '{"argv":["sh","-c","for i in 1 2 3; do echo $i; sleep 1; done"],"timeout_ms":10000}'
# SSE 增量事件
```

---

## 3. 文件传输

### 设计
- `POST .../files/upload?path=/work/a.txt`：raw body 上传，agent 自动建父目录，权限 0644。
- `GET .../files/download?path=...`：下载，Content-Disposition 附件。
- `GET .../files/ls?path=...`：列目录 `{items:[{name,size,mode,mtime,is_dir}]}`。
- 路径安全：拒绝 traversal（`..`、编码 `%2e%2e`）、symlink 逃逸、控制字符；缺失文件映射 404（`guest_fs_error`）。

### 验证
```bash
curl -X POST "localhost:8080/api/v1/sandboxes/$ID/files/upload?path=/work/hello.txt" \
  -H "Authorization: Bearer $KEY" --data-binary "hello"
curl "localhost:8080/api/v1/sandboxes/$ID/files/download?path=/work/hello.txt" -H "Authorization: Bearer $KEY"
curl "localhost:8080/api/v1/sandboxes/$ID/files/ls?path=/work" -H "Authorization: Bearer $KEY"
```

---

## 4. 网络隔离

### 设计
每沙盒独立 **netns 拓扑**：
```
netns clo-<hash>:
  br0 (10.a.b.1/30) ← tap0 ← FC eth0 (10.a.b.2)
              ← vn<id> ←→ vh<id>（宿主侧 .1/30 + 路由）
```
- 网段：默认 `sandbox_id` SHA-256 派生 `10.{a}.{b}.0/30`；快照继承路径用 `SubnetAllocator` 顺序分配。
- **宿主侧 nftables**（`clo_<hash>` 表）：`allow_egress` 白名单 accept + `10/8`/`127/8`/established accept + **drop 兜底**（空 allowlist = 全拒出站）。
- **netns 内 nftables**：forward drop + 仅 agent(5201)/DNS(53)/已建立放行 + SNAT。
- **DNS 代理**：白名单域名解析（hickory），动态放行解析出的 IP。
- 带宽：`bandwidth_mbps` → netns vn 上 `tc tbf`。
- `network.enabled=false`：完全离线（仅管理网）。

### 配置
| 参数 | 说明 |
|---|---|
| `network.enabled` | true/false（false=离线） |
| `network.allow_egress` | 出站域名白名单；空=拒绝全部 |
| `network.deny_egress` | 出站 IP/CIDR 拒绝（allow 优先） |
| `resources.bandwidth_mbps` | 出站带宽上限 |

### 验证（实测）
- 空 allowlist：`wget http://93.184.216.34` → rc=1（拦截）
- 白名单 example.com：可达；非白名单域名：DNS 拒绝
- 跨沙盒 A→B guest IP：连接失败（隔离）
- host 入站非 agent 端口：拒绝
- 删除后 netns/veth/nft 零残留

---

## 5. 资源与数据面

### 设计
| spec 字段 | 校验 | 数据面 |
|---|---|---|
| `resources.vcpu` | 1–4 | FC machine-config / docker NanoCPUs |
| `resources.memory_mb` | 64–8192 | FC / docker memory |
| `resources.disk_mb` | ≥64 | FC 磁盘（docker-dev 记录限制不施加） |
| `resources.bandwidth_mbps` | 0 拒绝 | netns tc tbf |
| `resources.iops` | 0 拒绝 | host cgroup io.max（需 io 控制器暴露设备） |
| `resources.pids_max` | 0 拒绝 | guest cgroup v2 pids.max |
| `mounts` | source 存在 + target 绝对无穿越 | FC 共享 / docker bind ro |
| `secrets` | name 无穿越/重复拒绝 | agent 写 /run/secrets（0600） |
| `env` | 任意键值 | agent exec 注入 + 系统键可覆盖 |
| `init_command` | 非空 | agent 就绪后执行，失败回滚 Error |
| `volume_mounts` | E2B 卷 | 物化到 guest |

**rootfs 隔离**：冷创建复制每沙盒独立 ext4 副本（`rootfs_work_dir/{id}.ext4`，FC drive 指向副本），stop 清理——沙盒 A 写满不影响 B（实测）。

### 验证
```bash
curl -X POST localhost:8080/api/v1/sandboxes -H "Authorization: Bearer $KEY" \
  -d '{"image":{"reference":"docker.io/library/alpine:latest"},
       "resources":{"vcpu":2,"memory_mb":512,"disk_mb":512,"pids_max":20,"bandwidth_mbps":1},
       "secrets":[{"name":"token","value":"s3cret"}]}'
# pids.max=20 → guest fork 超限被拒（内核日志 cgroup: fork rejected）
# /run/secrets/token 内容 s3cret 权限 600；响应 REDACTED
```

---

## 6. 快照与预热

### 设计
- **预热池**：`warm_persisted_templates` 后台为持久化模板 `warm_snapshot`：临时沙盒（分配子网）→ agent Hello → 静默 5s → pause → Full 快照 → stop → 清理临时 netns。快照与子网一对一绑定。
- **create 快路径**：无 warm slot 时 `claim_snapshot` → 继承快照子网建 netns → `/snapshot/load`（restore 前**不配置任何设备**，FC 会拒绝）→ 跳过 start。实测 **0.19–0.21s**（冷创建 ~30x）。
- **公开 API**：`POST /sandboxes/{id}/snapshots`、`GET /snapshots`、`POST /sandboxes/{id}/fork`（从快照 fork 新沙盒，继承源子网，vsock 冲突处理）。
- 快照 clone 的 rootfs 共享为 FC dev-preview 已知限制。

### 配置
| 项 | 说明 |
|---|---|
| `CLOUISLE_WARM_POOL_MIN_IDLE` | 预热池最小空闲数（0=关闭） |
| 内核 | 快照兼容内核（实测 `/opt/clouisle/vmlinux-vsock` 4.14.193 稳定；自定义 7.0 内核恢复崩溃） |

### 验证（实测）
- create 命中快照：`201 in 0.20s`，`vmm_meta.extra.subnet="10.10"`，恢复后 exec 连续通过（+25s/+45s）
- fork：`snapshot 201 → fork 200 → running → exec 'forked-ok'`，子网继承一致

---

## 7. E2B 兼容

### 设计
- `/sandboxes`（E2B 创建/列表）、`/v2/sandboxes` 分页（`X-Total-Running`）、pause/resume/timeout/refresh/network。
- Filesystem/Process gRPC 风格端点：`/filesystem.Filesystem/*`、`/process.Process/*`（Start/List/Connect/Update/StreamInput/SendInput/SendSignal/CloseStdin）。
- envd REST：`/init`、`/envs`、`/files/compose`（freeze/unfreeze 等明确不可用）。
- 官方 Python/TypeScript SDK 已联调（`scripts/e2b-sdk-acceptance.py`）；TS 真 API 交叉验证。
- 控制面：teams/api-keys（单向散列）/access-tokens/templates/builds/tags/volumes/nodes/metrics/admin。

### 配置
`CLOUISLE_API_KEYS` 多 key 格式：`key1:tenant:scope,key2:tenant2:scope`（scope=full/read）。

### 验证
```bash
# 官方 Python SDK
python - <<'EOF'
from e2b import Sandbox
sb = Sandbox(template="docker.io/library/alpine:latest", api_key=os.environ["E2B_API_KEY"])
print(sb.is_running())      # True
print(sb.commands.run("echo hi").stdout)  # hi
sb.kill()
EOF
```

---

## 8. 多节点（clouisled gRPC）

### 设计
- `clouisled` 节点守护：gRPC `NodeService`（Register/Heartbeat 双向流/CreateSandbox/DeleteSandbox/Exec/FileOp）。
- API 侧：`--node-endpoint <grpc://host:port>` 直连单节点；`--cluster-scheduling` 按 heartbeat 注册表调度。
- 节点独立管理本地 FC + 网络；API 只做控制面。
- 部署约束：API 与 clouisled 必须用**独立 api-socket-dir**（共享会误判孤儿）。

### 验证
- Register（空=auto / 自定义）、Heartbeat 全字段、CreateSandbox（真实 FC）、Exec（stdout/exit）、空流 INVALID_ARGUMENT、删除/404 全部实测通过。

---

## 9. 认证与租户

### 设计
- `CLOUISLE_API_KEYS` 启动注册；无注册 key 时开发模式放行。
- Bearer 认证：full=读写，read=只读（403 拒绝写）；错误 key/Basic/大小写 → 401。
- `/health`、`/metrics` 免认证（探活/采集）。
- 租户：sandbox 绑定 `tenant_id`；e2b_cloud 路径 `require_tenant` 强制 team 隔离。
- **缺口**：本地单节点 API 的 read key 可读任意沙盒元数据（多租户部署需按 team 过滤查询）。

### 验证
```bash
curl -H "Authorization: Bearer wrong" localhost:8080/api/v1/sandboxes   # 401
curl -H "Authorization: Bearer $READ_KEY" -X POST .../sandboxes -d '{}'  # 403
curl localhost:8080/health                                                 # 200
```

---

## 10. 存储

### 设计
- 自动选择：`postgres://` / `postgresql://` → PostgresStore；其余 SQLite（WAL）。
- SQLite：WAL 模式、10 并发写无锁、重启恢复。
- PostgreSQL：HA 模式、断库 health 503 降级、**自动重连**（`ensure_client` is_closed 检测）、错误连接串快速失败。
- 存储对象：sandboxes（含 vmm_meta/状态机）、executions、nodes、e2b 控制面（teams/keys/templates/volumes/snapshots）。

### 验证
```bash
clouisle-api --db postgres://user:pass@host:5432/clouisle   # HA
clouisle-api --db clouisle.db                                # SQLite
# 断库 → health {"status":"degraded"}；恢复 → 自动重连日志 + 200
```

---

## 11. 可观测性

| 端点 | 说明 |
|---|---|
| `GET /health` | `{"status":"ok","store":"ok","version":"..."}` |
| `GET /health/live` | `{"status":"alive"}` |
| `GET /health/ready` | `{"status":"ready"}`；断库时 `not_ready` |
| `GET /metrics` | Prometheus 文本（`clouisle_api_requests_total` 等） |
| `X-Request-Id` | 响应透传 UUID（请求追踪） |

### 验证
```bash
curl localhost:8080/metrics | head
# clouisle_api_requests_total{method="GET",path="/health",status="200"} 3
```

---

## 12. Docker 开发后端（docker-dev）

### 设计
- `--backend docker-dev`：沙盒以 Docker 容器运行（应用镜像 + 注入静态 `clouisle-agent` 作 PID 1），复用同一帧协议。
- 网络：`clouisle-dev-mgmt`（**internal**，仅 agent 5201）+ `clouisle-dev-egress`（open 出网）。
- 资源：CPU/内存/pids 映射 Docker；iops/带宽/allowlist/restart≠never 创建时拒绝；快照明确不支持。
- 入口：`docker compose -f docker-compose.dev.yml up --build`（macOS/Windows Docker Desktop）。
- **安全边界**：Docker socket 仅 dev Compose 挂载（host 等效权限）；生产 manifest 零 socket。

### 配置
| 项 | 值 |
|---|---|
| `--backend docker-dev` | 与 `--node-endpoint`/`--cluster-scheduling` 互斥 |
| `DockerDevConfig.mgmt_network` | `clouisle-dev-mgmt`（internal） |
| `DockerDevConfig.egress_network` | `clouisle-dev-egress` |
| `DockerDevConfig.mount_root` | 允许的挂载根（默认 /tmp/clouisle-dev-mounts） |

### 验证（实测）
- create → running（`vmm_meta.backend=docker-dev`）→ exec → 文件 → 删除（容器零残留）
- offline：exec 通（mgmt）+ 无出口（internal）；open：出网 rc=0；allowlist 400
- fake engine 单测 4 个（顺序/拒绝/补偿）；FC KVM 回归 206/0 无破坏

---

## 13. 清理与回收

### 设计
- 删除/失败/TTL 到期统一走：VMM stop（进程组 SIGKILL）→ 删 socket → 删 netns/veth/nft 表 → 停 DNS 代理 → 删 rootfs 副本 → 删 io cgroup → 释放快照/资源。
- reconcile：孤儿 runtime 回收、死 runtime 标 Error、存活保持 Running、资源池恢复。
- 幂等：重复删除 404；已不存在资源容忍。

### 验证
删除后断言：`ip netns list` 无 `clo-*`、`nft list tables` 无 `clo_*`、`/tmp/clouisle-cache/.rootfs` 空、`/sys/fs/cgroup/clouisle-io` 空、docker 无 `com.clouisle.managed=true` 容器——全部实测为零。
