use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::{log_v, VERBOSITY};
use crate::error::{Error, Result};
use std::sync::atomic::Ordering;

const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json,",
    "application/vnd.oci.image.manifest.v1+json"
);

#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct RegistryClient {
    http: Client,
    token_cache: Arc<Mutex<HashMap<String, CachedToken>>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    // Docker Hub returns both fields with the same value; accept either
    token: Option<String>,
    access_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

impl TokenResponse {
    fn into_token(self) -> Option<String> {
        self.token.or(self.access_token)
    }
}

fn default_expires_in() -> u64 {
    300
}

impl RegistryClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            token_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn get_remote_digest(&self, image_ref: &str) -> Result<String> {
        let (registry_url, name, reference) = parse_image_ref(image_ref);
        log_v!(3, "registry={} name={} reference={}", registry_url, name, reference);

        let token = self.get_token(&registry_url, &name).await?;

        let url = format!("{}/v2/{}/manifests/{}", registry_url, name, reference);
        log_v!(3, "HEAD {}", url);

        let resp = self
            .http
            .head(&url)
            .header(ACCEPT, MANIFEST_ACCEPT)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .send()
            .await?;

        let status = resp.status();
        log_v!(3, "manifest response: {}", status);

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Auth {
                registry: registry_url,
                reason: "unauthorized — image may be private or require Docker Hub login".into(),
            });
        }

        if !status.is_success() {
            return Err(Error::NoManifest {
                image: image_ref.to_string(),
            });
        }

        let digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| Error::MissingDigest {
                image: image_ref.to_string(),
            })?;

        log_v!(3, "remote digest: {}", digest);
        Ok(digest)
    }

    async fn get_token(&self, registry_url: &str, name: &str) -> Result<String> {
        let cache_key = format!("{}:{}", registry_url, name);

        {
            let cache = self.token_cache.lock().await;
            if let Some(cached) = cache.get(&cache_key) {
                if cached.expires_at > Instant::now() {
                    log_v!(3, "token cache hit for {}", cache_key);
                    return Ok(cached.token.clone());
                }
            }
        }

        let probe_url = format!("{}/v2/{}/manifests/latest", registry_url, name);
        log_v!(3, "auth probe: HEAD {}", probe_url);
        let probe = self.http.head(&probe_url).send().await;

        let auth_header = match &probe {
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                resp.headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
            }
            _ => None,
        };

        let (token, expires_in) = if let Some(www_auth) = auth_header {
            log_v!(3, "www-authenticate: {}", www_auth);
            self.fetch_token(&www_auth).await?
        } else {
            log_v!(3, "no auth challenge, proceeding without token");
            (String::new(), 300)
        };

        let ttl = expires_in.saturating_sub(10).max(60);
        log_v!(2, "token obtained, ttl={}s", ttl);

        let mut cache = self.token_cache.lock().await;
        cache.insert(
            cache_key,
            CachedToken {
                token: token.clone(),
                expires_at: Instant::now() + Duration::from_secs(ttl),
            },
        );

        Ok(token)
    }

    async fn fetch_token(&self, www_auth: &str) -> Result<(String, u64)> {
        let params = parse_www_authenticate(www_auth)
            .ok_or_else(|| Error::AuthHeader(www_auth.to_string()))?;

        let realm = params
            .get("realm")
            .ok_or_else(|| Error::AuthHeader("missing realm".into()))?;

        log_v!(3, "fetching token from {}", realm);

        let mut req = self.http.get(realm);
        if let Some(service) = params.get("service") {
            req = req.query(&[("service", service)]);
        }
        if let Some(scope) = params.get("scope") {
            req = req.query(&[("scope", scope)]);
        }

        let response = req.send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Auth {
                registry: realm.to_string(),
                reason: format!("token endpoint returned {status}: {body}"),
            });
        }

        let body = response.text().await.unwrap_or_default();
        let resp: TokenResponse = serde_json::from_str(&body).map_err(|e| {
            // Include the raw body only at -vvv to avoid leaking token data in normal logs
            let reason = if VERBOSITY.load(Ordering::Relaxed) >= 3 {
                format!("failed to parse token response ({e}): {body}")
            } else {
                format!("failed to parse token response: {e}")
            };
            Error::Auth {
                registry: realm.to_string(),
                reason,
            }
        })?;

        let expires_in = resp.expires_in;
        let token = resp.into_token().ok_or_else(|| Error::Auth {
            registry: realm.to_string(),
            reason: "token response contained no token field".into(),
        })?;

        Ok((token, expires_in))
    }
}

fn parse_www_authenticate(header: &str) -> Option<HashMap<String, String>> {
    let header = header.strip_prefix("Bearer ")?.trim();
    let mut map = HashMap::new();
    let mut remaining = header;

    while !remaining.is_empty() {
        let eq_pos = remaining.find('=')?;
        let key = remaining[..eq_pos].trim().to_string();
        remaining = &remaining[eq_pos + 1..];

        let (value, rest) = if remaining.starts_with('"') {
            let end = remaining[1..].find('"')? + 1;
            let val = remaining[1..end].to_string();
            let after = remaining[end + 1..].trim_start_matches(',').trim_start();
            (val, after)
        } else {
            match remaining.find(',') {
                Some(pos) => (remaining[..pos].to_string(), remaining[pos + 1..].trim_start()),
                None => (remaining.to_string(), ""),
            }
        };

        map.insert(key, value);
        remaining = rest;
    }

    Some(map)
}

/// Returns (registry_base_url, image_name_for_api, reference)
fn parse_image_ref(image_ref: &str) -> (String, String, String) {
    let (name_part, reference) = if let Some(at_pos) = image_ref.find('@') {
        (&image_ref[..at_pos], image_ref[at_pos + 1..].to_string())
    } else {
        let (n, t) = split_tag(image_ref);
        (n, t)
    };

    let reference = if reference.is_empty() {
        "latest".to_string()
    } else {
        reference
    };

    let parts: Vec<&str> = name_part.splitn(2, '/').collect();
    let (registry_url, repo_path) = if parts.len() >= 2 && is_registry_host(parts[0]) {
        let host = parts[0];
        let path = parts[1];
        let url = if host == "docker.io" {
            "https://registry-1.docker.io".to_string()
        } else {
            format!("https://{}", host)
        };
        (url, path.to_string())
    } else {
        let path = if name_part.contains('/') {
            name_part.to_string()
        } else {
            format!("library/{}", name_part)
        };
        ("https://registry-1.docker.io".to_string(), path)
    };

    (registry_url, repo_path, reference)
}

fn split_tag(image_ref: &str) -> (&str, String) {
    if let Some(colon_pos) = image_ref.rfind(':') {
        let after_colon = &image_ref[colon_pos + 1..];
        if !after_colon.contains('/') {
            return (&image_ref[..colon_pos], after_colon.to_string());
        }
    }
    (image_ref, "latest".to_string())
}

fn is_registry_host(s: &str) -> bool {
    s.contains('.')
        || s.contains(':')
        || s == "localhost"
        || matches!(s, "ghcr.io" | "quay.io" | "gcr.io" | "docker.io")
}

pub fn info_url_for(image_ref: &str) -> String {
    let (registry_url, name, _) = parse_image_ref(image_ref);

    if registry_url.contains("registry-1.docker.io") || registry_url.contains("docker.io") {
        if name.starts_with("library/") {
            let repo = name.trim_start_matches("library/");
            format!("https://hub.docker.com/_/{}/tags", repo)
        } else {
            format!("https://hub.docker.com/r/{}/tags", name)
        }
    } else if registry_url.contains("lscr.io") {
        // LinuxServer.io registry — docs are at docs.linuxserver.io/images/docker-<name>
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        let image_name = if parts.len() == 2 { parts[1] } else { &name };
        format!("https://docs.linuxserver.io/images/docker-{}", image_name)
    } else if registry_url.contains("ghcr.io") {
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        if parts.len() == 2 {
            format!(
                "https://github.com/{}/pkgs/container/{}",
                parts[0], parts[1]
            )
        } else {
            format!("https://github.com/orgs/docker/packages/container/package/{}", name)
        }
    } else if registry_url.contains("quay.io") {
        format!("https://quay.io/repository/{}?tab=tags", name)
    } else {
        format!("{}/v2/{}/tags/list", registry_url, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_image_ref ---

    #[test]
    fn parse_image_ref_official_no_tag() {
        let (reg, name, reference) = parse_image_ref("nginx");
        assert_eq!(reg, "https://registry-1.docker.io");
        assert_eq!(name, "library/nginx");
        assert_eq!(reference, "latest");
    }

    #[test]
    fn parse_image_ref_official_with_tag() {
        let (reg, name, reference) = parse_image_ref("nginx:alpine");
        assert_eq!(reg, "https://registry-1.docker.io");
        assert_eq!(name, "library/nginx");
        assert_eq!(reference, "alpine");
    }

    #[test]
    fn parse_image_ref_user_image_with_tag() {
        let (reg, name, reference) = parse_image_ref("myuser/myimage:v1");
        assert_eq!(reg, "https://registry-1.docker.io");
        assert_eq!(name, "myuser/myimage");
        assert_eq!(reference, "v1");
    }

    #[test]
    fn parse_image_ref_ghcr() {
        let (reg, name, reference) = parse_image_ref("ghcr.io/owner/repo:latest");
        assert_eq!(reg, "https://ghcr.io");
        assert_eq!(name, "owner/repo");
        assert_eq!(reference, "latest");
    }

    #[test]
    fn parse_image_ref_explicit_docker_io() {
        let (reg, name, reference) = parse_image_ref("docker.io/library/redis:7");
        assert_eq!(reg, "https://registry-1.docker.io");
        assert_eq!(name, "library/redis");
        assert_eq!(reference, "7");
    }

    #[test]
    fn parse_image_ref_digest_reference() {
        let (reg, name, reference) = parse_image_ref("nginx@sha256:abc123");
        assert_eq!(reg, "https://registry-1.docker.io");
        assert_eq!(name, "library/nginx");
        assert_eq!(reference, "sha256:abc123");
    }

    #[test]
    fn parse_image_ref_registry_with_port() {
        let (reg, name, reference) = parse_image_ref("localhost:5000/myimage:tag");
        assert_eq!(reg, "https://localhost:5000");
        assert_eq!(name, "myimage");
        assert_eq!(reference, "tag");
    }

    // --- split_tag ---

    #[test]
    fn split_tag_with_explicit_tag() {
        let (name, tag) = split_tag("nginx:alpine");
        assert_eq!(name, "nginx");
        assert_eq!(tag, "alpine");
    }

    #[test]
    fn split_tag_without_tag_defaults_latest() {
        let (name, tag) = split_tag("nginx");
        assert_eq!(name, "nginx");
        assert_eq!(tag, "latest");
    }

    #[test]
    fn split_tag_registry_port_not_treated_as_tag() {
        let (name, tag) = split_tag("localhost:5000/myimage");
        assert_eq!(name, "localhost:5000/myimage");
        assert_eq!(tag, "latest");
    }

    // --- is_registry_host ---

    #[test]
    fn is_registry_host_fqdn() {
        assert!(is_registry_host("registry.example.com"));
    }

    #[test]
    fn is_registry_host_localhost() {
        assert!(is_registry_host("localhost"));
    }

    #[test]
    fn is_registry_host_with_port() {
        assert!(is_registry_host("localhost:5000"));
    }

    #[test]
    fn is_registry_host_simple_name_is_not_host() {
        assert!(!is_registry_host("nginx"));
    }

    #[test]
    fn is_registry_host_known_registries() {
        assert!(is_registry_host("ghcr.io"));
        assert!(is_registry_host("quay.io"));
        assert!(is_registry_host("docker.io"));
    }

    // --- parse_www_authenticate ---

    #[test]
    fn parse_www_auth_docker_hub_full_header() {
        let header = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#;
        let result = parse_www_authenticate(header).unwrap();
        assert_eq!(result["realm"], "https://auth.docker.io/token");
        assert_eq!(result["service"], "registry.docker.io");
        assert_eq!(result["scope"], "repository:library/nginx:pull");
    }

    #[test]
    fn parse_www_auth_non_bearer_returns_none() {
        assert!(parse_www_authenticate("Basic realm=\"test\"").is_none());
    }

    // --- info_url_for ---

    #[test]
    fn info_url_docker_hub_official() {
        assert_eq!(info_url_for("nginx"), "https://hub.docker.com/_/nginx/tags");
    }

    #[test]
    fn info_url_docker_hub_user_image() {
        assert_eq!(info_url_for("myuser/myimage:v1"), "https://hub.docker.com/r/myuser/myimage/tags");
    }

    #[test]
    fn info_url_ghcr() {
        assert_eq!(
            info_url_for("ghcr.io/owner/repo:latest"),
            "https://github.com/owner/pkgs/container/repo"
        );
    }

    #[test]
    fn info_url_lscr() {
        assert_eq!(
            info_url_for("lscr.io/linuxserver/calibre:latest"),
            "https://docs.linuxserver.io/images/docker-calibre"
        );
    }

    #[test]
    fn info_url_quay() {
        assert_eq!(
            info_url_for("quay.io/org/repo:tag"),
            "https://quay.io/repository/org/repo?tab=tags"
        );
    }

    #[test]
    fn info_url_custom_registry_falls_back_to_api() {
        let url = info_url_for("myreg.example.com/myimage:tag");
        assert!(url.contains("/v2/") && url.ends_with("/tags/list"));
    }
}
