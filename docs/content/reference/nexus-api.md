# Nexus API

This page documents the Nexus registry HTTP API that `mur` consumes in remote registry mode.

!!! info "Implemented vs Planned"
    **Implemented now:** authenticated artifact publish/download/index endpoints under `/v1/artifacts`.

    **Planned (not yet implemented):** broader registry features such as channel-pointer endpoints and expanded platform-aware resolution semantics.

Base path: `/v1`

Authentication: required on all endpoints

```http
Authorization: Bearer <NEXUS_API_KEY>
```

If missing/invalid: `401 {"error":"unauthorized"}`

---

## `POST /v1/artifacts`

Publish artifact bytes.

### Query parameters

| Name | Required | Description |
|---|---:|---|
| `name` | yes | artifact name |
| `version` | yes | artifact version |
| `runtime` | yes | `wasm` or `native` |
| `platform` | no | platform tag for native artifacts, `os-arch` format (e.g. `darwin-aarch64`). Repeatable for multiple variants. |

### Request body

Raw `.mur.zip` bytes (`application/octet-stream`)

### Responses

- `201 Created`

```json
{
  "artifact_id": "name@version",
  "sha256": "..."
}
```

- `400` bad input / invalid runtime / invalid platform
- `409` artifact conflict (`name@version` already exists)
- `422` reserved version rejected (`latest`, `stable`, `edge`)

---

## `GET /v1/artifacts/{name}/{version}`

Download artifact bytes.

### Query parameters

| Name | Required | Description |
|---|---:|---|
| `platform` | no | `os-arch` string (e.g. `darwin-aarch64`). When supplied, Nexus returns the platform-specific variant if it exists, or the generic (untagged) file as a fallback. When absent, the generic file is returned. A 404 is returned when neither exists; the error body names the requested platform. |

### Response

- `200 OK` with raw artifact bytes
- header: `Content-Type: application/octet-stream`
- header: `x-murmur-sha256: <sha256>`

Errors:

- `404` artifact not found, or no variant for requested platform — `{"error":"artifact 'name@version' has no variant for platform 'os-arch'"}`
- `401` unauthorized

---

## `GET /v1/artifacts`

List registry index metadata.

### Response

`200 OK` JSON array of `ArtifactMeta`:

```json
[
  {
    "name": "jsonl-line-count",
    "version": "0.1.0",
    "runtime": "wasm",
    "platforms": [],
    "description": null,
    "tags": []
  }
]
```

---

## Runtime/env configuration

Nexus binary (`crates/nexus/src/main.rs`) reads:

- `NEXUS_API_KEY` (required)
- `NEXUS_BIND` (optional, default `127.0.0.1:7800`)

---

## Source references

- `crates/nexus/src/lib.rs`
- `crates/nexus/src/main.rs`
- `crates/murmur-cli/src/registry_client.rs`
- `crates/murmur-artifact/src/registry.rs`
