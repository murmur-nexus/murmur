# How to prepare a server to launch many capsules at once

A capsule that can start a subprocess, through a shell or spawn grant or any WASM tool, driver or
hook, launches only inside a delegated cgroup v2 scope that bounds memory, processes and CPU across
its whole process tree. A desktop login gets that scope from the systemd user manager with no setup.
A fresh cloud server often cannot give one, and its first sealed launch fails with `E-RUN-012`. This
guide takes such a server to a host where hundreds of sealed capsules launch at once: it shows how
to tell whether the host needs setup, the two ways to provide the scope, and the host limits a long
burst runs into.

??? definition "What is a delegated cgroup?"
    A **delegated cgroup** is a cgroup v2 directory the kernel lets your own user manage, with the
    memory, pids and cpu controllers turned on for the cgroups below it. `mur` creates each
    capsule's cgroups inside it and sets the capsule's memory, process and CPU limits on them.

    Where a launch gets one:

    - **The systemd user manager**: `mur` asks it for a scope over the session bus.
    - **A delegated parent**: `mur` already runs inside a cgroup its user owns, such as a system
      unit started with `Delegate=yes`, and builds under it without asking systemd.

The host settings this guide changes are:

| Setting | Controls |
|---|---|
| `Delegate=` on `user@.service` | Which controllers the systemd user manager hands to the scopes it creates |
| `Delegate=yes` on a system unit | Whether the unit's cgroup belongs to the user who runs the launcher |
| `cgroup.subtree_control` of the unit's cgroup | Which controllers the cgroups below the unit get |
| `LimitNOFILE=` or `ulimit -n` | How many files the launcher may hold open |

!!! note "What was measured, and on what"
    The rates and thresholds in this guide come from one AWS `m7i.8xlarge` (32 vCPU, 16 cores)
    running a fresh Ubuntu 22.04 with kernel 6.8 and systemd 249, launching sealed capsules against
    a stub model that does no inference, 1,500 launches per run. The example output comes from a
    fresh Ubuntu 22.04 VM with 16 vCPU. Thresholds move on other hosts: treat each number as the
    order of magnitude to expect, and measure your own.

You need `mur` installed (see [Install](../getting-started/install.md)), `sudo` on the server, and a
project folder whose `murmur.yaml` declares a capsule that starts subprocesses.

---

## Step 1 — check whether the host needs this { #check-the-host }

Run `mur doctor` in the project folder and read its `Cgroup scope` block:

```bash
mur doctor
```

On a fresh Ubuntu 22.04 server, over ssh:

```text
Cgroup scope
  cgroup scope: unavailable — cgroup controllers [cpu] are not delegated to /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/murmur-8065-0.scope; the unit `mur` runs under needs `Delegate=yes` (or `Delegate=memory pids cpu io`); no delegated parent was adopted: parent not owned by this uid
  host setup: login session (session-23.scope) — a launch asks the systemd user manager for a scope; it delegates memory, pids
  delegated parent: not adopted — parent not owned by this uid
  open files: 1024 (hard 1048576)
  cause: this host's systemd user manager (user@1000.service) delegates only memory, pids to the scopes it creates, and a launch needs memory, pids, cpu
  fix: give the systemd user manager memory, pids, cpu and io to delegate (see https://docs.murmur.nexus/reference/resource-limits/#platform-behavior), then restart it — the restart ends this user's running processes; on a desktop, log out and back in instead:
      sudo mkdir -p /etc/systemd/system/user@.service.d
      printf "[Service]\nDelegate=memory pids cpu io\n" | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
      sudo systemctl daemon-reload && sudo systemctl restart user@$(id -u).service && sudo loginctl enable-linger $(whoami)
  refused: `mur run` refuses this capsule here with E-RUN-012, because it can run a subprocess and no cgroup scope can be created to bound it
```

The uid (`1000`), the session and the `murmur-<pid>-0.scope` name differ on your host.

| Line | What it tells you |
|---|---|
| `cgroup scope:` | Whether a launch from this shell gets its scope |
| `host setup:` | Where `mur` runs, and where a launch takes its scope from |
| `delegated parent:` | Whether a launch would build under the cgroup above `mur`'s, and the first check that declines it |
| `open files:` | The open-file limit of this shell, the subject of [Step 4](#open-files) |
| `cause:` | Why a launch gets no scope here; printed only then |
| `fix:` | The commands that fix that cause; `mur doctor` prints them and runs nothing |

A launch on that host refuses with `E-RUN-012`, naming the same cause after `this host:` and the
same fix as its hint:

```bash
mur run --task task.md
```

```text
status:  failed
error[E-RUN-012]: this capsule can spawn subprocesses but no cgroup v2 scope could be created to bound them (cgroup controllers [cpu] are not delegated to /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/murmur-8085-0.scope; the unit `mur` runs under needs `Delegate=yes` (or `Delegate=memory pids cpu io`); no delegated parent was adopted: parent not owned by this uid); this host: this host's systemd user manager (user@1000.service) delegates only memory, pids to the scopes it creates, and a launch needs memory, pids, cpu; on Linux the runtime refuses to launch rather than run a subprocess tree with no aggregate memory/pids/cpu ceiling — see the systemd user delegation requirement at https://docs.murmur.nexus/reference/resource-limits/#platform-behavior
  hint: give the systemd user manager memory, pids, cpu and io to delegate (see https://docs.murmur.nexus/reference/resource-limits/#platform-behavior), then restart it — the restart ends this user's running processes; on a desktop, log out and back in instead:
    sudo mkdir -p /etc/systemd/system/user@.service.d
    printf "[Service]\nDelegate=memory pids cpu io\n" | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
    sudo systemctl daemon-reload && sudo systemctl restart user@$(id -u).service && sudo loginctl enable-linger $(whoami)
```

The `fix:` and the hint are option one, [Step 2](#user-manager).

When the block prints no `cause:` or `fix:` line, a launch gets its scope and the host needs no
setup for a handful of capsules; [Step 3](#delegated-parent) is still the setup for many at once.
Every line of the block is described in [Cgroup scope](../reference/cli.md#doctor-cgroup-scope), how
a launch gets its scope in
[Whether the launch gets its cgroup scope](../reference/containment.md#cgroup-scope), and the code in
the [diagnostics index](../reference/diagnostics.md#index).

## Step 2 — option one: let the systemd user manager delegate { #user-manager }

The stock Ubuntu 22.04 user manager delegates only `memory pids`. Give it `memory pids cpu io` with a
drop-in for `user@.service`, then restart it:

```bash
sudo mkdir -p /etc/systemd/system/user@.service.d
printf "[Service]\nDelegate=memory pids cpu io\n" | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload && sudo systemctl restart user@$(id -u).service && sudo loginctl enable-linger $(whoami)
```

The restart ends every process your user runs under the user manager; on a desktop, log out and
back in instead of restarting. Over ssh, the restart line can fail, and on a fresh Ubuntu 22.04
server it does:

```text
Job for user@1000.service failed because the control process exited with error code.
See "systemctl status user@1000.service" and "journalctl -xeu user@1000.service" for details.
```

The drop-in is in place all the same. Log out of the server and log in again: the new login starts
the user manager, within seconds, with the new delegation. The failed restart also skipped
`enable-linger`, which keeps the user manager running when no one is logged in, so run it after
logging back in:

```bash
sudo loginctl enable-linger $(whoami)
```

Confirm what the user manager can now delegate:

```bash
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.controllers
```

```text
cpu io memory pids
```

Before the drop-in, the same file lists `memory pids`. Confirm that linger is on:

```bash
loginctl show-user $(whoami) -p Linger
```

```text
Linger=yes
```

Run `mur doctor` again in the project folder:

```bash
mur doctor
```

Its `Cgroup scope` block now prints no `cause:` or `fix:` line, and `mur run --task task.md` from
the ssh login ends `status:  ok`:

```text
Cgroup scope
  cgroup scope: delegated
  host setup: login session (session-36.scope) — a launch asks the systemd user manager for a scope; it delegates memory, pids, cpu, io
  delegated parent: not adopted — parent not owned by this uid
  open files: 1024 (hard 1048576)
```

`delegated parent: not adopted` is expected here: a login session's parent cgroup is not yours to
build under, so a launch asks the user manager for its scope instead.

Every launch then asks the user manager for a scope over the session bus. On the measurement host
this path launched 1,500 capsules at 800 in flight with no refusal in six runs, at 139–156 launches
per second with 50 in flight and 126–143 with 800, while the user manager itself used 18–23 % of a
CPU at 50 in flight.

## Step 3 — option two: launch from a delegated parent { #delegated-parent }

A host that launches many capsules at once can give `mur` a cgroup to build under directly. The
launcher runs in a system unit with `Delegate=yes` as your user, and every `mur run` it starts
adopts the unit's cgroup as its [delegated parent](../reference/resource-limits.md#delegated-parent):
it makes no systemd request and needs no session bus. On the measurement host this launched 1,500
capsules at 50, 200, 300, 500 and 800 in flight with no refusal, at 148–169 launches per second,
with the systemd user manager at 0 % CPU.

Create a `delegated-launch.sh` file in your home folder:

```bash
#!/usr/bin/env bash
U=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
mkdir -p "$U/launcher" && echo $$ > "$U/launcher/cgroup.procs"
echo "+memory +pids +cpu +io" > "$U/cgroup.subtree_control" || echo "delegated-launch: enable failed" >&2
exec "$@"
```

Start the launcher through it, from the project folder, with `<launcher command>` replaced by the
program that starts your `mur run` commands:

```bash
sudo systemd-run --uid=$(id -u) --gid=$(id -g) -p Delegate=yes -p LimitNOFILE=65536 \
  --wait --collect --pipe --working-directory=$PWD \
  --setenv=HOME=$HOME --setenv=PATH=$PATH \
  bash $HOME/delegated-launch.sh <launcher command>
```

| Part | What it does |
|---|---|
| `sudo systemd-run --uid=$(id -u) --gid=$(id -g)` | Starts a transient system unit that runs as you |
| `-p Delegate=yes` | Hands the unit's cgroup to your user, so `mur` may create cgroups in it |
| `-p LimitNOFILE=65536` | Raises the launcher's open-file limit; see [Step 4](#open-files) |
| `--wait --pipe` | Keeps the launcher in the foreground of your shell, with its output and exit status |
| `--collect` | Removes the unit when the launcher ends, together with any cgroup a killed launch left in it |
| `--working-directory=$PWD` | Starts the launcher in the current folder |
| `--setenv=HOME=$HOME --setenv=PATH=$PATH` | Gives the launcher your home folder and the `PATH` that finds `mur` |
| `delegated-launch.sh`, line 2 | Finds the unit's cgroup |
| `delegated-launch.sh`, line 3 | Moves the script into a `launcher` cgroup below it: a cgroup that hands controllers to the cgroups below it may hold no process itself |
| `delegated-launch.sh`, line 4 | Turns on `memory pids cpu io` for the cgroups below the unit |
| `delegated-launch.sh`, line 5 | Replaces the script with the launcher |

Every `mur run` the launcher starts sees the unit's cgroup as its parent, passes the
[parent checks](../reference/resource-limits.md#delegated-parent), and builds its cgroups in
`murmur-<pid>-p` there.

### Confirm that launches adopt the unit's cgroup { #confirm-adoption }

Run the checks through the same command, with a `mur` command as the launcher. `mur run
--explain-scope` prints the scope report without contacting a model:

```bash
sudo systemd-run --uid=$(id -u) --gid=$(id -g) -p Delegate=yes -p LimitNOFILE=65536 \
  --wait --collect --pipe --working-directory=$PWD \
  --setenv=HOME=$HOME --setenv=PATH=$PATH \
  bash $HOME/delegated-launch.sh mur run --explain-scope
```

```text
Running as unit: run-u70.service
Containment
  declared:  sealed
  achieved:  sealed
  host can back: sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp
  userns grant: restriction_disabled_host_wide
  landlock ABI: 3
  cgroup scope: delegated
```

`mur doctor` run the same way names the parent a launch adopts:

```bash
sudo systemd-run --uid=$(id -u) --gid=$(id -g) -p Delegate=yes -p LimitNOFILE=65536 \
  --wait --collect --pipe --working-directory=$PWD \
  --setenv=HOME=$HOME --setenv=PATH=$PATH \
  bash $HOME/delegated-launch.sh mur doctor
```

```text
Cgroup scope
  cgroup scope: delegated
  host setup: system unit (run-u71.service) — a launch adopts the delegated parent /sys/fs/cgroup/system.slice/run-u71.service
  delegated parent: would be adopted — /sys/fs/cgroup/system.slice/run-u71.service
  open files: 65536 (hard 65536)
```

The unit name, `run-u<n>.service`, changes with every `systemd-run`.

While a launch started through the unit runs, read its cgroup from another shell:

```bash
cat /proc/$(pgrep -o -x mur)/cgroup
```

```text
0::/system.slice/run-u72.service/murmur-8185-p/murmur-supervisor
```

The path ends in `murmur-<pid>-p/murmur-supervisor`, where `<pid>` is that `mur`'s process id.

!!! warning "Check adoption before a large run"
    A parent that fails a check is declined, and the reason is printed on the `delegated parent:`
    line. When the session bus is visible, a launch from a declined parent asks the systemd user
    manager for a scope instead, which moves it out of the unit: the capsule is still contained,
    but the unit's own limits no longer bound it. A unit without `Delegate=yes` declines with
    `parent not owned by this uid`; a script that did not turn on the controllers declines with
    `parent does not enable memory,pids,cpu`.

`systemd-run` rewrites `$` in the command it is given: `$$` becomes `$`, and `${NAME}` becomes the
value of `NAME` in the unit's own environment. Your shell has already expanded `$(id -u)`, `$PWD`
and `$HOME` by then, and the `$` signs in `delegated-launch.sh` sit in the file, where
`systemd-run` does not look. Two things follow:

- Put a launcher that uses shell variables in a script file, and pass the file as
  `<launcher command>`.
- In a system unit without `Delegate=yes`, the `fix:` of `mur doctor` prints this setup as one
  `sh -c '…'` line. Use `delegated-launch.sh` instead: in the one-line form `systemd-run` turns
  `echo $$` into `echo $`, so the line stops with `sh: 1: echo: echo: I/O error` and the launcher
  never starts.

### Run the launcher { #run-the-launcher }

With adoption confirmed, start the real launcher the same way. For example, this launcher starts
200 launches, 50 at a time, and counts the outcomes:

```bash
sudo systemd-run --uid=$(id -u) --gid=$(id -g) -p Delegate=yes -p LimitNOFILE=65536 \
  --wait --collect --pipe --working-directory=$PWD \
  --setenv=HOME=$HOME --setenv=PATH=$PATH \
  bash $HOME/delegated-launch.sh sh -c 'seq 200 | xargs -P 50 -I{} mur run --task task.md' > burst.log 2>&1
grep -c "status:  ok" burst.log; grep -c E-RUN-012 burst.log
```

```text
200
0
```

200 launches ended `status:  ok` and none refused. `grep -c` exits with status 1 when it counts 0.

## Step 4 — raise the launcher's open-file limit { #open-files }

A launcher holds pipes and files for every launch in flight. An ssh login on Ubuntu 22.04 starts
with a soft limit of 1,024 open files, and a launcher with 500 launches in flight crashed at that
limit until it was raised to 65,536. Check the limit in the shell that starts the launcher:

```bash
ulimit -n
```

The unit in [Step 3](#delegated-parent) sets it with `-p LimitNOFILE=65536`. A launcher started
from a shell, as in [Step 2](#user-manager), needs it raised in that shell first:

```bash
ulimit -n 65536
ulimit -n
mur doctor | grep "open files"
```

```text
65536
  open files: 65536 (hard 65536)
```

`ulimit -n 65536` needs a hard limit of at least 65,536; `mur doctor`'s `open files:` line prints
both. The raised limit lasts for that shell and what it starts.

## Step 5 — watch the journal and removed memory cgroups on long bursts { #long-bursts }

Two things build up over tens of thousands of launches on one host:

| Grows | Measured | How to read it |
|---|---|---|
| The system journal | 4,801 lines per 1,500 launches and 328 MB after about 60,000 launches, with an older `mur` on the user manager path. With the current `mur` on a 16 vCPU VM, 200 launches added 207 lines from an ssh login (option one) and 6 through the unit of option two | `journalctl --disk-usage` |
| Removed memory cgroups the kernel still keeps | 2 per launch on kernel 6.8; about 62,000 after 30,000 launches, when launches with 50 in flight were 16–20 % slower than on a fresh host | `grep nr_dying_descendants /sys/fs/cgroup/cgroup.stat` |

Read the journal's size, and shrink it when it grows too large:

```bash
journalctl --disk-usage
sudo journalctl --vacuum-size=500M
```

```text
Archived and active journals take up 16.0M in the file system.
Vacuuming done, freed 0B of archived journals from /run/log/journal.
Vacuuming done, freed 0B of archived journals from /var/log/journal.
Vacuuming done, freed 0B of archived journals from /var/log/journal/ec2b0f7650a1d007005c979e6fa7f9a7.
```

Read the count of removed memory cgroups:

```bash
grep nr_dying_descendants /sys/fs/cgroup/cgroup.stat
```

```text
nr_dying_descendants 889
```

For a big run, start on a fresh VM, or drop the kernel's caches between waves of launches: the
kernel then frees the removed cgroups that only cached pages were keeping.

```bash
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
grep nr_dying_descendants /sys/fs/cgroup/cgroup.stat
```

```text
3
nr_dying_descendants 507
```

On the measurement host, with an older `mur` that left about four removed cgroups per launch, 1,500
launches took 17.3–17.5 s with 166,387 removed cgroups, 10.4–10.7 s after `drop_caches` left
1,311 of them, and 9.3–9.5 s on a fresh host. `drop_caches` does not clear the count every time:
after 30,000 launches with the current `mur` it left between 15 and 1,821.

## What a laptop already has { #laptop }

A desktop or laptop needs none of the steps above for a handful of capsules:

- A graphical login runs a systemd user manager, and `mur` reaches it on the session bus.
- A recent systemd delegates `cpu` as well as `memory pids` to that user manager by default.

Check what yours delegates:

```bash
systemctl cat user@.service | grep Delegate
```

On Ubuntu 24.04 with systemd 255 the stock unit carries `Delegate=pids memory cpu`. On Ubuntu 22.04
with systemd 249 it carries `Delegate=pids memory`, which is why the server in
[Step 1](#check-the-host) refuses. A `Delegate=` line from a drop-in, such as the one in
[Step 2](#user-manager), prints after the stock one. Check `ulimit -n` in the
terminal that starts a launcher before a large burst there as well.

## Summary

| Feature / setting | How it works |
|---|---|
| `mur doctor`, `Cgroup scope` block | Names the host setup, where a launch takes its scope from, and, when it gets none, the `cause:` and the `fix:` |
| `E-RUN-012` | Refuses a capsule that can start subprocesses when no delegated scope is available; carries the same cause and fix |
| `user@.service` drop-in, `Delegate=memory pids cpu io` | The systemd user manager hands every scope it creates the controllers a launch needs |
| System unit with `Delegate=yes` and `delegated-launch.sh` | Every `mur run` the launcher starts builds under the unit's cgroup, with no systemd request and no session bus |
| `mur run --explain-scope`, `delegated parent:`, `/proc/<pid>/cgroup` | Confirm that launches adopt the unit's cgroup |
| `LimitNOFILE=65536` or `ulimit -n 65536` | Lets the launcher hold the files of hundreds of launches in flight |
| `journalctl --disk-usage`, `nr_dying_descendants` | Read what a long burst leaves behind; a fresh VM or `drop_caches` between waves recovers launch speed |
