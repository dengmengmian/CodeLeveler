#!/bin/sh
# Keep Cargo target directories from growing a deps folder so large that rustc
# spends its time in directory lookup.
#
# Unpacked macOS debug info leaves one .rcgu.o per codegen unit in deps, and
# Cargo keeps the previous hash. Removing those files in place does not shrink
# the directory inode, so a sick deps directory is renamed aside and deleted
# from a fresh directory.
#
# No arguments: the periodic run. It skips a target cargo is using.
# --dry-run           print actions.
# --drop-incremental  also retire incremental caches. Use once after a
#                     dev-profile rustc flag change; those caches cannot be reused.
set -eu

export PATH="${HOME}/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
max_deps_entries=20000
incremental_days=14
sweep_bytes_mb=20000
log_dir="${HOME}/Library/Logs"
log_file="$log_dir/codeleveler-sweep.log"
stamp_file="$log_dir/codeleveler-sweep.stamp"
profile_stamp="$log_dir/codeleveler-sweep.profile"
profile_id="dev-split-debuginfo=packed"
lock_dir="$log_dir/codeleveler-sweep.lockdir"

dry=0
drop_incremental=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry=1 ;;
        --drop-incremental) drop_incremental=1 ;;
        *)
            printf 'unknown argument: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

log() {
    mkdir -p "$log_dir"
    printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$log_file"
    printf '%s\n' "$*"
}

acquire_lock() {
    if [ "$dry" -eq 1 ]; then
        return 0
    fi
    mkdir -p "$log_dir"
    if [ -d "$lock_dir" ]; then
        # A killed sweeper must not block every later run.
        stale=$(find "$lock_dir" -maxdepth 0 -mmin +360 2>/dev/null || true)
        if [ -n "$stale" ]; then
            rmdir "$lock_dir" 2>/dev/null || true
        fi
    fi
    if ! mkdir "$lock_dir" 2>/dev/null; then
        log "skip: another sweep is running"
        exit 0
    fi
    trap 'rmdir "$lock_dir" 2>/dev/null || true' EXIT INT TERM
}

# Print 0 when a cargo/rustc process has this target on its command line.
compiler_busy() {
    python3 - "$1" <<'PY'
import subprocess, sys
needle = sys.argv[1] + "/"
out = subprocess.check_output(["ps", "-axww", "-o", "command="], text=True, errors="replace")
for line in out.splitlines():
    if "sweep-debug-artifacts" in line:
        continue
    if needle in line and ("/rustc" in line or "/cargo " in line or line.startswith("cargo ")):
        sys.exit(0)
sys.exit(1)
PY
}

# Print 0 when any .cargo-lock under the target is exclusively locked.
lock_held() {
    python3 - "$1" <<'PY'
import fcntl, os, sys
root = sys.argv[1]
held = False
if os.path.isdir(root):
    for dirpath, dirnames, filenames in os.walk(root):
        # Drop these names before walk lists them. Listing a sick deps
        # directory is the stall this script exists to avoid.
        dirnames[:] = [
            name
            for name in dirnames
            if name not in ("deps", "incremental", "build", ".fingerprint")
        ]
        if ".cargo-lock" not in filenames:
            continue
        fd = os.open(os.path.join(dirpath, ".cargo-lock"), os.O_RDWR)
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                held = True
            else:
                fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)
sys.exit(0 if held else 1)
PY
}

entry_count() {
    python3 - "$1" "$2" <<'PY'
import os, sys
limit = int(sys.argv[2])
count = 0
with os.scandir(sys.argv[1]) as it:
    for _ in it:
        count += 1
        if count > limit:
            break
print(count)
PY
}

target_busy() {
    compiler_busy "$1" || lock_held "$1"
}

rotate_dir() {
    src=$1
    if [ "$dry" -eq 1 ]; then
        log "would rotate $src"
        return 0
    fi
    dest=$(mktemp -d /tmp/codeleveler-sweep.XXXXXX)
    mv "$src" "$dest/gone"
    # The new compile uses a fresh inode. Removal of the old one stays off the hot path.
    nohup nice -n 19 rm -rf "$dest" >/dev/null 2>&1 &
    log "rotated $src"
}

list_special_dirs() {
    target_root=$1
    name=$2
    find "$target_root" \
        -type d \( -name deps -o -name incremental -o -name build -o -name .fingerprint \) \
        -prune -print \
        | while IFS= read -r dir; do
            if [ "$(basename "$dir")" = "$name" ]; then
                printf '%s\n' "$dir"
            fi
        done
}

sweep_tree() {
    target_root=$1
    if [ ! -d "$target_root" ]; then
        return 0
    fi
    if target_busy "$target_root"; then
        log "skip $target_root: cargo is using it"
        return 2
    fi

    list_file=$(mktemp)
    list_special_dirs "$target_root" deps >"$list_file"
    while IFS= read -r deps; do
        [ -n "$deps" ] || continue
        [ -d "$deps" ] || continue
        count=$(entry_count "$deps" "$max_deps_entries")
        if [ "$count" -gt "$max_deps_entries" ]; then
            rotate_dir "$deps"
        elif [ "$dry" -eq 1 ]; then
            log "would delete leftover .rcgu.o under $deps ($count entries)"
        else
            find "$deps" -name '*.rcgu.o' -delete
        fi
    done <"$list_file"

    list_special_dirs "$target_root" incremental >"$list_file"
    while IFS= read -r incr; do
        [ -n "$incr" ] || continue
        [ -d "$incr" ] || continue
        if [ "$drop_incremental" -eq 1 ]; then
            rotate_dir "$incr"
        elif [ "$dry" -eq 1 ]; then
            log "would prune $incr sessions older than ${incremental_days} days"
        else
            find "$incr" -mindepth 1 -maxdepth 1 -mtime +"$incremental_days" -exec rm -rf {} +
        fi
    done <"$list_file"
    rm -f "$list_file"
}

target_roots() {
    printf '%s\n' "$root/target"
    if command -v git >/dev/null 2>&1 && git -C "$root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        common=$(git -C "$root" rev-parse --git-common-dir)
        case "$common" in
            /*) ;;
            *) common="$root/$common" ;;
        esac
        common=$(CDPATH= cd -- "$common" && pwd)
        shared="$(dirname "$common")/.codeleveler-target"
        if [ "$shared" != "$root/target" ] && [ -d "$shared" ]; then
            printf '%s\n' "$shared"
        fi
    fi
}

maybe_byte_sweep() {
    if [ "$dry" -eq 1 ]; then
        log "would cargo sweep --maxsize $sweep_bytes_mb when the daily stamp is due"
        return 0
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        log "skip cargo sweep: cargo is not on PATH"
        return 0
    fi
    now=$(date +%s)
    last=0
    if [ -f "$stamp_file" ]; then
        last=$(cat "$stamp_file" 2>/dev/null || printf '%s' 0)
    fi
    case "$last" in
        ''|*[!0-9]*) last=0 ;;
    esac
    if [ $((now - last)) -lt 86400 ]; then
        return 0
    fi
    if target_busy "$root/target"; then
        log "skip cargo sweep: cargo is using $root/target"
        return 0
    fi
    if (cd "$root" && cargo sweep --maxsize "$sweep_bytes_mb"); then
        printf '%s\n' "$now" >"$stamp_file"
        log "cargo sweep --maxsize $sweep_bytes_mb"
    else
        log "cargo sweep failed"
    fi
}

# A dev-profile rustc flag change invalidates incremental caches. Retire them
# once, on the first run that can take the target lock.
if [ "$drop_incremental" -eq 0 ]; then
    current=$(cat "$profile_stamp" 2>/dev/null || true)
    if [ "$current" != "$profile_id" ]; then
        drop_incremental=1
    fi
fi

acquire_lock
log "start dry=$dry drop_incremental=$drop_incremental"
roots_file=$(mktemp)
target_roots >"$roots_file"
skipped=0
while IFS= read -r target_root; do
    [ -n "$target_root" ] || continue
    sweep_tree "$target_root" || skipped=1
done <"$roots_file"
rm -f "$roots_file"
if [ "$skipped" -eq 0 ] && [ "$dry" -eq 0 ]; then
    printf '%s\n' "$profile_id" >"$profile_stamp"
fi
maybe_byte_sweep
log "done"
