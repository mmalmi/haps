#!/bin/sh
# Haps binary installer. Keep execution at the end for curl | sh.
set -eu

main() {
    version=${HAPS_VERSION:-}
    bin_dir=${HAPS_INSTALL_DIR:-"$HOME/.local/bin"}
    explicit_dir=${HAPS_INSTALL_DIR:+true}
    force=false
    base=${HAPS_RELEASE_BASE_URL:-https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps}
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version) [ "$#" -ge 2 ] || die 'Missing version'; version=$2; shift 2 ;;
            --bin-dir) [ "$#" -ge 2 ] || die 'Missing directory'; bin_dir=$2; explicit_dir=true; shift 2 ;;
            --force) force=true; shift ;;
            --help|-h) printf 'Usage: install.sh [--version vX.Y.Z] [--bin-dir DIR] [--force]\n\nReuse an existing script-managed installation, or install to ~/.local/bin.\nOther installations are preserved unless --force explicitly replaces the destination.\nUse --force to reinstall the same version.\n'; return ;;
            *) die "Unknown option: $1" ;;
        esac
    done
    if [ -n "$version" ]; then
        printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || die 'Invalid release version'
    fi
    case "$base" in https://*) ;; *) die 'Release URL must use HTTPS' ;; esac
    case "$(uname -s)" in
        Darwin) platform=apple-darwin ;;
        Linux) platform=unknown-linux-gnu ;;
        *) die 'Use the Windows zip from the release downloads on Windows' ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) arch=aarch64 ;;
        x86_64|amd64) arch=x86_64 ;;
        *) die 'Unsupported CPU architecture' ;;
    esac
    for cmd in curl tar awk grep mktemp sort readlink; do
        command -v "$cmd" >/dev/null 2>&1 || die "Required command is missing: $cmd"
    done
    pinned=${version:+true}
    existing=$(command -v haps || true)
    if [ -n "$existing" ]; then
        found_version=$(installed_version "$existing")
        printf 'Found Haps %s at %s\n' "${found_version:-'(version unavailable)'}" "$existing"
        if [ -z "$explicit_dir" ]; then
            if managed_install "$existing"; then
                bin_dir=${existing%/*}
            else
                update_advice "$existing"
                printf 'No changes made. To install a separate copy, rerun with --bin-dir DIR.\n'
                return
            fi
        fi
    fi
    [ -n "$bin_dir" ] && [ ! -d "$bin_dir/haps" ] || die 'Invalid installation directory'
    case "$bin_dir" in /*) ;; *) bin_dir="$PWD/$bin_dir" ;; esac
    if [ -d "$bin_dir" ]; then bin_dir=$(CDPATH='' cd -- "$bin_dir" && pwd -P); fi
    managed=false
    installed=
    if [ -e "$bin_dir/haps" ] || [ -L "$bin_dir/haps" ]; then
        installed=$(installed_version "$bin_dir/haps")
        if managed_install "$bin_dir/haps"; then
            managed=true
        elif [ "$force" != true ]; then
            die "Existing $bin_dir/haps is not managed by this installer. Use its original package manager, choose another --bin-dir, or use --force to replace this command."
        else
            printf 'Replacing the existing command at %s/haps (--force).\n' "$bin_dir"
        fi
    fi
    [ ! -L "$bin_dir/.haps" ] || die 'Managed bundle directory must not be a symlink'
    if command -v sha256sum >/dev/null 2>&1; then
        hash_command=sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        hash_command=shasum
    else
        die 'Install sha256sum or shasum first'
    fi
    tmp=$(mktemp -d)
    stage=
    trap 'cleanup' 0
    trap 'exit 1' 1 2 15
    if [ -z "$version" ]; then
        fetch "${base%/}/latest/version.txt" "$tmp/version"
        version=$(cat "$tmp/version")
    fi
    printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || die 'Invalid release version'
    if [ "$managed" = true ] && [ -n "$installed" ]; then
        if [ "$installed" = "${version#v}" ] && [ "$force" != true ]; then
            printf 'Haps %s is already installed at %s/haps. Use --force to reinstall.\n' "$installed" "$bin_dir"
            path_notice
            return
        elif newer_than "$installed" "${version#v}"; then
            if [ -z "$pinned" ] && [ "$force" != true ]; then
                printf 'Installed Haps %s is newer than the latest release %s; keeping it.\n' "$installed" "${version#v}"
                path_notice
                return
            fi
            printf 'Downgrading Haps %s to %s at %s/haps.\n' "$installed" "${version#v}" "$bin_dir"
        elif [ "$installed" = "${version#v}" ]; then
            printf 'Reinstalling Haps %s at %s/haps.\n' "$installed" "$bin_dir"
        else
            printf 'Updating Haps %s to %s at %s/haps.\n' "$installed" "${version#v}" "$bin_dir"
        fi
    else
        printf 'Installing Haps %s to %s/haps.\n' "${version#v}" "$bin_dir"
    fi
    asset="haps-$version-$arch-$platform.tar.gz"
    release="${base%/}/$version/assets"
    printf 'Downloading Haps %s for %s…\n' "${version#v}" "$arch-$platform"
    fetch "$release/SHA256SUMS" "$tmp/checksums"
    fetch "$release/$asset" "$tmp/archive.tar.gz"
    expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/checksums")
    if [ "${#expected}" -ne 64 ] || ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]+$'; then
        die 'Missing or invalid release checksum'
    fi
    if [ "$hash_command" = sha256sum ]; then
        actual=$(sha256sum "$tmp/archive.tar.gz" | awk '{print $1}')
    else
        actual=$(shasum -a 256 "$tmp/archive.tar.gz" | awk '{print $1}')
    fi
    [ "$actual" = "$expected" ] || die 'Checksum mismatch; existing Haps was not changed'
    entries=$(tar -tzf "$tmp/archive.tar.gz" | LC_ALL=C sort)
    expected_entries=$(printf '%s\n' bundle.json haps libexec/git-remote-htree libexec/hashtree-LICENSE libexec/htree)
    bundled=false
    if [ "$entries" = "$expected_entries" ]; then
        bundled=true
    elif [ "$entries" != haps ]; then
        die 'Unexpected archive contents'
    fi
    mkdir -p "$tmp/payload/libexec"
    # Extract bytes into new regular files; never restore archive link entries.
    for entry in $entries; do
        tar -xOzf "$tmp/archive.tar.gz" "$entry" > "$tmp/payload/$entry"
    done
    cp "$tmp/payload/haps" "$tmp/haps"
    [ -f "$tmp/haps" ] && [ ! -L "$tmp/haps" ] || die 'Archive does not contain a regular binary'
    chmod 755 "$tmp/haps"
    reported=$("$tmp/haps" --version) || die 'This binary cannot run here; existing Haps was not changed'
    [ "$reported" = "haps ${version#v}" ] || die 'Release version mismatch'
    if [ "$bundled" = true ]; then
        chmod 755 "$tmp/payload/libexec/htree" "$tmp/payload/libexec/git-remote-htree"
        "$tmp/payload/libexec/htree" --version >/dev/null || die 'Bundled htree cannot run here; existing installation was not changed'
        helper_usage=$("$tmp/payload/libexec/git-remote-htree" 2>&1 || true)
        printf '%s\n' "$helper_usage" | grep -q 'Usage: git-remote-htree' || die 'Bundled Git helper cannot run here'
    fi
    mkdir -p "$bin_dir/.haps"
    stage=$(mktemp -d "$bin_dir/.haps/$version.XXXXXXXX")
    cp -R "$tmp/payload/." "$stage/"
    chmod 755 "$stage/haps"
    bundle_name=${stage##*/}
    link_stage=$(mktemp "$bin_dir/.haps-link.XXXXXXXX")
    rm -f "$link_stage"
    ln -s ".haps/$bundle_name/haps" "$link_stage"
    mv -f "$link_stage" "$bin_dir/haps"
    link_stage=
    # Keep the previous bundle available for recovery. Only replace helper links
    # we own; separate Cargo, Homebrew, or system installations stay in place.
    stage=
    if [ "$bundled" = true ]; then
        for tool in htree git-remote-htree; do
            managed=false
            case "$(readlink "$bin_dir/$tool" 2>/dev/null || true)" in
                .haps/*/libexec/"$tool") managed=true ;;
            esac
            if [ "$managed" = true ] || { [ ! -e "$bin_dir/$tool" ] && [ ! -L "$bin_dir/$tool" ] && ! command -v "$tool" >/dev/null 2>&1; }; then
                link_stage=$(mktemp "$bin_dir/.haps-link.XXXXXXXX")
                rm -f "$link_stage"
                ln -s ".haps/$bundle_name/libexec/$tool" "$link_stage"
                mv -f "$link_stage" "$bin_dir/$tool"
                link_stage=
            fi
        done
        printf 'Included htree and git-remote-htree; existing tools were preserved.\n'
    fi
    printf 'Installed %s to %s/haps\n' "$reported" "$bin_dir"
    path_notice
}

managed_install() {
    owned_link=$(readlink "$1" 2>/dev/null || true)
    case "$owned_link" in
        .haps/*/haps)
            owned_bundle=${owned_link#.haps/}; owned_bundle=${owned_bundle%/haps}
            case "$owned_bundle" in ''|.|..|*/*) return 1 ;; esac
            [ ! -L "${1%/*}/.haps" ] && [ ! -L "${1%/*}/.haps/$owned_bundle" ] ;;
        *) return 1 ;;
    esac
}
installed_version() {
    # Report only a version, never arbitrary output from an existing command.
    version_output=$("$1" --version 2>/dev/null) || return 0
    printf '%s\n' "$version_output" | awk '$0 ~ /^haps [0-9]+\.[0-9]+\.[0-9]+$/ {print $2; exit}'
}
newer_than() {
    awk -v a="$1" -v b="$2" 'BEGIN {split(a,x,"."); split(b,y,"."); for(i=1;i<=3;i++) {if(x[i]+0>y[i]+0) exit 0; if(x[i]+0<y[i]+0) exit 1} exit 1}'
}
update_advice() {
    if [ -f "$1" ] && awk 'NR == 2 {exit ($0 != "# Managed by Haps")} END {if (NR < 2) exit 1}' "$1"; then
        printf 'This copy is managed by Haps. Update it with: haps update haps\n'
        return
    fi
    case "$1" in
        "${CARGO_HOME:-$HOME/.cargo}/bin/haps") printf 'This copy is in Cargo\047s bin directory. Update it with: cargo install haps --locked\n' ;;
        */Cellar/*|/opt/homebrew/bin/haps|/home/linuxbrew/.linuxbrew/bin/haps) printf 'This appears to be a Homebrew installation. Update it with: brew upgrade haps\n' ;;
        *) printf 'Keep this copy updated through its original installation method.\n' ;;
    esac
}
path_notice() {
    hash -r 2>/dev/null || true
    active=$(command -v haps || true)
    if [ -n "$active" ]; then
        active_dir=$(CDPATH='' cd -- "${active%/*}" 2>/dev/null && pwd -P) || active_dir=
        if [ "$active_dir/haps" = "$bin_dir/haps" ]; then return; fi
        printf 'PATH still selects %s. Put %s before that directory in PATH to use this installation.\n' "$active" "$bin_dir"
    else
        printf 'Add %s to your PATH to use haps.\n' "$bin_dir"
    fi
    printf 'Open a new terminal after changing PATH.\n'
}

die() { printf 'haps-install: %s\n' "$*" >&2; exit 1; }
fetch() {
    curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
        --tlsv1.2 --retry 2 --connect-timeout 10 --max-time 180 "$1" -o "$2"
}
cleanup() {
    [ -z "${link_stage:-}" ] || rm -f "$link_stage"
    [ -z "${stage:-}" ] || rm -rf "$stage"
    [ -z "${tmp:-}" ] || rm -rf "$tmp"
}

main "$@"
