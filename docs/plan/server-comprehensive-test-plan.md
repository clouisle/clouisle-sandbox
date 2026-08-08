# 服务器全量验收测试计划

## 1. 文档目的

本计划用于在 Linux + KVM 服务器上，对 Clouisle Sandbox 当前代码实际提供的全部可执行功能、请求参数、边界条件、失败清理和 SDK 调用链进行一次完整验收。

本文件是**测试计划，不是测试结果**。执行完成前不得把未执行的用例标记为通过。

### 1.1 验收目标

- 每个当前暴露的 HTTP 路由至少覆盖：成功、参数边界、非法输入、资源不存在、状态冲突、服务异常。
- `SandboxSpec`、`Resources`、`NetworkConfig`、`ExecRequest`、文件查询参数和列表查询参数逐字段验证。
- 每个成功创建的沙盒验证真实 Firecracker、guest agent、网络、执行、文件和删除清理链路。
- 每个失败路径验证：API 错误格式、数据库状态、VMM 进程、netns、TAP、veth、nftables、DNS 线程和资源预留均无异常残留。
- SQLite 与 PostgreSQL 两种存储模式分别验证启动、读写、重启恢复和并发行为。
- Rust、Python、TypeScript/JavaScript SDK 对当前 API 的公开方法逐个调用；SDK 未覆盖的 API 必须记录为缺口，不得记为通过。
- `clouisled` gRPC 的 Register、Heartbeat、CreateSandbox、DeleteSandbox、Exec 五个 RPC 逐个验证。

### 1.2 当前实现边界

测试以当前代码为准，不以旧文档中的假设为准。以下差异必须在执行报告中单独记录：

- 文件上传当前是 `POST /files/upload?path=...`，不是旧测试策略中写过的 `PUT /files`。
- 指标当前是 `/metrics`，不是 `/api/v1/metrics`。
- 路由当前没有快照、审计、资源热更新、镜像拉取进度等端点；这些功能不能用“未测试”代替，应标为 `NOT_EXPOSED`，并附路由或源码证据。
- `CreateSandboxRequest.sync` 已被解析，但当前创建处理仍走同步启动路径；必须验证实际行为并记录是否需要修复。
- `ExecRequest.stream` 在同步端点上的语义必须实测；流式行为以 `/exec/stream` 的 SSE 响应为准。
- `GET .../exec` 的 `limit` 字段已定义，但当前 handler 可能未应用该参数；必须做边界测试并记录结果。
- 认证器在没有注册 key 时进入开发模式；需要分别验证空 key 开发模式和注册 key 后的受保护模式。若生产启动路径没有注册 key 的配置入口，记录为发布阻塞缺口。
- Python SDK 当前公开方法少于 Rust/TypeScript SDK；缺少的方法必须列入 SDK 缺口，不得绕过 SDK 直接调用 HTTP 后算 SDK 通过。

## 2. 测试环境与隔离

### 2.1 目标服务器

| 项目 | 值 |
|---|---|
| SSH 主机 | `root@10.144.144.2` |
| 平台 | Ubuntu 24.04，x86_64 |
| CPU | 16 vCPU |
| KVM | `/dev/kvm` 必须存在且可用 |
| Firecracker | v1.10.1 |
| 代码目录 | `/root/clouisle-sandbox` |
| 内核 | `/opt/clouisle/vmlinux` |
| Docker 容器 | `clouisle-test` |
| API 测试端口 | `9090` |
| 测试镜像 | `clouisle-sandbox:acceptance` |

执行前重新同步当前提交，不使用服务器上旧的 `target` 或旧镜像作为结论依据：

```bash
rsync -az --delete \
  --exclude=target --exclude=.git --exclude='*.db*' \
  -e ssh . root@10.144.144.2:/root/clouisle-sandbox/
```

### 2.2 测试数据与容器

- 使用独立测试数据库，例如 `/data/clouisle-acceptance.db`；禁止复用生产数据库。
- PostgreSQL 测试使用独立 database/schema，并在报告中记录连接串是否为 `postgres://` 或 `postgresql://`。
- 每个测试批次使用唯一前缀和时间戳；记录 sandbox ID、Firecracker PID、netns 名称、veth 名称、API 日志偏移量。
- 测试结束必须执行全局清理，并确认：
  - `GET /api/v1/sandboxes` 中没有本批次遗留对象；
  - `ip netns list` 没有本批次 `clo-*`；
  - `ip link` 没有本批次 `vh*` / `vn*` / `tap0` 残留；
  - 没有本批次 Firecracker 进程、API socket、vsock socket；
  - nftables 表、DNS 线程和路由均已清理。

### 2.3 证据要求

每个用例至少保存：

```text
case_id
request/command
HTTP 或 gRPC status
response body / headers
sandbox_id、exec_id（如适用）
相关日志片段
宿主状态快照（进程、netns、link、route、nft、cgroup）
result: PASS | FAIL | BLOCKED | NOT_EXPOSED
备注和缺陷编号
```

建议目录：

```text
/tmp/clouisle-acceptance/<timestamp>/
  preflight.txt
  api/
  network/
  vmm/
  storage/
  grpc/
  sdk/
  logs/
  cleanup.txt
  summary.md
```

## 3. 通过标准、失败标准和执行顺序

### 3.1 通过标准

- 返回状态码、错误 code、响应字段和副作用均符合当前契约。
- 成功路径不仅返回 HTTP 200/201，还必须验证真实副作用，例如 guest 内命令结果、文件内容、数据库记录和宿主清理状态。
- 负向用例必须验证错误发生在预期边界，并验证不产生半创建对象、资源泄漏或权限绕过。
- 所有参数边界都必须有明确结果：`PASS`、`FAIL` 或 `NOT_EXPOSED`；禁止空白。
- 任何一个安全隔离、跨沙盒访问或失败清理用例失败，整轮验收结论为 `FAIL`。

### 3.2 执行顺序

1. 服务器和镜像预检。
2. API 启动、健康检查、数据库连接。
3. 参数校验和纯 HTTP 负向用例。
4. 单沙盒完整生命周期。
5. 执行和文件传输。
6. 网络隔离和清理。
7. 失败注入、并发和资源压力。
8. SQLite/PostgreSQL 重启恢复。
9. clouisled gRPC。
10. 四种 SDK。
11. 全局清理、证据汇总和缺陷复测。

## 4. ENV：服务器、镜像和启动预检

| ID | 检查 | 步骤 | 通过标准 |
|---|---|---|---|
| ENV-SRV-001 | OS/架构 | `uname -a; uname -m; nproc` | Linux、x86_64、16 vCPU 信息与报告一致 |
| ENV-SRV-002 | KVM | `test -e /dev/kvm; stat /dev/kvm` | 文件存在、容器可访问；否则停止 KVM 用例 |
| ENV-SRV-003 | Firecracker | `firecracker --version` | 版本为 v1.10.1 |
| ENV-SRV-004 | 内核镜像 | `file /opt/clouisle/vmlinux` | 可被 Firecracker 接受的 x86_64 ELF |
| ENV-SRV-005 | rootfs | 检查 images 目录和 alpine rootfs | rootfs 存在、可读、大小合理；缺失时记录 VMM 前置失败 |
| ENV-SRV-006 | 工具 | `docker`, `curl`, `jq`, `ip`, `nft`, `ss`, `ps`, `sha256sum` | 工具齐全；缺少工具不得用人工猜测代替证据 |
| ENV-SRV-007 | cgroup | `stat -fc %T /sys/fs/cgroup; mount | grep cgroup` | cgroup v2，记录可写限制能力 |
| ENV-SRV-008 | 旧残留 | 清点旧容器、Firecracker、`clo-*` netns、`vh*`/`vn*` | 测试前基线明确；外部残留不得误归因于本轮 |
| ENV-SRV-009 | 镜像构建 | `docker build -t clouisle-sandbox:acceptance .` | 构建成功；记录 Docker 非阻断 warning |
| ENV-SRV-010 | Docker 运行参数 | 以 `--privileged --network host -v /dev/kvm:/dev/kvm` 启动 | API 监听成功，容器内可创建 netns 和 Firecracker |
| ENV-SRV-011 | API 监听参数 | `--addr 0.0.0.0:9090` 与默认地址各启动一次 | 指定地址和默认地址行为符合 CLI 说明 |
| ENV-SRV-012 | 数据库参数 | SQLite 路径、`postgres://...`、`postgresql://...` 各启动一次 | 自动选择正确 Store；错误连接串快速失败且错误明确 |

## 5. HEALTH：健康、就绪、指标和请求 ID

| ID | 请求/步骤 | 预期 |
|---|---|---|
| HEALTH-001 | `GET /health` | 200；`status=ok`、`store=ok`、`version` 存在 |
| HEALTH-002 | `GET /health/live` | 200；`{"status":"alive"}` |
| HEALTH-003 | `GET /health/ready` | 200；`{"status":"ready"}` |
| HEALTH-004 | 健康端点无 Authorization | 开发模式和注册 key 模式均可访问；确认 `/health` 不被认证拦截 |
| HEALTH-005 | `/metrics` 状态与 Content-Type | 200；`text/plain; version=0.0.4`；响应可被 Prometheus 文本解析 |
| HEALTH-006 | 指标初始值 | 清空测试库后抓取基线，记录相关指标名称和初值 |
| HEALTH-007 | 创建/执行/删除后的指标 | 对比前后计数、时延 histogram；无负数或格式损坏 |
| HEALTH-008 | `X-Request-Id` 已提供 | 若当前 middleware 暴露该行为，响应透传同值；否则标记实际缺口 |
| HEALTH-009 | 缺少 `X-Request-Id` | 若当前 middleware生成请求 ID，验证 UUID/格式；否则标记缺口 |
| HEALTH-010 | SQLite 故障 | 停止/破坏测试数据库后访问 `/health` 与 `/health/ready` | 返回 503/degraded 或 503/not_ready，不能伪报 ready |

## 6. CREATE：`POST /api/v1/sandboxes` 逐字段参数矩阵

请求体是 `CreateSandboxRequest`：`SandboxSpec` 字段扁平展开，另有 `sync`。

### 6.1 JSON 结构和通用输入

| ID | 输入 | 预期 |
|---|---|---|
| CREATE-JSON-001 | 最小合法体：`image.reference` + 默认资源 | 201；状态为 running；响应包含 id/spec/status/timestamps |
| CREATE-JSON-002 | 缺少 `image` | 4xx `VALIDATION` 或框架解码错误；错误可定位 |
| CREATE-JSON-003 | `image=null` | 4xx，不能 panic |
| CREATE-JSON-004 | 空对象 `{}` | 4xx，错误指出 image 或必需字段 |
| CREATE-JSON-005 | 非 JSON、数组、字符串、重复字段 | 4xx；服务保持可用 |
| CREATE-JSON-006 | 未知字段 | 记录 serde 当前行为；若被忽略，确认没有误覆盖已知字段 |
| CREATE-JSON-007 | `null` 与省略的可选字段 | 对每个字段比较默认值和显式 null 行为 |
| CREATE-JSON-008 | Unicode、超长字符串、特殊 JSON 字符 | 成功时原样持久化；失败时为明确 validation，不崩溃 |

### 6.2 `image`

| ID | 参数 | 边界/输入 | 预期 |
|---|---|---|---|
| CREATE-IMG-001 | `image.reference` | `alpine:latest` | 成功 |
| CREATE-IMG-002 | `image.reference` | 空字符串、全空格 | 400，字段 `image` |
| CREATE-IMG-003 | `image.reference` | registry、namespace、tag、digest 引用 | 解析/缓存键保持正确；实际 rootfs 存在时可启动 |
| CREATE-IMG-004 | `image.reference` | 超长、含空格、控制字符 | 明确拒绝或明确规范化，不允许路径逃逸 |
| CREATE-IMG-005 | `image.digest` | 省略/null | 使用 reference 缓存键 |
| CREATE-IMG-006 | `image.digest` | 合法 `sha256:...` | 响应和存储保留 digest；匹配镜像时成功 |
| CREATE-IMG-007 | `image.digest` | 错误算法、空串、非法字符 | 明确失败或记录当前未校验缺口 |
| CREATE-IMG-008 | 不存在镜像/rootfs | 合法 spec 指向不存在镜像 | 503/VMM 错误；数据库可见 error；无 netns/VMM 残留 |

### 6.3 `resources`

资源默认值：`vcpu=1`、`memory_mb=256`、`disk_mb=512`、`bandwidth_mbps=null`、`iops=null`、`pids_max=512`。

| ID | 参数 | 必测值 | 预期 |
|---|---|---|---|
| CREATE-RES-001 | `vcpu` | 缺省、0、1、4、5、最大整数 | 0 和 >4 为 400；1/4 在宿主资源足够时成功 |
| CREATE-RES-002 | `memory_mb` | 缺省、63、64、8192、8193 | `<64` 和 `>8192` 为 400；边界合法值可进入准入 |
| CREATE-RES-003 | `disk_mb` | 缺省、63、64、超大值 | `<64` 为 400；合法值持久化并传入 VMM |
| CREATE-RES-004 | `bandwidth_mbps` | 缺省/null、0、1、10 | 0 为 400；合法值响应/存储保留；实际限速另见 NET/RES |
| CREATE-RES-005 | `iops` | 缺省/null、0、1、1000 | 0 为 400；合法值响应/存储保留；实际限制另见 RES |
| CREATE-RES-006 | `pids_max` | 缺省、0、1、512、超大值 | 当前校验/生效行为必须实测；若仅持久化未施加 cgroup，记录缺口 |
| CREATE-RES-007 | 多字段同时非法 | vcpu=0、memory=16、disk=8、bandwidth=0、iops=0 | 返回完整字段错误集合，不能只返回第一个 |
| CREATE-RES-008 | 资源准入超限 | 并发或累计请求超过主机容量 | 507 `RESOURCE_EXHAUSTED`；已成功对象不超售，失败请求释放 reservation |
| CREATE-RES-009 | 删除释放资源 | 创建占满资源的对象，删除后再次创建同等资源 | 再次成功，资源计数恢复 |

### 6.4 `network`

| ID | 参数 | 必测值 | 预期 |
|---|---|---|---|
| CREATE-NET-001 | `network` 缺省 | 不传 network | 使用 enabled=true、空 allowlist 默认值 |
| CREATE-NET-002 | `enabled` | true/false | 响应保留值；false 时验证 guest 无外部网络且 TAP/VMM 行为符合设计 |
| CREATE-NET-003 | `allow_egress` | 空数组 | 所有非允许出站失败 |
| CREATE-NET-004 | `allow_egress` | 单域名、多域名、重复域名、大小写、尾点 | DNS/nft 规则与实际解析一致 |
| CREATE-NET-005 | `allow_egress` | 非法域名、IP 字面量、超长域名 | 明确拒绝或按设计不放行；不能绕过白名单 |
| CREATE-NET-006 | CNAME 域名 | 允许域名存在多跳 CNAME | 最终 A/AAAA 行为符合 allowlist 规则 |

### 6.5 `mounts`、`secrets`、租期和调度字段

| ID | 参数 | 必测值 | 预期 |
|---|---|---|---|
| CREATE-MISC-001 | `mounts` | 缺省、空数组、单挂载、多挂载 | 响应/存储字段完整；实际 guest 挂载行为必须验证，不支持则标缺口 |
| CREATE-MISC-002 | mount `source` | 存在、不存在、相对路径、`..`、文件而非目录 | 明确成功或 4xx/5xx；不能越界或静默忽略 |
| CREATE-MISC-003 | mount `target` | `/work`、空串、相对路径、`/`、路径穿越 | 明确拒绝非法目标；合法目标行为可观测 |
| CREATE-MISC-004 | mount `readonly` | true/false | guest 写入结果与只读语义一致 |
| CREATE-MISC-005 | `secrets` | 缺省、空数组、单/多 secret | `/run/secrets/<name>` 内容和权限正确 |
| CREATE-MISC-006 | secret `name` | 合法、空、`../x`、含 `/`、重复名 | 非法拒绝；不允许路径逃逸；重复处理明确 |
| CREATE-MISC-007 | secret `value` | 空、Unicode、换行、特殊字符 | guest 内容精确；日志、响应、审计不得泄露 value |
| CREATE-MISC-008 | `ttl_secs` | 缺省/null、1、边界、超大值 | null 永不过期；1 秒后强制清理；边界行为记录 |
| CREATE-MISC-009 | `start_timeout_secs` | 缺省、0、1、300、301 | 0/301 为 400；1 可触发超时清理；300 可接受 |
| CREATE-MISC-010 | `env` | 空、普通键值、空值、Unicode、覆盖系统键 | guest 可见且不泄露到不相关沙盒 |
| CREATE-MISC-011 | `node_selector` | 空、单标签、多标签、不匹配标签 | 单节点当前行为明确；不匹配不能静默调度到错误节点 |
| CREATE-MISC-012 | `restart_policy` | `never`、`on_failure`、`always`、未知值 | 合法值保留并触发对应行为；未知值必须拒绝或记录未校验缺口 |
| CREATE-MISC-013 | `tenant_id` | 缺省、合法、空、跨租户值 | 持久化和认证隔离行为符合当前实现；无租户功能时标缺口 |
| CREATE-MISC-014 | `sync` | true、false、缺省 | 实测是否同步等待 running；与响应状态和文档一致，否则记录缺口 |

## 7. LIFECYCLE：生命周期和列表查询参数

| ID | 请求 | 预期 |
|---|---|---|
| LIFE-001 | 创建 → GET `/api/v1/sandboxes/{id}` | id、spec、status、时间戳、vmm_meta 一致 |
| LIFE-002 | GET 合法 UUID v7 | 200 |
| LIFE-003 | GET 空、非法 UUID、路径编码 ID | 404 或 validation，不能 500 |
| LIFE-004 | GET 不存在 ID | 404 `NOT_FOUND` |
| LIFE-005 | DELETE running sandbox | 204；VMM 强制停止；网络和资源清理 |
| LIFE-006 | DELETE 不存在 ID | 404 |
| LIFE-007 | DELETE 同一 ID 两次 | 第一次 204，第二次 404；无残留 |
| LIFE-008 | DELETE 后 GET/exec/files | 均返回 404 或符合统一状态错误，不得访问旧 VMM |
| LIFE-009 | `status` 过滤 | pending、starting、running、stopping、stopped、error 逐个请求 | 只返回对应状态 |
| LIFE-010 | `status` 未知值 | 记录当前映射行为；不得把未知值误当 running |
| LIFE-011 | `limit` | 缺省、0、1、100、超过总数、极大值、非数字 | 记录实际默认/最小值和响应 total；不能 panic |
| LIFE-012 | `offset` | 缺省、0、1、总数、超过总数、极大值、非数字 | 空页或 4xx，行为稳定 |
| LIFE-013 | limit + offset + status 组合 | 组合分页结果与 total 正确 |
| LIFE-014 | 并发创建/删除 | 100 个请求或受宿主容量限制的批次 | 无重复 ID、无错误状态穿越、无资源超售 |

## 8. EXEC：同步、SSE 和执行历史

### 8.1 `ExecRequest` 字段

| ID | 参数 | 必测值 | 预期 |
|---|---|---|---|
| EXEC-PARAM-001 | `argv` | 缺失、空数组、`echo hello`、多参数、Unicode | 空数组 400；合法命令结果正确 |
| EXEC-PARAM-002 | `env` | 缺省、空、`FOO=bar`、多个值、特殊字符 | guest 进程可见，记录 spec 保留 |
| EXEC-PARAM-003 | `cwd` | 缺省/null、`/tmp`、存在目录、不存在路径、文件路径、路径穿越 | 合法 cwd 成功；非法返回清晰错误 |
| EXEC-PARAM-004 | `timeout_ms` | 缺省、0、1、500、30000、超大整数 | 0 为 validation；短超时 timed_out；默认 30000 生效 |
| EXEC-PARAM-005 | `stream` | 缺省、false、true | 同步端点与 SSE 端点行为分别记录，不允许参数静默改变协议 |
| EXEC-PARAM-006 | unknown/null 字段 | 类型错误、未知字段 | 4xx 或明确 serde 行为 |

### 8.2 执行功能

| ID | 用例 | 预期 |
|---|---|---|
| EXEC-001 | `echo hello` | 200，exit_code=0，stdout=`hello\n` |
| EXEC-002 | stdout/stderr 分流 | stdout、stderr 各自完整，不串流 |
| EXEC-003 | `exit 7` | exit_code=7，API 仍返回执行结果而非 API 错误 |
| EXEC-004 | 命令不存在 | 明确非零结果或 VMM 错误；服务不崩溃 |
| EXEC-005 | `sleep` 超时 | `timed_out=true`、exit_code=-1；guest 无残留进程 |
| EXEC-006 | 超时后的进程组 | 后台子进程也被回收，宿主无 orphan |
| EXEC-007 | 空输出/二进制输出 | 响应 UTF-8 转换行为稳定；截断字段准确 |
| EXEC-008 | 输出超过 1 MiB | stdout/stderr 分别截断到上限，`*_truncated=true`，数据库一致 |
| EXEC-009 | stopped/error sandbox exec | 409 或当前统一状态错误；不触发 guest 连接 |
| EXEC-010 | `/exec/stream` stdout | SSE event 为 stdout，数据按协议可解析 |
| EXEC-011 | `/exec/stream` stderr/exit | 收到 stderr 和 exit 事件，exit_code 正确 |
| EXEC-012 | SSE 长输出/增量输出 | 客户端按事件读取；记录当前实现是否真实增量而非一次性 fallback |
| EXEC-013 | 两个沙盒并发 exec | 输出、执行记录、进程和网络不串扰 |

### 8.3 执行历史

| ID | 请求 | 预期 |
|---|---|---|
| EXEC-HIST-001 | GET `/api/v1/sandboxes/{id}/exec` | 返回该 sandbox 的记录，字段和顺序稳定 |
| EXEC-HIST-002 | `limit` 缺省/0/1/大值/非法 | 验证 handler 是否真正应用 limit；当前若忽略必须记录缺陷 |
| EXEC-HIST-003 | GET 单条 `/exec/{exec_id}` | 返回完整记录，sandbox_id 匹配 |
| EXEC-HIST-004 | 使用其他 sandbox 的 exec_id | 404，不泄露跨 sandbox 记录 |
| EXEC-HIST-005 | 不存在 exec_id | 404 |
| EXEC-HIST-006 | 删除 sandbox 后历史 | 按当前 store 删除语义验证记录是否一并删除，并记录文档一致性 |

## 9. FILE：文件上传、下载、目录和路径安全

| ID | 请求/参数 | 必测值 | 预期 |
|---|---|---|---|
| FILE-001 | upload `path` | 缺失、空串 | 400 validation |
| FILE-002 | upload body | 空、1B、1KB、50MiB、50MiB+1 | 上限内成功；超过上限 400；无半文件 |
| FILE-003 | upload path | `/work/a.txt`、Unicode、空格、特殊字符 | 内容和名称精确 |
| FILE-004 | upload traversal | `/work/../../etc/passwd`、编码后的 `..` | 拒绝；宿主和 guest `/etc/passwd` 不变 |
| FILE-005 | upload symlink escape | guest 内准备指向 `/etc` 的 symlink | 拒绝或安全限制在沙盒根内 |
| FILE-006 | upload parent missing | `/work/nope/a.txt` | 按当前 agent 语义成功自动建目录或明确失败；记录实际契约 |
| FILE-007 | download existing | 文本、二进制、空文件 | bytes、Content-Type、Content-Disposition 正确 |
| FILE-008 | download missing | 不存在 path | 404/明确 agent 错误 |
| FILE-009 | download traversal | 多种编码路径穿越 | 拒绝 |
| FILE-010 | list files | `/`、`/work`、空、missing dir | `items` 结构、name/size/mode/mtime/is_dir 正确 |
| FILE-011 | stopped/nonexistent sandbox | upload/download/ls | 404 或 409；无 VMM 连接尝试 |
| FILE-012 | 并发同路径写入 | 两个不同内容并发 upload | 最终内容完整，不出现交错半文件 |
| FILE-013 | SDK 字节传输 | Rust/Python/TypeScript upload/download | sha256 与 HTTP 直测一致 |

## 10. NET：每沙盒网络隔离和清理

每个网络用例都保存创建前、运行中、删除后的 `ip`, `bridge fdb`, `ip route`, `ip neigh`, `nft list ruleset`, `ss`, `ps` 快照。

| ID | 用例 | 预期 |
|---|---|---|
| NET-001 | 创建后 netns/TAP/veth | `clo-*`、tap0、vn、vh、br0 存在且接口状态 UP |
| NET-002 | guest ↔ host agent TCP | host 到 guest 5201 可握手；Hello 成功 |
| NET-003 | allowlist 单域名 | 允许域名 DNS 和 TCP 成功 |
| NET-004 | 空 allowlist | 非 DNS 出站被拒绝；DNS 代理不放行未授权 IP |
| NET-005 | 非白名单域名 | 解析/连接失败，不能绕过规则 |
| NET-006 | 直接 IP 绕过 DNS | 未允许 IP 的 TCP 连接失败 |
| NET-007 | DNS 代理 | UDP 53 可达；响应、超时、非法域名行为符合设计 |
| NET-008 | CNAME 多跳 | allowlist 域名最终地址处理正确 |
| NET-009 | host 入站默认拒绝 | 除明确 agent 端口外，宿主到 guest 其他端口被拒绝 |
| NET-010 | 沙盒 A→B | A/B guest IP 不能互通；无 FDB/route 泄漏 |
| NET-011 | `network.enabled=false` | guest 无外部网络；确认是否仍创建内部管理网络，并记录实际语义 |
| NET-012 | 多沙盒并发网络 | 10/50 个沙盒各自访问独立目标，无跨沙盒流量 |
| NET-013 | 删除清理 | netns、tap、veth、br0、route、nft、DNS 线程、Firecracker 全部消失 |
| NET-014 | 创建失败清理 | rootfs 不存在、VMM 配置失败、Hello 超时后重复检查无残留 |
| NET-015 | 删除时序 | guest 正在 exec/传输时 DELETE；连接终止，资源最终收回 |
| NET-016 | host veth ARP | host veth 有 `.1/30` 地址，guest ARP 可达，不出现 `INCOMPLETE` |

## 11. VMM：Firecracker 生命周期和故障注入

| ID | 注入/操作 | 预期 |
|---|---|---|
| VMM-001 | create 配置 | 检查 Firecracker 日志和 UDS 请求：machine-config、boot-source、rootfs、vsock、network |
| VMM-002 | InstanceStart | guest 启动，agent 监听 5201，Hello 成功 |
| VMM-003 | GET/exec 生命周期 | VMM metadata 中 pid/socket/vmm_id 可用于后续操作 |
| VMM-004 | force stop | Firecracker 进程组整体退出，子进程无残留 |
| VMM-005 | graceful/force 路径 | 按支持的 StopMode 分别验证；不支持的路径记录明确错误 |
| VMM-006 | rootfs 缺失 | create 失败、HTTP 503/VMM、netns/route/socket 清理 |
| VMM-007 | kernel 缺失/非法 | 明确错误，无 Firecracker orphan |
| VMM-008 | Firecracker 二进制缺失 | 启动前检查失败，无数据库活动对象和网络残留 |
| VMM-009 | guest agent 不启动 | 在 timeout 内失败；VMM、netns、资源 reservation 清理 |
| VMM-010 | 手动 `kill -9` Firecracker | API 后续 exec 失败；对象状态和 reconciler/清理行为符合设计 |
| VMM-011 | 多 VM 并发 | 10 个 VM ID/PID/socket/CID/netns 独立，输出无串扰 |
| VMM-012 | 资源边界 | vCPU、memory、disk 的边界值实际传递到 Firecracker 并可启动 |
| VMM-013 | 进程组 orphan | 删除/超时/kill 后用 `ps`、进程组、socket、netns 多角度确认零残留 |

## 12. STORAGE：SQLite 与 PostgreSQL

### 12.1 SQLite

| ID | 用例 | 预期 |
|---|---|---|
| STORE-SQLITE-001 | 新库启动 | 自动建表，API ready |
| STORE-SQLITE-002 | WAL | `PRAGMA journal_mode` 为 WAL |
| STORE-SQLITE-003 | sandbox CRUD | 创建、读取、更新状态、删除一致 |
| STORE-SQLITE-004 | execution CRUD | 保存、单条查询、列表查询、sandbox 隔离 |
| STORE-SQLITE-005 | 重启恢复 | API 重启后记录存在；资源池恢复 active spec |
| STORE-SQLITE-006 | 并发写入 | 并发 exec/状态写无 database locked 或数据丢失 |
| STORE-SQLITE-007 | 损坏/只读路径 | 启动失败明确；不产生半初始化服务 |

### 12.2 PostgreSQL

| ID | 用例 | 预期 |
|---|---|---|
| STORE-PG-001 | `postgres://` 自动选择 | 日志和实际连接确认 PostgresStore |
| STORE-PG-002 | `postgresql://` 自动选择 | 同上 |
| STORE-PG-003 | schema 初始化 | 新 schema 可启动并读写 |
| STORE-PG-004 | CRUD 与 SQLite 一致性 | 同一 API 场景响应字段一致 |
| STORE-PG-005 | 重启恢复 | apiserver 重启后 sandbox/execution 可查询 |
| STORE-PG-006 | 并发事务 | 并发创建不超售，事务失败可重试/回滚 |
| STORE-PG-007 | 断开数据库 | health/ready 降级，API 返回明确存储错误 |
| STORE-PG-008 | 凭据/数据库不存在 | 启动快速失败，不启动半可用监听 |

## 13. AUTH：开发模式、API key 和租户隔离

| ID | 用例 | 预期 |
|---|---|---|
| AUTH-001 | 无注册 key + 无 Authorization | 当前 dev 模式允许；日志/文档行为明确 |
| AUTH-002 | 注册 full key | Bearer key 可访问读写接口 |
| AUTH-003 | 注册 read key | GET/list/health 可访问；create/delete/exec/upload 被拒绝 |
| AUTH-004 | 无 header | 注册 key 模式返回 401 `UNAUTHENTICATED` |
| AUTH-005 | `Basic` scheme | 401 |
| AUTH-006 | 空 Bearer/错误 key | 401 |
| AUTH-007 | 多余空格、大小写 scheme | 按严格 header 规则验证 |
| AUTH-008 | tenant A/B | 跨租户 get/delete/exec 不泄露对象；若当前 handler 未做 tenant 过滤，标阻塞缺口 |
| AUTH-009 | `/health`、`/metrics` 免认证 | 注册 key 模式仍可访问 |
| AUTH-010 | key 注册入口 | 查当前 API/配置是否存在；没有则标 `NOT_EXPOSED`，不能伪造成功 |

## 14. CLOUISLED-GRPC：节点服务

使用生成的 `node.proto` 客户端或 grpcurl（若服务器安装）验证：

| ID | RPC/字段 | 预期 |
|---|---|---|
| GRPC-001 | Register 空 `node_id` | 返回 `node_id=auto` |
| GRPC-002 | Register 自定义 node_id | 原值返回；hostname、vcpu、memory、disk、KVM、kernel、Firecracker、labels 可接收 |
| GRPC-003 | Register 边界字段 | 0/极大资源、空字符串、多个 labels 不 panic |
| GRPC-004 | Heartbeat 首条 report | 返回空 command stream，连接正常关闭/保持语义明确 |
| GRPC-005 | Heartbeat report 全字段 | allocated_vcpu/memory、running_sandboxes、pool_ready、load_avg 保留 |
| GRPC-006 | Heartbeat 空流 | 明确返回空命令或错误，不挂起 |
| GRPC-007 | CreateSandbox 合法 `spec_json` | 返回 SandboxHandle，字段与本地 sandbox 一致 |
| GRPC-008 | CreateSandbox 非法 JSON | gRPC `INVALID_ARGUMENT` |
| GRPC-009 | CreateSandbox 非法 spec | validation status 映射正确；无资源/netns残留 |
| GRPC-010 | DeleteSandbox 存在 | `ok=true`，进程/数据库/网络清理 |
| GRPC-011 | DeleteSandbox 不存在 | `ok=false` + error，不把业务错误伪装为 transport success |
| GRPC-012 | Exec 单请求流 | stdout/stderr/exit 事件顺序正确 |
| GRPC-013 | Exec 空流/无 ExecRequest | `INVALID_ARGUMENT` |
| GRPC-014 | Exec cwd/env/timeout | 每个字段透传且行为正确 |
| GRPC-015 | Exec 不存在 sandbox | error stream/status 明确 |
| GRPC-016 | gRPC 端口/非法地址/重复启动 | 启动失败明确；服务可被健康探测 |

## 15. SDK：远程服务器逐方法验收

所有 SDK 测试使用同一远程 API 和独立 sandbox，必须同时保存 SDK 级异常/返回值和 HTTP 直测对照结果。

### 15.1 Rust SDK `sdk/rust`

- `Client::new`：完整 URL、无 scheme 主机、API key、空 key。
- `create_sandbox`、`get_sandbox`、`list_sandboxes(status, limit, offset)`、`delete_sandbox`。
- `exec`、`exec_cmd`：argv/env/cwd/timeout、成功/错误/超时。
- `get_execution`、`list_executions(limit)`：记录字段和 limit 行为。
- `upload_file`、`download_file`、`list_files`：文本/二进制/边界路径。
- `health`、`liveness`、`readiness`、`metrics`：返回类型和 Content-Type。
- `SdkError`：HTTP、API、序列化和网络错误分支。

### 15.2 Python SDK `sdk/python`

- `Client` URL、API key、默认 30 秒 HTTP timeout。
- `create_sandbox`、`get_sandbox`、`list_sandboxes(status, limit, offset)`、`delete_sandbox`。
- `exec`、`exec_cmd`；`ExecRequest` 全字段。
- `upload_file`、`download_file`。
- `health` 和错误 `SandboxError(status_code, code, message, details)`。
- 对照当前公开接口，`list_files`、执行历史、liveness、readiness、metrics 等未公开方法必须标 `SDK_GAP`，不得省略。

### 15.3 TypeScript/JavaScript SDK `sdk/typescript`

- `Client` baseUrl 尾斜杠、API key、axios timeout。
- `createSandbox`、`getSandbox`、`listSandboxes({status,limit,offset})`、`deleteSandbox`。
- `exec`、`execCmd`；ExecRequest 所有字段。
- `uploadFile(Buffer/Uint8Array)`、`downloadFile(ArrayBuffer)`、`listFiles`。
- `health`、`liveness`、`readiness`、`metrics`。
- `SandboxError` 解析 HTTP JSON 错误、纯文本错误、网络错误。
- `tsc` 类型检查和构建产物中 JavaScript 运行一次，验证类型声明与运行时路径一致。

### 15.4 SDK 交叉一致性

同一 sandbox 依次由 HTTP、Rust、Python、TypeScript 调用：

1. 创建后四种客户端读取到相同 id/status/spec。
2. 一种客户端上传，另一种客户端下载并校验 SHA-256。
3. 一种客户端 exec，其他客户端读取 execution record。
4. 任意客户端删除后，其他客户端统一收到 404/SDK 错误。
5. 空 key、错误 key、read-only key 的错误 code 和 HTTP status 一致。

## 16. CLI：`clouislectl` 参数和命令

| ID | 命令/参数 | 预期 |
|---|---|---|
| CLI-001 | `health` 默认 API | 请求默认 `http://127.0.0.1:8080/health` |
| CLI-002 | 每个命令 `--api` | 指定远程 `http://host:port` 生效 |
| CLI-003 | `create --image` | image 必需；默认 vcpu=1、memory=256；响应为 API 结果 |
| CLI-004 | `create --vcpu` | 1/4 成功，0/5 由 API 拒绝 |
| CLI-005 | `create --memory-mb` | 合法/边界值透传；CLI 不吞错误 |
| CLI-006 | `list` | 空列表、非空列表、`--status` 过滤 |
| CLI-007 | `delete <id>` | 204 输出；不存在 ID 输出非成功状态 |
| CLI-008 | `exec <id> command...` | 多个 argv 保持顺序；成功、非零、错误输出 |
| CLI-009 | 缺少子命令/必需参数 | clap 非零退出、usage 输出 |
| CLI-010 | API 不可达/返回 4xx/5xx | 进程非零或明确输出，不 panic |

## 17. 并发、稳定性和容量

这些用例在单功能验证全部通过后执行，避免把基础错误放大成容量噪声：

| ID | 场景 | 观测 |
|---|---|---|
| LOAD-001 | 10 个并发创建 | 成功数、失败原因、创建延迟、资源总和 |
| LOAD-002 | 50 个并发创建（按服务器容量调整） | 不超售；无重复 id；无跨沙盒网络 |
| LOAD-003 | 并发 exec/文件传输 | 无数据串扰、无死锁、无数据库锁错误 |
| LOAD-004 | 创建/删除循环 30 分钟 | FD、内存、netns、route、Firecracker 数量回到基线 |
| LOAD-005 | API 重启 | SQLite/Postgres 记录、健康、资源池恢复一致 |
| LOAD-006 | 容器重启 | 明确记录运行中 VM 是否保留；若设计要求保留则 exec 必须恢复 |
| LOAD-007 | SIGTERM/SIGINT | API 优雅退出；进行中的请求和网络清理符合文档 |
| LOAD-008 | 磁盘空间/数据库不可写 | 新请求明确失败，既有对象不被破坏 |

## 18. 现有功能缺口检查表

下列项目在当前路由或公开 SDK 中可能没有实现。执行时先用路由、CLI `--help`、SDK public symbol 和源码证据确认：

- 快照 create/restore/list/delete。
- 镜像拉取进度 SSE 与 OCI 镜像构建链路。
- 资源热更新端点。
- 审计日志查询、哈希链验证和签名验证端点/CLI。
- API key 注册、撤销、过期和租户配额管理入口。
- Python SDK 的 list files、execution history、liveness、readiness、metrics。
- clouisled 真正的 `/proc` Firecracker 扫描与 orphan kill；当前 reconciler 的 live 集合仍是可测试模拟。
- mounts、secrets、bandwidth、iops、pids_max、ttl、restart_policy 的真实数据面生效程度。

每一项必须有三选一结论：

1. `PASS`：服务器实测可用；
2. `FAIL`：有可重现缺陷；
3. `NOT_EXPOSED` / `NOT_IMPLEMENTED`：当前没有可调用入口或实现，附证据并进入后续开发任务。

## 19. 测试报告模板

```markdown
# Server Acceptance Test Report

- Commit:
- Server:
- Image:
- Kernel:
- Firecracker:
- Store:
- Start time:
- End time:

## Summary
- PASS:
- FAIL:
- BLOCKED:
- NOT_EXPOSED:
- Cleanup status:

## Failed cases
| ID | Reproduction | Expected | Actual | Evidence | Issue |
|---|---|---|---|---|---|

## Parameter coverage
| Object | Field | Values tested | Result |
|---|---|---|---|

## Resource and isolation evidence
- Firecracker processes:
- netns:
- TAP/veth:
- nftables:
- routes:
- cgroups:
- database rows:

## SDK coverage
| SDK | Methods passed | Gaps | Result |
|---|---|---|---|

## Final verdict
`PASS` only when no required case is FAIL/BLOCKED and all NOT_EXPOSED items are explicitly accepted as product gaps.
```

## 20. 交付物

执行完成后必须保留：

1. 本计划文档；
2. 服务器测试报告；
3. 原始 HTTP/gRPC/SDK 输出；
4. API、VMM、网络和数据库日志；
5. cleanup 前后宿主状态快照；
6. 失败用例的最小复现命令；
7. 缺口清单和对应修复/放弃决定。
