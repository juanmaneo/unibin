#!/bin/sh
# unibin hardware test for Linux, FreeBSD and macOS. Run it from the kit:
#
#   sh run.sh
#
# It runs probe.com and probe-x86_64.com in every way a user might, checks
# what the probe reports, and writes everything to hwtest-<OS>-<arch>.txt.
# Send that file back. Exit status: 0 if every check passed.

set -u
cd "$(dirname "$0")" || exit 1
kit=$(pwd)
s=$(uname -s) a=$(uname -m)
case $a in amd64|x64) a=x86_64 ;; arm64) a=aarch64 ;; esac
host=$s-$a
report=$kit/hwtest-$host.txt
: >"$report" || exit 1
pass=0 fail=0 skip=0
work=$(mktemp -d "${TMPDIR:-/tmp}/unibin-hwtest.XXXXXX") || exit 1
trap 'rm -rf "$work"' EXIT
# Slices are extracted here, so leftovers are easy to spot.
TMPDIR=$work/tmp
export TMPDIR
mkdir "$TMPDIR" || exit 1

log() { printf '%s\n' "$*" | tee -a "$report"; }
ok() { pass=$((pass + 1)); log "PASS $1"; }
bad() { fail=$((fail + 1)); log "FAIL $1: $2"; }
skipped() { skip=$((skip + 1)); log "SKIP $1: $2"; }

# probe NAME CMD...: run CMD with stdin from $input (default /dev/null).
# Leaves the status in $rc, stdout in $work/out and stderr in $work/err,
# and copies all three into the report.
input=/dev/null
probe() {
    name=$1
    shift
    "$@" <"$input" >"$work/out" 2>"$work/err"
    rc=$?
    {
        echo "--- $name: $*"
        echo "    rc=$rc"
        sed 's/^/    out| /' "$work/out"
        sed 's/^/    err| /' "$work/err"
    } >>"$report"
}

fact() { sed -n "s/^$1=//p" "$work/out" | head -n 1; }

# expect NAME RC SLICE [ARG...]: the last probe exited RC from slice SLICE
# and received exactly ARG... (pass "-" as SLICE to skip that check).
expect() {
    name=$1 want_rc=$2 want_slice=$3
    shift 3
    why=
    [ "$rc" = "$want_rc" ] || why="$why exit $rc, want $want_rc;"
    if [ "$want_slice" != - ]; then
        got=$(fact slice)
        [ "$got" = "$want_slice" ] || why="$why slice '$got', want '$want_slice';"
        got=$(fact argc)
        [ "$got" = $# ] || why="$why argc '$got', want $#;"
        i=1
        for want in "$@"; do
            got=$(fact arg$i)
            [ "$got" = "$want" ] || why="$why arg$i '$got', want '$want';"
            i=$((i + 1))
        done
    fi
    case $rc in
    137) why="$why (SIGKILL: on macOS usually a missing or broken code signature);" ;;
    esac
    if grep -q 'ELF binary type' "$work/err"; then
        why="$why (FreeBSD rejected the ELF brand: check EI_OSABI);"
    fi
    why=${why# }
    if [ -z "$why" ]; then ok "$name"; else bad "$name" "${why%;}"; fi
}

# expect_fact NAME KEY VALUE
expect_fact() {
    got=$(fact "$2")
    if [ "$got" = "$3" ]; then ok "$1"; else bad "$1" "$2 '$got', want '$3'"; fi
}

# expect_err NAME TEXT: the last probe failed with 126 and TEXT on stderr.
expect_err() {
    if [ "$rc" = 126 ] && grep -qF "$2" "$work/err"; then
        ok "$1"
    else
        bad "$1" "exit $rc, want 126 with '$2' on stderr"
    fi
}

# The extraction directory is removed by a watcher that polls once a second.
expect_cleanup() {
    n=0
    while [ -n "$(ls -A "$TMPDIR")" ] && [ $n -lt 10 ]; do
        sleep 1
        n=$((n + 1))
    done
    left=$(ls -A "$TMPDIR")
    if [ -z "$left" ]; then ok "$1"; else bad "$1" "left behind in \$TMPDIR: $left"; fi
}

log "unibin hardware test, host $host"
{
    echo "== system"
    uname -a
    case $s in
    Linux) sed -n 's/^PRETTY_NAME=//p' /etc/os-release 2>/dev/null ;;
    FreeBSD) freebsd-version -ku 2>/dev/null ;;
    Darwin) sw_vers 2>/dev/null ;;
    esac
    ls -l /bin/sh
    for f in probe.com probe-x86_64.com probe-signed.com; do
        [ -f "$f" ] && ls -l "$f"
    done
    [ "$s" = Darwin ] && xattr -l probe.com 2>/dev/null
    echo "== checks"
} >>"$report" 2>&1

for f in probe.com probe-x86_64.com; do
    [ -f "$f" ] || { log "missing $kit/$f: run this from the unpacked kit"; exit 1; }
done

# 1. Every way of starting probe.com runs this host's native slice.
probe sh-explicit sh ./probe.com --exit 42 'a b' '' 'c*' "it's"
expect sh-explicit 42 "$host" 'a b' '' 'c*' "it's"
expect_fact sh-explicit-not-translated translated 0

probe sh-bare-name sh probe.com --exit 3 x
expect sh-bare-name 3 "$host" x

probe exit-255 sh ./probe.com --exit 255
expect exit-255 255 "$host"

printf 'hello\n' >"$work/in"
input=$work/in
probe stdin sh ./probe.com --stdin
input=/dev/null
expect stdin 0 "$host"
expect_fact stdin-content stdin hello

mkdir "$work/dir with spaces" "$TMPDIR/tmp with spaces"
cp probe.com "$work/dir with spaces/probe.com"
probe spaces-in-paths env TMPDIR="$TMPDIR/tmp with spaces" sh "$work/dir with spaces/probe.com" --exit 6 z
expect spaces-in-paths 6 "$host" z
sleep 2
rmdir "$TMPDIR/tmp with spaces" 2>/dev/null

# Direct exec relies on the shell's ENOEXEC fallback. If the kernel hands
# MZ files to an interpreter (wine's binfmt_misc), that never happens.
printf 'MZ=1\necho plain-sh\n' >"$work/mz"
chmod 755 "$work/mz"
if [ "$(sh -c '"$0"' "$work/mz" 2>/dev/null)" != plain-sh ]; then
    skipped direct-exec "a binfmt_misc handler claims MZ files (wine?); use 'sh ./app'"
else
    for shell in sh bash dash zsh ksh mksh yash fish; do
        command -v $shell >/dev/null 2>&1 || continue
        version=$($shell --version 2>/dev/null | head -n 1)
        log "     $shell: $(command -v $shell) ${version:-}"
        probe exec-$shell $shell -c './probe.com --exit 5 "a b"'
        expect exec-$shell 5 "$host" 'a b'
    done
    probe exec-via-path env PATH="$kit:$PATH" sh -c 'probe.com --exit 4 y'
    expect exec-via-path 4 "$host" y
fi

expect_cleanup temp-dir-cleanup

# 2. x86_64 slices only: native on x86_64, the OS's fallback on arm64.
emu=
tail -c 4096 probe-x86_64.com | grep -a -q '^emu=Linux-aarch64' && emu=yes
probe fallback sh ./probe-x86_64.com --exit 7 'a b'
case $host in
*-x86_64)
    expect fallback-native 7 "$host" 'a b'
    ;;
Darwin-aarch64)
    expect fallback-rosetta 7 Darwin-x86_64 'a b'
    expect_fact fallback-rosetta-translated mac.proc_translated 1
    grep -q 'Bad CPU type' "$work/err" && log "     hint: install Rosetta: softwareupdate --install-rosetta"
    ;;
Linux-aarch64)
    if [ -n "$emu" ]; then
        expect fallback-emu 7 Linux-x86_64 'a b'
        expect_fact fallback-emu-translated translated 1
    else
        expect_err fallback-no-emu "has no slice for Linux-aarch64"
    fi
    ;;
FreeBSD-aarch64)
    expect_err fallback-no-slice "has no slice for FreeBSD-aarch64"
    ;;
*)
    skipped fallback "no expectation for $host"
    ;;
esac
expect_cleanup fallback-cleanup

# 3. A copy that run.ps1 signed on Windows still runs through sh.
if [ -f probe-signed.com ]; then
    probe signed sh ./probe-signed.com --exit 42 'a b'
    expect signed 42 "$host" 'a b'
    expect_cleanup signed-cleanup
else
    skipped signed "no probe-signed.com (made by run.ps1 on Windows)"
fi

log "RESULT $host pass=$pass fail=$fail skip=$skip (report: $report)"
[ $fail = 0 ]
