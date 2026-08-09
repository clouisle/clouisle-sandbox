# Container-only Runtime Design Document

## Background & Goals

Clouisle must not expose or document a host-binary runtime. The supported operating model is a Docker container for single-node deployments and Kubernetes Pods for clustered deployments. The Linux host supplies only the Docker daemon, `/dev/kvm`, and explicitly mounted guest kernel/rootfs paths.

Success means release artifacts contain only multi-architecture OCI images; operator documentation contains no `cargo run` or direct binary-runtime instructions; and a Dockerized API starts a Firecracker guest from an OCI image that has both Python and Node.js available.

## High-Level Design

The `Dockerfile` remains the sole build boundary for deployable Rust binaries, Firecracker, and the statically linked guest agent. Docker Compose and Kubernetes select executable commands inside that image. API users may send HTTP requests from the host, but `clouisle-api`, `clouisled`, `clouislectl`, Firecracker, and the guest agent execute inside containers or Pods.

GitHub Releases are container-image releases. Source-level CI may compile for validation, but it does not publish native runtime binaries.

## Implementation Plan

### Stage 1: Remove native runtime delivery
- **Files modified**: `.github/workflows/release.yml`.
- **Specific logic**: Remove native binary build, archive packaging, and binary upload from tag releases. Retain multi-architecture image publication and the GitHub Release record, explicitly describing the OCI image as the deployment artifact.
- **Validation**: Parse the workflow and confirm its release job has no `cargo build --release`, archive, or binary upload step.

### Stage 2: Document the container-only contract
- **Files modified**: `docker-compose.yml`, `README.md`, `README.zh-CN.md`, `docs/IMPLEMENTATION_PLAN.md`, `docs/plan/container-only-runtime.md`.
- **Specific logic**: Make Compose valid with scalar volume anchors and a host-network PostgreSQL endpoint reachable by the host-network API container. State that Docker/Kubernetes are the only supported runtime paths; describe the host prerequisites precisely; replace native CLI instructions with `docker compose exec`; and preserve direct HTTP examples because they communicate with the containerized API rather than run project code on the host.
- **Validation**: Run `docker compose config --quiet`; search the operator documentation for `cargo run`; and confirm there are no remaining host-binary commands.

### Stage 3: Dockerized KVM acceptance
- **Files modified**: Docker image build context and acceptance evidence only.
- **Specific logic**: Build the current source with `docker build` on a Linux host with `/dev/kvm`, run the API in a privileged host-network container, create `docker.io/nikolaik/python-nodejs:python3.13-nodejs22`, and execute both `python3 --version` and `node --version` within its Firecracker guest.
- **Validation**: Require HTTP 201 for sandbox creation, `running` status, exit code 0 for the combined command, nonempty Python and Node version output, then HTTP 204 delete and no residual container-owned sandbox network state.

## Testing Strategy

- Inspect the release workflow for only OCI publication.
- Search both READMEs for removed native runtime commands and verify the Compose CLI equivalent is documented.
- On the Linux KVM host, build and start only the Dockerized API; use authenticated HTTP to create, execute in, and delete the Python-and-Node OCI guest.

## Risks & Mitigation

- Docker requires elevated capabilities for `/dev/kvm`, netns, TAP, and nftables. The supported runtime command therefore uses `--privileged`, `--network host`, and an explicit KVM mount; do not weaken these requirements silently.
- Host kernel/rootfs assets remain inputs to Firecracker, not host-installed project runtime binaries. Mount them read-only where possible.
- Removing binary release artifacts may break undocumented consumers. This is an intentional clean cutover: operators must pull the versioned OCI image.
- Roll back by running the preceding versioned OCI image; persistent data stays in the mounted `/data` volume.