# formation-sender fixture

A test-only script capsule that sends one message to a formation callee's virtual address, so a
script capsule's `murmur:message/send` can be seen recorded as a `member_request` that the formation
view joins to the callee's task.

`mur run --roster` refuses a script capsule as a member with `E-RUN-046` (a script capsule opens no
door, so it cannot hear its formation lifeline). The `member_request` line of a script capsule's
send is covered by `member_request_from_a_script_message_send_precedes_its_a2a_send` in
`crates/capsule-runtime/src/runtime.rs` and the join by
`member_request_from_a_script_member_joins_its_callee` in `crates/murmur-cli/src/formation_trace.rs`.
This component is the member a roster would launch once a script capsule can be one.

`capsule/formation-sender.wasm` exports `murmur:capsule/run@0.1.0` and imports
`murmur:message/send@0.1.0`, through a world of its own declared inline in `src/lib.rs`: the guest
`capsule` world does not import `murmur:message`. Its packages come from
`crates/capsule-runtime/wit/guest` and `crates/capsule-runtime/wit/host/deps/murmur-message`.

Its `run` sends one message (`messageId` `formation-sender`) to the first address
`MURMUR_FORMATION_PEERS` lists and writes one line to `out/result.txt`:

```text
task=<callee task id> state=<state>
```

or `error=<what>` when there is no callee or the send failed. It holds no token and no port: the
member's runtime resolves the virtual address to the callee's real door and presents the callee's
formation token.

## Rebuild

From the repository root:

```bash
scripts/rebuild-components.sh formation-sender
```
