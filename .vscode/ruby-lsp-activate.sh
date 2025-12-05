#!/bin/bash
if [[ -n "${BASH_SOURCE[0]}" ]]; then
    SCRIPT_PATH="${BASH_SOURCE[0]}"
elif [[ -n "${ZSH_VERSION}" ]]; then
    SCRIPT_PATH="${(%):-%x}"
else
    SCRIPT_PATH="$0"
fi
HOMEBREW_PREFIX="$(cd "$(dirname "${SCRIPT_PATH}")"/../ && pwd)"

# These are used by the functions needed from utils/ruby.sh
export HOMEBREW_BREW_FILE="${HOMEBREW_PREFIX}/bin/brew"
export HOMEBREW_LIBRARY="${HOMEBREW_PREFIX}/Library"
export BUNDLE_WITH="style:typecheck:vscode"

# shellcheck disable=SC1091
source "${HOMEBREW_PREFIX}/Library/Homebrew/utils/ruby.sh"

setup-ruby-path
setup-gem-home-bundle-gemfile
ensure-bundle-dependencies
