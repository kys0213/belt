#!/usr/bin/env bash
# Belt installer for macOS and Linux
# Usage: curl -sSf https://raw.githubusercontent.com/kys0213/belt/main/install.sh | bash
#
# Environment variables:
#   BELT_VERSION      - Version to install (default: latest)
#   BELT_INSTALL_DIR  - Installation directory (default: ~/.belt/bin)

set -eu

REPO="kys0213/belt"
GITHUB_API="https://api.github.com"
GITHUB_RELEASE="https://github.com/${REPO}/releases"
DOWNLOADER=""
_tmpdir=""

# --- Helpers ---

say() {
    printf 'belt-installer: %s\n' "$*"
}

err() {
    say "ERROR: $*" >&2
    exit 1
}

need_cmd() {
    if ! command -v "$1" > /dev/null 2>&1; then
        err "need '$1' (command not found)"
    fi
}

# --- Detect download command ---

detect_downloader() {
    if command -v curl > /dev/null 2>&1; then
        DOWNLOADER="curl"
    elif command -v wget > /dev/null 2>&1; then
        DOWNLOADER="wget"
    else
        err "need 'curl' or 'wget' to download files"
    fi
}

# Download a URL to a file
download() {
    local _url="$1"
    local _output="$2"

    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsSL --retry 3 "$_url" -o "$_output" || err "failed to download $_url"
    elif [ "$DOWNLOADER" = "wget" ]; then
        wget -q --tries=3 "$_url" -O "$_output" || err "failed to download $_url"
    fi
}

# Download URL and output to stdout
download_to_stdout() {
    local _url="$1"

    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsSL --retry 3 "$_url" || err "failed to download $_url"
    elif [ "$DOWNLOADER" = "wget" ]; then
        wget -q --tries=3 "$_url" -O - || err "failed to download $_url"
    fi
}

# --- Detect platform ---

detect_os() {
    local _os
    _os="$(uname -s)"
    case "$_os" in
        Linux)  echo "linux" ;;
        Darwin) echo "darwin" ;;
        *)      err "unsupported OS: $_os (supported: Linux, macOS)" ;;
    esac
}

detect_arch() {
    local _arch
    _arch="$(uname -m)"
    case "$_arch" in
        x86_64 | amd64)    echo "x86_64" ;;
        aarch64 | arm64)   echo "aarch64" ;;
        *)                 err "unsupported architecture: $_arch (supported: x86_64, aarch64/arm64)" ;;
    esac
}

# Map OS + arch to Rust target triple
target_triple() {
    local _os="$1"
    local _arch="$2"

    case "${_os}-${_arch}" in
        linux-x86_64)   echo "x86_64-unknown-linux-gnu" ;;
        linux-aarch64)  echo "aarch64-unknown-linux-gnu" ;;
        darwin-x86_64)  echo "x86_64-apple-darwin" ;;
        darwin-aarch64) echo "aarch64-apple-darwin" ;;
        *)              err "unsupported platform: ${_os} ${_arch}" ;;
    esac
}

# --- Resolve version ---

resolve_version() {
    if [ -n "${BELT_VERSION:-}" ]; then
        echo "$BELT_VERSION"
        return
    fi

    say "fetching latest release version..."
    local _url="${GITHUB_API}/repos/${REPO}/releases/latest"
    local _response
    _response="$(download_to_stdout "$_url")" || err "failed to fetch latest release info"

    # Extract tag_name without jq dependency
    local _version
    _version="$(printf '%s' "$_response" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"

    if [ -z "$_version" ]; then
        err "could not determine latest version from GitHub API"
    fi

    echo "$_version"
}

# --- Detect shell profile ---

detect_profile() {
    local _shell
    _shell="$(basename "${SHELL:-/bin/sh}")"

    case "$_shell" in
        zsh)
            if [ -f "$HOME/.zshrc" ]; then
                echo "$HOME/.zshrc"
            else
                echo "$HOME/.profile"
            fi
            ;;
        bash)
            if [ -f "$HOME/.bashrc" ]; then
                echo "$HOME/.bashrc"
            elif [ -f "$HOME/.bash_profile" ]; then
                echo "$HOME/.bash_profile"
            else
                echo "$HOME/.profile"
            fi
            ;;
        *)
            echo "$HOME/.profile"
            ;;
    esac
}

# --- Main ---

main() {
    need_cmd uname
    need_cmd tar
    detect_downloader

    local _os _arch _triple _version _install_dir _asset _url

    _os="$(detect_os)"
    _arch="$(detect_arch)"
    _triple="$(target_triple "$_os" "$_arch")"

    say "detected platform: ${_os} ${_arch} (${_triple})"

    _version="$(resolve_version)"

    # Normalize version prefix
    case "$_version" in
        v*) ;;
        *)  _version="v${_version}" ;;
    esac

    # Validate version format
    if ! printf '%s' "$_version" | grep -qE '^v[0-9]+\.[0-9]+'; then
        err "invalid version format: $_version (expected vX.Y.Z)"
    fi

    say "installing belt ${_version}"

    _install_dir="${BELT_INSTALL_DIR:-$HOME/.belt/bin}"
    _asset="belt-${_triple}.tar.gz"
    _url="${GITHUB_RELEASE}/download/${_version}/${_asset}"

    # Create install directory
    mkdir -p "$_install_dir" || err "failed to create install directory: $_install_dir"

    # Download to temp directory
    _tmpdir="$(mktemp -d)" || err "failed to create temp directory"
    trap 'rm -rf "$_tmpdir"' EXIT

    say "downloading ${_url}..."
    download "$_url" "${_tmpdir}/${_asset}"

    # Extract
    say "extracting to ${_install_dir}..."
    tar -xzf "${_tmpdir}/${_asset}" -C "$_tmpdir" || err "failed to extract archive"

    # Find and install the belt binary
    if [ -f "${_tmpdir}/belt" ]; then
        mv "${_tmpdir}/belt" "${_install_dir}/belt"
    elif [ -f "${_tmpdir}/belt-${_triple}/belt" ]; then
        mv "${_tmpdir}/belt-${_triple}/belt" "${_install_dir}/belt"
    else
        # Try to find belt binary in extracted contents
        local _found
        _found="$(find "$_tmpdir" -maxdepth 2 -name "belt" -type f | head -1)"
        if [ -n "$_found" ]; then
            mv "$_found" "${_install_dir}/belt"
        else
            err "could not find 'belt' binary in downloaded archive"
        fi
    fi

    chmod +x "${_install_dir}/belt"

    say "belt ${_version} installed to ${_install_dir}/belt"

    # PATH guidance
    case ":${PATH}:" in
        *":${_install_dir}:"*)
            say "belt is already in your PATH"
            ;;
        *)
            local _profile
            _profile="$(detect_profile)"
            local _path_line="export PATH=\"${_install_dir}:\$PATH\""

            echo ""
            say "add belt to your PATH by running:"
            echo ""
            echo "    echo '${_path_line}' >> ${_profile} && source ${_profile}"
            echo ""
            ;;
    esac

    say "run 'belt --version' to verify the installation"
}

main "$@"
