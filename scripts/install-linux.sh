#!/bin/sh
set -eu
if [ "$#" -gt 1 ] || [ "${1:-}" = '--help' ]; then
    echo 'Usage: sh install.sh [INSTALL_DIRECTORY]'
    echo 'Default: $HOME/.local/bin. The installer does not edit shell configuration.'
    if [ "$#" -gt 1 ]; then exit 2; fi
    exit 0
fi
package_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
install_dir=${1:-"${HOME:?HOME is not set}/.local/bin"}
cd "$package_dir"
sha256sum --check SHA256SUMS.txt
mkdir -p -- "$install_dir"
install_dir=$(CDPATH='' cd -- "$install_dir" && pwd)
if [ -d "$install_dir/codex-vault" ]; then
    echo 'The executable destination is a directory.' >&2
    exit 1
fi
pending=$(mktemp "$install_dir/.codex-vault.XXXXXX")
trap 'rm -f -- "$pending"' EXIT HUP INT TERM
install -m 755 codex-vault "$pending"
mv -f -- "$pending" "$install_dir/codex-vault"
printf 'Installed: %s/codex-vault\n' "$install_dir"
case ":${PATH:-}:" in
    *":$install_dir:"*) echo 'Run codex-vault --help to get started.' ;;
    *) echo 'Add the installation directory to your PATH, or run the executable by its full path.' ;;
esac
