#!/usr/bin/env bash
# Builds the pinned Ghostty libghostty-vt static archive. This script is the
# native build contract of tui-frame: build.rs invokes it with a package-local
# cache. The build uses the same verified native revision as go-tui-frame.
# Invoke through bash; executable permissions are not required.
#
# ROOT is the absolute output directory. TARGET is the Zig target, such as
# native or x86_64-linux-musl. PREFIX and CACHE name the installation and Zig
# cache directories relative to ROOT. The script writes only under ROOT, never
# next to itself, and prints the archive's pkg-config flags when it finishes.
set -euo pipefail
revision='33da6848d63b3bba2b4f31ab1531d618f2795192'
archive_sha='bbda18d6f6666ff05dec33974c134483b096fe321ff68a22eba9399b70354de6'
usage() {
    printf 'usage: bash %s ROOT TARGET PREFIX CACHE\n%s\n' "$0" "$1" >&2
    exit 2
}
test "$#" -eq 4 || usage 'Expected four arguments.'
root=$1
target=$2
prefix=$3
cache=$4
# ROOT must be absolute: zig build runs in ROOT's source directory with
# installation and cache paths under ROOT, and git documents
# GIT_CEILING_DIRECTORIES as a list of absolute paths.
case "$root" in
    /*) ;;
    *) usage "ROOT must be an absolute path: $root" ;;
esac
test -n "$target" || usage 'TARGET must not be empty.'
for name in "$prefix" "$cache"; do
    case "$name" in
        '' | /* | .. | ../* | */.. | */../*) usage "PREFIX and CACHE must be relative paths inside ROOT: $name" ;;
    esac
done
source="$root/ghostty"
test "$(zig version)" = '0.16.0' || { printf 'Zig 0.16.0 is required.\n' >&2; exit 1; }
command -v pkg-config >/dev/null
mkdir -p "$root"
check_source() {
    test -f "$source/.frame-source-revision" && test "$(cat "$source/.frame-source-revision")" = "$revision" || { printf 'Existing native source is unmarked or uses another revision: %s\n' "$source" >&2; exit 1; }
}
# stop runs the cleanup function $1 for a signal trap and re-raises the
# signal ($2). When the whole group gets SIGTERM, just forwards a second
# one, which can kill bash before its EXIT trap runs. This happens with
# macOS's /bin/bash 3.2, and with bash 5.2 during the archive check, so
# SIGTERM gets a trap like Ctrl+C and Ctrl+\. bash, except macOS's
# /bin/bash 3.2, ignores SIGQUIT again once the trap is reset, so the
# shell then exits with $3, the status bash gives a command that the
# signal ends: 128 plus its POSIX number (INT 2, QUIT 3, TERM 15).
stop() {
    "$1"
    trap - EXIT "$2"
    kill -s "$2" "$$"
    exit "$3"
}
# trap_cleanup runs the cleanup function $1 when the shell exits or
# gets SIGINT, SIGQUIT, or SIGTERM.
trap_cleanup() {
    trap "$1" EXIT
    trap "stop $1 INT 130" INT
    trap "stop $1 QUIT 131" QUIT
    trap "stop $1 TERM 143" TERM
}
if test -e "$source"; then
    check_source
else
    command -v shasum >/dev/null || { printf 'shasum is required.\n' >&2; exit 1; }
    archive="$root/ghostty-$revision.tar.gz"
    url="https://codeload.github.com/ghostty-org/ghostty/tar.gz/$revision"
    # shasum --check exits 1 when the digest differs or the file cannot
    # be read. Any other failure, such as shasum ending on Ctrl+C or
    # Ctrl+\, stops the script with that status, so an interrupted check
    # never counts as a failed archive.
    verify_archive() {
        local status=0
        printf '%s  %s\n' "$archive_sha" "$1" | shasum -a 256 --check || status=$?
        case "$status" in
            0 | 1) return "$status" ;;
            *) exit "$status" ;;
        esac
    }
    # curl runs as a job so that remove_download, the cleanup trap below,
    # can stop and reap it before removing its file. A signal sent only
    # to this shell would otherwise leave curl running, and curl would
    # create the file again. Ctrl+C and Ctrl+\ reach curl too, but a job
    # started without job control ignores SIGINT and SIGQUIT, so the
    # trap stops it. curl_pid is cleared once wait reaps curl.
    curl_pid=''
    remove_download() {
        if test -n "$curl_pid"; then
            kill "$curl_pid" 2>/dev/null || true
            wait "$curl_pid" 2>/dev/null || true
        fi
        rm -f "$download"
    }
    # Only a verified download is renamed to $archive. A cached archive
    # that fails the check, such as a partial file from an older native build,
    # is removed and downloaded again.
    if test -f "$archive" && ! verify_archive "$archive"; then
        rm -f "$archive"
        printf 'Removed cached archive %s: it failed the check against SHA-256 %s.\n' "$archive" "$archive_sha" >&2
    fi
    if ! test -f "$archive"; then
        download=$(mktemp "$archive.XXXXXX")
        trap_cleanup remove_download
        curl --fail --location --retry 3 "$url" --output "$download" &
        curl_pid=$!
        curl_status=0
        wait "$curl_pid" || curl_status=$?
        curl_pid=''
        test "$curl_status" -eq 0 || exit "$curl_status"
        verify_archive "$download" || {
            printf 'Downloaded %s does not match SHA-256 %s; %s was not saved.\n' "$url" "$archive_sha" "$archive" >&2
            exit 1
        }
        mv "$download" "$archive"
        trap - EXIT INT QUIT TERM
    fi
    # Runs that share $root, such as `just native` and `just native-linux`,
    # can both find $source missing and extract the archive. mv(1)
    # moves a directory into an existing target directory instead of
    # failing, so a run checks $source again and renames its tree only
    # while it holds $lock. With set -C (noclobber), bash creates the
    # lock with open(2)'s O_CREAT and O_EXCL flags, which check for the
    # file and create it in one atomic step. The lock holds this shell's
    # PID, and remove_extraction removes it only while it holds that
    # PID. The lock is held only for the check and the rename, so a run
    # waits for it at most 30 seconds: a run ended by SIGKILL leaves it
    # behind.
    lock="$source.lock"
    lock_source() {
        local status=0
        set -C
        printf '%s\n' "$$" 2>/dev/null > "$lock" || status=$?
        set +C
        return "$status"
    }
    remove_extraction() {
        if test "$(cat "$lock" 2>/dev/null)" = "$$"; then
            rm -f "$lock"
        fi
        rm -rf "$extracted"
    }
    extracted=$(mktemp -d "$root/source.XXXXXX")
    trap_cleanup remove_extraction
    tar -xzf "$archive" --strip-components=1 -C "$extracted"
    printf '%s\n' "$revision" > "$extracted/.frame-source-revision"
    deadline=$((SECONDS + 30))
    until lock_source; do
        test "$SECONDS" -lt "$deadline" || {
            printf 'Could not create %s within 30 seconds. Remove it if no native build is running.\n' "$lock" >&2
            exit 1
        }
        sleep 0.1
    done
    if test -e "$source"; then
        check_source
    else
        mv "$extracted" "$source"
    fi
    remove_extraction
    trap - EXIT INT QUIT TERM
fi
cd "$source"
# Ghostty's build runs git in its source directory to detect a version.
# The extracted archive is not a repository. Clear the repository
# variables a calling git process exports (hooks, rebase --exec), then
# stop git's repository discovery at $root so it cannot find this checkout.
git_local_env=''
if command -v git >/dev/null; then
    git_local_env=$(git rev-parse --local-env-vars)
fi
(
    unset -v $git_local_env
    GIT_CEILING_DIRECTORIES="$root" zig build -Demit-lib-vt -Demit-xcframework=false -Doptimize=ReleaseFast -Dtarget="$target" --prefix "$root/$prefix" --cache-dir "$root/$cache" --global-cache-dir "$root/zig-global-cache"
)
PKG_CONFIG_PATH="$root/$prefix/share/pkgconfig" pkg-config --static --libs --cflags libghostty-vt-static
