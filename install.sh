#!/usr/bin/env bash
# Belt installer for macOS and Linux
# Usage: curl -sSf https://raw.githubusercontent.com/kys0213/belt/main/install.sh | bash
#        curl -sSf ... | bash -s -- --yes
#
# Options:
#   --yes             - Skip interactive confirmations
#   --help            - Show this help message
#
# Environment variables:
#   BELT_VERSION      - Version to install (default: latest)
#   BELT_INSTALL_DIR  - Installation directory (default: ~/.belt/bin)

set -eu

# Exit codes
readonly EXIT_SUCCESS=0
readonly EXIT_GENERAL=1
readonly EXIT_MISSING_CMD=2
readonly EXIT_UNSUPPORTED_PLATFORM=3
readonly EXIT_NETWORK=4
readonly EXIT_DOWNLOAD=5
readonly EXIT_EXTRACT=6

REPO="kys0213/belt"
GITHUB_API="https://api.github.com"
GITHUB_RELEASE="https://github.com/${REPO}/releases"
DOWNLOADER=""
_tmpdir=""
YES_FLAG=false

# --- Helpers ---

say() {
    printf 'belt-installer: %s\n' "$*" >&2
}

err() {
    local _code="${2:-$EXIT_GENERAL}"
    say "ERROR: $1" >&2
    exit "$_code"
}

need_cmd() {
    if ! command -v "$1" > /dev/null 2>&1; then
        err "need '$1' (command not found). Please install '$1' and try again." "$EXIT_MISSING_CMD"
    fi
}

confirm() {
    if [ "$YES_FLAG" = true ]; then
        return 0
    fi

    # If stdin is not a terminal (piped install), skip confirmation
    if [ ! -t 0 ]; then
        return 0
    fi

    local _prompt="$1"
    printf '%s [y/N] ' "$_prompt" >&2
    local _answer
    read -r _answer
    case "$_answer" in
        [yY] | [yY][eE][sS]) return 0 ;;
        *) return 1 ;;
    esac
}

usage() {
    cat >&2 <<'USAGE'
Belt installer for macOS and Linux

Usage:
    curl -sSf https://raw.githubusercontent.com/kys0213/belt/main/install.sh | bash
    curl -sSf ... | bash -s -- --yes

Options:
    --yes       Skip interactive confirmations
    --help      Show this help message

Environment variables:
    BELT_VERSION      Version to install (default: latest)
    BELT_INSTALL_DIR  Installation directory (default: ~/.belt/bin)
USAGE
    exit "$EXIT_SUCCESS"
}

# --- Parse arguments ---

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --yes | -y)
                YES_FLAG=true
                shift
                ;;
            --help | -h)
                usage
                ;;
            *)
                err "unknown option: $1. Run with --help for usage." "$EXIT_GENERAL"
                ;;
        esac
    done
}

# --- Detect download command ---

detect_downloader() {
    if command -v curl > /dev/null 2>&1; then
        DOWNLOADER="curl"
    elif command -v wget > /dev/null 2>&1; then
        DOWNLOADER="wget"
    else
        err "need 'curl' or 'wget' to download files. Install one of them and try again." "$EXIT_MISSING_CMD"
    fi
}

# Download a URL to a file
download() {
    local _url="$1"
    local _output="$2"

    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsSL --retry 3 "$_url" -o "$_output" || err "failed to download $_url. Check your network connection and that the release exists at: $_url" "$EXIT_DOWNLOAD"
    elif [ "$DOWNLOADER" = "wget" ]; then
        wget -q --tries=3 "$_url" -O "$_output" || err "failed to download $_url. Check your network connection and that the release exists at: $_url" "$EXIT_DOWNLOAD"
    fi
}

# Download URL and output to stdout
download_to_stdout() {
    local _url="$1"

    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsSL --retry 3 "$_url" || err "failed to download $_url. Check your network connection and try again." "$EXIT_DOWNLOAD"
    elif [ "$DOWNLOADER" = "wget" ]; then
        wget -q --tries=3 "$_url" -O - || err "failed to download $_url. Check your network connection and try again." "$EXIT_DOWNLOAD"
    fi
}

# --- Network check ---

check_network() {
    say "checking network connectivity..."
    if [ "$DOWNLOADER" = "curl" ]; then
        if ! curl -fsSL --max-time 10 "https://github.com" > /dev/null 2>&1; then
            err "cannot reach github.com. Please check your internet connection or proxy settings." "$EXIT_NETWORK"
        fi
    elif [ "$DOWNLOADER" = "wget" ]; then
        if ! wget -q --timeout=10 --spider "https://github.com" 2>/dev/null; then
            err "cannot reach github.com. Please check your internet connection or proxy settings." "$EXIT_NETWORK"
        fi
    fi
}

# --- Detect platform ---

detect_os() {
    local _os
    _os="$(uname -s)"
    case "$_os" in
        Linux)  echo "linux" ;;
        Darwin) echo "darwin" ;;
        MINGW* | MSYS* | CYGWIN*)
            err "Windows is not supported. Please use WSL (Windows Subsystem for Linux) instead." "$EXIT_UNSUPPORTED_PLATFORM"
            ;;
        *)
            err "unsupported OS: $_os. Belt supports Linux and macOS only." "$EXIT_UNSUPPORTED_PLATFORM"
            ;;
    esac
}

detect_arch() {
    local _arch
    _arch="$(uname -m)"
    case "$_arch" in
        x86_64 | amd64)    echo "x86_64" ;;
        aarch64 | arm64)   echo "aarch64" ;;
        i386 | i686)
            err "32-bit x86 is not supported. Belt requires a 64-bit system." "$EXIT_UNSUPPORTED_PLATFORM"
            ;;
        *)
            err "unsupported architecture: $_arch. Belt supports x86_64 and aarch64/arm64 only." "$EXIT_UNSUPPORTED_PLATFORM"
            ;;
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
        *)              err "unsupported platform: ${_os} ${_arch}" "$EXIT_UNSUPPORTED_PLATFORM" ;;
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
    _response="$(download_to_stdout "$_url")" || err "failed to fetch latest release info. You can set BELT_VERSION manually, e.g.: BELT_VERSION=v0.1.0 bash install.sh" "$EXIT_DOWNLOAD"

    # Extract tag_name without jq dependency
    local _version
    _version="$(printf '%s' "$_response" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"

    if [ -z "$_version" ]; then
        err "could not determine latest version from GitHub API. Specify a version manually: BELT_VERSION=v0.1.0 bash install.sh" "$EXIT_DOWNLOAD"
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
            elif [ -f "$HOME/.zprofile" ]; then
                echo "$HOME/.zprofile"
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
        fish)
            echo "$HOME/.config/fish/config.fish"
            ;;
        *)
            echo "$HOME/.profile"
            ;;
    esac
}

# --- Main ---

main() {
    parse_args "$@"

    need_cmd uname
    need_cmd tar
    detect_downloader

    local _os _arch _triple _version _install_dir _asset _url

    _os="$(detect_os)"
    _arch="$(detect_arch)"
    _triple="$(target_triple "$_os" "$_arch")"

    say "detected platform: ${_os} ${_arch} (${_triple})"

    check_network

    _version="$(resolve_version)"

    # Normalize version prefix
    case "$_version" in
        v*) ;;
        *)  _version="v${_version}" ;;
    esac

    # Validate version format
    if ! printf '%s' "$_version" | grep -qE '^v[0-9]+\.[0-9]+'; then
        err "invalid version format: $_version (expected vX.Y.Z)" "$EXIT_GENERAL"
    fi

    _install_dir="${BELT_INSTALL_DIR:-$HOME/.belt/bin}"
    _asset="belt-${_triple}.tar.gz"
    _url="${GITHUB_RELEASE}/download/${_version}/${_asset}"

    say "will install belt ${_version} to ${_install_dir}"

    if ! confirm "Proceed with installation?"; then
        say "installation cancelled by user"
        exit "$EXIT_SUCCESS"
    fi

    # Create install directory
    mkdir -p "$_install_dir" || err "failed to create install directory: $_install_dir. Check directory permissions." "$EXIT_GENERAL"

    # Download to temp directory
    _tmpdir="$(mktemp -d)" || err "failed to create temp directory" "$EXIT_GENERAL"
    trap 'rm -rf "$_tmpdir"' EXIT

    say "downloading ${_url}..."
    download "$_url" "${_tmpdir}/${_asset}"

    # Extract
    say "extracting to ${_install_dir}..."
    tar -xzf "${_tmpdir}/${_asset}" -C "$_tmpdir" || err "failed to extract archive. The download may be corrupted; try again." "$EXIT_EXTRACT"

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
            err "could not find 'belt' binary in downloaded archive. The release asset may be malformed; please report this issue at https://github.com/${REPO}/issues" "$EXIT_EXTRACT"
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
            local _shell_name
            _shell_name="$(basename "${SHELL:-/bin/sh}")"
            local _path_line

            if [ "$_shell_name" = "fish" ]; then
                _path_line="set -gx PATH ${_install_dir} \$PATH"
            else
                _path_line="export PATH=\"${_install_dir}:\$PATH\""
            fi

            if [ "$YES_FLAG" = true ]; then
                # Auto-add to profile when --yes is specified
                # Idempotency guard: skip if the exact PATH line already exists
                if [ -f "$_profile" ] && grep -qF "$_path_line" "$_profile"; then
                    say "PATH entry already exists in ${_profile}, skipping"
                else
                    # Create profile file if it doesn't exist
                    if [ "$_shell_name" = "fish" ]; then
                        mkdir -p "$(dirname "$_profile")"
                    fi
                    echo "$_path_line" >> "$_profile"
                    say "added belt to PATH in ${_profile}"
                    say "restart your shell or run: source ${_profile}"
                fi
            else
                echo ""
                say "add belt to your PATH by running:"
                echo ""
                echo "    echo '${_path_line}' >> ${_profile} && source ${_profile}"
                echo ""
            fi
            ;;
    esac

    say "run 'belt --version' to verify the installation"
    exit "$EXIT_SUCCESS"
}

main "$@"
