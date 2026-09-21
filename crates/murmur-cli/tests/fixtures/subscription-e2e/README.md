# Fixtures for `process_subscription_e2e.rs`

`mock.py` is a copy of the scripted inference endpoint used to record the Claude Code streams,
taken from the roadmap fixtures at `.nexus/roadmap/fixtures/tools/mock.py`. It lives here as well
because `.nexus/` is not tracked by git: a clone, or a worktree, has no copy of it, and the test
that needs it must be runnable from either. Point `MURMUR_E2E_MOCK_SCRIPT` at another copy to run
against a newer one without editing this file.

It speaks Anthropic Messages at `/v1/messages`, logs every request body into `$MOCK_LOG` as
numbered JSON files, and reads `$MOCK_SCEN` on every request, so a test can change its behaviour
mid-run by rewriting one file. The scenarios the test uses are `text`, `call:<json>`, `slow`,
`401` and `429`.

Only the endpoint is redirected, never the credential. `ANTHROPIC_API_KEY` cannot reach a capsule
— `capabilities.env.allow` refuses credential-shaped names with `E-CAP-016` — so the test copies
the operator's `~/.claude/.credentials.json` into its scratch `HOME` and lets `ANTHROPIC_BASE_URL`
alone do the pointing. The turns therefore cost nothing and still report `auth: subscription`.
Point `MURMUR_E2E_LOGIN_FILE` at another login store to run as a different account.

`murmur.yaml` is the nexus capsule manifest, byte for byte. The test installs
it to prove `mur install` resolves `murmur-driver-claude-code` from the local artifact store.
