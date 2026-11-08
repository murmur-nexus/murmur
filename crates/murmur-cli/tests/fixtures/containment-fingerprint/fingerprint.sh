#!/usr/bin/env bash
# Prints one `key: value` line per containment field of the process running it, so one program
# started two ways can be compared field by field.
#
#   bash fingerprint.sh            the fields of this bash
#   bash fingerprint.sh --depth2   the fields of a bash this one starts, each key prefixed `d2.`
#
# Bash builtins only, apart from `readlink` and `rm`: at the Landlock tier a command can exec only
# `capabilities.shell.allow` binaries, so a capsule running this allows `bash`, `readlink` and
# `rm` and nothing else it depends on. Files are read with redirections in this shell, never in a
# subshell, so `/proc/self` is this bash. A field that cannot be read prints `unreadable: <error>`,
# never an empty value.

if [[ $1 == --depth2 ]]; then
    bash -c '. "$0" --nested' "$0"
    exit $?
fi

prefix=
[[ $1 == --nested ]] && prefix=d2.

emit() {
    local value=${2//$'\n'/\\n}
    printf '%s%s: %s\n' "$prefix" "$1" "${value:-<empty>}"
}

# The error text of a failed redirection or exec, without the `bash: line N:` prefix.
reason() {
    local text=${1%$'\n'}
    text=${text##*: }
    printf '%s' "${text:-failed}"
}

# ---------------------------------------------------------------- /proc/self/status

declare -A status_wanted=(
    [Uid]=1 [Gid]=1 [Groups]=1 [CapInh]=1 [CapPrm]=1 [CapEff]=1 [CapBnd]=1 [CapAmb]=1
    [NoNewPrivs]=1 [Seccomp]=1 [Seccomp_filters]=1 [SigBlk]=1 [SigIgn]=1 [SigCgt]=1 [Umask]=1
    [Cpus_allowed_list]=1 [NSpid]=1
)
declare -A status_seen=()
if error=$({ : </proc/self/status; } 2>&1); then
    while IFS=: read -r key value; do
        [[ -n ${status_wanted[$key]} ]] || continue
        set -f
        # shellcheck disable=SC2206
        words=($value)
        set +f
        if [[ $key == NSpid ]]; then
            # The pid values are not comparable; how many pid namespaces deep this process is, is.
            emit status.NSpid.depth "${#words[@]}"
        else
            emit "status.$key" "${words[*]}"
        fi
        status_seen[$key]=1
    done </proc/self/status
    for key in "${!status_wanted[@]}"; do
        [[ -n ${status_seen[$key]} ]] || emit "status.$key" "unreadable: no such line"
    done
else
    for key in "${!status_wanted[@]}"; do
        emit "status.$key" "unreadable: $(reason "$error")"
    done
fi

# ---------------------------------------------------------------- /proc/self/limits

if error=$({ : </proc/self/limits; } 2>&1); then
    {
        read -r _header
        while IFS= read -r line; do
            name=${line:0:26}
            name=${name%"${name##*[![:space:]]}"}
            set -f
            # shellcheck disable=SC2206
            words=(${line:26})
            set +f
            emit "limit.$name" "${words[*]}"
        done
    } </proc/self/limits
else
    emit limit "unreadable: $(reason "$error")"
fi

# ---------------------------------------------------------------- cgroup, LSM label, oom, personality

read_file() {
    local key=$1 path=$2 error line count=0
    if error=$({ : <"$path"; } 2>&1); then
        while IFS= read -r line || [[ -n $line ]]; do
            count=$((count + 1))
            if ((count == 1)); then emit "$key" "$line"; else emit "$key.$count" "$line"; fi
        done <"$path"
        ((count > 0)) || emit "$key" "<empty>"
    else
        emit "$key" "unreadable: $(reason "$error")"
    fi
}

read_file cgroup /proc/self/cgroup
read_file lsm.current /proc/self/attr/current
read_file oom_score_adj /proc/self/oom_score_adj
read_file personality /proc/self/personality

# ---------------------------------------------------------------- namespaces

for ns in cgroup ipc mnt net pid pid_for_children time time_for_children user uts; do
    if target=$(readlink "/proc/$$/ns/$ns" 2>&1); then
        emit "ns.$ns" "$target"
    else
        emit "ns.$ns" "unreadable: $(reason "$target")"
    fi
done

# ---------------------------------------------------------------- process group, session, parent

if error=$({ : <"/proc/$$/stat"; } 2>&1); then
    read -r -a stat <"/proc/$$/stat"
    # Fields after `(comm)`, which holds no space for bash: 3 ppid, 4 pgrp, 5 session, 18 nice.
    emit proc.pid "${stat[0]}"
    emit proc.ppid "${stat[3]}"
    emit proc.sid "${stat[5]}"
    if [[ ${stat[4]} == "${stat[0]}" ]]; then emit proc.pgid_is_pid yes; else emit proc.pgid_is_pid no; fi
    if [[ ${stat[5]} == "${stat[0]}" ]]; then emit proc.sid_is_pid yes; else emit proc.sid_is_pid no; fi
    emit proc.nice "${stat[18]}"
else
    emit proc "unreadable: $(reason "$error")"
fi

# No /proc file holds the parent-death signal: only `prctl(PR_GET_PDEATHSIG)` in the process itself
# reads it, which a shell cannot call.
emit proc.pdeathsig "not readable from a shell"

# ---------------------------------------------------------------- open descriptors

# Two entries are this script's own: fd 255, bash's handle on a script file it runs, and the
# lowest free fd (3 when nothing was inherited), which the glob below opens to list the folder and
# the `$(readlink …)` then reads through. A descriptor the process inherited takes a number of its
# own and pushes those up by one, so it shows as an extra `fd.<n>` key.

fds=("/proc/$$/fd/"*)
if [[ ${fds[0]} == "/proc/$$/fd/*" ]]; then
    emit fd "unreadable: /proc/$$/fd lists nothing"
else
    for path in "${fds[@]}"; do
        fd=${path##*/}
        if target=$(readlink "$path" 2>&1); then
            # A pipe's or socket's inode names the one object, not what kind it is.
            case $target in
            pipe:\[*\] | socket:\[*\]) target=${target%%:*} ;;
            esac
            emit "fd.$fd" "$target"
        else
            emit "fd.$fd" "unreadable: $(reason "$target")"
        fi
    done
fi

# ---------------------------------------------------------------- environment

# `compgen -e` lists the exported names in sorted order.
for name in $(compgen -e); do
    [[ $name == _ ]] && continue
    emit "env.$name" "${!name}"
done

# ---------------------------------------------------------------- /dev and /dev/shm

list_dir() {
    local key=$1 dir=$2 error entries=() entry
    if error=$({ : <"$dir"; } 2>&1); then
        shopt -s nullglob dotglob
        for entry in "$dir"/*; do entries+=("${entry##*/}"); done
        shopt -u nullglob dotglob
        emit "$key" "${entries[*]}"
    else
        emit "$key" "unreadable: $(reason "$error")"
    fi
}

list_dir dev.entries /dev
list_dir devshm.entries /dev/shm

# What `/`, `/dev`, `/tmp`, `/proc` and `/sys` and the mounts beneath `/dev` are, and how many
# mounts there are in all. The mount ids and the `major:minor` device are left out: a tmpfs, devpts
# or procfs a command mounts for itself gets a device number from a host-wide pool, so two commands
# alike differ there whenever anything else on the host mounts one in between. The rest of the
# table is left out: a shell result holds 16 KiB.
if error=$({ : </proc/self/mountinfo; } 2>&1); then
    declare -A mount_seen=()
    mounts=0
    while read -r -a fields; do
        mounts=$((mounts + 1))
        point=${fields[4]}
        case $point in
        / | /dev | /dev/* | /tmp | /proc | /sys) ;;
        *) continue ;;
        esac
        rest="${fields[3]} ${fields[*]:5}"
        count=$((${mount_seen[$point]:-0} + 1))
        mount_seen[$point]=$count
        if ((count == 1)); then emit "mount.$point" "$rest"; else emit "mount.$point.$count" "$rest"; fi
    done </proc/self/mountinfo
    emit mount.count "$mounts"
else
    emit mount "unreadable: $(reason "$error")"
fi

# ---------------------------------------------------------------- Landlock outcomes

probe() {
    local key=$1 error
    shift
    if error=$("$@" 2>&1); then
        emit "landlock.$key" ok
    else
        emit "landlock.$key" "$(reason "$error")"
    fi
}

read_passwd() { read -r _ </etc/passwd; }
list_home() { : <"$HOME"; }
write_to() { printf x >"$1" && rm -f "$1"; }
exec_unallowed() { /usr/bin/true; }

probe read_etc_passwd read_passwd
probe list_home list_home
probe write_workdir write_to "./.murmur-fingerprint.$$"
probe write_tmp write_to "/tmp/murmur-fingerprint.$$"
probe write_dev_shm write_to "/dev/shm/murmur-fingerprint.$$"
probe exec_unallowed exec_unallowed

emit end ok
