# Cross-platform Docker Development Backend Design Document

> **Status: planning complete; implementation has not started.**
>
> This document deliberately adds no production fallback. `FirecrackerVmm` remains the only production execution backend.

## Background & Goals

The production runtime is intentionally Linux + `/dev/kvm` + Firecracker. That boundary is correct for microVM isolation, Firecracker snapshots, the per-sandbox TAP/veth topology, and host-veth nftables egress enforcement. It also means a Mac or Windows workstation cannot reproduce the data plane locally.

The developer experience must nevertheless support macOS and Windows without running Clouisle binaries directly on those hosts. Docker Desktop and WSL2 provide a Linux container runtime, so the development control plane and its disposable sandboxes can execute entirely as containers. This preserves API/SDK/CLI/file-transfer development velocity while leaving production semantics honest.

### Goals

1. Give macOS and Windows developers a Docker-only local workflow for sandbox CRUD, exec, streamed exec, files, secret materialization, TTL, auth, audit, and SDK integration.
2. Keep `FirecrackerVmm` the only backend allowed for production and KVM acceptance.
3. Make development degradation explicit: a Docker container is never reported as a Firecracker microVM.
4. Reuse the framed guest-agent protocol for exec and file APIs so both backends exercise the same application-facing transport contract.
5. Keep Docker socket authority confined to an opt-in local development Compose topology; production Compose and Kubernetes must never mount it.
6. Preserve existing API request and response contracts unless a new explicit capability endpoint is required.

### Non-goals

- Emulating KVM, Firecracker, vsock, Firecracker snapshots, host-veth nftables, or domain-level egress enforcement on Docker Desktop.
- Making `docker-dev` an automatic fallback when Firecracker or `/dev/kvm` is unavailable.
- Supporting Windows containers; the development path uses **Linux containers** through Docker Desktop or WSL2.
- Making Docker socket access safe for untrusted production workloads. It is a local developer convenience with host-equivalent authority.
- Changing the existing Linux/KVM Docker production deployment into a Docker-in-Docker design.

## Current-State Evidence

| Area | Current implementation | Consequence for this plan |
|---|---|---|
| Backend abstraction | `crates/clouisle-vmm/src/lib.rs` defines the `Vmm` trait and `VmmCapabilities`; only `FirecrackerVmm` is exported on Linux. | Add a distinct `DockerDevVmm` implementation; do not weaken the Firecracker path. |
| Startup selection | `crates/clouisle-api/src/main.rs` chooses local Firecracker on Linux and errors on non-Linux without a remote node endpoint. | Add explicit backend selection inside the Linux API container. |
| Lifecycle orchestration | `handlers/sandbox.rs` creates the network, VMM, then waits for `AgentConnector::connect_and_hello`. | DockerDevVmm must provide a real container and DockerDevAgentConnector must satisfy the same hello gate. |
| Data-plane API | `AgentConnection` already owns exec, streaming, files, and ping. | Reuse its framed TCP protocol instead of implementing a divergent Docker-exec/file API. |
| Guest agent | `clouisle-agent` is a static PID 1 and currently configures Firecracker guest networking before binding TCP 5201. | Add an explicit development serve mode that skips Firecracker network configuration but retains the protocol. |
| Production container contract | The current Dockerfile packages the API, node daemon, Firecracker, and static guest agent; production Compose uses KVM and host networking. | Add a separate standalone development Compose file. Do not overlay or mutate production Compose into a Docker Desktop configuration. |
| Historical docs | `docs/plan/00-architecture-decisions.md` still describes removed Mock/Docker production backends. | Supersede the obsolete portions with a new ADR during implementation; do not silently revive the old design. |

## Architecture Decisions

### AD-DEV-01: DockerDevVmm is explicit and development-only

Add a `--backend` CLI value enum:

```text
firecracker   default; only supported production backend
docker-dev    local development backend; must run inside the development API container
```

`--backend docker-dev` conflicts with `--node-endpoint` and `--cluster-scheduling`. The default remains `firecracker`; absence of KVM remains a startup error rather than silently switching semantics.

The implementation type is named `DockerDevVmm`, never `DockerVmm`, to make its restricted role apparent in logs, metadata, code review, and dashboards. Every dev-created `VmHandle` and persisted `VmmMeta` uses `backend: "docker-dev"`.

### AD-DEV-02: The Docker Engine API is accessed only from the API development container

`DockerDevVmm` uses an audited async Docker Engine API client (Bollard, pinned at implementation time) over `/var/run/docker.sock`. The socket is mounted only by `docker-compose.dev.yml`.

A small `DockerEngine` trait wraps the Engine operations needed by the VMM. The trait makes unit tests deterministic and constrains the production of Docker calls to: image pull, network lookup/create, container create, archive upload, start, inspect, pause, unpause, stop/kill, remove, and one-shot statistics.

Production `docker-compose.yml`, Kubernetes manifests, and released KVM deployments retain no Docker socket mount. Docker socket access is documented as host-equivalent privilege.

### AD-DEV-03: Reuse the guest-agent protocol through a private Docker management network

Do not proxy exec and file operations through Docker `exec` / archive APIs. That would duplicate timeout, streaming, file, secret, and protocol behavior.

Instead, inject the already static `clouisle-agent` into each Docker development sandbox before it starts:

1. DockerDevVmm creates the application-image container with entrypoint `/clouisle-dev-agent serve --skip-network-config`.
2. Before start, it uploads the image-matching static agent from the API container into `/clouisle-dev-agent` using Docker Engine `put_archive`.
3. The agent runs as container PID 1, listens on TCP 5201, and speaks the existing framed protocol.
4. DockerDevAgentConnector connects to `<deterministic-container-name>:5201` across the internal management bridge and reuses the existing framed `AgentConnection` behavior.

`--skip-network-config` is an explicit agent argument, valid only for this container-development lifecycle. The normal no-argument and `serve` paths retain fail-fast Firecracker networking setup.

### AD-DEV-04: Separate management and application networking

DockerDevVmm always connects sandbox containers and the API development container to a named internal bridge, `clouisle-dev-mgmt`. This bridge carries only agent TCP/5201 traffic; it has no published sandbox ports and no external route.

| Requested network configuration | DockerDevVmm behavior |
|---|---|
| `network.enabled: false` | Attach only `clouisle-dev-mgmt`; the application has no egress route. The management agent remains reachable. |
| `network.enabled: true`, empty `allow_egress` | Attach `clouisle-dev-mgmt` plus `clouisle-dev-egress`, a normal Docker bridge. Application egress is open for local development. |
| `network.enabled: true`, nonempty `allow_egress` | Reject at create time with a validation error. DockerDevVmm cannot truthfully enforce the production domain allowlist. |

The production Firecracker path continues to own TAP, netns, DNS proxy, and nftables lifecycle. DockerDevVmm sets `AppState.manage_network = false` so it never invokes the Linux firewall manager.

### AD-DEV-05: Resource and mount behavior is precise about what is and is not enforced

| `SandboxSpec` field | DockerDevVmm behavior |
|---|---|
| `resources.vcpu` | Map to Docker `NanoCpus` (`vcpu × 1_000_000_000`). |
| `resources.memory_mb` | Map to Docker memory bytes. |
| `resources.pids_max` | Map to Docker `PidsLimit`; preserve the current default when present. |
| `resources.disk_mb` | Not enforced portably by Docker storage drivers; record this limitation in `VmmMeta.extra`, never claim a quota. |
| `resources.iops` / `bandwidth_mbps` | Reject non-`None` values with a validation error in docker-dev mode. |
| `mounts` | Allow only canonical sources beneath repeatable `--docker-dev-mount-root` configuration. Reject symlink escapes, missing sources, relative targets, and mount targets outside the container root. Honor read-only mapping. |
| `secrets` | Keep secret values out of Docker labels, environment, logs, and metadata. Materialize only through the authenticated agent after hello, matching Firecracker behavior. |
| `restart_policy` | Support `never` only in the first release; reject `on_failure` and `always` until persistent Docker restart reconciliation is designed. |

Every development container uses labels `com.clouisle.managed=true`, `com.clouisle.backend=docker-dev`, and the sandbox ID/tenant ID. Labels must never include image credentials or secret values.

### AD-DEV-06: Docker lifecycle semantics

- **Create**: validate the dev-specific constraints, pull the exact image reference/digest when absent, create the management network if necessary, create a deterministically named container, upload the static agent, and return a `VmHandle` whose ID is the container name/ID.
- **Start**: start the container. The existing handler then waits for framed agent hello; only hello changes the sandbox to `Running`.
- **Pause/resume**: map to Docker pause/unpause. These operations retain their lifecycle contract but are not snapshots.
- **Snapshot/restore**: return an explicit unsupported `ClouisleError` with `ErrorKind::Vmm`; DockerDevVmm declares `snapshot: false`, `vsock: false`, and `balloon: false`.
- **Stats**: map Docker one-shot stats to the currently representable `VmStats` fields. Unknown values remain `None`.
- **Stop/delete**: graceful stop sends Docker stop; force sends Docker kill. Both paths remove the container and tolerate already-removed resources. Failure compensation after create/start/agent-hello errors removes the container and detaches only project-owned networks.
- **Restart reconciliation**: on API startup, reconcile only containers bearing both Clouisle labels. Preserve containers that correspond to active store records; remove stale labelled containers with no durable sandbox record; never touch unlabelled containers.

## Requirement Inventory and Traceability

| ID | Requirement | Primary plan stage | Observable acceptance evidence |
|---|---|---|---|
| XDEV-01 | All Mac/Windows developer runtime processes run as Linux Docker containers. | 5 | Docker Desktop/WSL2 Compose starts API; no host Clouisle process is required. |
| XDEV-02 | Production remains explicit Firecracker/KVM with no automatic fallback. | 1 | Default backend remains Firecracker; DockerDevVmm requires `--backend docker-dev`; production manifests contain no Docker socket. |
| XDEV-03 | Docker development sandboxes are visibly distinct from microVMs. | 1 | API sandbox response persists `vmm_meta.backend = docker-dev`; logs/metrics include backend. |
| XDEV-04 | Docker-dev supports create, delete, pause, resume, exec, streaming exec, files, secrets, TTL, auth, audit, and SDK paths. | 3 | Docker-only integration suite passes each route through an injected agent. |
| XDEV-05 | Guest-agent behavior is shared between Firecracker and Docker development modes. | 2 | Agent protocol fixtures plus Docker-dev E2E use Hello, exec, file, and ping frames. |
| XDEV-06 | Offline development sandboxes remain agent-reachable without application egress. | 3 | Offline sandbox executes via management network and cannot reach public DNS/IP. |
| XDEV-07 | Docker-dev never pretends to enforce production egress allowlists. | 3 | Nonempty `allow_egress` is rejected; enabled/empty mode is documented as open local egress. |
| XDEV-08 | CPU, memory, and PID limits map to Docker; unsupported limits are rejected or surfaced accurately. | 3 | Docker inspect shows limits; IOPS/bandwidth reject; metadata reports disk limitation. |
| XDEV-09 | Docker socket and host mounts have an explicit development-only security boundary. | 4 | Dev Compose alone mounts the socket; production Compose/Kubernetes scans show no socket; mount-root escape cases fail. |
| XDEV-10 | Create/start failures, delete, TTL expiry, and restart reconciliation leave no labelled containers or networks behind. | 4 | Fault-injection and restart tests assert exact labels/network cleanup. |
| XDEV-11 | Existing Firecracker KVM behavior regresses nowhere. | 6 | Linux KVM acceptance creates the Python-and-Node image, executes both runtimes, and verifies cleanup. |
| XDEV-12 | CI and documentation keep portable developer and KVM production assertions separate. | 6 | Linux Docker dev suite, KVM acceptance, docs command checks, and platform matrix are green. |
| XDEV-13 | The obsolete historical DockerVmm/MockVmm ADR is superseded rather than silently contradicted. | 7 | New ADR records DockerDevVmm scope; old claims are marked superseded. |

Every requirement above has exactly one primary implementation stage in this document.

## High-Level Design

```mermaid
flowchart LR
    subgraph Desktop[macOS / Windows host]
        Docker[Docker Desktop or WSL2 Linux engine]
    end
    subgraph DevCompose[Docker-only development topology]
        API[clouisle-api\n--backend docker-dev]
        MGT[clouisle-dev-mgmt\ninternal bridge]
        EGRESS[clouisle-dev-egress\noptional local egress]
        SBX[OCI application container\ninjected clouisle agent]
        API --- MGT --- SBX
        SBX -. network.enabled true only .-> EGRESS
    end
    Docker --> DevCompose
    subgraph Production[Linux KVM production topology]
        FCAPI[clouisle-api\n--backend firecracker]
        FC[Firecracker microVM]
        FW[netns / TAP / nftables]
        FCAPI --> FW --> FC
    end
```

The API keeps one `Vmm` and one `AgentConnector` in `AppState`:

| Backend | `Vmm` | `AgentConnector` | `manage_network` |
|---|---|---|---|
| `firecracker` | existing `FirecrackerVmm` | existing TCP-over-TAP `VsockAgentConnector` | `true` for local single-node mode |
| `docker-dev` | planned `DockerDevVmm` | planned `DockerDevAgentConnector`, TCP to management DNS name | `false` |
| remote node / cluster | existing gRPC adapters | existing gRPC adapter | `false` |

The handlers do not learn Docker Engine details. They keep their current state transitions and always gate `Running` on `connect_and_hello`.

## Planned Module and Contract Changes

### `crates/clouisle-vmm`

| Planned file | Change |
|---|---|
| `Cargo.toml` | Add the audited Docker Engine client dependency and only the features needed for Unix-socket Linux containers. |
| `src/lib.rs` | Export `DockerDevVmm` only for Linux container builds; preserve the existing Vmm trait and Firecracker export. Define a backend-neutral unsupported-operation helper if needed. |
| `src/docker_dev.rs` (new) | Implement VMM lifecycle, label ownership, image pull, static-agent archive injection, resource mapping, Docker network attachment, stats mapping, compensation, and idempotent cleanup. |
| `src/docker_engine.rs` (new) | Define the narrow async Docker Engine adapter and the production Bollard implementation; expose a fake implementation only to tests. |
| `src/error.rs` | Replace Firecracker-specific display messages with backend-neutral VMM errors where DockerDevVmm needs the same error path. |
| `src/docker_dev_tests.rs` or module tests | Test engine call ordering, request mapping, error classification, and all compensation branches with the fake engine. |

### `crates/clouisle-agent`

| Planned file | Change |
|---|---|
| `src/main.rs` | Parse `serve --skip-network-config`; reject unknown combinations. Preserve no-argument PID 1 and plain `serve` behavior. |
| `src/serve.rs` | Accept an explicit serve configuration. Only the Docker-dev mode skips static-IP configuration; both modes bind TCP 5201 and use identical frames. |
| `src/init.rs` | Leave Firecracker network setup unchanged; expose no Docker-specific network behavior. |
| tests | Cover argument parsing and assert only the dev flag bypasses configure-network. |

### `crates/clouisle-api`

| Planned file | Change |
|---|---|
| `src/main.rs` | Add `BackendKind` CLI parsing/config validation. Construct matching VMM/agent pairs. Reject docker-dev combined with remote/cluster modes, and enforce the Docker socket/agent path only in docker-dev mode. |
| `src/agent.rs` | Extract the existing framed TCP connection into a reusable connection type. Add DockerDevAgentConnector that resolves the deterministic management hostname and performs the existing hello retry. |
| `src/state.rs` | Make local-network ownership backend-aware rather than derived solely from remote scheduling flags. |
| `src/handlers/sandbox.rs` | Add backend-specific preflight validation before resource reservation: allowlist, unsupported resource values, restart policy, and approved mount sources. Keep core spec validation backend-neutral. |
| API integration tests | Use a fake Docker engine + loopback framed agent for handler contract tests; do not replace TestVmm fixture coverage. |

### Deployment, CI, and documentation

| Planned file | Change |
|---|---|
| `docker-compose.dev.yml` (new) | Standalone Docker Desktop/WSL2 topology: API build, published HTTP port, development API key, Docker socket, named data volume, internal management network, optional egress network. No KVM, privileged mode, host network, or host rootfs mounts. |
| `Dockerfile` | Ensure the runtime image contains the architecture-matching static agent that DockerDevVmm uploads. Do not add a Docker CLI; use Engine API. |
| `docker-compose.yml` | Keep KVM production semantics. Add assertions/comments only if necessary; never add a Docker socket. |
| `.github/workflows/ci.yml` | Add Linux Docker development-backend integration coverage, separate from KVM acceptance. Keep build-only macOS CI. |
| `docs/plan/00-architecture-decisions.md` | Add an ADR superseding obsolete DockerVmm/MockVmm claims with the explicit DockerDevVmm boundary. |
| `README.md`, `README.zh-CN.md` | Document development Compose separately from production Compose; include backend limitations and remote KVM workflow. |
| SDK docs/examples | Add `backend=docker-dev` response examples and state that SDK contracts are shared while isolation guarantees differ. |

## Implementation Plan

### Stage 1: Establish backend identity and compile-time seams

- **Requirements**: XDEV-02, XDEV-03, XDEV-13.
- **Files modified**: `crates/clouisle-vmm/{Cargo.toml,src/lib.rs,src/error.rs}`, `crates/clouisle-api/src/main.rs`, `crates/clouisle-api/src/state.rs`, ADR document, focused tests.
- **Specific logic**:
  - Define `BackendKind` with `firecracker` and `docker-dev`; default to Firecracker.
  - Validate mutually exclusive backend, node endpoint, and cluster flags before opening the store or provisioning resources.
  - Make `manage_resources` and `manage_network` derive from selected backend/ownership, not target OS alone.
  - Define `DockerEngine` as the only boundary around the Docker client.
  - Mark the old ADR description superseded; keep prior historical context readable.
- **Validation**:
  - CLI parser tests reject invalid backend combinations.
  - Linux build proves DockerDevVmm is included in the container runtime.
  - macOS build proves Linux-gated implementation details do not leak into portable compilation.
  - Default start without KVM still fails instead of selecting docker-dev.

### Stage 2: Build the injected development-agent path

- **Requirements**: XDEV-05.
- **Files modified**: `crates/clouisle-agent/src/{main.rs,serve.rs}`, `crates/clouisle-vmm/src/docker_dev.rs`, `Dockerfile`, agent tests.
- **Specific logic**:
  - Add the explicit skip-network-config argument and a typed serve configuration.
  - Have DockerDevVmm archive-upload the static agent before container start and use it as PID 1.
  - Reuse TCP port 5201 and existing Hello/exec/file frame semantics.
  - Do not suppress network configuration failures in the normal Firecracker agent mode.
- **Validation**:
  - Unit-test argument parsing and mode selection.
  - Fake-engine test asserts `create → put_archive → start` ordering.
  - Docker integration test observes hello, ping, sync exec, streaming exec, and a timeout through the real injected agent.

### Stage 3: Implement DockerDevVmm lifecycle and developer network semantics

- **Requirements**: XDEV-04, XDEV-06, XDEV-07, XDEV-08.
- **Files modified**: `crates/clouisle-vmm/src/{docker_dev.rs,docker_engine.rs}`, `crates/clouisle-api/src/{agent.rs,state.rs,handlers/sandbox.rs}`, tests.
- **Specific logic**:
  - Pull exact image references/digests and create deterministic, labelled containers.
  - Map CPU, memory, PIDs, and supported mounts; record non-portable disk limits in metadata.
  - Create/attach internal management and optional egress networks as specified above.
  - Add DockerDevAgentConnector and share the framed TCP connection code with the Firecracker connector.
  - Return explicit unsupported errors for snapshot/restore and unsupported policy/resource requests.
- **Validation**:
  - Fake-engine tests cover image pull failure, archive upload failure, start failure, stop failure, pause/resume, stats, and idempotent not-found cleanup.
  - Integration tests validate API state progression `Pending → Starting → Running`, metadata backend value, each resource mapping, and every unsupported request.
  - Offline sandbox executes through the agent but cannot reach the egress network; open-development mode can resolve and connect; nonempty domain allowlists return 4xx validation errors.

### Stage 4: Secure local authority, mounts, and reconciliation

- **Requirements**: XDEV-09, XDEV-10.
- **Files modified**: DockerDev engine/VMM modules, API startup/reaper logic, mount validation helpers, Compose dev file, focused tests.
- **Specific logic**:
  - Require the Docker socket only when docker-dev is selected and return a clear startup error when it is absent/unusable.
  - Run no development sandbox privileged, with host PID, host IPC, host networking, or automatically inherited host paths.
  - Canonicalize source paths; enforce configured allowed mount roots; reject symlink escapes before Docker receives a bind request.
  - Reconcile only strongly labelled Clouisle development containers and project-owned networks after API restart.
  - Make cleanup compensation exact and idempotent for API failure, TTL reaping, forced delete, and restart reconciliation.
- **Validation**:
  - Verify production Compose/Kubernetes contain no Docker socket string or mount.
  - Negative tests cover absent socket, denied socket, path traversal, source outside approved roots, symlink escape, leaked label cleanup, and protection of unlabelled containers.
  - Crash/restart test retains a durable active sandbox and removes an orphaned labelled sandbox.

### Stage 5: Ship a standalone Docker Desktop / WSL2 development topology

- **Requirements**: XDEV-01, XDEV-04, XDEV-09.
- **Files modified**: `docker-compose.dev.yml`, `.env.dev.example` if secrets require configuration, READMEs, SDK examples.
- **Specific logic**:
  - Provide one documented `docker compose -f docker-compose.dev.yml up --build` command.
  - Attach only the API container to `clouisle-dev-mgmt`; DockerDevVmm attaches sandbox containers programmatically.
  - Publish API HTTP only; do not expose guest agent ports.
  - Use a multi-architecture Python-and-Node fixture image built from `python:3.13-alpine` plus the matching distro Node package so arm64 Docker Desktop testing does not depend on a third-party multi-arch manifest.
- **Validation**:
  - `docker compose -f docker-compose.dev.yml config --quiet` succeeds on macOS Docker Desktop, Windows WSL2 Docker Desktop, and Linux Docker Engine.
  - No Clouisle binary needs to be installed or invoked on the host.
  - CLI, REST, and all four SDKs create the fixture, run both `python3` and `node`, transfer a file, and delete the sandbox.

### Stage 6: Layered automated acceptance and production regression

- **Requirements**: XDEV-04, XDEV-10, XDEV-11, XDEV-12.
- **Files modified**: API/VMM/agent tests, Docker integration harness, CI workflow, server acceptance script/report.
- **Specific logic**:
  - Keep deterministic Rust unit tests independent of a daemon using the DockerEngine fake.
  - Add a Linux Docker Engine integration job for docker-dev. It must start the API through Compose, not with host `cargo run`.
  - Keep macOS CI as a build/portable-check job; Docker Desktop smoke testing belongs to documented developer acceptance or managed platform runners where a daemon is available.
  - Keep Linux KVM acceptance isolated on the KVM server, with the existing container-only Firecracker test.
- **Validation**:
  - Test matrix includes create/error compensation, auth/tenant isolation, secrets redaction, upload/download/list, sync/SSE exec, timeout, TTL, offline/open/invalid network configs, resource limits, restart reconciliation, and cleanup.
  - KVM regression creates `docker.io/nikolaik/python-nodejs:python3.13-nodejs22`, obtains Python and Node versions in the same Firecracker guest, and confirms `HTTP 204` deletion plus zero residual network/Firecracker state.

### Stage 7: Documentation, observability, rollout, and rollback

- **Requirements**: XDEV-03, XDEV-12, XDEV-13.
- **Files modified**: READMEs, ADR, changelog, deployment documentation, metrics definitions if needed.
- **Specific logic**:
  - Add backend labels to sandbox lifecycle and exec metrics (`firecracker`, `docker-dev`, `remote`) without adding tenant labels.
  - Document exact support boundaries, socket authority, platform prerequisites, and the remote-KVM escape hatch.
  - Release docker-dev as an opt-in developer feature; production deployment templates retain the Firecracker default and have no Docker socket.
- **Validation**:
  - Documentation lint/search verifies every docker-dev command targets the standalone dev Compose file and every production command remains Firecracker/KVM.
  - Metrics test verifies backend labels are bounded and do not contain sandbox IDs or tenant IDs.
  - Rollback test removes the dev Compose topology and confirms normal production Firecracker startup still works from the same image with the default backend.

## Testing Strategy

### Contract and unit tests

- `BackendKind` parsing, defaults, conflicts, and absent Docker socket behavior.
- DockerEngine fake verifies exact lifecycle ordering and compensation for every failure point.
- Container configuration tests verify labels, entrypoint, injected agent path, management/egress networks, resource values, and mount policy.
- Agent tests ensure only `serve --skip-network-config` bypasses static-IP setup.
- Existing TestVmm tests remain test-only and are not repurposed as DockerDevVmm coverage.

### Docker integration tests

- Start the API with `docker-compose.dev.yml`, then use authenticated HTTP and each SDK.
- Build/pull a portable Python-and-Node fixture; prove both runtimes execute in the same development sandbox.
- Test input validation and real behavior for files, secrets, sync/SSE exec, timeout, TTL, delete, restart/reconnect, and image pull failure.
- Inspect Docker state by labels: one management network, correctly attached containers, no public agent port, resource configuration present, and no residual labelled container after each failure path.

### Production regression

- Build the production container image without a Docker socket.
- On a Linux host with `/dev/kvm`, run the existing privileged host-network Firecracker acceptance.
- Assert Firecracker metadata, guest-agent hello, network enforcement, Python+Node execution, teardown, and zero residual netns/Firecracker state.

### Negative acceptance cases

| Case | Expected outcome |
|---|---|
| `--backend docker-dev` with `--node-endpoint` or `--cluster-scheduling` | Startup validation error. |
| Docker socket absent, unreadable, or non-Docker | Clear startup/configuration error; no sandbox record or container. |
| Nonempty `allow_egress` on docker-dev | HTTP validation error before container create. |
| IOPS/bandwidth limit on docker-dev | HTTP validation error before container create. |
| Out-of-root or symlink-escaping host mount | HTTP validation error; no Docker bind mount. |
| Docker image pull, create, archive upload, start, or hello failure | Sandbox is `Error`; no labelled container/network leak. |
| Snapshot or restore request on docker-dev | Explicit unsupported VMM error, never a fake snapshot. |
| API restart with orphaned labelled container | Reconciler removes only the orphan; unrelated Docker containers survive. |
| API restart with durable active docker-dev sandbox | Connector reestablishes agent connection and exec/files remain available. |
| Production runtime accidentally receives Docker socket | Deployment policy/test fails; socket is not consumed by Firecracker mode. |

## Rollout and Rollback

1. Land the backend selection and standalone dev Compose topology behind explicit `--backend docker-dev`.
2. Exercise Linux Docker Engine CI plus manual Docker Desktop macOS and WSL2 acceptance.
3. Publish the workflow as **development preview**; default production behavior remains unchanged.
4. Promote only after the Docker suite and KVM regression both pass repeatedly.

Rollback is configuration-only for operators: stop `docker-compose.dev.yml` and remove its labelled containers/networks. Production rollback is unaffected because it uses the default Firecracker backend and never mounts the Docker socket. If implementation needs to be reverted, remove DockerDevVmm selection and its standalone Compose file while retaining database rows as ordinary historical sandbox metadata with `backend: docker-dev`.

## Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Docker socket is host-equivalent authority. | Development-only standalone Compose; no production socket mount; explicit documentation and deployment scan. |
| Docker Desktop networking does not match host-veth nftables. | Separate management network; reject domain allowlists; label all docker-dev results as non-production. |
| Arbitrary OCI images lack a shell or a long-running default command. | Inject a static Clouisle agent as PID 1 rather than relying on `sleep`, `sh`, or the image CMD. |
| Docker API streaming/timeout behavior differs from guest execution. | Reuse the agent TCP protocol, not Docker exec. |
| Mac arm64 and Windows Docker Desktop image availability differs. | Use a project-owned multi-architecture Python-and-Node fixture; require engine-matching image manifests for developer images. |
| Container restart leaves unmanaged resources. | Strong labels, durable-store comparison, idempotent cleanup, and explicit orphan tests. |
| Stale ADR claims confuse implementation. | Add a superseding ADR before implementation begins. |
| Backend semantics leak into SDK assumptions. | Preserve request/response schemas; expose `backend: docker-dev` metadata and document capability limits. |
