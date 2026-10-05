//! The bundled React assets, embedded at compile time from the build
//! manifest.
//!
//! `ui/dist/ui-manifest.json` names every file under `/ui/assets/*` with its
//! content type and byte count. The manifest is embedded with `include_str!`;
//! the bytes come from a constant table generated to match the manifest
//! exactly, one `include_bytes!` per entry. Every literal path stays inside
//! the root crate, so `scripts/check-crate-includes.sh` stays green.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::http::HttpState;

/// The build manifest, embedded at compile time. The frontend build writes
/// it; this table and the bytes below must change with it.
const MANIFEST: &str = include_str!("../../ui/dist/ui-manifest.json");

/// The asset bytes, one row per manifest entry, generated to match the
/// manifest exactly. The name is the path after `/ui/assets/`.
const UI_ASSET_BYTES: &[(&str, &[u8])] = &[
    (
        "index-C3TUkMSs.js",
        include_bytes!("../../ui/dist/assets/index-C3TUkMSs.js"),
    ),
    (
        "index-CGyvWhOu.css",
        include_bytes!("../../ui/dist/assets/index-CGyvWhOu.css"),
    ),
    (
        "page-C3KEu0l_.css",
        include_bytes!("../../ui/dist/assets/page-C3KEu0l_.css"),
    ),
    (
        "page-QzbbjlrZ.js",
        include_bytes!("../../ui/dist/assets/page-QzbbjlrZ.js"),
    ),
];

/// The serving metadata of one built asset, read from the manifest.
struct AssetMeta {
    content_type: String,
    bytes: usize,
}

/// Register the asset route. Unknown names answer 404 — the manifest is the
/// checked list, and a name outside it is a broken bundle.
///
/// The manifest is parsed here, at route build: a manifest that does not
/// parse is a packaging defect — a binary cannot serve assets it claims to
/// carry — so the failure must surface at startup, not on the first asset
/// request.
pub fn attach(router: Router<HttpState>) -> Router<HttpState> {
    assets();
    router.route("/ui/assets/{*name}", get(asset_handler))
}

/// The embedded manifest, parsed once into name → metadata order the handler
/// looks up.
fn assets() -> &'static Vec<(String, AssetMeta)> {
    static ASSETS: std::sync::LazyLock<Vec<(String, AssetMeta)>> = std::sync::LazyLock::new(|| {
        serde_json::from_str::<serde_json::Value>(MANIFEST)
            .expect("the embedded UI manifest is valid JSON")["files"]
            .as_object()
            .expect("the manifest carries a files map")
            .into_iter()
            .map(|(path, meta)| {
                Ok((
                    path.strip_prefix("/ui/assets/")
                        .expect("a manifest asset path keeps its prefix")
                        .to_owned(),
                    AssetMeta {
                        content_type: meta["contentType"]
                            .as_str()
                            .expect("a manifest content type")
                            .to_owned(),
                        bytes: meta["bytes"].as_u64().expect("a manifest byte count") as usize,
                    },
                ))
            })
            .collect::<std::result::Result<Vec<_>, serde_json::Error>>()
            .expect("the embedded UI manifest has the expected shape")
    });
    &ASSETS
}

/// `GET /ui/assets/{name}` — one bundled file, with the manifest content
/// type and byte count. The hashed names are immutable, so the reply is
/// cacheable for a year.
async fn asset_handler(
    State(_state): State<HttpState>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    let meta = assets()
        .iter()
        .find(|(path, _)| *path == name)
        .map(|(_, meta)| meta);
    let Some(meta) = meta else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(bytes) = UI_ASSET_BYTES
        .iter()
        .find(|(path, _)| *path == name)
        .map(|(_, bytes)| bytes)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, meta.content_type.clone()),
            (header::CONTENT_LENGTH, meta.bytes.to_string()),
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_string(),
            ),
        ],
        Bytes::from_static(bytes),
    )
        .into_response()
}
