#!/bin/sh
# procstat.sh - sample the memory and descriptor usage of a running process.
#
# Usage:
#   procstat.sh PID [INTERVAL [COUNT]]
#
#   PID        process to watch (must exist, or the script exits with status 1)
#   INTERVAL   seconds between samples (whole seconds); omit for a single sample
#   COUNT      number of samples to take; omit to sample until the process exits
#
# Each sample is one line:
#
#   2026-01-01T12:00:00Z pid=1234 VmHWM=18432kB VmRSS=16120kB fds=207 nofile_soft=1024 nofile_hard=4096
#
#   VmHWM         peak resident set size since the process started
#   VmRSS         current resident set size
#   fds           number of entries in /proc/PID/fd (open descriptors)
#   nofile_soft / nofile_hard
#                 the "Max open files" limit from /proc/PID/limits
#
# A value that cannot be read (for example the fd directory of another user's
# process, without root) is printed as "?"; a process without VmHWM/VmRSS
# lines (kernel thread, zombie) shows "n/a".
#
# Only POSIX sh and the date/sleep utilities are used, so it works with dash,
# busybox ash and plain coreutils. Linux /proc is required.
#
# Examples:
#   procstat.sh "$(pidof lorawan-sniffer)"              # one sample
#   procstat.sh 1234 10                                 # every 10 s until it exits
#   procstat.sh 1234 30 40 | tee run.log                # 40 samples, 30 s apart

prog=${0##*/}

usage() {
    echo "usage: $prog PID [INTERVAL [COUNT]]" >&2
    exit 2
}

is_uint() {
    case $1 in
        '' | *[!0-9]*) return 1 ;;
    esac
    return 0
}

[ $# -ge 1 ] && [ $# -le 3 ] || usage

pid=$1
interval=${2-}
count=${3-}

is_uint "$pid" || { echo "$prog: invalid PID: $pid" >&2; usage; }
if [ -n "$interval" ]; then
    is_uint "$interval" || { echo "$prog: invalid INTERVAL: $interval" >&2; usage; }
    [ "$interval" -gt 0 ] || { echo "$prog: INTERVAL must be at least 1 second" >&2; usage; }
fi
if [ -n "$count" ]; then
    is_uint "$count" || { echo "$prog: invalid COUNT: $count" >&2; usage; }
    [ -n "$interval" ] || { echo "$prog: COUNT needs an INTERVAL" >&2; usage; }
    [ "$count" -gt 0 ] || { echo "$prog: COUNT must be at least 1" >&2; usage; }
fi

proc="/proc/$pid"

if [ ! -d "$proc" ]; then
    echo "$prog: no such process: $pid" >&2
    exit 1
fi

# Prints one sample line. Returns 1 when the process has gone away, which is
# detected by /proc/PID/status no longer being readable.
sample() {
    hwm=n/a
    rss=n/a
    if [ ! -r "$proc/status" ]; then
        return 1
    fi
    while read -r key val; do
        case $key in
            VmHWM:) hwm=${val%% *}kB ;;
            VmRSS:) rss=${val%% *}kB ;;
        esac
    done < "$proc/status" 2>/dev/null || return 1

    fds='?'
    if [ -r "$proc/fd" ] && [ -x "$proc/fd" ]; then
        fds=0
        for fd in "$proc"/fd/*; do
            # Descriptors are symlinks (often to non-paths such as
            # "socket:[123]"), so test the link itself; an unmatched glob
            # leaves the literal pattern, which is not a link.
            if [ -L "$fd" ]; then
                fds=$((fds + 1))
            fi
        done
    fi

    soft='?'
    hard='?'
    if [ -r "$proc/limits" ]; then
        # Line format: "Max open files            1024                 4096                 files"
        while read -r w1 w2 w3 lsoft lhard _; do
            if [ "$w1 $w2 $w3" = "Max open files" ]; then
                soft=$lsoft
                hard=$lhard
                break
            fi
        done < "$proc/limits" 2>/dev/null
    fi

    printf '%s pid=%s VmHWM=%s VmRSS=%s fds=%s nofile_soft=%s nofile_hard=%s\n' \
        "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$pid" "$hwm" "$rss" "$fds" "$soft" "$hard"
    return 0
}

n=0
while :; do
    if ! sample; then
        if [ "$n" -eq 0 ]; then
            echo "$prog: cannot read $proc/status for process $pid" >&2
            exit 1
        fi
        echo "$prog: process $pid exited" >&2
        exit 0
    fi
    n=$((n + 1))
    [ -n "$interval" ] || exit 0
    if [ -n "$count" ] && [ "$n" -ge "$count" ]; then
        exit 0
    fi
    sleep "$interval"
done
