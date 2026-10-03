//! Vision OCR provider: one rendered page per request to a chat endpoint.
//!
//! The `[ocr]` section of the server configuration file stays a plain value
//! here. Resolution happens when the extractor role starts, and a resolution
//! failure never stops the process: it produces a [`RejectedOcr`] provider
//! that fails every PDF job at stage `config`, while text extraction keeps
//! working with the same settings.

use std::sync::Arc;

use base64::Engine;

use thiserror::Error;

/// The model used when `[ocr] model` is absent. It is independent of the
/// `[indexer]` embedding model.
pub const DEFAULT_OCR_MODEL: &str = "gpt-4o-mini";

/// The default OpenAI vision endpoint. Only a first-party `openai` provider
/// may use it. The `[indexer] openai-url` is an embeddings endpoint and never
/// becomes the vision URL.
pub const DEFAULT_VISION_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

/// The host of the default OpenAI vision endpoint. An inherited
/// `openai-compatible` provider must not send its key to this host.
const DEFAULT_VISION_HOST: &str = "api.openai.com";

/// The maximum number of characters a vision response may contribute to one
/// page before the worker truncates it.
const MAX_PAGE_CHARS: usize = 100_000;

/// The vision request deadline. It is deliberately shorter than the
/// extractor's job lease: the worker renews its lease only after a request
/// returns, and a request that outlived the lease would fail the renewal
/// fence and discard the transcription. `pdf.rs` pins this invariant against
/// the lease and the render deadline in a test, so the two cannot drift
/// apart.
pub const VISION_TIMEOUT_US: i64 = 15_000_000;

#[derive(Debug, Error)]
pub enum OcrError {
    /// The OCR configuration is invalid. The job fails at stage `config` and
    /// is not retried.
    #[error("invalid OCR configuration: {0}")]
    Config(String),
    /// The vision endpoint failed. The job fails at stage `provider` and is
    /// retried.
    #[error("OCR provider request failed: {0}")]
    Provider(String),
}

/// One rendered page through a vision model. Implementations must be usable
/// from the worker's blocking thread.
pub trait OcrProvider: Send + Sync {
    fn transcribe_page(&self, image: &[u8], mime: &str) -> Result<String, OcrError>;
}

/// The raw `[ocr]` section. Unresolved, so a bad section fails a PDF job at
/// stage `config` instead of stopping a process whose text extraction works.
#[derive(Clone, Debug)]
pub struct OcrConfig {
    pub provider: String,
    pub model: Option<String>,
    pub vision_url: Option<String>,
    pub api_key_file: Option<String>,
}

/// The primary embedding provider summary the `inherit` rule reads. Derive it
/// from the effective `[indexer]` settings, so the vision key and the
/// embedding key are the same value and follow the same precedence.
#[derive(Clone, Debug, Default)]
pub struct PrimaryProvider {
    /// The profile's `provider_kind`: `openai`, `openai-compatible`, or
    /// another kind that cannot serve vision.
    pub kind: Option<String>,
    /// The effective primary API key, or `None` when no key is configured.
    pub api_key: Option<String>,
}

/// The resolved vision endpoint and credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisionSettings {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
}

/// Resolve the `[ocr]` section into a working provider, or into a
/// [`RejectedOcr`] that carries the configuration reason. `None` means the
/// section is absent: PDF extraction is then impossible and fails each job at
/// stage `config`, while text extraction still works.
pub fn resolve_ocr(
    ocr: Option<&OcrConfig>,
    primary: &PrimaryProvider,
) -> Option<Arc<dyn OcrProvider>> {
    let config = ocr?;
    match resolve_vision(config, primary) {
        Ok(settings) => match VisionOcr::new(settings) {
            Ok(provider) => Some(Arc::new(provider)),
            Err(error) => Some(Arc::new(RejectedOcr::new(error.to_string()))),
        },
        Err(reason) => Some(Arc::new(RejectedOcr::new(reason))),
    }
}

/// Resolve the effective vision settings. The rules:
///
/// - `provider = "inherit"` reads the primary provider. A first-party
///   `openai` primary uses the default vision host only when `vision-url` is
///   absent. An `openai-compatible` primary needs an explicit `vision-url`
///   outside the default OpenAI host and never inherits the embeddings URL.
/// - An explicit `openai` provider needs a readable, non-empty
///   `api-key-file`; it may use the default OpenAI host.
/// - Any other provider name is `unsupported ocr provider '<kind>'`.
pub fn resolve_vision(
    config: &OcrConfig,
    primary: &PrimaryProvider,
) -> Result<VisionSettings, String> {
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_OCR_MODEL.to_owned());
    match config.provider.as_str() {
        "inherit" => {
            let kind = primary.kind.as_deref().ok_or_else(|| {
                "config 'ocr.provider': cannot inherit: no primary embedding provider is configured"
                    .to_owned()
            })?;
            let api_key = primary.api_key.clone().ok_or_else(|| {
                "config 'ocr.provider': the primary embedding provider has no API key to inherit"
                    .to_owned()
            })?;
            match kind {
                "openai" => {
                    let endpoint = match config.vision_url.clone() {
                        Some(endpoint) => {
                            check_endpoint(&endpoint, "ocr.vision-url")?;
                            endpoint
                        }
                        None => DEFAULT_VISION_ENDPOINT.to_owned(),
                    };
                    Ok(VisionSettings {
                        endpoint,
                        api_key,
                        model,
                    })
                }
                "openai-compatible" => {
                    let endpoint = config.vision_url.clone().ok_or_else(|| {
                        "config 'ocr.vision-url': an inherited 'openai-compatible' provider needs \
                         an explicit vision URL outside the default OpenAI host"
                            .to_owned()
                    })?;
                    check_endpoint(&endpoint, "ocr.vision-url")?;
                    if on_default_vision_host(&endpoint) {
                        return Err(
                            "config 'ocr.vision-url': an inherited 'openai-compatible' provider \
                             must not use the default OpenAI vision host; the primary key would \
                             be sent to OpenAI"
                                .to_owned(),
                        );
                    }
                    Ok(VisionSettings {
                        endpoint,
                        api_key,
                        model,
                    })
                }
                other => Err(format!("unsupported ocr provider '{other}'")),
            }
        }
        "openai" => {
            let path = config.api_key_file.as_deref().ok_or_else(|| {
                "config 'ocr.api-key-file': an explicit 'openai' OCR provider needs a readable \
                 non-empty key file"
                    .to_owned()
            })?;
            let api_key = read_key_file(path)?;
            let endpoint = match config.vision_url.clone() {
                Some(endpoint) => {
                    check_endpoint(&endpoint, "ocr.vision-url")?;
                    endpoint
                }
                None => DEFAULT_VISION_ENDPOINT.to_owned(),
            };
            Ok(VisionSettings {
                endpoint,
                api_key,
                model,
            })
        }
        other => Err(format!("unsupported ocr provider '{other}'")),
    }
}

fn read_key_file(path: &str) -> Result<String, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("config 'ocr.api-key-file': cannot read '{path}': {error}"))?;
    let key = contents.trim();
    if key.is_empty() {
        return Err(format!("config 'ocr.api-key-file': '{path}' is empty"));
    }
    Ok(key.to_owned())
}

fn check_endpoint(endpoint: &str, field: &str) -> Result<(), String> {
    let url = url::Url::parse(endpoint)
        .map_err(|error| format!("config '{field}': invalid URL '{endpoint}': {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "config '{field}': '{endpoint}' must be an http(s) endpoint"
        ));
    }
    if url.username() != "" || url.password().is_some() {
        // A credential in the URL would be sent as basic auth and echoed
        // verbatim into error strings; refusals belong in config, not logs.
        return Err(format!(
            "config '{field}': userinfo in the URL is refused; use api-key-file"
        ));
    }
    Ok(())
}

fn on_default_vision_host(endpoint: &str) -> bool {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(|host| host == DEFAULT_VISION_HOST))
        .unwrap_or(false)
}

/// A vision OCR provider that posts one rendered page per request.
pub struct VisionOcr {
    client: reqwest::blocking::Client,
    settings: VisionSettings,
}

impl VisionOcr {
    pub fn new(settings: VisionSettings) -> Result<Self, OcrError> {
        // No redirects: a redirect can move the page image and the bearer
        // credential off the operator-named host. Same policy as the webhook
        // and OAuth egress clients.
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_millis(
                (VISION_TIMEOUT_US / 1_000) as u64,
            ))
            .build()
            .map_err(|error| {
                OcrError::Provider(format!("cannot build the vision client: {error}"))
            })?;
        Ok(Self { client, settings })
    }

    /// The resolved endpoint, key and model. Tests inspect the effective
    /// endpoint through this accessor.
    pub const fn settings(&self) -> &VisionSettings {
        &self.settings
    }
}

impl OcrProvider for VisionOcr {
    fn transcribe_page(&self, image: &[u8], mime: &str) -> Result<String, OcrError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(image);
        let body = serde_json::json!({
            "model": self.settings.model,
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Transcribe every character of text on this page, exactly as written. Return only the transcription."},
                    {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{encoded}")}}
                ]
            }]
        });
        let response = self
            .client
            .post(&self.settings.endpoint)
            .bearer_auth(&self.settings.api_key)
            .json(&body)
            .send()
            .map_err(|error| OcrError::Provider(format!("vision request failed: {error}")))?;
        let status = response.status();
        let text = response.text().map_err(|error| {
            OcrError::Provider(format!("vision response could not be read: {error}"))
        })?;
        if !status.is_success() {
            let excerpt: String = text.chars().take(300).collect();
            return Err(OcrError::Provider(format!(
                "vision endpoint returned {status}: {excerpt}"
            )));
        }
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| OcrError::Provider(format!("vision response is not JSON: {error}")))?;
        // `finish_reason=length` means the model stopped at the token
        // limit: the transcription is a partial page and must not be
        // published as complete. An absent finish_reason is accepted — some
        // compatible endpoints omit it.
        let finish_reason = value
            .pointer("/choices/0/finish_reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("stop");
        if finish_reason == "length" {
            return Err(OcrError::Provider(
                "the vision response hit the token limit; the page transcription is truncated"
                    .into(),
            ));
        }
        let content = value
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                OcrError::Provider(
                    "vision response has no choices[0].message.content string".into(),
                )
            })?;
        Ok(content.chars().take(MAX_PAGE_CHARS).collect())
    }
}

/// A provider that always fails at stage `config`. The extractor role builds
/// it when the `[ocr]` section cannot be resolved, so the configuration error
/// lands on the PDF job that needs it instead of stopping a process whose
/// text extraction works.
pub struct RejectedOcr {
    reason: String,
}

impl RejectedOcr {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl OcrProvider for RejectedOcr {
    fn transcribe_page(&self, _image: &[u8], _mime: &str) -> Result<String, OcrError> {
        Err(OcrError::Config(self.reason.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn primary(kind: &str, key: Option<&str>) -> PrimaryProvider {
        PrimaryProvider {
            kind: Some(kind.to_owned()),
            api_key: key.map(str::to_owned),
        }
    }

    fn config(provider: &str) -> OcrConfig {
        OcrConfig {
            provider: provider.to_owned(),
            model: None,
            vision_url: None,
            api_key_file: None,
        }
    }

    #[test]
    fn inherited_openai_uses_the_default_vision_host_without_a_url() {
        let settings = resolve_vision(&config("inherit"), &primary("openai", Some("key")))
            .expect("first-party inheritance resolves");
        assert_eq!(settings.endpoint, DEFAULT_VISION_ENDPOINT);
        assert_eq!(settings.api_key, "key");
        assert_eq!(settings.model, DEFAULT_OCR_MODEL);
    }

    #[test]
    fn inherited_openai_honors_an_explicit_vision_url() {
        let mut configured = config("inherit");
        configured.vision_url = Some("http://127.0.0.1:9/v1/chat/completions".into());
        let settings = resolve_vision(&configured, &primary("openai", Some("key")))
            .expect("an explicit first-party URL applies");
        assert_eq!(settings.endpoint, "http://127.0.0.1:9/v1/chat/completions");
    }

    #[test]
    fn vision_url_refuses_embedded_credentials_without_echoing_them() {
        let mut configured = config("inherit");
        configured.vision_url =
            Some("https://user:secret@vision.example/v1/chat/completions".into());
        let error = resolve_vision(&configured, &primary("openai", Some("key")))
            .expect_err("the vision URL must not supply credentials");
        assert!(error.contains("userinfo"), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn vision_request_does_not_follow_redirects_with_the_page_or_key() {
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("local vision endpoint");
        let endpoint = format!("http://{}/transcribe", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().expect("first request");
            let mut request = [0; 8192];
            let received = first.read(&mut request).expect("read the vision request");
            assert!(
                request[..received].starts_with(b"POST "),
                "vision uses POST"
            );
            first
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /untrusted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("redirect response");
            drop(first);

            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let deadline = Instant::now() + Duration::from_millis(300);
            loop {
                match listener.accept() {
                    Ok((mut redirected, _)) => {
                        let received = redirected
                            .read(&mut request)
                            .expect("read redirect request");
                        assert!(
                            request[..received].starts_with(b"GET "),
                            "a followed 302 uses GET"
                        );
                        let body = br#"{"choices":[{"message":{"content":"leaked"}}]}"#;
                        redirected
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                )
                                .as_bytes(),
                            )
                            .expect("redirect response headers");
                        redirected.write_all(body).expect("redirect response body");
                        return true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept redirect: {error}"),
                }
            }
        });

        let provider = VisionOcr::new(VisionSettings {
            endpoint,
            api_key: "secret-key".into(),
            model: DEFAULT_OCR_MODEL.into(),
        })
        .expect("vision client");
        let result = provider.transcribe_page(b"private page", "image/png");
        let redirected = server.join().expect("vision server");
        assert!(
            !redirected,
            "the redirect target must not receive a request"
        );
        assert!(
            matches!(&result, Err(OcrError::Provider(error)) if error.contains("302")),
            "{result:?}"
        );
    }

    #[test]
    fn inherited_openai_requires_a_primary_key() {
        let error = resolve_vision(&config("inherit"), &primary("openai", None))
            .expect_err("a first-party inherit without a key cannot authenticate");
        assert!(error.contains("no API key to inherit"), "{error}");
    }

    #[test]
    fn inherited_compatible_needs_an_explicit_non_openai_vision_url() {
        let error = resolve_vision(
            &config("inherit"),
            &primary("openai-compatible", Some("key")),
        )
        .expect_err("a compatible inherit without a vision URL is refused");
        assert!(error.contains("explicit vision URL"), "{error}");

        let mut openai_host = config("inherit");
        openai_host.vision_url = Some("https://api.openai.com/v1/chat/completions".into());
        let error = resolve_vision(&openai_host, &primary("openai-compatible", Some("key")))
            .expect_err("the primary key must never reach the default OpenAI host");
        assert!(error.contains("default OpenAI vision host"), "{error}");

        let mut compatible = config("inherit");
        compatible.vision_url = Some("https://vision.example/v1/chat/completions".into());
        let settings = resolve_vision(
            &compatible,
            &primary("openai-compatible", Some("compat-key")),
        )
        .expect("an explicit compatible URL resolves");
        assert_eq!(
            settings.endpoint,
            "https://vision.example/v1/chat/completions"
        );
        assert_eq!(settings.api_key, "compat-key");
    }

    #[test]
    fn explicit_openai_reads_a_trimmed_non_empty_key_file() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let key_path = dir.path().join("vision-key");
        std::fs::File::create(&key_path)
            .expect("key file")
            .write_all(b"  explicit-key\n")
            .expect("write key");

        let mut configured = config("openai");
        configured.api_key_file = Some(key_path.to_string_lossy().into_owned());
        let settings = resolve_vision(&configured, &PrimaryProvider::default())
            .expect("an explicit openai provider reads its own key file");
        assert_eq!(settings.api_key, "explicit-key");
    }

    #[test]
    fn explicit_openai_refuses_a_missing_or_empty_key_file() {
        let mut missing = config("openai");
        missing.api_key_file = Some("/nonexistent/vision-key".into());
        let error = resolve_vision(&missing, &PrimaryProvider::default())
            .expect_err("a missing key file must fail resolution");
        assert!(error.contains("api-key-file"), "{error}");

        let dir = tempfile::tempdir().expect("temporary directory");
        let key_path = dir.path().join("empty-key");
        std::fs::File::create(&key_path).expect("empty key file");
        let mut empty = config("openai");
        empty.api_key_file = Some(key_path.to_string_lossy().into_owned());
        let error = resolve_vision(&empty, &PrimaryProvider::default())
            .expect_err("an empty key file must fail resolution");
        assert!(error.contains("is empty"), "{error}");

        let mut no_file = config("openai");
        no_file.api_key_file = None;
        let error = resolve_vision(&no_file, &PrimaryProvider::default())
            .expect_err("an explicit openai provider without a key file is refused");
        assert!(error.contains("api-key-file"), "{error}");
    }

    #[test]
    fn unknown_provider_names_are_rejected_named() {
        for kind in ["bogus", "ollama"] {
            let error = resolve_vision(&config(kind), &PrimaryProvider::default())
                .expect_err("an unknown OCR provider kind fails resolution");
            assert_eq!(error, format!("unsupported ocr provider '{kind}'"));
        }
    }

    #[test]
    fn inherit_has_no_primary_provider_to_read() {
        let error = resolve_vision(&config("inherit"), &PrimaryProvider::default())
            .expect_err("inherit with no primary provider is refused");
        assert!(error.contains("no primary embedding provider"), "{error}");
    }

    #[test]
    fn rejected_ocr_fails_at_config_and_absent_ocr_resolves_to_none() {
        let rejected = RejectedOcr::new("unsupported ocr provider 'bogus'");
        match rejected.transcribe_page(b"image", "image/png") {
            Err(OcrError::Config(reason)) => assert_eq!(reason, "unsupported ocr provider 'bogus'"),
            other => panic!("expected a config failure, got {other:?}"),
        }

        assert!(
            resolve_ocr(None, &PrimaryProvider::default()).is_none(),
            "an absent [ocr] section leaves OCR unconfigured"
        );
        let resolved = resolve_ocr(Some(&config("bogus")), &primary("openai", Some("key")))
            .expect("a bad section still resolves to a provider object");
        assert!(!resolved.transcribe_page(b"image", "image/png").is_ok());
    }
}
