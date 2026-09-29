#!/usr/bin/env bash
set -euo pipefail

# Colors (defined early for error_exit)
MUTED=$'\033[0;2m'
RED=$'\033[0;31m'
ORANGE=$'\033[38;5;214m'
GREEN=$'\033[0;32m'
NC=$'\033[0m'

# Error handler (defined early so it can be used during initialization)
error_exit() {
    local format="$1"
    shift || true

    local exit_code=1
    local last_arg=""
    local arg=""
    for arg in "$@"; do
        last_arg="$arg"
    done
    case "$last_arg" in
    '' | *[!0-9]*)
        ;;
    *)
        exit_code="$last_arg"
        set -- "${@:1:$(($# - 1))}"
        ;;
    esac

    if [[ $# -gt 0 ]]; then
        local msg
        printf -v msg "$format" "$@"
        printf "\r\033[K"
        printf '\033[?25h'
        printf "%b[✗]%b %b\n" "$RED" "$NC" "$msg" >&2
        exit "$exit_code"
    fi

    printf "\r\033[K"
    printf '\033[?25h'
    printf "%b[✗]%b %b\n" "$RED" "$NC" "$format" >&2
    exit "$exit_code"
}

# Cleanup on exit
BUILD_OUTPUT=""
DOWNLOAD_TEMP_DIR=""
SOURCE_TEMP_DIR=""
DOWNLOADED_B_PATH=""
DOWNLOADED_BX_PATH=""
cleanup() {
    printf '\033[?25h'  # Restore cursor
    [[ -n "$BUILD_OUTPUT" && -f "$BUILD_OUTPUT" ]] && rm -f "$BUILD_OUTPUT"
    [[ -n "$DOWNLOAD_TEMP_DIR" && -d "$DOWNLOAD_TEMP_DIR" ]] && rm -rf "$DOWNLOAD_TEMP_DIR"
    [[ -n "$SOURCE_TEMP_DIR" && -d "$SOURCE_TEMP_DIR" ]] && rm -rf "$SOURCE_TEMP_DIR"
    return 0
}
trap cleanup EXIT

BREW_REPO="https://github.com/i-nick/brew.git"
BREW_BIN="$HOME/.local/bin"
ZSHRC_BLOCK_START="# >>> brew installer >>>"
ZSHRC_BLOCK_END="# <<< brew installer <<<"

# brew only supports macOS on Apple Silicon.
if [[ "$(uname -s)" != "Darwin" ]]; then
    error_exit "brew only supports macOS on Apple Silicon. Detected OS: $(uname -s)"
fi
case "$(uname -m)" in
arm64 | aarch64) ;;
*)
    error_exit "brew only supports Apple Silicon (arm64) Macs. Detected architecture: $(uname -m)"
    ;;
esac

BREW_ROOT="/opt/brew"
: "${BREW_PREFIX:=$BREW_ROOT}"

export BREW_ROOT
export BREW_PREFIX

# Ensure system tools are used instead of brew-installed ones.
# A prior `b init` adds $BREW_PREFIX/bin to PATH, which can cause
# brew's curl/git (linked against brew's OpenSSL) to be used by
# this script. On some macOS versions that leads to dyld symbol errors.
# see https://github.com/i-nick/brew/issues/288
sanitized_path=""
IFS=':' read -ra _path_parts <<< "$PATH"
for _p in "${_path_parts[@]}"; do
    case "$_p" in
        "$BREW_PREFIX"/bin|"$BREW_ROOT"/bin) ;;
        *) sanitized_path="${sanitized_path:+$sanitized_path:}$_p" ;;
    esac
done
export PATH="$sanitized_path"
unset sanitized_path _path_parts _p

# Prevent running with sudo - the script handles its own privilege escalation
if [[ $EUID -eq 0 ]]; then
    error_exit "Do not run this script with sudo or as root. The installer will automatically request privileges when needed."
fi

no_modify_path=false
binary_paths=()

usage() {
    printf "%bbrew%b Installer\n" "$ORANGE" "$NC"
    printf "\n"
    printf "Usage: install.sh %b[options]%b\n" "$MUTED" "$NC"
    printf "\n"
    printf "Options:\n"
    printf "    -h, --help               %bDisplay this help message%b\n" "$MUTED" "$NC"
    printf "    -b, --binary <path>...   %bInstalls binaries (b, bx) to ~/.local/bin%b\n" "$MUTED" "$NC"
    printf "        --no-modify-path     %bDon't modify shell config files (.zshrc, .bashrc, etc.)%b\n" "$MUTED" "$NC"
    printf "\n"
    printf "Examples:%b\n" "$MUTED"
    printf "    ./install.sh --no-modify-path\n"
    printf "    ./install.sh -b /path/to/b\n"
    printf "    ./install.sh -b /path/to/b /path/to/bx%b\n" "$NC"
}

spinner() {
    local msg="$1"
    local pid="$2"
    local spin=$'|/-\\'
    local i=0
    local exit_code=0

    printf '\033[?25l'

    while kill -0 "$pid" 2>/dev/null; do
        i=$(((i + 1) % 4))
        printf "\r%b[%s]%b %b" "$ORANGE" "${spin:$i:1}" "$NC" "$msg"
        sleep 0.1
    done

    wait "$pid" 2>/dev/null && exit_code=0 || exit_code=$?

    printf "\r\033[K"
    printf '\033[?25h'

    return "$exit_code"
}

completed() {
    printf "%b[✓]%b %b\n" "$GREEN" "$NC" "$1"
}

warn() {
    printf "%b[!]%b %b\n" "$ORANGE" "$NC" "$1" >&2
}

check_command() {
    local cmd="$1"
    local install_hint="${2:-}"

    if ! command -v "$cmd" >/dev/null 2>&1; then
        local msg="Required command '$cmd' not found"
        if [[ -n "$install_hint" ]]; then
            msg="$msg. Hint: $install_hint"
        fi
        error_exit "$msg"
    fi
}

install_bin() {
    local target_dir="$1"
    shift
    local paths_to_install=("$@")

    if ! mkdir -p "$target_dir"; then
        error_exit "Failed to create directory: $target_dir"
    fi

    for binary_path in "${paths_to_install[@]}"; do
        if [[ ! -f "$binary_path" ]]; then
            error_exit "Binary not found at ${binary_path}"
        fi

        local binary_name
        binary_name=$(basename "$binary_path")

        if ! install -m755 "$binary_path" "$target_dir/$binary_name"; then
            error_exit "Failed to copy $binary_name to $target_dir"
        fi

        completed "Installed ${ORANGE}$binary_name${NC} to $target_dir"
    done
}

# Ensure $HOME/.local/bin is on PATH for future zsh sessions.
#
# `b init` also adds this directory to PATH, but it writes to .zshenv when
# that file exists, so .zshrc can be left without the entry. This adds a
# self-contained, idempotent block to .zshrc as well. The guard means PATH
# never gains a duplicate entry when both blocks run.
add_local_bin_to_zshrc() {
    local no_modify="$1"

    if [[ "$no_modify" == "true" ]]; then
        return 0
    fi

    # Only touch .zshrc for zsh users; other shells are handled by `b init`.
    if [[ "${SHELL:-}" != *zsh* ]]; then
        return 0
    fi

    local zshrc="${ZDOTDIR:-$HOME}/.zshrc"

    if [[ -f "$zshrc" ]] && grep -qF "$ZSHRC_BLOCK_START" "$zshrc"; then
        completed "PATH entry for ${ORANGE}~/.local/bin${NC} already present in $zshrc"
        return 0
    fi

    local block
    block=$(
        printf '\n%s\n' "$ZSHRC_BLOCK_START"
        printf 'case ":${PATH}:" in\n'
        printf '    *:"$HOME/.local/bin":*) ;;\n'
        printf '    *) export PATH="$HOME/.local/bin:$PATH" ;;\n'
        printf 'esac\n'
        printf '%s' "$ZSHRC_BLOCK_END"
    )

    # Write with a single simple command: bash only reports a failed
    # redirection through `!` reliably for simple commands, not for groups.
    # stderr is redirected first so bash's own "Permission denied" is
    # suppressed in favour of the warning below.
    if ! printf '%s\n' "$block" 2>/dev/null >> "$zshrc"; then
        warn "Could not write to $zshrc. Add this line to it manually:"
        warn '  export PATH="$HOME/.local/bin:$PATH"'
        return 0
    fi

    completed "Added ${ORANGE}~/.local/bin${NC} to PATH in $zshrc"
}

b_init() {
    local b_path="$1"
    local no_modify="$2"
    local init_args=()

    if [[ "$no_modify" == "true" ]]; then
        init_args+=("--no-modify-path")
    fi

    "$b_path" init ${init_args[@]+"${init_args[@]}"} >/dev/null 2>&1 || error_exit "Failed to initialize brew"
}

finalize_installation() {
    local no_modify="$1"

    # Verify the binary works
    if ! "$BREW_BIN/b" --version >/dev/null 2>&1; then
        error_exit "Installation succeeded but binary does not execute properly"
    fi

    # `brew` is an alias for `b`.
    if ! ln -sfn b "$BREW_BIN/brew"; then
        error_exit "Failed to create brew alias in $BREW_BIN"
    fi
    completed "Linked ${ORANGE}brew${NC} -> b in $BREW_BIN"

    # Add b to PATH for current session if not already present
    if [[ ":$PATH:" != *":$BREW_BIN:"* ]]; then
        export PATH="$BREW_BIN:$PATH"
    fi

    b_init "$BREW_BIN/b" "$no_modify"
    add_local_bin_to_zshrc "$no_modify"

    print_logo
    completed "Installation complete"
}

resolve_release_asset() {
    local binary_name="$1"
    echo "${binary_name}-darwin-arm64"
}

download_release_binary() {
    local asset_name="$1"
    local output_name="$2"
    local required="${3:-true}"
    local downloaded_path="$DOWNLOAD_TEMP_DIR/${output_name}"
    local download_url="https://github.com/i-nick/brew/releases/latest/download/${asset_name}"

    (
        curl -fsL --retry 3 --retry-delay 1 --connect-timeout 10 \
            "$download_url" \
            -o "$downloaded_path" \
            >/dev/null 2>&1
    ) &
    if ! spinner "Downloading ${ORANGE}${asset_name}${NC} from latest release" $!; then
        if [[ "$required" == "true" ]]; then
            return 1
        fi
        warn "Optional ${ORANGE}${asset_name}${NC} not found."
        return 0
    fi

    if ! chmod +x "$downloaded_path"; then
        if [[ "$required" == "true" ]]; then
            return 1
        fi
        warn "Failed to prepare optional asset ${asset_name}. Continuing without it."
        return 0
    fi

    if [[ "$output_name" == "b" ]]; then
        DOWNLOADED_B_PATH="$downloaded_path"
    elif [[ "$output_name" == "bx" ]]; then
        DOWNLOADED_BX_PATH="$downloaded_path"
    fi

    completed "Downloaded ${ORANGE}${asset_name}${NC} from GitHub Releases"
    return 0
}

try_release_install() {
    local b_asset bx_asset

    DOWNLOAD_TEMP_DIR=$(mktemp -d)
    DOWNLOADED_B_PATH=""
    DOWNLOADED_BX_PATH=""

    if ! b_asset=$(resolve_release_asset "b"); then
        warn "No prebuilt release binary for b on $(uname -s)/$(uname -m). Falling back to source build."
        return 1
    fi

    if ! download_release_binary "$b_asset" "b" "true"; then
        warn "Release binary download failed for ${b_asset}. Falling back to source build."
        return 1
    fi

    if bx_asset=$(resolve_release_asset "bx"); then
        download_release_binary "$bx_asset" "bx" "false"
    fi

    local binaries_to_install=("$DOWNLOADED_B_PATH")
    if [[ -n "$DOWNLOADED_BX_PATH" && -f "$DOWNLOADED_BX_PATH" ]]; then
        binaries_to_install+=("$DOWNLOADED_BX_PATH")
    fi

    install_bin "$BREW_BIN" "${binaries_to_install[@]}"
    finalize_installation "$no_modify_path"
    return 0
}

print_logo() {
    printf "\n"
    printf "%b▄▄▄▄  ▄▄▄▄  ▄▄▄▄▄ ▄▄   ▄▄%b\n" "$ORANGE" "$NC"
    printf "%b██▄██ ██▄█▄ ██▄▄  ██ ▄ ██%b\n" "$ORANGE" "$NC"
    printf "%b██▄█▀ ██ ██ ██▄▄▄  ▀█▀█▀ %b\n" "$ORANGE" "$NC"
    printf "\n"

    printf "%bStart installing %bPackages%b with %bbrew%b:\n\n" "$MUTED" "$NC" "$MUTED" "$ORANGE" "$NC"
    printf "  b install %bffmpeg%b    # Install a Package%b\n" "$ORANGE" "$MUTED" "$NC"
    printf "  bx %byetris%b           # Single-time Run%b\n\n" "$ORANGE" "$MUTED" "$NC"
    printf "%bFor more information visit %bhttps://brew.rs/docs\n\n" "$MUTED" "$NC"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    --no-modify-path)
        no_modify_path=true
        shift
        ;;
    -b | --binary)
        if [[ -n "${2:-}" ]]; then
            binary_paths+=("$2")
            shift 2
            if [[ -n "${1:-}" && "${1:0:1}" != "-" ]]; then
                binary_paths+=("$1")
                shift
            fi
        else
            error_exit "--binary requires a path argument"
        fi
        ;;
    *)
        error_exit "Unknown option '%s'" "$1"
        ;;
    esac
done

# Skip all if binary path is provided
if [[ ${#binary_paths[@]} -gt 0 ]]; then
    install_bin "$BREW_BIN" "${binary_paths[@]}"
    finalize_installation "$no_modify_path"
    exit 0
fi

# Check for required commands
check_command "curl" "Install curl and try again"
check_command "git" "Install the Xcode Command Line Tools with 'xcode-select --install'"
check_command "mkdir" "Your system should have mkdir installed by default"
check_command "cp" "Your system should have cp installed by default"
check_command "chmod" "Your system should have chmod installed by default"
check_command "uname" "Your system should have uname installed by default"

# Try latest prebuilt release first, then fall back to source build if needed.
if try_release_install; then
    exit 0
fi

# Check for Rust/Cargo
if ! command -v cargo >/dev/null 2>&1; then
    (
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    ) &
    if ! spinner "Installing ${ORANGE}Rust toolchain${NC}" $!; then
        error_exit "Failed to install Rust toolchain. Check your network connection and try again."
    fi
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    completed "${ORANGE}Rust toolchain${NC} installed"
fi

# Ensure cargo is available
if ! command -v cargo >/dev/null 2>&1; then
    error_exit "Cargo not found after installing Rust. Try restarting your terminal or running: source ~/.cargo/env"
fi

# Clone source into a temporary checkout when prebuilt binaries are unavailable.
SOURCE_TEMP_DIR=$(mktemp -d)
(
    if ! git clone --depth 1 "$BREW_REPO" "$SOURCE_TEMP_DIR" >/dev/null 2>&1; then
        printf "Failed to clone repository\n" >&2
        exit 1
    fi
) &
if ! spinner "Cloning ${ORANGE}brew${NC} repository" $!; then
    error_exit "Failed to clone brew repository. Check your network connection and that the repository exists."
fi
completed "Cloned ${ORANGE}brew${NC} repository"
cd "$SOURCE_TEMP_DIR" || error_exit "Failed to enter directory: $SOURCE_TEMP_DIR"

# Build
if [[ -d "$BREW_PREFIX/lib/pkgconfig" ]]; then
    export PKG_CONFIG_PATH="$BREW_PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
fi

# Use a temp file to capture cargo's JSON output for binary path detection
BUILD_OUTPUT=$(mktemp)

(
    if ! cargo build --release --bin b --bin bx --message-format=json > "$BUILD_OUTPUT" 2>&1; then
        exit 1
    fi
) &
if ! spinner "Building ${ORANGE}brew${NC}" $!; then
    error_exit "Failed to build brew. Run 'cargo build --release --bin b --bin bx' to see details."
fi
completed "Built ${ORANGE}brew${NC}"

# Parse cargo's JSON output to find the actual binary paths
# This handles custom CARGO_TARGET_DIR, .cargo/config.toml target-dir, etc.
parse_binary_path() {
    local binary_name="$1"
    local path
    # Each JSON line from cargo is self-contained. Find lines that:
    # 1. Are compiler-artifact messages (contain "reason":"compiler-artifact")
    # 2. Have an executable (contain "executable":)
    # 3. Match our binary name (contain "name":"$binary_name")
    # The name field in target uniquely identifies the binary
    path=$(grep "\"reason\":\"compiler-artifact\"" "$BUILD_OUTPUT" \
        | grep "\"executable\":" \
        | grep "\"name\":\"$binary_name\"" \
        | sed -E 's/.*"executable":"([^"]+)".*/\1/' \
        | tail -n1)
    echo "$path"
}

B_PATH=$(parse_binary_path "b")
BX_PATH=$(parse_binary_path "bx")

if [[ -z "$B_PATH" || ! -f "$B_PATH" ]]; then
    error_exit "Build succeeded but could not locate b binary. Check cargo configuration."
fi

if [[ -z "$BX_PATH" || ! -f "$BX_PATH" ]]; then
    error_exit "Build succeeded but could not locate bx binary. Check cargo configuration."
fi

install_bin "$BREW_BIN" "$B_PATH" "$BX_PATH"
finalize_installation "$no_modify_path"
