# Web portal

The blirp UI is a web app served by the daemon. The desktop app shows it in a native window, and `blirp open` opens it in your browser; both use the local listener on `127.0.0.1`. A **hub** can additionally serve the same UI over HTTPS on the local network, so a phone, tablet or another computer can watch sessions, read memory and, if allowed, type into terminals.

## Local access

- The daemon listens on `127.0.0.1:<port>` only (default 47770; it does not start when that port is taken; the real port is in `~/.blirp/runtime.json` and `blirp status`).
- `blirp open` opens `http://127.0.0.1:<port>/#token=<runtime token>`. The fragment never reaches a server; the UI stores the token for its own origin, removes it from the address bar and sends it as a bearer token (WebSockets use single-use tickets). No cookie is set: a cookie for `127.0.0.1` would go to every other local server too. The desktop app does the same inside its window.
- The runtime token is regenerated at every daemon start, so after a daemon restart run `blirp open` again (the desktop app re-authenticates by itself).

## LAN portal (hub only)

The portal runs when the machine is the hub **and** `portal.lan = true`:

1. Make the machine the hub ([sync-and-hub.md](sync-and-hub.md)).
2. Turn on **Settings > Portal > Serve the portal on the local network** (or set `[portal] lan = true` in `config.toml`), optionally change **HTTPS port** (default 47771).
3. Restart the daemon (`blirp service` users: `launchctl kickstart -k gui/$(id -u)/dev.blirp.daemon`, `systemctl --user restart blirp`, or log out and in on Windows; otherwise quit and start blirp). The portal is started and stopped at daemon start and when the hub is enabled or disabled; toggling the setting alone does not start or stop a running portal.
4. Allow inbound TCP on the port in the hub's firewall if one is active (e.g. `sudo ufw allow 47771/tcp`; macOS asks on first use).

**Settings > Portal** and `blirp hub status` then show the **Address** (`https://<LAN IP>:47771`) and the **Certificate fingerprint**.

### Certificate

The portal uses a self-signed certificate generated on first start and stored in `~/.blirp/tls/` (`cert.pem`, `key.pem` with mode 0600), valid for `localhost`, `127.0.0.1` and the LAN IP the hub had at that time. Browsers warn about it. Before accepting it, compare the SHA-256 fingerprint your browser shows (certificate details) with the one in **Settings > Portal**; if they match, you are talking to your hub. If the hub's LAN IP changed, delete `~/.blirp/tls/` and restart to get a certificate for the new address (the fingerprint changes; browsers will warn again).

TLS 1.2 and 1.3 only, HTTP/1.1 (needed for the terminal WebSockets).

### Signing in a phone or browser

Devices never see the runtime token. They sign in with a one-time link:

1. On a screen that is already signed in (the hub's desktop app, `blirp open` on the hub, or another signed-in portal device), open **Settings > Portal > Sign in a phone or browser > Show login QR**.
2. Scan the QR code with the phone (or open the link shown under it). It is `https://<LAN IP>:<port>/device-login?invite=<token>`, valid for 5 minutes and usable once.
3. Accept the certificate after checking the fingerprint. The browser gets a device cookie (`blirp_device`: HttpOnly, Secure, `SameSite=Strict`, 400 days) and lands in the UI.

The device appears under **Settings > Machines & Sync > Devices** named after its browser and OS (e.g. "Safari on iPhone"). Login attempts are limited to 10 per minute per client IP. The server stores only SHA-256 hashes of invites and device tokens.

### What a browser device may do

| | Browser device | With **Terminal control** on |
|---|---|---|
| Read projects, sessions, transcripts, summaries, memory, search | yes | yes |
| Browse project files (read-only) and git status/diffs | yes | yes |
| Edit memory (brief, records, wiki, resources, suggestions), rename and register projects | yes | yes |
| Change settings (`PATCH /api/settings`) and create new device login links | yes | yes |
| Start, stop, resume sessions; distill a session; type into terminals and resize them | no (403 `control_not_allowed`; terminals are view-only) | yes, also for sessions on other paired machines (relayed by the hub) |
| Pair machines, enable/disable the hub, invite, revoke or change devices | no (403 `admin_only`) | no |
| Install global hooks, open folders/editors on the hub, stop the daemon, use `/mcp` | no | no |

New devices start without terminal control. Toggle **Terminal control** or **Revoke** a device under **Settings > Machines & Sync > Devices** on the hub (or `blirp devices list|revoke`). Revoking or changing a device closes its open WebSockets at once, so it continues only with its new rights.

Treat a signed-in device like a logged-in session on the hub: it can read all your synced history and change settings. Revoke devices you no longer use.

## Tailscale

For access from outside the LAN, keep the portal off the internet and use Tailscale.

- **Directly over the tailnet:** the portal listens on all interfaces, so tailnet devices can reach it at `https://<hub tailnet IP or MagicDNS name>:47771` (self-signed certificate, same fingerprint check). To sign in, take the token from a login link and open `https://<hub tailnet name>:47771/device-login?invite=<token>`.
- **With a trusted certificate:** put `tailscale serve` in front of the portal:
  ```sh
  tailscale serve --bg https+insecure://127.0.0.1:47771
  ```
  The UI is then at `https://<hub>.<tailnet>.ts.net/` with a valid certificate, for tailnet devices only. Sign in by opening `https://<hub>.<tailnet>.ts.net/device-login?invite=<token>` with the token from **Show login QR**. `tailscale serve reset` removes it. Never use `tailscale funnel`, which publishes to the internet.

Do not point `tailscale serve` at the daemon's local port (47770): that listener accepts only the runtime token, which grants full local rights (pairing, device management, daemon shutdown) and changes on every restart. Browser devices are designed for the portal port.

## Security headers

All responses carry a Content-Security-Policy (scripts from the same origin only, no inline scripts, `connect-src` limited to the same host), `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`; API responses are `Cache-Control: no-store`. Mutating requests and WebSocket upgrades with an `Origin` header must come from the same host (CSRF protection). See [security.md](security.md).
