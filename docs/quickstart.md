# 快速上手

5 分钟跑通 Clouisle：创建沙盒 → 执行命令 → 文件传输 → 删除。两种运行模式任选。

## 选择模式

| 你的环境 | 模式 | 隔离 | 命令 |
|---|---|---|---|
| Linux + KVM（生产语义） | firecracker | 独立微VM | `docker compose up -d` |
| macOS / Windows（Docker Desktop） | docker-dev | Docker 容器 | `docker compose -f docker-compose.dev.yml up --build` |

> 生产（firecracker）需要 `/dev/kvm`。没有 KVM 就用 docker-dev 开发模式。

## 模式 A：Linux + KVM（推荐）

### 1. 启动
```bash
git clone <repo> && cd clouisle-sandbox
docker compose up -d            # API + PostgreSQL + Firecracker
curl localhost:8080/health
# → {"status":"ok","store":"ok","version":"0.1.0"}
```

### 2. 获取开发 key
Compose 内置开发 key（生产务必替换）：
```bash
export KEY="local-development-key"   # 见 docker-compose.yml 的 CLOUISLE_API_KEYS
```

### 3. 创建沙盒（真实微VM，alpine）
```bash
curl -X POST localhost:8080/api/v1/sandboxes \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"image":{"reference":"docker.io/library/alpine:latest"}}'
# → 201 {"id":"019f...","status":"running",...}
export SBX=<上面的 id>
```

### 4. 执行命令
```bash
curl -X POST localhost:8080/api/v1/sandboxes/$SBX/exec \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"argv":["sh","-c","echo hello from microVM; cat /etc/os-release | head -1"],"timeout_ms":10000}'
# → {"exit_code":0,"stdout":"hello from microVM\nNAME=\"Alpine Linux\"\n",...}
```

### 5. 文件传输
```bash
curl -X POST "localhost:8080/api/v1/sandboxes/$SBX/files/upload?path=/work/hello.txt" \
  -H "Authorization: Bearer $KEY" --data-binary "Hello from host"
curl "localhost:8080/api/v1/sandboxes/$SBX/files/download?path=/work/hello.txt" \
  -H "Authorization: Bearer $KEY"
```

### 6. 删除
```bash
curl -X DELETE localhost:8080/api/v1/sandboxes/$SBX -H "Authorization: Bearer $KEY"   # 204
curl localhost:8080/api/v1/sandboxes/$SBX -H "Authorization: Bearer $KEY"             # 404
```

## 模式 B：Docker 开发（macOS / Windows）

```bash
docker compose -f docker-compose.dev.yml up --build
export KEY="e2b_dev_00000000000000000000000000000000000000"
export API="http://localhost:18080"
```
同样的 curl 流程（把 `localhost:8080` 换成 `$API`）。沙盒是 Docker 容器（注入 agent），
API 语义一致，但**不承诺生产隔离**（无快照/iops/带宽/allowlist）。

## 用 CLI / SDK

```bash
# CLI（容器内）
docker compose exec apiserver clouislectl --api http://localhost:8080 --key $KEY \
  create --image docker.io/library/alpine:latest

# Python SDK（官方 E2B SDK 或本项目 SDK）
pip install e2b   # 或使用 sdk/python
from e2b import Sandbox
sb = Sandbox(template="docker.io/library/alpine:latest", api_key="$KEY")
print(sb.commands.run("echo hi").stdout)
sb.kill()

# TypeScript SDK
npm install    # sdk/typescript
```

## 下一步

- 每个功能怎么设计 → [features.md](features.md)
- 全部配置项 → [configuration.md](configuration.md)
- API 参考 → [api.md](api.md) / [spec/openapi.json](../spec/openapi.json)
- 生产部署（HA / 多节点）→ [deployment.md](deployment.md)
