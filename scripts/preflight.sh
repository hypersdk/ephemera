#!/usr/bin/env bash
# Copyright 2026 Zyvor
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

missing=0
for cmd in qemu-system-x86_64 qemu-img cloud-localds virt-customize ip; do
  if command -v "$cmd" >/dev/null 2>&1; then
    printf '%-24s %s\n' "$cmd" "OK ($(command -v "$cmd"))"
  else
    printf '%-24s %s\n' "$cmd" "MISSING"
    missing=1
  fi
done

for cmd in cloud-hypervisor firecracker; do
  if command -v "$cmd" >/dev/null 2>&1; then
    printf '%-24s %s\n' "$cmd" "OK ($(command -v "$cmd"))"
  else
    printf '%-24s %s\n' "$cmd" "optional/not installed"
  fi
done

if [[ -e /dev/kvm ]]; then
  echo "/dev/kvm                 OK"
else
  echo "/dev/kvm                 MISSING"
  missing=1
fi

exit "$missing"
