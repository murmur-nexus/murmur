# Security Policy

Murmur's whole reason to exist is that an agent can only reach what its capsule declares — the sandbox, capability, and network-policy code is the thing keeping that promise. If you've found a way around it, we want to hear from you.

## Supported versions

Murmur is pre-1.0 and moves fast. We only support the latest released version — please reproduce on the newest `mur` release before reporting, and expect fixes to land there rather than as backports.

## Reporting a vulnerability

**Please don't open a public issue, PR, or Discord message for security bugs.** Report privately, one of two ways:

- **GitHub** (preferred): open a private report via the repo's [Security → Report a vulnerability](https://github.com/murmur-nexus/murmur/security/advisories/new) page. This keeps the discussion private and lets us credit you cleanly.
- **Email**: `murmurnexus` \at\ gmail.com. Use the subject line `SECURITY` so it doesn't get lost.

A good report includes:

- The version (`mur --version`) and platform (macOS/Linux, arch).
- What boundary you crossed — sandbox escape, a capability or network-allowlist bypass, an artifact/manifest that runs something it didn't declare, supply-chain or install-path issues, etc.
- A minimal capsule, manifest, or command sequence that reproduces it, and what you expected to be blocked vs. what actually happened.

You don't need to have a fix, and you don't need to be certain it's exploitable — if something looks like it lets an agent reach beyond what was granted, send it.

## What to expect

- **Acknowledgement** within 3 business days.
- An initial assessment and a severity call within about a week, and we'll keep you posted as we work a fix.
- Once a fix is released, we'll publish an advisory and credit you by name (or keep you anonymous — your call).

Please give us a reasonable window to ship a fix before disclosing publicly. We're a small team and we'll move as fast as we can; if you feel a report is being neglected, escalate by email.

## Scope

In scope: anything in this repository — the `mur` CLI, `capsule-runtime`, the sandbox/capability/network-policy layers, the install script served from `install.murmur.rs`, and the release artifacts.

Out of scope: vulnerabilities in third-party dependencies (report those upstream, though a heads-up is welcome if we're pinning a bad version), and issues that require an already-compromised host or a capsule the operator explicitly granted the capability in question — Murmur enforces what you declare, it doesn't second-guess what you deliberately allow.

Thanks for helping keep Murmur trustworthy.
