set export
set dotenv-load
set unstable
set script-interpreter := ['bash', '-euo', 'pipefail']

BREW_ROOT := env('BREW_ROOT', '/opt/brew')
BREW_BIN := env('HOME', '~') / '.local' / 'bin'
BREW_PREFIX := env('BREW_PREFIX', BREW_ROOT)
BREW_INSTALLED_BIN := BREW_BIN / 'b'
BX_INSTALLED_BIN := BREW_BIN / 'bx'
BREW_ALIAS_BIN := BREW_BIN / 'brew'

SUDO := if which('doas') != '' {
    'doas'
} else {
    require('sudo')
}

alias b := build
alias i := install
alias t := test
alias l := lint
alias f := fmt

[doc('List available recipes')]
default:
    @just --list --unsorted

[doc('Build the b binary')]
[group('build')]
build: fmt-check lint
    cargo build --bin b --bin bx

[doc('Install b, bx and the brew alias to ~/.local/bin')]
[group('install')]
[script]
install: build
    if [[ -d "$BREW_PREFIX/lib/pkgconfig" ]]; then
        export PKG_CONFIG_PATH="$BREW_PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
    fi

    mkdir -p "$BREW_BIN"
    install -Dm755 target/debug/b "$BREW_BIN/b"
    install -Dm755 target/debug/bx "$BREW_BIN/bx"
    echo "Installed b to $BREW_BIN/b"
    ln -sfn b "$BREW_BIN/brew"
    echo "Installed bx to $BREW_BIN/bx"
    echo "Linked brew -> b in $BREW_BIN"

    "$BREW_BIN/b" init

[private]
[script]
_get_brew_configs:
    shell_configs=(
        "${ZDOTDIR:-$HOME}/.zshenv"
        "${ZDOTDIR:-$HOME}/.zshrc"
        "$HOME/.bashrc"
        "$HOME/.bash_profile"
        "$HOME/.profile"
    )

    for config in "${shell_configs[@]}"; do
        if [[ -f "$config" ]] && grep -q '^# brew$' "$config" 2>/dev/null; then
            echo "$config"
        fi
    done

[private]
[script]
_clean_shell_config config:
    tmp_file=$(mktemp)
    sed -e '/^# brew$/,/^}$/d' \
        -e '/_b_path_append/d' \
        "$config" > "$tmp_file" 2>/dev/null || true
    cat -s "$tmp_file" > "$config"
    rm "$tmp_file"
    echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Cleaned '"$config"''

[private]
[script]
_confirm msg:
    read -rp "{{msg}} [y/N] " confirm
    if [[ "$confirm" =~ ^[Yy]$ ]]; then
        exit 0
    else
        exit 1
    fi

[doc('Uninstall b and remove all data')]
[group('install')]
[script]
uninstall:
    mapfile -t configs_to_clean < <(just _get_brew_configs)

    echo 'Running this will remove:'
    echo -en '{{BOLD}}{{RED}}'
    echo -e  "\t$BREW_INSTALLED_BIN"
    echo -e  "\t$BX_INSTALLED_BIN"
    echo -e  "\t$BREW_ALIAS_BIN"
    echo -e  "\t$BREW_ROOT"
    for config in "${configs_to_clean[@]}"; do
        echo -e "\tbrew entries in $config"
    done
    echo -en '{{NORMAL}}'

    just _confirm "Continue?" || exit 0

    # Clean shell configuration files
    for config in "${configs_to_clean[@]}"; do
        just _clean_shell_config "$config"
    done

    [[ -f "$BREW_INSTALLED_BIN" ]] && rm -- "$BREW_INSTALLED_BIN"
    [[ -f "$BX_INSTALLED_BIN" ]] && rm -- "$BX_INSTALLED_BIN"
    [[ -L "$BREW_ALIAS_BIN" ]] && rm -- "$BREW_ALIAS_BIN"

    if [[ -d "$BREW_ROOT" ]]; then
        $SUDO rm -r -- "$BREW_ROOT"
    fi

    echo ''
    echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} brew uninstalled successfully!'
    echo ''
    echo 'Restart your terminal or run: exec $SHELL'

[doc('Reset brew completely (removes data and re-initializes)')]
[group('install')]
[script]
reset:
    mapfile -t configs_to_clean < <(just _get_brew_configs)

    echo -e '{{BOLD}}{{YELLOW}}Warning:{{NORMAL}} This will reset brew completely:'
    echo -en '{{BOLD}}{{RED}}'
    echo -e  "\t$BREW_ROOT"
    for config in "${configs_to_clean[@]}"; do
        echo -e "\tbrew entries in $config"
    done
    echo -en '{{NORMAL}}'

    just _confirm "Continue?" || exit 0

    # Clean shell configuration files
    for config in "${configs_to_clean[@]}"; do
        just _clean_shell_config "$config"
    done
    if [[ -d "$BREW_ROOT" ]]; then
        $SUDO rm -rf -- "$BREW_ROOT" && echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Removed '"$BREW_ROOT"''
    fi

    echo ''
    echo -e '{{BOLD}}{{CYAN}}==>{{NORMAL}} Re-initializing brew...'

    if [[ -f "$BREW_INSTALLED_BIN" ]]; then
        "$BREW_INSTALLED_BIN" init
        echo ''
        echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Reset complete!'
    else
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} b binary not found at $BREW_INSTALLED_BIN'
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} Run {{BOLD}}just install{{NORMAL}} first to install brew'
    fi

[doc('Format code with rustfmt')]
[group('lint')]
[script]
fmt:
    if command -v rustup &>/dev/null && rustup toolchain list | grep -q nightly; then
        cargo +nightly fmt --all
    else
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} Using stable rustfmt (nightly not available)'
        cargo fmt --all
    fi

[doc('Check code formatting with rustfmt')]
[group('lint')]
[script]
fmt-check:
    if command -v rustup &>/dev/null && rustup toolchain list | grep -q nightly; then
        cargo +nightly fmt --all -- --check
    else
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} Using stable rustfmt (nightly not available)'
        cargo fmt --all -- --check
    fi

[doc('Run Clippy linter')]
[group('lint')]
lint:
    cargo clippy --workspace -- -D warnings

[doc('Run all tests')]
[group('test')]
test:
    cargo test --workspace -- --include-ignored
