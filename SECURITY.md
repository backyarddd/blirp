# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: **Security > Report a vulnerability** on https://github.com/backyarddd/blirp (private vulnerability reporting). Do not open a public issue.

Include what you found, how to reproduce it, the blirp version (`blirp --version`) and OS. We aim to acknowledge reports within 3 working days and to ship a fix or mitigation for confirmed high-severity issues within 30 days, and we will credit you in the release notes unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes. The desktop app updates itself; standalone installs should upgrade to the latest release.

## Scope

In scope, for example:

- Access to the daemon API, terminals or data by another local user, a web page in your browser (CSRF, DNS rebinding, cross-origin WebSocket), or a device on the network.
- Bypassing pairing, device login or machine/device revocation; impersonating a hub or node.
- Secrets that survive redaction into storage, sync, summaries or logs.
- Code execution or file writes outside the documented, opt-in `blirp hooks install` flow; path traversal in the files API.
- The desktop shell exposing native capabilities to the web UI or to other origins; update signature bypass.

Out of scope: attacks that require an already compromised user account on the machine running blirp (it runs as you, with your agents' privileges), behaviour of the agent CLIs themselves, and findings in dependencies without an impact on blirp (please report those upstream).

## How blirp protects your data

- The daemon binds `127.0.0.1` only. Clients authenticate with a random 256-bit token stored in `~/.blirp/runtime.json` (mode 0600 on Unix; inherits the profile ACL on Windows). Browsers get it as an HttpOnly, `SameSite=Strict` cookie via a one-time `/auth?token=` link.
- Mutating requests and WebSocket upgrades from browsers must come from the same origin; responses carry a strict Content-Security-Policy, `X-Frame-Options: DENY` and `nosniff`.
- Transcript text is redacted (gitleaks-style rules) before it is stored, synced or summarized. Logs never contain transcript text or secrets.
- Sync runs over QUIC with each machine's own key; pairing uses single-use short codes with SPAKE2, and only known, non-revoked machines are accepted afterwards.
- The desktop app renders only the local UI and its bundled loading page, grants no native (IPC) capabilities to the UI's origin, and opens every other link in your browser. Updates are downloaded from GitHub Releases and installed only if their minisign signature matches the public key built into the app.
