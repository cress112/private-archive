#!/usr/bin/env bash
# Project-owned entry point; execute this file rather than sourcing it.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    printf '%s\n' 'private-archive: execute this script instead of sourcing it' >&2
    return 2
fi
archive_script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)" || exit 127
archive_binary="${PRIVATE_ARCHIVE_BIN:-"$archive_script_dir/../target/bin/private-archive"}"
if [[ "$archive_binary" != */* ]]; then
    archive_binary="$(type -P -- "$archive_binary")" || {
        printf '%s\n' 'private-archive: executable not found; build the application or set PRIVATE_ARCHIVE_BIN' >&2
        exit 127
    }
fi
if [[ ! -x "$archive_binary" ]]; then
    printf '%s\n' 'private-archive: executable not found; build the application or set PRIVATE_ARCHIVE_BIN' >&2
    exit 127
fi
exec /bin/bash "$archive_script_dir/private-archive-session.sh" "$archive_binary" "$@"
