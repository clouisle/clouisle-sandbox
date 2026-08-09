# Production Completeness Design Document

## Background & Goals

The repository’s unit and host-level acceptance tests exercise substantial single-node behavior, but the advertised production topology is incomplete: `clouisled` is a library without a process entrypoint, Kubernetes references placeholder images, authentication defaults to an unrestricted principal, tenant identity is not applied at handlers, OCI rootfs construction is not on the VMM create path, gRPC is not used by the API scheduler, and snapshot restore returns an intentional error.

Success requires that each documented production capability be executable rather than merely represented by types, routes, or test doubles. A deployment must start the documented binaries, enforce credentials and tenant boundaries, resolve image references to a rootfs, schedule lifecycle calls through live node agents, and recover snapshots.

## High-Level Design

`clouisle-apiserver` owns authentication, durable metadata, tenant authorization, scheduling, and node discovery. `clouisled` becomes an executable node daemon that owns Firecracker, its rootfs cache, guest-agent channel, network lifecycle, registration, and heartbeat. The API communicates with selected daemons via authenticated gRPC. OCI resolution runs in the node that will launch the VM, so built rootfs files are local to that node.

All handlers receive an authenticated `Principal`; protected resource records carry an owning tenant ID. Cluster configurations accept deploy-time image registry and API-key secret values instead of hard-coded development defaults. Lifecycle compensation remains local and exact: any create failure stops its VM and removes its network state.

## Implementation Plan

### Stage 1: Executable topology
- **Files modified**: `crates/clouisled/Cargo.toml`, `crates/clouisled/src/main.rs`, `crates/clouisled/src/*`, `Dockerfile`, `docker-compose.yml`, `deploy/*.yaml`.
- **Specific logic**: Add the node-daemon CLI and process initialization; build it into the runtime image; give the API and node processes unambiguous production commands, storage configuration, health probes, and mounted kernel/rootfs/cache paths.
- **Validation**: Build both binaries, start the documented Compose topology, confirm the node daemon listens and reports healthy.

### Stage 2: Authentication and tenant authorization
- **Files modified**: `crates/clouisle-api/src/{main.rs,auth.rs,middleware_auth.rs,handlers/*}`, `crates/clouisle-core/src/*`, `crates/clouisle-store/src/*`, migrations and API tests.
- **Specific logic**: Load production credentials from configuration, fail closed outside explicit development mode, persist ownership, require write scope for mutations, and constrain every read/write/query to the caller tenant.
- **Validation**: Test missing, malformed, read-only, and cross-tenant credentials against every resource endpoint.

### Stage 3: Node scheduling and gRPC lifecycle forwarding
- **Files modified**: `crates/clouisle-api/src/*`, `crates/clouisled/src/*`, `crates/clouisle-store/src/*`, `proto/node.proto`, integration tests.
- **Specific logic**: Persist registrations/heartbeats, select a healthy eligible node atomically, forward create/delete/exec through that node’s live client, and reconcile unavailable nodes. Secure the API-to-daemon channel rather than exposing unauthenticated cluster RPC.
- **Validation**: Run two node daemons, create and execute through a selected node, verify capacity accounting and unavailable-node rejection.

### Stage 4: OCI rootfs and guest execution
- **Files modified**: `crates/clouisle-images/src/*`, `crates/clouisled/src/*`, `crates/clouisle-vmm/src/*`, guest-agent packaging, integration tests.
- **Specific logic**: Resolve `ImageRef` to a verified local ext4 image, inject the guest agent, cache by immutable digest, and pass the produced rootfs to Firecracker. Keep mock connectors test-only; production paths must use the real guest transport. Implement actual incremental SSE forwarding if the advertised streaming contract remains.
- **Validation**: Start a VM from an OCI reference not pre-provisioned in the cache, execute a command, and verify upload/download and streaming output.

### Stage 5: Snapshot recovery and operational contract
- **Files modified**: `crates/clouisle-vmm/src/*`, API routes/handlers, store metadata, tests, README deployment sections.
- **Specific logic**: Complete Firecracker snapshot restore, record recoverable metadata, expose only supported snapshot operations, and remove claims that no longer match runtime behavior.
- **Validation**: Snapshot a paused VM, restore it, reconnect to the guest agent, and verify process/network cleanup after failure.

### Stage 6: Deployment validation
- **Files modified**: `deploy/*.yaml`, `Dockerfile`, CI workflows, acceptance scripts and report.
- **Specific logic**: Replace all placeholder registry names, align ports and service discovery, include required dependencies and immutable configuration, and validate manifests with the actual executable commands.
- **Validation**: Render/apply manifests in a Linux KVM cluster and execute the full API, SDK, lifecycle, network, auth, and cleanup acceptance matrix.

## Testing Strategy

- Unit tests cover validation, tenant policy, rootfs selection, snapshot state transitions, and node eligibility.
- Integration tests exercise authenticated HTTP-to-gRPC lifecycle forwarding, OCI cache miss/hit behavior, guest agent command/file transport, and SSE output ordering.
- End-to-end Linux/KVM acceptance proves Docker and Kubernetes startup, real Firecracker launch, cross-tenant denial, firewall policy, snapshot recovery, and no residual VM/network processes after forced failures.

## Risks & Mitigation

- Production topology changes cross crate boundaries. Keep one authoritative lifecycle owner (`clouisled`) and reject direct API VMM control in cluster mode.
- Tenant schema changes require explicit migrations and backfill policy; do not silently assign existing production sandboxes.
- OCI layers and guest-agent injection are security-sensitive. Pin image digests, validate extraction paths, and build rootfs only beneath managed cache paths.
- Snapshot restore is sensitive to Firecracker version/kernel compatibility. Persist and verify compatibility metadata before restore; return a clear unsupported response when the selected backend lacks the capability.
- Roll back by deploying the prior tagged image and preserving database migrations that are backward-readable until the cutover is verified.
