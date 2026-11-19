# openshell-image-gateway

OCI image based on the [OpenShell](https://github.com/NVIDIA/OpenShell) gateway
(`ghcr.io/nvidia/openshell/gateway`) that adds a Rust **admission hook**. The
hook rewrites Windows bind-mount paths in sandbox creation requests to the path
the container machine sees (Docker Desktop / Podman machine).

```
openshell sandbox create --driver-config-json \
  '{"podman":{"mounts":[{"type":"bind","source":"C:\\Users\\me\\proj","target":"/sandbox/proj"}]}}'

            ── CreateSandbox ──▶ gateway ── modify_operation ──▶ admission hook (127.0.0.1:12777)
                                    ◀── JSON patch: source = /mnt/c/Users/me/proj ──
                                    ── podman driver creates the bind mount
```

## How it works

* **Admission hook.** A gRPC
  [gateway interceptor](https://docs.nvidia.com/openshell/latest/extensibility/gateway-interceptors)
  (`openshell.gateway_interceptor.v1.GatewayInterceptor`) bound to
  `openshell.v1.OpenShell/CreateSandbox` in the `modify_operation` phase.
  * It checks `spec.template.driverConfig.docker.mounts[]` and
    `spec.template.driverConfig.podman.mounts[]`.
  * For every `type: "bind"` entry whose `source` is a Windows path, it returns
    an RFC 6902 `replace` patch.
  * Windows paths it can't map (UNC shares, `..`, drive-relative `C:foo`) are
    denied with `INVALID_ARGUMENT`.

  | Driver | `C:\Users\me\proj` becomes | Override |
  |---|---|---|
  | docker (Docker Desktop) | `/run/desktop/mnt/host/c/Users/me/proj` | `ADMISSION_HOOK_DOCKER_PREFIX` |
  | podman (Podman machine) | `/mnt/c/Users/me/proj` | `ADMISSION_HOOK_PODMAN_PREFIX` |

  Both `\` and `/` separators are accepted, as are `\\?\C:\...` paths. POSIX
  paths and volume names are left untouched.

* **Startup order.** The upstream image is distroless (no shell), so the hook
  binary is the entrypoint (PID 1):
  1. It binds and serves the hook on `127.0.0.1:12777`.
  2. It registers the hook in the gateway config. It takes the file given by
     `--config`, `OPENSHELL_GATEWAY_CONFIG`, or the gateway's XDG default,
     copies it, appends the `[[openshell.gateway.interceptors]]` block below,
     and writes the result to `/tmp/openshell-admission-hook/gateway.toml`.
     Your file is never modified. If no config exists, it generates a minimal
     one.
  3. It starts `/usr/local/bin/openshell-gateway` with the container
     arguments, pointing it at the generated config.
  4. It forwards SIGTERM, SIGINT, SIGQUIT and SIGHUP, reaps zombies, and exits
     with the gateway's exit code. If the hook dies, it stops the gateway, so
     the container never runs without the hook.

  Injected registration:

  ```toml
  [[openshell.gateway.interceptors]]
  name = "windows-mount-admission"
  grpc_endpoint = "http://127.0.0.1:12777"
  allow_insecure_transport = true   # loopback inside the container only
  order = -1000                     # runs before other interceptors
  failure_policy = "fail_closed"
  binding_policy = "exact"
  timeout = "2s"

  [[openshell.gateway.interceptors.bindings]]
  rpc = "openshell.v1.OpenShell/CreateSandbox"
  phases = ["modify_operation"]
  ```

  At startup the gateway logs one warning about `allow_insecure_transport`.
  This is expected: the hook only listens on loopback inside the gateway
  container.

## Build

```sh
podman build -t openshell-gateway-admission .
# or: docker build -f Containerfile -t openshell-gateway-admission .

# choose the upstream gateway version from the OPENSHELL_VERSION env var
# (image tag, no "v" prefix); without it the base is gateway:latest
export OPENSHELL_VERSION=0.1.9
podman build --build-arg OPENSHELL_VERSION -t openshell-gateway-admission:0.1.9 .

# or pass it directly (GATEWAY_TAG overrides OPENSHELL_VERSION)
podman build --build-arg OPENSHELL_VERSION=0.1.9 .

# multi-arch (each platform builds natively; non-native ones need qemu binfmt)
podman build --platform linux/amd64,linux/arm64 --manifest openshell-gateway-admission .
```

The build stage uses Red Hat's hardened Rust builder
(`registry.access.redhat.com/hi/rust:1.100.0-builder`, override with
`--build-arg RUST_IMAGE=...`). It has a distro-packaged toolchain with no
rustup or cross-compilers, so there is no cross-compilation. The binary is
linked against the builder's glibc (2.43), but only uses symbols up to
`GLIBC_2.39`, so it runs on the gateway's Debian 13 base (glibc 2.41).
`tests/test.sh` runs it in the final image to catch any regression.

### OpenShell protos

The protobuf definitions aren't vendored. The version in
[`openshell.version`](openshell.version) selects which ones to use. It is a
bare version such as `0.1.9`, with no leading `v`, the same form as the
gateway image tag. At build time, `crates/path-admission/build.rs`:

1. downloads `gateway_interceptor.proto` and `extension.proto` from
   `https://raw.githubusercontent.com/NVIDIA/OpenShell/v<version>/proto/`, plus every
   non-`google/` file they import, recursively;
2. caches them per tag under `target/<profile>/openshell-proto/<tag>/`;
3. compiles them with [`protox`](https://crates.io/crates/protox) (a pure-Rust
   protobuf compiler with the well-known types built in, so no system `protoc`
   is needed), then generates the code with `tonic-prost-build`.

To move to another OpenShell release, edit `openshell.version` and rebuild.
Keep it in line with the gateway image; the same value works for both:
`OPENSHELL_VERSION=$(cat openshell.version) podman build --build-arg OPENSHELL_VERSION .`.
The gateway interceptor protocol must have the same major version. Overrides:

| Env var | Purpose |
|---|---|
| `OPENSHELL_PROTO_DIR` | use a local `proto/` directory (offline / OpenShell checkout); no download |
| `OPENSHELL_PROTO_BASE_URL` | mirror instead of `https://raw.githubusercontent.com/NVIDIA/OpenShell` |

The build runs the unit tests (`cargo test`). The crate is Unix-only because it
uses signals and `waitpid`. On Windows, build it through the container.

### CI

The GitHub workflows follow `openkaiden/openshell-image-claude` and use the
shared `openkaiden/gh-actions` OCI image workflow (buildah, linux/amd64 and
linux/arm64):

| Workflow | Trigger | Does |
|---|---|---|
| `pr-check.yaml` | pull requests | `cargo fmt --check`, `cargo clippy -D warnings`, image build and tests (no push) |
| `next-build.yaml` | push to `main`, manual | image build and tests, push `ghcr.io/openkaiden/openshell-image-gateway:next` (+ commit SHA tag) with build provenance |

The shared workflow runs the image tests per platform:
- `tests/setup.sh` runs once and starts a rootless Podman API socket.
- `tests/test.sh` checks that:
  - the hook binary runs;
  - the hook starts before the gateway and the gateway's exit code is passed through;
  - `config preflight` accepts the generated config;
  - a running gateway (Podman driver) negotiates with the hook;
  - a UNC bind mount is denied;
  - `C:\Users\test\proj` is rewritten to `/mnt/c/Users/test/proj`.

To run the tests locally against a built image:

```sh
bash tests/setup.sh
IMAGE=openshell-gateway-admission PLATFORM=linux/amd64 bash tests/test.sh
```

Dependabot updates the GitHub Actions and the cargo dependencies (tonic and
prost grouped). The `docker` entry is kept to match the other images, but the
Containerfile `FROM` lines use build args (`OPENSHELL_VERSION`/`GATEWAY_TAG`, `RUST_IMAGE`), which
Dependabot doesn't bump.

## Run

Use the image exactly like the upstream gateway image: same arguments, same
environment variables, same mounts. For example, swap the image in OpenShell's
`deploy/docker/docker-compose.yml`. Upstream's `command: []` trick still works,
because the hook passes the gateway arguments through unchanged.

Bind mounts are disabled in the gateway by default. To use them, the operator
must enable them in the driver table:

```toml
[openshell.drivers.podman]          # or [openshell.drivers.docker]
allow_driver_config = true
enable_bind_mounts  = true

[openshell.drivers.podman.resource_admission]
enabled = false                     # bind mounts are denied while label admission is on
```

### Settings

| Env var | Default | Purpose |
|---|---|---|
| `ADMISSION_HOOK_LISTEN` | `127.0.0.1:12777` | hook listen address |
| `ADMISSION_HOOK_ENDPOINT` | `http://<listen>` | endpoint written into the gateway config |
| `ADMISSION_HOOK_DOCKER_PREFIX` | `/run/desktop/mnt/host` | Docker drive prefix |
| `ADMISSION_HOOK_PODMAN_PREFIX` | `/mnt` | Podman drive prefix |
| `ADMISSION_HOOK_TIMEOUT` | `2s` | interceptor RPC timeout (10ms to 30s) |
| `ADMISSION_HOOK_SKIP_CONFIG_INJECTION` | unset | `true`: don't touch the config; register the hook yourself |
| `ADMISSION_HOOK_RUNTIME_DIR` | `/tmp/openshell-admission-hook` | where the generated config is written |
| `ADMISSION_HOOK_LOG` | `info` | log filter (`debug` logs every evaluation) |

`openshell-admission-hook serve` runs the hook alone, without a gateway.

## Layout

Cargo workspace; the admission hook is the `crates/path-admission` crate
(binary `openshell-admission-hook`).

| Path | |
|---|---|
| `crates/path-admission/src/paths.rs` | Windows path to machine path conversion |
| `crates/path-admission/src/hook.rs` | gRPC interceptor (Describe negotiation, Evaluate, patches) |
| `crates/path-admission/src/launcher.rs` | entrypoint: config injection and gateway supervision |
| `crates/path-admission/build.rs` | downloads and compiles the OpenShell protos for the pinned tag |
| `openshell.version` | OpenShell version (e.g. `0.1.9`) the protos are taken from (git tag `v<version>`) |
