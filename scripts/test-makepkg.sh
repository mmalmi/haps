#!/usr/bin/env bash
set -euo pipefail
# Run the already-built Linux Haps binary with real Arch tooling as a normal user.
docker run --rm -v "$PWD:/workspace:ro" archlinux:base-devel bash -ceu '
  pacman -Syu --noconfirm --needed python git
  useradd --create-home builder
  install -m755 /workspace/target/debug/haps /usr/local/bin/haps
  su builder -c "HAPS_TEST_BINARY=/usr/local/bin/haps python /workspace/tests/makepkg_test.py"
'
