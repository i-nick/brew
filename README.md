<div align="center">

<h2>brew</h2>

[![CI](https://github.com/i-nick/brew/actions/workflows/ci.yml/badge.svg)](https://github.com/i-nick/brew/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/i-nick/brew?display_name=tag)](https://github.com/i-nick/brew/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE-MIT.md)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](./LICENSE-APACHE.md)

<img alt="brew demo" src="./assets/b-demo.gif" />

<p><strong>brew brings uv-style architecture to package management on Apple Silicon Macs.</strong></p>

</div>

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/i-nick/brew/refs/heads/main/install.sh | bash
```

After install, run the `export` command it prints (or restart your terminal).

The CLI is `b`; `brew` is installed as an alias, so `brew install jq` works too.

### Updating brew itself

```bash
b self-update            # update b and bx to the latest release
b self-update --check    # only report whether a newer release exists
b self-update 0.2.1      # install a specific release (also downgrades)
```

Downloads are checked against the release's `SHA256SUMS` and the new `b` is
test-run before the old binaries are atomically replaced. `b` also checks for
a new release at most once a day and prints a one-line notice; set
`BREW_NO_UPDATE_CHECK=1` to turn that off.

## Quick start

```bash
b install jq                   # install one package
b install wget git             # install multiple
b install hashicorp/tap/terraform  # install a third-party formula by explicit ref
b bundle                       # install from Brewfile
b bundle install -f myfile     # install from custom file
b bundle dump                  # export installed packages to Brewfile
b bundle dump -f out --force   # dump to custom file (overwrite)
b uninstall jq                 # uninstall one package
b reset                        # uninstall everything
b gc                           # garbage collect unused store entries
bx jq --version                # run without linking
```

## How it works

- Content-addressable storage for deduplication
- APFS clonefiles for zero-overhead copying
- Source build fallback using a Ruby formula DSL shim

brew does not maintain a separate tap registry. Install third-party formulas with explicit
references such as `owner/repo/formula`, and use the same explicit ref in your Brewfiles.

Package metadata and bottles come from the Homebrew project; see [LICENSE-HOMEBREW](./LICENSE-HOMEBREW).

## Project status

<div align="center">
  <a href="https://star-history.com/#i-nick/brew&Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=i-nick/brew&type=Date&theme=dark" />
      <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=i-nick/brew&type=Date" />
    </picture>
  </a>
</div>

- **Status:** Experimental, but already useful for many common formulas.
- **Feedback:** If you hit incompatibilities, please open an issue or PR.
- **License:** Dual-licensed under [Apache 2.0](./LICENSE-APACHE.md) OR [MIT](./LICENSE-MIT.md), at your choice.
