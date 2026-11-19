# Run bundles

A run bundle is one ended session's sealed record packed for someone else: a support request, an
audit, a failing run to share, or an outside evaluator who checks it with the host's public key
alone. [`mur trace export`](cli.md#mur-trace-export) writes one, as a directory or as a `.zip` of
the same tree, and [`mur trace verify --bundle`](cli.md#mur-trace-verify-bundle) checks one. The
format, `murmur.bundle/1`, is **stable**.

## Layout { #layout }

The bundle root is laid out as a [records directory](workdir.md#records-directory), with the
conversation record under `conversations/`:

```
bundle.json
trace.jsonl                     trace.jsonl.seal
hooks/eval.jsonl                hooks/eval.jsonl.seal          (when present)
completion.json                 completion.json.seal           (when present)
blobs/<sha256>
attachments/<attachment_id>.json
conversations/<record>/<context-id>/conversation.jsonl
conversations/<record>/<context-id>/conversation.jsonl.seal
conversations/<record>/<context-id>/blobs/<sha256>
```

| Packed | From |
|---|---|
| `trace.jsonl`, `hooks/eval.jsonl`, `completion.json` and each one's `.seal` | The records directory, whichever exist |
| `blobs/` | Every file of the records directory's [`blobs/`](observability-schemas.md#trace-blobs), written under `trace.capture: content` |
| `attachments/` | Every `*.json` file of the records directory's [`attachments/`](observability-schemas.md#record-attachments) |
| `conversations/<record>/<context-id>/` | The [conversation record](workdir.md#the-conversation-record) that `trace.jsonl.seal`'s `conversation` key names, with its seal and its image blobs, when that record is on the exporting host |

A bundle never holds the records directory's `logs/`, `out/`, `tmp/`, `home/`, `children/` or
`hooks/logs/`, a delegated child's or formation member's record, or either host key. Directories
in a bundle are mode `0700` and files `0600`, zip entries included: blobs are the unredacted wire
payload.

## bundle.json { #bundle-json }

One pretty-printed JSON object and a newline, at the bundle root.

| Key | Type | Value |
|---|---|---|
| `format` | string | `"murmur.bundle/1"` |
| `session_id` | string or null | The session: `session_id` of `trace.jsonl` line 1, else of `trace.jsonl.seal` |
| `exported_at` | integer | Milliseconds since the epoch |
| `exported_by` | string | `"mur <version>"` |
| `formats` | object | `{"seal": "murmur.seal/1", "attachment": "murmur.attachment/1"}`: the [seal](observability-schemas.md#sealed-records) and [attachment](observability-schemas.md#record-attachments) formats inside |
| `key_id` | string or null | `trace.jsonl.seal`'s `key_id`; `null` when the trace has no readable seal |
| `conversation` | object or null | `{"record": …, "context_id": …}` from `trace.jsonl.seal` when the conversation record was packed, else `null` |
| `files` | array | One `{"path", "bytes", "sha256"}` per file of the bundle except `bundle.json`, sorted by `path`. `path` is relative to the bundle root with `/` separators; `sha256` is the lowercase hex SHA-256 of the file's bytes |

## Verify with mur { #verify-with-mur }

```bash
mur trace verify --bundle 1a2b.bundle.zip --public-key host.pub
```

`host.pub` is the exporting host's `~/.murmur/keys/record-seal.pub`, handed over separately from
the bundle. The command reads only the bundle and that key, and exits 0 only when every check
below holds. [Bundles](cli.md#mur-trace-verify-bundle) lists its output and refusals.

## Check without Murmur { #check-without-murmur }

From the bundle root, with `sha256sum`, `jq` and OpenSSL 3:

1. **Manifest.** Every file `files` lists exists with that size and SHA-256:
   `jq -r '.files[] | "\(.sha256)  \(.path)"' bundle.json | sha256sum -c`. A file present but not
   listed is extra.
2. **Blob names.** Every file directly under `blobs/` or `conversations/<record>/<context-id>/blobs/`
   whose name is 64 lowercase hex characters has a SHA-256 equal to its name.
3. **Streams.** Check `trace.jsonl`, `hooks/eval.jsonl` and `completion.json` against their seals
   as [Sealed records](observability-schemas.md#sealed-records) states: every `trace.jsonl` line
   ends with `"prev_hash":"<line hash of the line before>"}` (`"genesis"` on line 1), and the seal's
   `key_id`, `lines`, `head` and `signature` match.
4. **Conversation record.** When `conversation` is set, check
   `conversations/<record>/<context-id>/conversation.jsonl` against its seal the same way; its
   chain is listed.
5. **Attachments.** Each attachment's `record.head` equals `trace.jsonl.seal`'s `head`, per
   [Attachments](observability-schemas.md#record-attachments).

To check a seal's Ed25519 signature with `openssl`, build a PEM public key from the 32-byte key
that `record-seal.pub` holds as 64 hex characters: the base64 of the bytes
`302a300506032b6570032100` followed by the 32 key bytes, between `-----BEGIN PUBLIC KEY-----` and
`-----END PUBLIC KEY-----` lines. Write the signed message
(`murmur.seal/1\n<stream>\n<session_id>\n<lines>\n<head>\n`) and the 64 bytes the seal's hex
`signature` decodes to into files, then:

```bash
openssl pkeyutl -verify -pubin -inkey host.pem -rawin -in message -sigfile signature
```

## What a bundle proves { #what-a-bundle-proves }

Anything a bundle shows is what the run recorded, to whoever holds the exporting host's public
key:

| Change | Detected |
|---|---|
| A byte of a stream | Yes: the stream's chain or seal fails |
| A byte of a blob | Yes: the blob no longer hashes to its name |
| A blob withheld | No: the record names its hash, the bundle lacks its body |
| `hooks/eval.jsonl`, `completion.json` or the conversation record withheld with its seal | No: the stream reads as `absent` |
| An attachment edited, added or removed | No, once `bundle.json` is rewritten to match: attachments are not signed |
| `bundle.json` rewritten | No: only the seals are signed |

`bundle.json` catches a file changed or lost in transit; it vouches for nothing the exporter
chose to leave out or edit.
