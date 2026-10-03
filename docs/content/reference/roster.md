# Roster Schema

A `roster.yaml` declares a formation: the capsules that run together as its **members**, the one
**entry member** that receives the formation's task, and which members may call which. It sits in
the project directory, beside `murmur.yaml`.

```yaml
roster_version: 1
members:
  - name: planner
    capsule: planner
    version: 0.3.0
    entry: true
  - name: coder
    capsule: coder
    version: 1.2.0
  - name: reviewer
    capsule: reviewer
    version: 0.9.0
reachability:
  - from: planner
    to: [coder, reviewer]
  - from: reviewer
    to: [coder]
```

Nothing launches a roster. [`mur doctor`](cli.md#mur-doctor) admits it and reports the result, and
`mur run` does not read `roster.yaml`.

## Fields { #fields }

Every key is listed below. Any other key, at any level, is refused with
[`E-ROS-001`](diagnostics.md#e-ros-001).

| Field | Type | Required | Notes |
|---|---|---:|---|
| `roster_version` | integer | yes | Must be `1`. |
| `members` | list | yes | At least one entry. Order matters: admission reports faults in this order. |
| `members[].name` | string | yes | The member's name within this roster. Matches `^[a-z][a-z0-9-]{0,31}$`. Unique within the roster. |
| `members[].capsule` | string | yes | The installed capsule the member runs. A valid artifact name. Two members may run the same capsule. |
| `members[].version` | string | yes | The exact version of `capsule`. Not blank, and not `latest`, `stable` or `edge`. An unquoted number such as `1.10` reads as its text, as `artifacts[].version` does in `murmur.yaml`. |
| `members[].entry` | boolean | no | `true` on exactly one member. Default: `false`. |
| `reachability` | `all` or list | no | Which members may call which. Default: no member may call another. An empty list means the same. |
| `reachability[].from` | string | yes | The calling member. |
| `reachability[].to` | list of strings | yes | The members `from` may call. At least one, and never `from` itself. |

A key written with no value, such as `entry:`, is the same as an absent key.

## Members and versions { #members }

A member is a capsule at an exact version. Admission looks it up in the project store
(`.murmur/artifacts/`) and then the global store (`~/.murmur/artifacts/`), the order
`mur run --capsule` uses, and binds the member to the sha256 of the artifact it found. Install
each member first with `mur install`.

The `murmur.yaml` beside the roster is a member only when a member names its capsule.

### `murmur.lock` { #murmur-lock }

When the project's `murmur.lock` pins a member's capsule, the version and hash admission resolved
must match the pin, or the roster is refused with [`E-REG-005`](diagnostics.md#index). A capsule
the lock does not pin is admitted at the version the roster names. Admission never creates or
changes `murmur.lock`.

## The entry member { #entry-member }

The entry member receives the formation's task, splits it up and hands the parts to the members
it may call. Its task outcome is the formation's outcome.

The entry member gets no edge of its own. Under `reachability: all` it calls and is called only
if it serves peers, like any other member. To let it call members without being callable, write
a rule for it.

## `reachability` { #reachability }

A rule means "`from` may call each member in `to`". Rules add together, and the same edge written
twice is one edge.

| Value | Edges |
|---|---|
| absent, or `[]` | None |
| `all` | Every ordered pair of distinct members that both declare [`exports.peer_tasks.accept: true`](manifest.md#field-exports-peer-tasks) |
| list of rules | Each rule's `from` to each member in its `to` |

A rule names members only. Every member a rule lists in `to` must declare
`exports.peer_tasks.accept: true`, or the roster is refused with
[`E-ROS-005`](diagnostics.md#e-ros-005). A caller does not need to.

## Authentication { #authentication }

When the roster has at least one edge, **every** member must declare
[`network.authentication`](manifest.md#field-network-authentication), including members on no
edge. A peer task carries no credential of its own, so a public door in a formation takes a task
from any member. A missing declaration is refused with [`E-ROS-006`](diagnostics.md#e-ros-006).

A roster with one member, no `reachability`, or an `all` that pairs no members has no edges and
needs no authentication.

`murmur.yaml` refuses `network.authentication` beside a non-empty `capabilities.spawn.allow`
(`E-MAN-003`), so a member of a roster with edges cannot also delegate to child capsules.

## Admission order { #admission }

Admission admits the whole roster or refuses it with one code. It checks, in this order, and stops
at the first failure. Within a check, members are taken in roster order.

| Order | Check | Code |
|---:|---|---|
| 1 | `roster.yaml` reads and has the shape above | [`E-ROS-001`](diagnostics.md#e-ros-001) |
| 2 | Member names are unique | [`E-ROS-003`](diagnostics.md#e-ros-003) |
| 3 | Exactly one member has `entry: true` | [`E-ROS-002`](diagnostics.md#e-ros-002) |
| 4 | Every rule's `from` and `to` names a member | [`E-ROS-004`](diagnostics.md#e-ros-004) |
| 5 | Each member is installed at its version, and its packed `murmur.yaml` parses | [`E-ROS-007`](diagnostics.md#e-ros-007) |
| 5 | `murmur.lock` agrees with each member it pins | [`E-REG-005`](diagnostics.md#index) |
| 6 | Every member a rule calls serves peers | [`E-ROS-005`](diagnostics.md#e-ros-005) |
| 7 | With any edge, every member's door requires authentication | [`E-ROS-006`](diagnostics.md#e-ros-006) |

Checks 1–4 read no store, so a roster with a structural fault is refused even when no member is
installed. Check 5 takes each member in turn through both rows before the next.
