#!/usr/bin/env bash
set -euo pipefail
# Run the already-built Linux Haps binary with real Arch tooling as a normal user.
# Permit nested user namespaces for bubblewrap; the build itself runs as builder
# inside Haps's unprivileged sandbox. No privileged container or host socket mount.
# Bubblewrap must set a quota inside its own user namespace through /proc/sys;
# Docker's read-only system paths otherwise prevent --disable-userns from working.
readarray -t arch_inputs < <(python3 -c 'import json; v=json.load(open("scripts/build-inputs.json"))["arch"]; print(v["image"]); print(v["snapshot"]); print(v["databases"]["core"]); print(v["databases"]["extra"])')
docker run --rm --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
  --security-opt systempaths=unconfined \
  --platform linux/amd64 \
  -e ARCH_SNAPSHOT="${arch_inputs[1]}" -e ARCH_CORE_SHA256="${arch_inputs[2]}" \
  -e ARCH_EXTRA_SHA256="${arch_inputs[3]}" \
  -v "$PWD:/workspace:ro" "${arch_inputs[0]}" bash -ceu '
  # The pinned database hashes fix every package version and archive checksum.
  # Never refresh these databases from a mutable mirror during installation.
  printf "Server = https://archive.archlinux.org/repos/%s/\$repo/os/\$arch\n" "$ARCH_SNAPSHOT" > /etc/pacman.d/mirrorlist
  mkdir -p /var/lib/pacman/sync
  rm -f /var/lib/pacman/sync/core.db.sig /var/lib/pacman/sync/extra.db.sig
  for repository in core extra; do
    curl --fail --location --output "/var/lib/pacman/sync/$repository.db" \
      "https://archive.archlinux.org/repos/$ARCH_SNAPSHOT/$repository/os/x86_64/$repository.db"
  done
  printf "%s  /var/lib/pacman/sync/core.db\n%s  /var/lib/pacman/sync/extra.db\n" \
    "$ARCH_CORE_SHA256" "$ARCH_EXTRA_SHA256" | sha256sum --check --strict
  pacman -Su --noconfirm --needed python git bubblewrap
  useradd --create-home builder
  install -m755 /workspace/target/debug/haps /usr/local/bin/haps
  su builder -c "HAPS_TEST_BINARY=/usr/local/bin/haps python /workspace/tests/makepkg_test.py"
'
