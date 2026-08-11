# Clouisle 文档

Clouisle Sandbox 是一个基于 Firecracker/KVM 的沙盒即服务控制平面：每个沙盒是一台独立微虚拟机（自有内核 + rootfs），宿主经帧协议与 guest agent 通信，提供命令执行、文件传输、网络隔离、快照与多节点调度。

## 快速开始

| 场景 | 入口 |
|---|---|
| 5 分钟跑起来（推荐） | [quickstart.md](quickstart.md) |
| 生产 KVM 部署 | [deployment.md](deployment.md)（生产 / 开发 / 多节点） |
| 每个功能怎么设计 | [features.md](features.md) |
| 所有配置项 | [configuration.md](configuration.md) |
| API 参考 | [api.md](api.md) + [spec/openapi.json](../spec/openapi.json) |
| 本地开发 / 构建 / 测试 | [development.md](development.md) |
| 系统架构总览 | [architecture.md](architecture.md) |

## 文档地图

```
README.md                 快速上手（顶层）
docs/
  quickstart.md           5 分钟快速体验：两种模式、创建/执行/删除
  architecture.md         组件、数据流、后端矩阵、状态机
  features.md             每个功能的 设计 / 配置 / 验证
  configuration.md        全部配置项（CLI / env / Compose / 后端差异）
  api.md                  API 端点指南（v1 + E2B + 控制面）
  deployment.md           生产（KVM）/ 开发（Docker）/ 多节点（gRPC）部署
  development.md          构建、测试、SDK 接入
  plan/                   详细设计记录（快照预热、E2B、验收、安全等）
spec/openapi.json         OpenAPI 3.0 规范（95 operations，脚本生成）
```
