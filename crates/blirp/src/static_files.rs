//! Embedded SPA (`web/dist`) with index.html fallback for client routes.
//! `web/dist` may be missing (fresh checkout): the crate still compiles
//! (`allow_missing`) and serves a built-in page explaining how to build the UI.
//! Debug builds read `web/dist` from disk at runtime; release builds embed it.

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::RustEmbed)]
// Relative to this crate's Cargo.toml (`$VAR` paths need rust-embed's
// `interpolate-folder-path` feature; without it they silently resolve to nothing).
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    embedded(path)
}

fn embedded(path: &str) -> Response {
    let (file, name) = match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (f, path),
        // Client-side routes (no file extension) get the SPA shell.
        None if !path.rsplit('/').next().unwrap_or("").contains('.') => {
            match Assets::get("index.html") {
                Some(f) => (f, "index.html"),
                None => return not_built(),
            }
        }
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let mime = file.metadata.mimetype().to_string();
    let cache = if name.starts_with("assets/") {
        // Vite fingerprints everything under assets/.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut resp = file.data.into_owned().into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

const NOT_BUILT: &str = "<!doctype html>
<html lang=\"en\"><head><meta charset=\"utf-8\"><title>blirp</title>
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.5}
code{background:#8882;padding:.1rem .3rem;border-radius:.2rem}</style></head>
<body><h1>blirp is running</h1>
<p>The web UI was not built into this binary. Build it with
<code>pnpm -C web install &amp;&amp; pnpm -C web build</code>, then rebuild blirp.</p>
<p>The API is available under <code>/api/</code>.</p></body></html>";

fn not_built() -> Response {
    let mut resp = (StatusCode::OK, NOT_BUILT).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}
