//! Releases and self-update (docs/install.md): find a GitHub release, pick
//! this platform's assets, download them and verify them against the
//! minisign-signed `SHA256SUMS.txt`. `install` owns the files on disk (install
//! receipt, replacing binaries, uninstall). Used by `blirp update`,
//! `blirp uninstall` and `GET /api/update`.

pub mod install;

use anyhow::{Context as _, bail};
use reqwest::header::ACCEPT;
use semver::Version;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
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
    let base = base_url();
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
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("GET {url}: {status} {}", body.trim());
    }
    resp.json()
        .await
        .with_context(|| format!("unexpected answer from {url}"))
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

type Checked = (Instant, Option<Arc<Latest>>);

/// Last answer of [`latest_cached`]; one check per day (per hour after a failure).
static LATEST: tokio::sync::Mutex<Option<Checked>> = tokio::sync::Mutex::const_new(None);

/// The latest release, asking GitHub at most once a day. None when the check
/// fails (logged).
pub async fn latest_cached() -> Option<Arc<Latest>> {
    const OK_TTL: Duration = Duration::from_secs(24 * 3600);
    const ERR_TTL: Duration = Duration::from_secs(3600);
    // Held across the request so concurrent callers share one check.
    let mut slot = LATEST.lock().await;
    if let Some((at, latest)) = slot.as_ref() {
        let ttl = if latest.is_some() { OK_TTL } else { ERR_TTL };
        if at.elapsed() < ttl {
            return latest.clone();
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let release = fetch_release(&http()?, None).await?;
        anyhow::Ok(Latest {
            version: release.version()?,
            notes_url: release.html_url,
        })
    })
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
    let latest = match result {
        Ok(l) => Some(Arc::new(l)),
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "update check failed");
            None
        }
    };
    *slot = Some((Instant::now(), latest.clone()));
    latest
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
    fn release_key_is_the_shipped_key() {
        assert_eq!(
            key_line(RELEASE_KEY_FILE),
            "RWQUGAux2LF3nsIhgzZsZL6OfhV6O3mpN1jUyApoyh04lnSVLsoZ2NX5"
        );
        assert!(minisign_verify::PublicKey::from_base64(key_line(RELEASE_KEY_FILE)).is_ok());
    }
}
