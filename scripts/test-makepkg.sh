#!/usr/bin/env bash
set -euo pipefail
# Run the already-built Linux Haps binary with real Arch tooling as a normal user.
# Permit nested user namespaces for bubblewrap; the build itself runs as builder
# inside Haps's unprivileged sandbox. No privileged container or host socket mount.
# Bubblewrap must set a quota inside its own user namespace through /proc/sys;
# Docker's read-only system paths otherwise prevent --disable-userns from working.
docker run --rm --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
  --security-opt systempaths=unconfined \
  -v "$PWD:/workspace:ro" archlinux:base-devel bash -ceu '
  pacman -Syu --noconfirm --needed python git bubblewrap
  useradd --create-home builder
  install -m755 /workspace/target/debug/haps /usr/local/bin/haps
  su builder -c "HAPS_TEST_BINARY=/usr/local/bin/haps python /workspace/tests/makepkg_test.py"
'
