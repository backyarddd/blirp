//! Releases and self-update (docs/install.md): find a GitHub release, pick
//! this platform's assets, download them and verify them against the
//! minisign-signed `SHA256SUMS.txt`. `install` owns the files on disk (install
//! receipt, replacing binaries, uninstall). Used by `blirp update`,
//! `blirp uninstall` and the daemon's `/api/update` routes.

pub mod install;

use anyhow::{Context as _, bail};
use blirp_core::model::UpdateOutcome;
use blirp_core::paths::Paths;
use reqwest::header::ACCEPT;
use semver::Version;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt as _;

/// GitHub releases API of the blirp repository.
pub const RELEASES_API: &str = "https://api.github.com/repos/backyarddd/blirp/releases";
/// Replaces [`RELEASES_API`] (mirrors, tests). The install scripts read it too.
pub const BASE_URL_ENV: &str = "BLIRP_RELEASE_BASE_URL";
/// Sent as a bearer token when set (private repository, rate limits).
pub const TOKEN_ENV: &str = "GITHUB_TOKEN";
pub const SUMS: &str = "SHA256SUMS.txt";
pub const SUMS_SIG: &str = "SHA256SUMS.txt.sig";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// minisign public key of the release key (the former Tauri updater key; its
/// secret half is the `TAURI_SIGNING_PRIVATE_KEY` release secret).
const RELEASE_KEY_FILE: &str = include_str!("../../../../packaging/minisign.pub");

/// The key `SHA256SUMS.txt.sig` must verify with. A build with
/// `BLIRP_UPDATE_PUBKEY=<key line>` in its environment trusts that key
/// instead (test builds against a fake release); it is fixed at compile time,
/// never read at run time.
pub fn release_key() -> &'static str {
    option_env!("BLIRP_UPDATE_PUBKEY").unwrap_or_else(|| key_line(RELEASE_KEY_FILE))
}

/// The base64 key line of a minisign `.pub` file.
fn key_line(file: &str) -> &str {
    file.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .unwrap_or("")
}

pub fn http() -> anyhow::Result<reqwest::Client> {
    crate::install_crypto_provider();
    Ok(reqwest::Client::builder()
        .user_agent(concat!("blirp/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()?)
}

fn token() -> Option<String> {
    std::env::var(TOKEN_ENV)
        .ok()
        .filter(|t| !t.trim().is_empty())
}

fn base_url() -> String {
    std::env::var(BASE_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| RELEASES_API.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn authed(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match token() {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Asset {
    pub name: String,
    /// API URL; with a token it serves the bytes for `Accept: application/octet-stream`.
    pub url: String,
    pub browser_download_url: String,
}

impl Release {
    pub fn version(&self) -> anyhow::Result<Version> {
        parse_version(&self.tag_name)
    }

    pub fn asset(&self, name: &str) -> anyhow::Result<&Asset> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("release {} has no asset {name}", self.tag_name))
    }
}

pub fn parse_version(s: &str) -> anyhow::Result<Version> {
    let v = s.trim();
    Version::parse(v.strip_prefix('v').unwrap_or(v))
        .with_context(|| format!("not a version: {s:?}"))
}

/// The latest published (non-draft, non-prerelease) release, or tag `v<version>`.
pub async fn fetch_release(
    client: &reqwest::Client,
    version: Option<&Version>,
) -> anyhow::Result<Release> {
    fetch_release_from(client, &base_url(), version).await
}

/// [`fetch_release`] from the releases API at `base`.
pub async fn fetch_release_from(
    client: &reqwest::Client,
    base: &str,
    version: Option<&Version>,
) -> anyhow::Result<Release> {
    let url = match version {
        Some(v) => format!("{base}/tags/v{v}"),
        None => format!("{base}/latest"),
    };
    let resp = authed(client.get(&url))
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        let what = match version {
            Some(v) => format!("release v{v} not found"),
            None => "no published release found".to_string(),
        };
        let hint = if token().is_some() {
            ""
        } else {
            " (for a private repository set GITHUB_TOKEN)"
        };
        bail!("{what} at {base}{hint}");
    }
    if rate_limited(status, resp.headers()) {
        let hint = if token().is_some() {
            ""
        } else {
            "; set GITHUB_TOKEN to raise the limit"
        };
        bail!("the GitHub API rate limit is used up, try again later{hint}");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("GET {url}: {status} {}", body.trim());
    }
    resp.json()
        .await
        .with_context(|| format!("unexpected answer from {url}"))
}

/// GitHub answers an exhausted rate limit with 403 or 429 and
/// `x-ratelimit-remaining: 0`.
fn rate_limited(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> bool {
    matches!(status.as_u16(), 403 | 429)
        && headers
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v.as_bytes() == b"0")
}

/// Download an asset to `dest`; returns its SHA-256 (lowercase hex).
pub async fn download(
    client: &reqwest::Client,
    asset: &Asset,
    dest: &Path,
) -> anyhow::Result<String> {
    // With a token the API URL works for private repositories too; reqwest
    // drops the Authorization header on the redirect to the storage host.
    let req = match token() {
        Some(t) => client
            .get(&asset.url)
            .bearer_auth(t)
            .header(ACCEPT, "application/octet-stream"),
        None => client.get(&asset.browser_download_url),
    };
    let mut resp = req
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("download {}", asset.name))?;
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("download {}", asset.name))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("write {}", dest.display()))?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(hex::encode(hasher.finalize()))
}

/// `sha256sum` output: file name -> lowercase hex digest.
pub fn parse_sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (hash, name) = line.trim_end().split_once(char::is_whitespace)?;
            // `*name` marks binary mode in sha256sum output.
            let name = name.trim_start();
            let name = name.strip_prefix('*').unwrap_or(name);
            (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && !name.is_empty())
                .then(|| (name.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

/// Verify a minisign signature (`.sig` file text) over `data` with the
/// base64 public key line `key`.
pub fn verify_signature(data: &[u8], sig: &str, key: &str) -> anyhow::Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(key)
        .map_err(|e| anyhow::anyhow!("invalid release public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(sig)
        .map_err(|e| anyhow::anyhow!("invalid signature file: {e}"))?;
    pk.verify(data, &sig, false)
        .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))
}

/// Compare a downloaded asset with its line in the verified sums.
pub fn check_sha256(
    sums: &BTreeMap<String, String>,
    name: &str,
    actual: &str,
) -> anyhow::Result<()> {
    let Some(expected) = sums.get(name) else {
        bail!("{name} is not listed in {SUMS}");
    };
    if !expected.eq_ignore_ascii_case(actual) {
        bail!("checksum mismatch for {name}: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Release asset names for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetNames {
    /// CLI archive (`.tar.gz`, Windows `.zip`) ...
    pub cli: String,
    /// ... whose files sit in this top-level folder.
    pub cli_dir: String,
    /// Desktop app: macOS `.app.tar.gz`, Linux AppImage, Windows portable zip.
    pub app: String,
}

/// Names for `os`/`arch` as in `std::env::consts`; None when there is no build.
pub fn asset_names(os: &str, arch: &str, version: &Version) -> Option<AssetNames> {
    let v = version;
    let (triple, app) = match (os, arch) {
        ("linux", "x86_64") => (
            "x86_64-unknown-linux-gnu",
            format!("blirp_{v}_amd64.AppImage"),
        ),
        ("linux", "aarch64") => (
            "aarch64-unknown-linux-gnu",
            format!("blirp_{v}_aarch64.AppImage"),
        ),
        ("macos", "aarch64") => (
            "aarch64-apple-darwin",
            format!("blirp_{v}_aarch64.app.tar.gz"),
        ),
        ("macos", "x86_64") => ("x86_64-apple-darwin", format!("blirp_{v}_x64.app.tar.gz")),
        ("windows", "x86_64") => (
            "x86_64-pc-windows-msvc",
            format!("blirp_{v}_x64-portable.zip"),
        ),
        _ => return None,
    };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    Some(AssetNames {
        cli: format!("blirp-{v}-{triple}.{ext}"),
        cli_dir: format!("blirp-{v}-{triple}"),
        app,
    })
}

pub fn this_platform(version: &Version) -> anyhow::Result<AssetNames> {
    asset_names(std::env::consts::OS, std::env::consts::ARCH, version).with_context(|| {
        format!(
            "no release builds for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

/// The newest published release, as the daemon reports it.
#[derive(Debug, Clone)]
pub struct Latest {
    pub version: Version,
    pub notes_url: String,
}

/// One ask to GitHub.
#[derive(Debug, Clone)]
pub struct Checked {
    at: Instant,
    /// Unix ms of the ask, for clients.
    pub at_ms: i64,
    /// A failure is formatted for clients.
    pub latest: Result<Arc<Latest>, String>,
}

/// Starts the updater `cli args` with `dir` as data dir ([`start_updater`]).
pub type Spawner = Arc<dyn Fn(&Path, &[String], &Path) -> std::io::Result<()> + Send + Sync>;

/// Replacements for what the daemon's update routes reach outside the
/// process. Only tests set them.
#[derive(Clone, Default)]
pub struct Overrides {
    /// Releases API instead of [`BASE_URL_ENV`] / [`RELEASES_API`].
    pub base_url: Option<String>,
    /// Treat this as the installed CLI (self-update possible) instead of
    /// reading the install receipt.
    pub cli: Option<PathBuf>,
    /// Instead of starting the updater process.
    pub spawn: Option<Spawner>,
}

/// The daemon's update state (`/api/update*`).
#[derive(Default)]
pub struct Updates {
    /// Last ask to GitHub. Held across the request so concurrent callers
    /// share one check.
    checked: tokio::sync::Mutex<Option<Checked>>,
    /// When `POST /api/update/apply` last started the updater (unix ms).
    pub started_at: std::sync::Mutex<Option<i64>>,
    overrides: std::sync::RwLock<Overrides>,
}

/// A good answer is reused for a day, a failure for an hour.
const OK_TTL: Duration = Duration::from_secs(24 * 3600);
const ERR_TTL: Duration = Duration::from_secs(3600);
/// A forced check (`POST /api/update/check`) asks GitHub at most this often;
/// the anonymous API allows 60 requests an hour per address.
pub const FORCE_TTL: Duration = Duration::from_secs(60);

impl Updates {
    pub fn set_overrides(&self, o: Overrides) {
        *self
            .overrides
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = o;
    }

    pub fn overrides(&self) -> Overrides {
        self.overrides
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The latest release: the last answer while it is fresh (a day, an
    /// hour after a failure; `force` shortens both to [`FORCE_TTL`]), else a
    /// new ask to GitHub. A failure is logged and kept as the answer.
    pub async fn latest(&self, force: bool) -> Checked {
        let mut slot = self.checked.lock().await;
        if let Some(c) = slot.as_ref() {
            let ttl = match (&c.latest, force) {
                (_, true) => FORCE_TTL,
                (Ok(_), false) => OK_TTL,
                (Err(_), false) => ERR_TTL,
            };
            if c.at.elapsed() < ttl {
                return c.clone();
            }
        }
        let base = self.overrides().base_url.unwrap_or_else(base_url);
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let release = fetch_release_from(&http()?, &base, None).await?;
            anyhow::Ok(Latest {
                version: release.version()?,
                notes_url: release.html_url,
            })
        })
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("GitHub did not answer within 20 s")));
        let latest = result.map(Arc::new).map_err(|e| {
            let e = format!("{e:#}");
            tracing::warn!(error = %e, "update check failed");
            e
        });
        let c = Checked {
            at: Instant::now(),
            at_ms: blirp_core::now_ms(),
            latest,
        };
        *slot = Some(c.clone());
        c
    }
}

/// Where `blirp update` records its install attempts, one JSON
/// [`UpdateOutcome`] per line; the daemon reports the last one.
pub fn outcome_log(paths: &Paths) -> PathBuf {
    paths.logs_dir().join("update.log")
}

pub fn record_outcome(paths: &Paths, outcome: &UpdateOutcome) -> anyhow::Result<()> {
    use std::io::Write as _;
    let path = outcome_log(paths);
    std::fs::create_dir_all(paths.logs_dir())
        .with_context(|| format!("create {}", paths.logs_dir().display()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{}", serde_json::to_string(outcome)?)
        .with_context(|| format!("write {}", path.display()))
}

/// The last readable line of the outcome log; None without one.
pub fn last_outcome(paths: &Paths) -> Option<UpdateOutcome> {
    let text = match std::fs::read_to_string(outcome_log(paths)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(error = %e, "read the update log");
            return None;
        }
    };
    text.lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
}

/// The updater's arguments: `update --version <version>`.
pub fn updater_args(version: &Version) -> Vec<String> {
    vec![
        "update".to_string(),
        "--version".to_string(),
        version.to_string(),
    ]
}

/// `systemd-run --user` arguments that run `cli args` in a transient unit
/// of its own. Stopping the daemon's service kills everything in the
/// service's cgroup, the updater included; `KillMode=process` keeps a
/// `daemon --detach` the updater starts (when the service cannot) alive
/// once the updater's own unit ends. `--setenv=NAME` copies a variable from
/// the environment `systemd-run` gets; `present` says which are set.
pub fn systemd_run_args(
    cli: &Path,
    args: &[String],
    present: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut out: Vec<String> = [
        "--user",
        "--collect",
        "--quiet",
        "--property=KillMode=process",
        "--description=blirp update",
    ]
    .map(String::from)
    .to_vec();
    for name in [
        blirp_core::paths::HOME_ENV,
        "PATH",
        "XDG_DATA_HOME",
        TOKEN_ENV,
        BASE_URL_ENV,
    ] {
        if present(name) {
            out.push(format!("--setenv={name}"));
        }
    }
    out.push("--".to_string());
    out.push(cli.display().to_string());
    out.extend(args.iter().cloned());
    out
}

/// Start the updater `cli args` so that it outlives this daemon, which it
/// stops; `dir` is the data dir. On Linux under a systemd service
/// (`INVOCATION_ID`) through `systemd-run`, which returns once the unit
/// started (its failure is this error); without `systemd-run`, and
/// elsewhere, as a detached process like `daemon --detach`. Blocking.
pub fn start_updater(cli: &Path, args: &[String], dir: &Path) -> std::io::Result<()> {
    if cfg!(target_os = "linux") && std::env::var_os("INVOCATION_ID").is_some() {
        let wrapped = systemd_run_args(cli, args, |n| std::env::var_os(n).is_some());
        match blirp_core::process::command("systemd-run")
            .args(&wrapped)
            .env(blirp_core::paths::HOME_ENV, dir)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                tracing::info!(updater = %cli.display(), "updater started with systemd-run");
                return Ok(());
            }
            Ok(out) => {
                return Err(std::io::Error::other(format!(
                    "systemd-run failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("systemd-run not found; starting the updater directly");
            }
            Err(e) => return Err(e),
        }
    }
    let pid = crate::daemon::spawn_detached(cli, args, dir)?;
    tracing::info!(pid, updater = %cli.display(), "updater started");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_semver() {
        let v = |s| parse_version(s).unwrap();
        assert!(v("v0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert_eq!(v(" v0.1.0 "), v("0.1.0"));
        assert!(parse_version("latest").is_err());
        assert!(parse_version("v1.2").is_err());
        assert!(parse_version(CURRENT).is_ok());
    }

    #[test]
    fn asset_names_per_platform() {
        let v = parse_version("0.2.0").unwrap();
        let n = |os, arch| asset_names(os, arch, &v).unwrap();
        assert_eq!(
            n("linux", "x86_64"),
            AssetNames {
                cli: "blirp-0.2.0-x86_64-unknown-linux-gnu.tar.gz".into(),
                cli_dir: "blirp-0.2.0-x86_64-unknown-linux-gnu".into(),
                app: "blirp_0.2.0_amd64.AppImage".into(),
            }
        );
        assert_eq!(
            n("linux", "aarch64").cli,
            "blirp-0.2.0-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(n("linux", "aarch64").app, "blirp_0.2.0_aarch64.AppImage");
        assert_eq!(
            n("macos", "aarch64").cli,
            "blirp-0.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(n("macos", "aarch64").app, "blirp_0.2.0_aarch64.app.tar.gz");
        assert_eq!(
            n("macos", "x86_64").cli,
            "blirp-0.2.0-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(n("macos", "x86_64").app, "blirp_0.2.0_x64.app.tar.gz");
        assert_eq!(
            n("windows", "x86_64"),
            AssetNames {
                cli: "blirp-0.2.0-x86_64-pc-windows-msvc.zip".into(),
                cli_dir: "blirp-0.2.0-x86_64-pc-windows-msvc".into(),
                app: "blirp_0.2.0_x64-portable.zip".into(),
            }
        );
        assert_eq!(asset_names("windows", "aarch64", &v), None);
        assert_eq!(asset_names("freebsd", "x86_64", &v), None);
        // The running platform is one we ship (CI covers all three OSes).
        assert!(this_platform(&v).is_ok());
    }

    #[test]
    fn sums_parse_sha256sum_output() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let text = format!(
            "{a}  blirp-0.2.0-x86_64-pc-windows-msvc.zip\n{b} *blirp_0.2.0_x64-portable.zip\r\nnot a line\n{}  short\n",
            "c".repeat(10)
        );
        let sums = parse_sums(&text);
        assert_eq!(sums.len(), 2);
        assert_eq!(sums["blirp-0.2.0-x86_64-pc-windows-msvc.zip"], a);
        assert_eq!(sums["blirp_0.2.0_x64-portable.zip"], "b".repeat(64));
        assert!(check_sha256(&sums, "blirp_0.2.0_x64-portable.zip", &b).is_ok());
        assert!(check_sha256(&sums, "blirp_0.2.0_x64-portable.zip", &a).is_err());
        assert!(check_sha256(&sums, "other.zip", &a).is_err());
    }

    /// Generated with the real Tauri signer (`tauri signer generate -p testpw`,
    /// `tauri signer sign`) on a throwaway key; the `.sig` it writes is base64
    /// of this minisign signature, which the release workflow decodes.
    const TEST_KEY: &str = "RWQioa1jNOXPBqmFatniQSb8EmJwhGEsbQXoDN74GUgDzr+JXAmnDEAY";
    const TEST_DATA: &str = "aaaa  blirp-0.2.0-x86_64-pc-windows-msvc.zip\n";
    const TEST_SIG: &str = "untrusted comment: signature from tauri secret key
RUQioa1jNOXPBqA0eq55EkPy99LGy6vif9LId8BYMIkTBMgjngA6SUoIiro7ehK5w0Wq0OR1uoBRFZCGclJ0vwd826sKly+S7AM=
trusted comment: timestamp:1790406316\tfile:SHA256SUMS.txt
U9d1YnP09dRsKTqDZBVlbrzr0GNVnDVjBx4vqKQqfwyTXTiIr3dIkL33LD0QhQ6UtqF3neyOI/DD6jVISVH6CA==
";

    #[test]
    fn minisign_signature_from_tauri_signer() {
        verify_signature(TEST_DATA.as_bytes(), TEST_SIG, TEST_KEY).unwrap();
        // Any change to the data, the signature or the key fails.
        assert!(verify_signature(b"aaab  x\n", TEST_SIG, TEST_KEY).is_err());
        let other_key = key_line(RELEASE_KEY_FILE);
        assert!(verify_signature(TEST_DATA.as_bytes(), TEST_SIG, other_key).is_err());
        let bad_sig = TEST_SIG.replace("U9d1", "U9d2");
        assert!(verify_signature(TEST_DATA.as_bytes(), &bad_sig, TEST_KEY).is_err());
        assert!(verify_signature(TEST_DATA.as_bytes(), "garbage", TEST_KEY).is_err());
    }

    #[test]
    fn updater_leaves_the_service_cgroup_under_systemd() {
        let cli = Path::new("/home/me/.local/bin/blirp");
        let args = updater_args(&parse_version("0.2.0").unwrap());
        assert_eq!(args, ["update", "--version", "0.2.0"]);
        assert_eq!(
            systemd_run_args(cli, &args, |n| n != TOKEN_ENV),
            [
                "--user",
                "--collect",
                "--quiet",
                "--property=KillMode=process",
                "--description=blirp update",
                "--setenv=BLIRP_HOME",
                "--setenv=PATH",
                "--setenv=XDG_DATA_HOME",
                "--setenv=BLIRP_RELEASE_BASE_URL",
                "--",
                "/home/me/.local/bin/blirp",
                "update",
                "--version",
                "0.2.0",
            ]
        );
    }

    #[test]
    fn outcome_log_keeps_the_last_attempt() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::at(root.path());
        assert_eq!(last_outcome(&paths), None);
        let outcome = |ok, at| UpdateOutcome {
            from: "0.1.0".into(),
            to: Some("0.2.0".into()),
            installed: ok,
            ok,
            error: (!ok).then(|| "disk full".to_string()),
            finished_at: at,
        };
        record_outcome(&paths, &outcome(false, 1)).unwrap();
        record_outcome(&paths, &outcome(true, 2)).unwrap();
        assert_eq!(last_outcome(&paths), Some(outcome(true, 2)));
        // A torn last line (a crash while writing) falls back to the one before.
        let mut text = std::fs::read_to_string(outcome_log(&paths)).unwrap();
        text.push_str("{\"from\":");
        std::fs::write(outcome_log(&paths), text).unwrap();
        assert_eq!(last_outcome(&paths), Some(outcome(true, 2)));
    }

    #[test]
    fn rate_limit_is_recognized() {
        use reqwest::StatusCode;
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut empty = HeaderMap::new();
        assert!(!rate_limited(StatusCode::FORBIDDEN, &empty));
        empty.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        assert!(rate_limited(StatusCode::FORBIDDEN, &empty));
        assert!(rate_limited(StatusCode::TOO_MANY_REQUESTS, &empty));
        assert!(!rate_limited(StatusCode::NOT_FOUND, &empty));
        empty.insert("x-ratelimit-remaining", HeaderValue::from_static("12"));
        assert!(!rate_limited(StatusCode::FORBIDDEN, &empty));
    }

    #[test]
    fn release_key_is_the_shipped_key() {
        assert_eq!(
            key_line(RELEASE_KEY_FILE),
            "RWR6n63Gj9buu2zM3R/x2t2GdHNFgEy1em/gM8aKhteWB+zielbybJVW"
        );
        assert!(minisign_verify::PublicKey::from_base64(key_line(RELEASE_KEY_FILE)).is_ok());
    }
}
