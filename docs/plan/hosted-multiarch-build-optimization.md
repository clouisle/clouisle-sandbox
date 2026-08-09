# Hosted Multi-architecture Build Optimization Design Document

## Background & Goals

The `v0.1.4` release spent 1 hour 31 minutes 47 seconds in `Build and push Docker image (multi-arch)`, versus 3 minutes 24 seconds for the separate native Rust release build. The Docker step therefore accounted for roughly 96% of the release wall time.

The current release builds both `linux/amd64` and `linux/arm64` in one `ubuntu-latest` amd64 job, has no external BuildKit cache, and copies all workspace source before its only Cargo build layer. ARM64 compilation is consequently emulated and every source change invalidates the full Rust dependency layer.

Success requires both architectures, standard GitHub-hosted runners only, no QEMU Rust compilation, architecture-isolated caches, and a final OCI manifest carrying both `latest` and the Git tag.

## High-Level Design

- Build `linux/amd64` on GitHub-hosted `ubuntu-24.04`.
- Build `linux/arm64` on GitHub-hosted `ubuntu-24.04-arm`.
- Run the architecture jobs concurrently and push each result by immutable digest.
- Merge the two digests into the versioned and `latest` manifest only after both succeed.
- Use architecture-scoped GitHub Actions BuildKit caches.
- Refactor the Dockerfile with `cargo-chef`: dependency recipes are cached independently of project source, with separate GNU application and musl guest-agent cooks.
- Merge Linux clippy/build CI onto one runner, add Rust caches, use `cargo check` for the non-runtime macOS portability job, and cancel superseded branch runs.

GitHub's hosted-runner reference lists `ubuntu-24.04-arm` as a standard arm64 runner for both public and private repositories. Docker's official multi-platform GitHub Actions guidance maps Linux ARM platforms to that runner when builds are distributed.

## Implementation Plan

### Stage 1: Cache-stable Docker build

- **Files modified**: `Dockerfile`.
- **Specific logic**:
  - Add a pinned `cargo-chef` installation stage based on the same Rust image used by all build stages.
  - Install system build dependencies and the architecture-matching musl target before workspace source is copied.
  - Generate `recipe.json` in a planner stage.
  - Cook GNU dependencies for `clouisle-api`, `clouislectl`, and `clouisled` in one cacheable layer.
  - Cook target-specific musl dependencies for `clouisle-agent` in a separate cacheable layer.
  - Copy source only after both dependency layers, then perform locked release builds.
  - Remove redundant explicit `FROM --platform=$TARGETPLATFORM`; native runners already select the requested platform.
- **Validation**: Build the complete amd64 image with Docker BuildKit, verify all four binaries are copied, and repeat the build to confirm layer reuse.

### Stage 2: Native parallel release jobs

- **Files modified**: `.github/workflows/release.yml`.
- **Specific logic**:
  - Replace the single multi-platform job with a two-entry matrix: amd64 on `ubuntu-24.04`, arm64 on `ubuntu-24.04-arm`.
  - Build one platform per runner with current Docker actions and push by canonical digest.
  - Scope BuildKit GHA caches per architecture.
  - Upload the digest filename as a one-day workflow artifact.
  - Add a merge job that downloads both digests, creates one OCI manifest with the Git tag and `latest`, inspects the published manifest, then creates the GitHub Release.
  - Do not publish either mutable tag unless both architecture jobs pass.
- **Validation**: Parse the workflow, confirm both hosted runner labels and platforms occur exactly once, and confirm the merge job depends on both matrix builds.

### Stage 3: Remove duplicate CI compilation

- **Files modified**: `.github/workflows/ci.yml`.
- **Specific logic**:
  - Add workflow concurrency cancellation by workflow/ref.
  - Merge Linux fmt, clippy, and build into one runner so Cargo artifacts are reused in-process.
  - Add architecture/OS-separated Rust caches.
  - Use `--locked` for build/check operations.
  - Change macOS from `cargo build` to `cargo check`; macOS is a portability check, not a shipped native runtime.
  - Keep KVM tests outside hosted CI.
- **Validation**: Parse workflow YAML and verify one Linux dependency-install/build job plus one macOS check job.

### Stage 4: Release verification and rollout

- **Files modified**: `CHANGELOG.md`, `docs/IMPLEMENTATION_PLAN.md`, this document.
- **Specific logic**: Record the build topology change and keep the currently running `v0.1.5` release untouched. The optimized release path takes effect on the next tag.
- **Validation**:
  - Local/remote Docker build succeeds for amd64.
  - Workflow YAML parses.
  - Production source code is unchanged.
  - The first optimized tag proves both digests and the final manifest; compare step timing with the `v0.1.4` 91m47s baseline.

## Testing Strategy

- Dockerfile smoke: `docker build --platform linux/amd64` on the Linux Docker host.
- Cache smoke: repeat the same build and inspect that cargo-chef cook layers are cached.
- Static workflow assertions: matrix has exactly two architectures; arm64 runner is `ubuntu-24.04-arm`; caches use distinct scopes; manifest merge requires build completion.
- YAML parsing for both workflows.
- No application service or Firecracker VM is started by this optimization verification.
- First-tag acceptance: inspect the registry manifest and require both `linux/amd64` and `linux/arm64` descriptors before GitHub Release completion.

## Risks & Mitigation

- **Cold first build**: cargo-chef and both architecture caches start empty. Native parallelism still removes QEMU; later releases gain dependency-layer reuse.
- **Cache collision**: cache scopes include architecture, preventing amd64 and arm64 target contamination.
- **Partial release**: architecture jobs push only digests; mutable tags are created by the dependent merge job after both succeed.
- **ARM hosted-runner availability**: use the documented standard `ubuntu-24.04-arm` label; do not add self-hosted fallback.
- **Registry digest behavior**: use Docker's canonical `push-by-digest` output and inspect the assembled manifest before creating the GitHub Release.
- **Toolchain cache drift**: use the same Rust base in chef/planner/builder and locked Cargo resolution.
- **Current release**: workflow changes cannot accelerate an already-running tag. Do not move or recreate `v0.1.5`; validate on a subsequent tag.

## Rollback

Revert the release workflow and Dockerfile together. Existing versioned OCI manifests remain immutable. If manifest assembly fails, no new mutable version/`latest` tag is published; digest objects may remain unreferenced and can be cleaned by registry retention policy.