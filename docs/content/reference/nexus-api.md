# Nexus API

Nexus is the artifact registry that `mur` publishes to and installs from in remote registry mode. Pointing `mur` at an instance is covered in [Registry selection rules](config.md#registry-selection-rules).

Base path: `/v1`. Every request `mur` makes carries a 30-second total timeout.

Authentication is required on every endpoint:

```http
Authorization: Bearer <NEXUS_API_KEY>
```

A missing or invalid key returns `401 {"error":"unauthorized"}`. `mur` treats `401` and `403` alike and fails with `E-IO-003` and the message `registry authentication failed for <url>. Check NEXUS_API_KEY and try again.` The codes named on this page are indexed in [Diagnostics](diagnostics.md).

---

## `POST /v1/artifacts` { #publish }

Publish artifact bytes. Used by `mur publish`.

**Query parameters**

| Name | Required | Notes |
|---|---:|---|
| `name` | yes | Artifact name |
| `version` | yes | Artifact version. `latest`, `stable` and `edge` are reserved |
| `runtime` | yes | `wasm`, `native` or `static` |
| `platform` | no | Platform tag for native artifacts, in `os-arch` form (e.g. `darwin-aarch64`). Repeatable for multiple variants |

**Request body**

Raw `.mur.zip` bytes, `application/octet-stream`.

**Success — `201 Created`**

```json
{
  "artifact_id": "name@version",
  "sha256": "..."
}
```

**Error responses**

| Status | Condition | `mur` reports |
|---|---|---|
| `400 Bad Request` | Bad input, invalid runtime, or invalid platform | `E-IO-003` |
| `409 Conflict` | `name@version` already exists | `E-REG-003` |
| `422 Unprocessable Entity` | Reserved version. The body must read `{"error":"reserved artifact version '<version>' is not allowed"}` for `mur` to recognize it as one | `E-REG-004` |

---

## `GET /v1/artifacts/{name}/{version}` { #download }

Download artifact bytes.

**Query parameters**

| Name | Required | Notes |
|---|---:|---|
| `platform` | no | `os-arch` string (e.g. `darwin-aarch64`). Nexus returns the platform-specific variant when one exists, and the generic (untagged) file as a fallback. When absent, the generic file is returned |

**Success — `200 OK`**

Raw artifact bytes, with two headers:

| Header | Value |
|---|---|
| `Content-Type` | `application/octet-stream` |
| `x-murmur-sha256` | SHA-256 of the body |

`x-murmur-sha256` is mandatory. `mur` fails the download when it is absent, and hashes the bytes it received against it; a mismatch is `E-REG-002`.

**Error responses**

| Status | Condition | `mur` reports |
|---|---|---|
| `401 Unauthorized` | Missing or invalid key | `E-IO-003` |
| `404 Not Found` | No such artifact, or no variant for the requested platform. The error body names the requested platform: `{"error":"artifact 'name@version' has no variant for platform 'os-arch'"}` | `E-REG-001` |

---

## `GET /v1/artifacts` { #index }

List registry index metadata. Returns `200 OK` and a JSON array of objects:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | Artifact name |
| `version` | string | yes | Artifact version |
| `runtime` | string | yes | `wasm`, `native` or `static` |
| `artifact_runtime` | string | yes | The artifact's [`runtime`](manifest.md) role — `tool`, `hook`, `driver`, `skill` |
| `platforms` | list<list<string>> | no | Platform variants as `[os, arch]` pairs, e.g. `[["darwin","aarch64"]]`. Default: `[]` |
| `description` | string \| null | no | Default: `null` |
| `tags` | list<string> | no | Default: `[]` |

An object that omits `name`, `version`, `runtime` or `artifact_runtime` fails to parse, and `mur` reports `E-IO-003`.

---

## Server environment { #server-environment }

| Variable | Required | Notes |
|---|---:|---|
| `NEXUS_API_KEY` | yes | The bearer token clients must present |
| `NEXUS_BIND` | no | Address the server binds. Default: `127.0.0.1:7800` |
