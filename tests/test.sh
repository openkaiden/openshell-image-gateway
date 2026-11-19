#!/usr/bin/env bash
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

# Smoke test of the built image, run by the shared oci-image workflow once per platform.
# IMAGE and PLATFORM are provided by the workflow; tests/setup.sh started the
# Podman API service used by the gateway's compute driver.
set -euo pipefail

workdir=/tmp/openshell-image-gateway-test
socket="${workdir}/podman.sock"
grpcurl_image=docker.io/fullstorydev/grpcurl:v1.9.3
port=18080
name="openshell-gateway-test-${PLATFORM//\//-}"

fail() {
  echo "::error::$*"
  exit 1
}

# 1. The hook binary runs on this platform.
version=$(podman run --rm --platform "${PLATFORM}" \
  --entrypoint /usr/local/bin/openshell-admission-hook "${IMAGE}" --version)
echo "hook version: ${version}"
[[ "${version}" == openshell-admission-hook\ * ]] || fail "unexpected --version output: ${version}"

# 2. The launcher starts the hook before the gateway, and the gateway accepts
#    the generated config (interceptor registration included) and exit codes
#    are passed through.
log=$(podman run --rm --platform "${PLATFORM}" "${IMAGE}" config preflight 2>&1) \
  || fail "gateway config preflight failed: ${log}"
echo "${log}"
hook_line=$(grep -n 'admission hook listening' <<< "${log}" | cut -d: -f1)
gateway_line=$(grep -n 'gateway started' <<< "${log}" | cut -d: -f1)
[[ -n "${hook_line}" && -n "${gateway_line}" && "${hook_line}" -lt "${gateway_line}" ]] \
  || fail "the admission hook must start before the gateway"
grep -q 'gateway exited code=0' <<< "${log}" || fail "launcher did not report the gateway exit code"

set +e
podman run --rm --platform "${PLATFORM}" "${IMAGE}" --no-such-flag > /dev/null 2>&1
code=$?
set -e
[[ "${code}" -ne 0 ]] || fail "a failing gateway must fail the container"

# 3. A running gateway negotiates with the hook and routes CreateSandbox
#    through it.
config_dir=$(mktemp -d)
cleanup() {
  podman rm -f "${name}" > /dev/null 2>&1 || true
  rm -rf "${config_dir}"
}
trap cleanup EXIT

cat > "${config_dir}/gateway.toml" <<'EOF'
[openshell]
version = 2

[openshell.gateway]
compute_driver = "podman"
disable_tls    = true

[openshell.gateway.auth]
allow_unauthenticated_users = true

[openshell.drivers.podman]
socket_path         = "/run/podman/podman.sock"
allow_driver_config = true
enable_bind_mounts  = true

[openshell.drivers.podman.resource_admission]
enabled = false
EOF
chmod -R a+rX "${config_dir}"

podman run -d --name "${name}" --platform "${PLATFORM}" --user 0 \
  -p "127.0.0.1:${port}:8080" \
  -v "${socket}:/run/podman/podman.sock" \
  -v "${config_dir}:/etc/openshell:ro" \
  -e OPENSHELL_GATEWAY_CONFIG=/etc/openshell/gateway.toml \
  "${IMAGE}" > /dev/null

# Logs are captured before grepping: with pipefail, `podman logs | grep -q`
# fails when grep exits early and podman logs gets SIGPIPE.
gateway_logs() {
  podman logs "${name}" 2>&1
}

ready=""
for _ in $(seq 1 120); do
  logs=$(gateway_logs)
  if grep -q 'gateway interceptors initialized bindings=1' <<< "${logs}"; then
    ready=yes
    break
  fi
  if [[ "$(podman inspect -f '{{.State.Running}}' "${name}")" != "true" ]]; then
    break
  fi
  sleep 1
done
if [[ -z "${ready}" ]]; then
  gateway_logs | tail -50
  fail "the gateway did not initialize the admission hook"
fi
grep -q 'gateway connected (Describe)' <<< "${logs}" || fail "the hook did not see Describe"

create_sandbox() {
  podman run --rm -i --network host "${grpcurl_image}" \
    -plaintext -max-time 60 -d @ "127.0.0.1:${port}" openshell.v1.OpenShell/CreateSandbox 2>&1 || true
}

# Wait for the API listener.
for _ in $(seq 1 60); do
  if podman run --rm --network host "${grpcurl_image}" -plaintext "127.0.0.1:${port}" list > /dev/null 2>&1; then
    break
  fi
  sleep 1
done

# An unmappable Windows path is denied by the hook.
response=$(create_sandbox <<'EOF'
{
  "workspaceScope": {"workspace": "default"},
  "name": "unc-path",
  "spec": {"template": {"driverConfig": {"podman": {"mounts": [
    {"type": "bind", "source": "\\\\server\\share\\dir", "target": "/data"}
  ]}}}}
}
EOF
)
echo "UNC response: ${response}"
grep -q 'InvalidArgument' <<< "${response}" || fail "the hook did not deny a UNC bind mount"
grep -q 'UNC network paths are not supported' <<< "${response}" \
  || fail "the UNC denial does not come from the hook"

# A Windows drive path is rewritten to the Podman machine path. The image
# reference is deliberately invalid so the driver rejects the request before
# creating any container (it still pulls its runtime images); the gateway log
# shows the patch the hook applied.
response=$(create_sandbox <<'EOF'
{
  "workspaceScope": {"workspace": "default"},
  "name": "drive-path",
  "spec": {"template": {"image": "INVALID IMAGE REFERENCE", "driverConfig": {"podman": {"mounts": [
    {"type": "bind", "source": "C:\\Users\\test\\proj", "target": "/data"}
  ]}}}}
}
EOF
)
echo "drive path response: ${response}"
logs=$(gateway_logs)
grep -q 'to=/mnt/c/Users/test/proj' <<< "${logs}" \
  || fail "the hook did not rewrite C:\\Users\\test\\proj"
grep -q 'interceptor=windows-mount-admission .*patch_count=1' <<< "${logs}" \
  || fail "the gateway did not apply the hook's patch"

echo "all tests passed for ${PLATFORM}"
