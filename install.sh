#!/bin/sh
# synthlite installer.
#
# Downloads the prebuilt static binary for this machine from GitHub Releases,
# verifies it against the release SHA256SUMS, and installs it without sudo.
#
#   curl -fsSL https://raw.githubusercontent.com/scogo-ai/synthlite/main/install.sh | sh
#
# Environment:
#   SYNTHLITE_VERSION        release tag to install, e.g. v0.4.0 (default: latest)
#   SYNTHLITE_INSTALL_DIR    where to put the binary (default: $HOME/.local/bin)
#   SYNTHLITE_DOWNLOAD_BASE  URL of a directory holding the release assets and
#                            SHA256SUMS; overrides the GitHub URL (mirrors, tests)
#
# Supported: Linux x86_64 and aarch64 (static musl builds, any distro) and
# macOS arm64 and x86_64. Elsewhere, build from source with cargo.

set -eu

REPO="scogo-ai/synthlite"

say() {
    printf 'synthlite-install: %s\n' "$*"
}

die() {
    printf 'synthlite-install: error: %s\n' "$*" >&2
    exit 1
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# Print the release asset name for this OS and CPU.
detect_asset() {
    os=$(uname -s)
    arch=$(uname -m)

    case "$os" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) die "no prebuilt binary for $os; build from source: cargo install --git https://github.com/$REPO --locked" ;;
    esac

    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "no prebuilt binary for $arch; build from source: cargo install --git https://github.com/$REPO --locked" ;;
    esac

    if [ "$os" = macos ]; then
        # A shell running under Rosetta reports x86_64 on Apple silicon;
        # install the native build instead.
        if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
            arch=aarch64
        fi
        if [ "$arch" = aarch64 ]; then
            arch=arm64
        fi
    fi

    printf 'synthlite-%s-%s\n' "$os" "$arch"
}

# Print the directory URL that holds the assets and SHA256SUMS.
download_base() {
    if [ -n "${SYNTHLITE_DOWNLOAD_BASE:-}" ]; then
        printf '%s\n' "${SYNTHLITE_DOWNLOAD_BASE%/}"
        return
    fi
    version=${SYNTHLITE_VERSION:-}
    if [ -z "$version" ] || [ "$version" = latest ]; then
        printf 'https://github.com/%s/releases/latest/download\n' "$REPO"
        return
    fi
    case "$version" in
        v*) ;;
        *) version="v$version" ;;
    esac
    printf 'https://github.com/%s/releases/download/%s\n' "$REPO" "$version"
}

# download URL DEST
download() {
    if have curl; then
        curl --fail --silent --show-error --location --proto-redir '=https' \
            --retry 3 --output "$2" "$1" || die "download failed: $1"
    elif have wget; then
        wget --quiet --output-document="$2" "$1" || die "download failed: $1"
    else
        die "curl or wget is required"
    fi
}

# Print the lowercase SHA-256 of FILE.
sha256_of() {
    if have sha256sum; then
        sum=$(sha256sum "$1")
    elif have shasum; then
        sum=$(shasum -a 256 "$1")
    else
        die "sha256sum or shasum is required to verify the download"
    fi
    printf '%s\n' "${sum%% *}" | tr 'ABCDEF' 'abcdef'
}

main() {
    asset=$(detect_asset)
    base=$(download_base)

    if [ -n "${SYNTHLITE_INSTALL_DIR:-}" ]; then
        install_dir=$SYNTHLITE_INSTALL_DIR
    else
        [ -n "${HOME:-}" ] || die "HOME is not set; set SYNTHLITE_INSTALL_DIR"
        install_dir="$HOME/.local/bin"
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t synthlite) || die "cannot create a temporary directory"
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    say "downloading $asset from $base"
    download "$base/$asset" "$tmp/$asset"
    download "$base/SHA256SUMS" "$tmp/SHA256SUMS"

    expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' "$tmp/SHA256SUMS" | tr 'ABCDEF' 'abcdef')
    [ -n "$expected" ] || die "SHA256SUMS has no entry for $asset"
    actual=$(sha256_of "$tmp/$asset")
    if [ "$actual" != "$expected" ]; then
        die "checksum mismatch for $asset (expected $expected, got $actual); nothing was installed"
    fi
    say "checksum verified ($actual)"

    mkdir -p "$install_dir" || die "cannot create $install_dir"
    chmod 755 "$tmp/$asset"
    "$tmp/$asset" --version >/dev/null 2>&1 || die "the downloaded $asset does not run on this machine; nothing was installed"
    # Copy next to the target, then rename, so a running synthlite is never
    # overwritten in place and a failed copy leaves the old binary intact.
    cp "$tmp/$asset" "$install_dir/.synthlite.new.$$" || die "cannot write to $install_dir"
    mv -f "$install_dir/.synthlite.new.$$" "$install_dir/synthlite" || {
        rm -f "$install_dir/.synthlite.new.$$"
        die "cannot write to $install_dir"
    }

    version=$("$install_dir/synthlite" --version 2>/dev/null) || die "installed $install_dir/synthlite but it does not run on this machine"
    say "installed $version to $install_dir/synthlite"

    case ":${PATH:-}:" in
        *":$install_dir:"*) ;;
        *)
            say "$install_dir is not on your PATH; add it with:"
            # shellcheck disable=SC2016 # print $PATH literally for the user's shell profile
            printf '\n    export PATH="%s:$PATH"\n\n' "$install_dir"
            ;;
    esac
}

main "$@"
