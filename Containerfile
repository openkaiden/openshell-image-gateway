#
# Copyright (C) 2026 Red Hat, Inc.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# SPDX-License-Identifier: Apache-2.0

# OpenShell gateway + Windows bind-mount admission hook.
#
# The admission hook (Rust, gRPC GatewayInterceptor) is the entrypoint. It
# listens on 127.0.0.1:12777, registers itself in the gateway config, and
# then starts the stock OpenShell gateway as a child process.

ARG GATEWAY_IMAGE=ghcr.io/nvidia/openshell/gateway
# Gateway image tag, without the "v" prefix of git tags (e.g. 0.1.9).
# Taken from the OPENSHELL_VERSION environment variable when it is passed
# without a value, otherwise "latest":
#   OPENSHELL_VERSION=0.1.9 podman build --build-arg OPENSHELL_VERSION .
ARG VERSION=0.1.9
ARG GATEWAY_TAG=${VERSION}
# Red Hat hardened (Hummingbird) Rust builder. It ships a distro-packaged
# toolchain without rustup or cross-compilers, so each platform is built
# natively (arm64 runs under qemu in multi-arch builds).
ARG RUST_IMAGE=registry.access.redhat.com/hi/rust:1.100.0-builder

FROM ${RUST_IMAGE} AS build
# No extra packages: the protos are compiled by protox (pure Rust), not protoc.
WORKDIR /src

# Fetch dependencies first so they are cached across source changes.
COPY Cargo.toml Cargo.lock openshell.version ./
COPY crates/path-admission/Cargo.toml crates/path-admission/build.rs crates/path-admission/
RUN mkdir crates/path-admission/src && echo 'fn main() {}' > crates/path-admission/src/main.rs \
 && cargo fetch --locked \
 && rm -rf crates/path-admission/src

# build.rs downloads the OpenShell protos for the tag in openshell.version.
COPY crates/path-admission/src crates/path-admission/src
RUN cargo test --release --locked \
 && cargo build --release --locked \
 && install -D -m 0755 target/release/openshell-admission-hook /out/openshell-admission-hook

FROM ${GATEWAY_IMAGE}:${GATEWAY_TAG}

COPY --from=build /out/openshell-admission-hook /usr/local/bin/openshell-admission-hook

# 12777: admission hook (loopback by default, see ADMISSION_HOOK_LISTEN)
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/openshell-admission-hook", "launch", "--", "/usr/local/bin/openshell-gateway"]
# Same default arguments as the upstream gateway image.
CMD ["--bind-address", "0.0.0.0", "--port", "8080"]
