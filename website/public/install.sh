#!/bin/sh
# Haps binary installer. Keep execution at the end for curl | sh.
set -eu

main() {
    version=${HAPS_VERSION:-}
    bin_dir=${HAPS_INSTALL_DIR:-"$HOME/.local/bin"}
    base=${HAPS_RELEASE_BASE_URL:-https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps}
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version) [ "$#" -ge 2 ] || die 'Missing version'; version=$2; shift 2 ;;
            --bin-dir) [ "$#" -ge 2 ] || die 'Missing directory'; bin_dir=$2; shift 2 ;;
            --help|-h) printf 'Usage: install.sh [--version v0.1.3] [--bin-dir DIR]\n'; return ;;
            *) die "Unknown option: $1" ;;
        esac
    done
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
    asset="haps-$version-$arch-$platform.tar.gz"
    release="${base%/}/$version/assets"
    printf 'Downloading Haps %s for %s…\n' "${version#v}" "$arch-$platform"
    fetch "$release/SHA256SUMS" "$tmp/checksums"
    fetch "$release/$asset" "$tmp/archive.tar.gz"
    expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/checksums")
    [ "${#expected}" -eq 64 ] && printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]+$' || die 'Missing or invalid release checksum'
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
    [ -n "$bin_dir" ] && [ ! -d "$bin_dir/haps" ] || die 'Invalid installation directory'
    [ ! -L "$bin_dir/.haps" ] || die 'Managed bundle directory must not be a symlink'
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
    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *) printf 'Add %s to your PATH to use haps.\n' "$bin_dir" ;;
    esac
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
