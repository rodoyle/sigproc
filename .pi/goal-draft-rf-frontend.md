# Goal draft (parked) — sigproc RF front-end container pipeline

> Parked after two session replacements at `propose_goal_draft`. All interview
> decisions are locked (user confirmed "Nothing changed"). Reload later with
> `/goal plan` and propose immediately, or execute directly.

## Seed (verbatim)

We need to clean up the docker file I sketched out (./Dockerfile), build an image
with kaniko in our k8s cluster and then deploy a single instance of the precisely
to rpi-four-2. This and only this node has the LibreSDR B210 SDR device connected
via usb. This is going to be the RF front end that pairs with
$env:USERPROFILE/src/waterfall. We definitely want rust, rust-uhd installed. We may
want to also install sdr++ so we can use its network sink to automatically send
captured packets to our cluster. We probably need to build sdr++ from source so we
can include libuhd and the USRP source. Recall we need an arm64 build for the
raspberry pi. Rust version doesn't matter, probably 1.98 or nightly for the neon
extensions.

## Locked decisions (4 interview rounds, user-confirmed)

| # | Category | Decision |
| --- | --- | --- |
| 1 | sdr++ | Deferred — Rust-only, VITA49 sink in Rust |
| 2 | Architecture | Two containers, network link (sigproc → VITA49 UDP → waterfall) |
| 3 | Scope | sigproc container only; waterfall deploy is a follow-up goal |
| 4 | Runtime | UHD capture + VITA49 forward (FMA kernels available, not yet called) |
| 5 | Dockerfile | Production-grade multi-stage, linux/arm64, cargo-chef caching |
| 6 | Toolchain | Latest nightly Rust |
| 7 | Deliverables | Dockerfile + k8s Deployment + standalone kaniko Job YAML |
| 8 | Build arch | Kaniko on any arm64 node (native, no QEMU) |
| 9 | Registry | GHCR: `ghcr.io/rodoyle/sigproc:latest` |
| 10 | GHCR auth | Secret already exists in cluster (name unknown — list secrets and pick the obvious one; kaniko needs docker config.json format) |
| 11 | USB access | privileged: true + hostPID: true |
| 12 | Config | ConfigMap `sigproc-config` with config.toml |
| 13 | config.toml schema | center_freq, sample_rate, rx_gain, antenna, bandwidth, vita49_dest_host, vita49_dest_port |
| 14 | Acceptance | `uhd_usrp_probe` exits 0 inside deployed pod, output shows B210 |

## Objective (proposal-ready)

### Current state

- **sigproc** (`C:/Users/rodoyle/src/sigproc`): Rust library with NEON FMA kernels
  for ARM64 signal processing. `lib.rs` has pure-SIMD primitives; `main.rs` is a
  hello-world stub. `Cargo.toml` declares no external dependencies (no UHD, no
  networking). `Dockerfile` is a rough sketch with shell prompts (`$`), missing
  `RUN` prefixes, no architecture targeting, commented-out sdr++ stanzas.
- **waterfall** (`~/src/waterfall`): full-stack SDR spectrum analyzer (bridge
  server, WebSocket frontend, planned VITA49 consumer at M2). At M1 (synthetic
  I/Q). Not in scope.
- **Target hardware**: Raspberry Pi 4 ("rpi-four-2") with LibreSDR B210 USB 3.0
  attached. Only cluster node with the B210.

### Architectural decisions

1. **Two containers, network link.** sigproc captures RF on rpi-four-2 and
   forwards VITA49 UDP to waterfall elsewhere in the cluster. Isolates the
   privileged USB container to one node; waterfall stays unprivileged.
   - Rejected: one container (couples RF hardware to web frontend).
   - Rejected: sigproc as library crate of waterfall (different deploy/security profiles).

2. **Production-grade multi-stage Dockerfile.** Stage 1: nightly Rust + UHD dev +
   cargo-chef for dependency caching. Stage 2: thin runtime with UHD runtime libs
   - compiled binary.
   - Rejected: single-stage (bloated, no layer caching); fix-syntax-only (rewritten immediately).

3. **privileged + hostPID for USB access.** Simplest path to `/dev/bus/usb` for
   UHD's libusb transport. Node-pinned pod contains the security surface.
   - Rejected: device-specific hostPath (fragile across replug/reboot).
   - Rejected: udev + cgroup allowlist (host-side setup outside goal).

4. **ConfigMap-driven config.toml** for RF parameters and forwarding targets.
   - Rejected: env vars (too flat for nested UHD args); hardcoded (not portable).

5. **sdr++ deferred.** VITA49 sink handled in Rust directly; avoids a C++/ARM64
   build chain.

### Milestones

- **M0 — Clean Dockerfile.** Multi-stage, `linux/arm64`. Stage 1: rust nightly +
  Ettus PPA `libuhd-dev` + cargo-chef. Stage 2: minimal Debian + UHD runtime +
  binary. Chef layer caching for deps.
- **M1 — Rust UHD capture binary.** Replace hello-world `main.rs`: init UHD,
  discover B210, read `config.toml`, acquire IQ at configured rate/freq/gain,
  forward as VITA49 UDP to configured destination. `lib.rs` FMA kernels remain
  available for future preprocessing.
- **M2 — Kubernetes manifests.** (a) Deployment: `nodeSelector:
  kubernetes.io/hostname=rpi-four-2`, privileged, hostPID, ConfigMap volume for
  config.toml. (b) Standalone kaniko Job: builds Dockerfile, pushes to
  `ghcr.io/rodoyle/sigproc:latest` using existing GHCR secret.
- **M3 — End-to-end verification.** Kaniko builds + pushes; Deployment pulls on
  rpi-four-2; `kubectl exec` runs `uhd_usrp_probe`, confirms B210 visible.

### Risks & failure modes

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Ettus PPA lacks arm64 packages | UHD must build from source | Fall back to cmake source build; +~20min build |
| Rust nightly breaks NEON intrinsics | Build failure on rebuild | Pin nightly date; track in Cargo.toml |
| USB device path changes on reboot | Pod can't find B210 | UHD discovers by VID/PID, resilient |
| GHCR secret wrong format for kaniko | Push fails | docker config.json format; validate step |
| Kaniko arm64 executor unavailable | No builder | gcr.io/kaniko-project/executor is multi-arch; arm64 supported |

### Verification contract

**M0 — Dockerfile build**

1. `docker build --platform linux/arm64 -t sigproc:test .` exits 0
2. `docker run --rm sigproc:test uhd_find_devices --help` exits 0
3. `docker run --rm sigproc:test sigproc --version` exits 0
4. Image size < 500MB

**M1 — Rust binary**
5. `cargo build --target aarch64-unknown-linux-gnu` passes
6. `cargo test` passes (FMA kernel tests intact)
7. Binary reads config.toml, logs parsed parameters on startup (`--dry-run`)
8. Binary exits cleanly with error message if no UHD device found

**M2 — Kubernetes manifests**
9. `kubectl apply --dry-run=client -f deploy/sigproc-deployment.yaml` validates
10. `kubectl apply --dry-run=client -f deploy/kaniko-job.yaml` validates
11. Deployment has `nodeSelector.kubernetes.io/hostname: rpi-four-2`
12. Deployment has `privileged: true` and `hostPID: true`
13. ConfigMap `sigproc-config` / `config.toml` has all standard fields

**M3 — End-to-end (requires cluster + hardware)**
14. `kubectl get job kaniko-build-sigproc -o jsonpath='{.status.succeeded}'` = `1`
15. `skopeo inspect docker://ghcr.io/rodoyle/sigproc:latest` returns manifest
16. `kubectl get pod -l app=sigproc -o jsonpath='{.items[0].spec.nodeName}'` = `rpi-four-2`
17. **Acceptance gate:** `kubectl exec deploy/sigproc -- uhd_usrp_probe` exits 0
    and output contains B210 device identification
