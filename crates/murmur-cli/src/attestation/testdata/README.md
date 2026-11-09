# Attestation fixtures

`cli-cli-v2.103.3-linux-amd64.sigstore.json` is a real GitHub artifact attestation, kept to check
`verify_bundle` against the built-in trusted root with no network.

| Field | Value |
|---|---|
| Repository | `cli/cli` (repository id 212613049) |
| Owner id | 59704711 |
| Release | `v2.103.3` |
| Asset checked | `gh_2.103.3_linux_amd64.tar.gz`, one of the statement's 21 subjects |
| Asset sha256 | `4c806978ffce1cec70853302a56d70ac6da4f19bae7adca4e1a8ab00633dd91d` |
| Signer | `https://github.com/cli/cli/.github/workflows/deployment.yml@refs/heads/trunk` |
| Rekor entry | `dsse` 0.0.1, log index 3494597069, integrated 2026-10-26T19:18:15Z |
| Fetched | 2026-11-08, `gh api repos/cli/cli/attestations/sha256:4c80…d91d`, the `slsa.dev/provenance/v1` attestation's `bundle_url` (snappy-compressed), decompressed |

The other attestation GitHub lists for that digest is the release attestation
(`in-toto.io/attestation/release/v0.2`), signed under GitHub's own trust root with a signed
timestamp and no Rekor entry; `mur` reports that form as unverifiable.
