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

# Prepares the runner once, before tests/test.sh runs for each platform.
# The gateway under test uses the Podman compute driver, so start a rootless
# Podman API service it can reach through a mounted socket. The service keeps
# running for the rest of the job.
set -euo pipefail

workdir=/tmp/openshell-image-gateway-test
socket="${workdir}/podman.sock"
mkdir -p "${workdir}"

if [[ ! -S "${socket}" ]]; then
  nohup podman system service --time=0 "unix://${socket}" > "${workdir}/podman-service.log" 2>&1 &
fi

for _ in $(seq 1 30); do
  if [[ -S "${socket}" ]] && podman --url "unix://${socket}" version > /dev/null 2>&1; then
    echo "podman API service ready at ${socket}"
    exit 0
  fi
  sleep 1
done
echo "::error::podman API service did not start"
cat "${workdir}/podman-service.log"
exit 1
