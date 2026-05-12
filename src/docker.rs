use std::collections::HashSet;
use std::path::Path;

use bollard::Docker;
use bollard::container::ListContainersOptions;
use bollard::image::ListImagesOptions;
use colored::Colorize;

use crate::log_v;
use crate::error::Result;
use crate::types::{ContainerInfo, ItemState, PruneableInfo};

pub struct DockerClient {
    inner: Docker,
}

impl DockerClient {
    pub fn connect() -> Result<Self> {
        if !check_socket_access() {
            std::process::exit(1);
        }
        log_v!(2, "connecting to Docker daemon");
        let inner = Docker::connect_with_local_defaults()?;
        Ok(Self { inner })
    }

    pub async fn list_containers(&self, include_stopped: bool) -> Result<Vec<ContainerInfo>> {
        let options = ListContainersOptions::<String> {
            all: include_stopped,
            ..Default::default()
        };

        let containers = self.inner.list_containers(Some(options)).await?;
        log_v!(2, "found {} container(s) (include_stopped={})", containers.len(), include_stopped);
        let mut results = Vec::new();

        for c in containers {
            let container_name = c
                .names
                .as_deref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| c.id.clone().unwrap_or_default());

            let state = if c.state.as_deref() == Some("running") {
                ItemState::Running
            } else {
                ItemState::Stopped
            };

            let raw_image_ref = match c.image {
                Some(ref img) if !img.is_empty() => img.clone(),
                _ => continue,
            };

            let image_id = match c.image_id {
                Some(ref id) => id.clone(),
                None => continue,
            };

            let image_ref = if raw_image_ref.starts_with("sha256:") {
                match self.resolve_image_name(&image_id).await {
                    Some(resolved) => {
                        log_v!(3, "container '{}' resolved digest to '{}'", container_name, resolved);
                        resolved
                    }
                    None => {
                        log_v!(3, "container '{}' image is digest-only, no registry info found", container_name);
                        raw_image_ref
                    }
                }
            } else {
                raw_image_ref
            };

            let local_digest = if image_ref.starts_with("sha256:") {
                None
            } else {
                self.get_local_digest(&image_id, &image_ref).await
            };

            log_v!(3, "container '{}' image='{}' state={:?} local_digest={}",
                container_name, image_ref, state, local_digest.as_deref().unwrap_or("<none>"));

            results.push(ContainerInfo {
                display_name: container_name,
                image_ref,
                local_digest,
                state,
            });
        }

        Ok(results)
    }

    /// Returns all locally stored images that have no associated container.
    pub async fn list_standalone_images(&self) -> Result<Vec<ContainerInfo>> {
        // Collect all image IDs currently referenced by any container
        let all_containers = self
            .inner
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }))
            .await?;

        let container_image_ids: HashSet<String> = all_containers
            .iter()
            .filter_map(|c| c.image_id.clone())
            .map(|id| id.trim_start_matches("sha256:").to_string())
            .collect();

        let images = self
            .inner
            .list_images(Some(ListImagesOptions::<String> {
                all: false,
                ..Default::default()
            }))
            .await?;

        log_v!(2, "found {} local image(s)", images.len());

        let mut results = Vec::new();

        for img in images {
            let image_id = img.id.trim_start_matches("sha256:").to_string();

            if container_image_ids.contains(&image_id) {
                continue;
            }

            // Only include images with a real tag — skip dangling/untagged images
            let image_ref = match img.repo_tags.iter().find(|t| !t.contains("<none>")).cloned() {
                Some(r) => r,
                None => continue,
            };

            let local_digest = img
                .repo_digests
                .first()
                .and_then(|d| d.split('@').nth(1))
                .map(|s| s.to_string());

            log_v!(3, "standalone image '{}' local_digest={}", image_ref, local_digest.as_deref().unwrap_or("<none>"));

            results.push(ContainerInfo {
                display_name: image_ref.clone(),
                image_ref,
                local_digest,
                state: ItemState::Image,
            });
        }

        Ok(results)
    }

    /// Returns all manifest digests (sha256:...) present in the local image store.
    pub async fn local_digests(&self) -> HashSet<String> {
        match self
            .inner
            .list_images(Some(ListImagesOptions::<String> {
                all: false,
                ..Default::default()
            }))
            .await
        {
            Ok(images) => images
                .iter()
                .flat_map(|img| img.repo_digests.iter())
                .filter_map(|d| d.split('@').nth(1))
                .map(|s| s.to_string())
                .collect(),
            Err(_) => HashSet::new(),
        }
    }

    async fn resolve_image_name(&self, image_id: &str) -> Option<String> {
        let id = image_id.trim_start_matches("sha256:");
        let inspect = self.inner.inspect_image(id).await.ok()?;

        if let Some(tags) = inspect.repo_tags {
            if let Some(tag) = tags.into_iter().find(|t| !t.contains("<none>")) {
                return Some(tag);
            }
        }

        if let Some(digests) = inspect.repo_digests {
            if let Some(name) = digests.first().and_then(|d| d.split('@').next()) {
                return Some(format!("{}:latest", name));
            }
        }

        None
    }

    pub async fn pruneable_info(
        &self,
        container_refs: &HashSet<String>,
        pulled_digests: &HashSet<String>,
    ) -> PruneableInfo {
        let used_ids: HashSet<String> = match self
            .inner
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }))
            .await
        {
            Ok(cs) => cs
                .iter()
                .filter_map(|c| c.image_id.clone())
                .map(|id| id.trim_start_matches("sha256:").to_string())
                .collect(),
            Err(_) => HashSet::new(),
        };

        // Normalize container refs: add :latest where no tag/digest is present
        let normalized_refs: HashSet<String> = container_refs
            .iter()
            .map(|r| {
                if r.contains(':') || r.contains('@') {
                    r.clone()
                } else {
                    format!("{}:latest", r)
                }
            })
            .collect();

        let images = match self
            .inner
            .list_images(Some(ListImagesOptions::<String> {
                all: false,
                ..Default::default()
            }))
            .await
        {
            Ok(imgs) => imgs,
            Err(_) => return PruneableInfo::default(),
        };

        let mut info = PruneableInfo::default();

        for img in images {
            let image_id = img.id.trim_start_matches("sha256:").to_string();
            let size = img.size.max(0) as u64;
            let is_dangling = img.repo_tags.is_empty()
                || img.repo_tags.iter().all(|t| t.contains("<none>"));

            if is_dangling {
                // Only count dangling images that are not actively used by a container
                if !used_ids.contains(&image_id) {
                    info.dangling_count += 1;
                    info.dangling_bytes += size;
                }
            } else if !used_ids.contains(&image_id)
                && !img.repo_tags.iter().any(|t| normalized_refs.contains(t))
                && !img.repo_digests.iter()
                    .filter_map(|d| d.split('@').nth(1))
                    .any(|d| pulled_digests.contains(d))
            {
                info.unused_count += 1;
                info.unused_bytes += size;
            }
        }

        info
    }

    async fn get_local_digest(&self, image_id: &str, image_ref: &str) -> Option<String> {
        let image_id = image_id.trim_start_matches("sha256:");
        let inspect = self.inner.inspect_image(image_id).await.ok()?;
        let repo_digests = inspect.repo_digests?;

        let repo_name = image_ref_to_repo_name(image_ref);
        let digest_entry = repo_digests
            .iter()
            .find(|d| {
                let d_repo = d.split('@').next().unwrap_or("");
                normalize_repo(d_repo) == normalize_repo(&repo_name)
            })
            .or_else(|| repo_digests.first())?;

        digest_entry.split('@').nth(1).map(|s| s.to_string())
    }
}

fn check_socket_access() -> bool {
    let socket = std::env::var("DOCKER_HOST")
        .ok()
        .and_then(|h| h.strip_prefix("unix://").map(str::to_string))
        .unwrap_or_else(|| "/var/run/docker.sock".to_string());

    if Path::new(&socket).exists() {
        if std::os::unix::net::UnixStream::connect(&socket).is_err() {
            eprintln!(
                "{} insufficient permissions to access Docker socket at {socket}",
                "error:".red().bold()
            );
            return false;
        }
    }
    true
}

fn image_ref_to_repo_name(image_ref: &str) -> String {
    let without_digest = image_ref.split('@').next().unwrap_or(image_ref);
    let without_tag = without_digest.split(':').next().unwrap_or(without_digest);
    without_tag.to_string()
}

fn normalize_repo(repo: &str) -> String {
    let repo = repo.trim_start_matches("docker.io/");
    if !repo.contains('/') {
        format!("library/{}", repo)
    } else {
        repo.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_repo_strips_docker_io() {
        assert_eq!(normalize_repo("docker.io/library/nginx"), "library/nginx");
    }

    #[test]
    fn normalize_repo_official_image_gets_library_prefix() {
        assert_eq!(normalize_repo("nginx"), "library/nginx");
    }

    #[test]
    fn normalize_repo_user_image_unchanged() {
        assert_eq!(normalize_repo("myuser/myimage"), "myuser/myimage");
    }

    #[test]
    fn image_ref_to_repo_name_strips_tag() {
        assert_eq!(image_ref_to_repo_name("nginx:alpine"), "nginx");
    }

    #[test]
    fn image_ref_to_repo_name_strips_digest() {
        assert_eq!(image_ref_to_repo_name("nginx@sha256:abc123"), "nginx");
    }

    #[test]
    fn image_ref_to_repo_name_plain() {
        assert_eq!(image_ref_to_repo_name("nginx"), "nginx");
    }

    #[test]
    fn image_ref_to_repo_name_ghcr_with_tag() {
        assert_eq!(image_ref_to_repo_name("ghcr.io/owner/repo:v1"), "ghcr.io/owner/repo");
    }
}
