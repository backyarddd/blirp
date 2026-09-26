# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: **Security > Report a vulnerability** on https://github.com/backyarddd/blirp (private vulnerability reporting). Do not open a public issue.

Include what you found, how to reproduce it, the blirp version (`blirp --version`) and OS. We aim to acknowledge reports within 3 working days and to ship a fix or mitigation for confirmed high-severity issues within 30 days, and we will credit you in the release notes unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes. Upgrade with `blirp update` (it verifies the release signature and checksums; see docs/install.md).

## Scope

In scope, for example:

- Access to the daemon API, terminals or data by another local user, a web page in your browser (CSRF, DNS rebinding, cross-origin WebSocket), or a device on the network.
- Bypassing pairing, device login or machine/device revocation; impersonating a hub or node.
- Secrets in a format the redaction rules are meant to cover that still reach storage, sync, summaries or logs, and any transcript text in logs.
- Code execution or file writes outside the documented, opt-in `blirp hooks install` flow; path traversal in the files API.
- The desktop shell exposing native capabilities to the web UI or to other origins; update signature bypass.

Out of scope: attacks that require an already compromised user account on the machine running blirp (it runs as you, with your agents' privileges), behaviour of the agent CLIs themselves, and findings in dependencies without an impact on blirp (please report those upstream).

## How blirp protects your data

Summary; the full threat model, storage, network exposure and limits are in [docs/security.md](docs/security.md).

- The daemon binds `127.0.0.1` only. Clients authenticate with a random 256-bit token stored in `~/.blirp/runtime.json` (mode 0600 inside a 0700 data directory on macOS/Linux; the profile ACL on Windows), regenerated at every start. The CLI and the desktop app hand it to the web UI in a URL fragment (`/#token=`, never sent to a server); the UI keeps it in its own origin's storage and sends it as a bearer token, and WebSockets use single-use tickets. The loopback listener accepts no cookies, and the daemon does not start when its port is taken.
- Mutating requests and WebSocket upgrades from browsers must come from the same origin; responses carry a strict Content-Security-Policy, `X-Frame-Options: DENY` and `nosniff`.
- Transcript text is redacted (gitleaks-style rules) before it is stored, synced or summarized. Logs never contain transcript text or secrets.
- Nothing listens on the network unless you make a machine a hub (QUIC sync endpoint) and enable its LAN portal (HTTPS, one-time device login links, per-device permissions).
- Sync runs over QUIC with each machine's own key; pairing uses single-use short codes with SPAKE2 and key confirmation, and only known, non-revoked machines are accepted afterwards.
- The desktop app renders only the local UI and its bundled loading page, grants no native (IPC) capabilities to the UI's origin, and opens every other link in your browser. Updates are installed only if their minisign signature matches the public key built into the app.
