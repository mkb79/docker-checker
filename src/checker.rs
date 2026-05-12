use std::collections::HashSet;
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use crate::registry::{info_url_for, RegistryClient};
use crate::types::{CheckResult, ContainerInfo};

pub async fn check_all(
    containers: Vec<ContainerInfo>,
    local_digests: HashSet<String>,
    concurrency: usize,
) -> Vec<CheckResult> {
    let client = match RegistryClient::new() {
        Ok(c) => c,
        Err(e) => {
            return containers
                .into_iter()
                .map(|c| CheckResult::Error {
                    state: c.state,
                    name: c.display_name,
                    image: c.image_ref,
                    reason: format!("failed to create HTTP client: {}", e),
                })
                .collect();
        }
    };

    let local_digests = Arc::new(local_digests);

    stream::iter(containers)
        .map(|info| {
            let client = client.clone();
            let local_digests = Arc::clone(&local_digests);
            async move { check_one(info, client, &local_digests).await }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await
}

async fn check_one(
    info: ContainerInfo,
    client: RegistryClient,
    local_digests: &HashSet<String>,
) -> CheckResult {
    let state = info.state.clone();

    let local_digest = match &info.local_digest {
        Some(d) => d.clone(),
        None => {
            return CheckResult::LocalOnly {
                name: info.display_name,
                image: info.image_ref,
                state,
            }
        }
    };

    match client.get_remote_digest(&info.image_ref).await {
        Ok(remote_digest) => {
            if local_digest == remote_digest {
                CheckResult::UpToDate {
                    name: info.display_name,
                    image: info.image_ref,
                    state,
                }
            } else {
                let already_pulled = local_digests.contains(&remote_digest);
                let info_url = info_url_for(&info.image_ref);
                CheckResult::UpdateAvailable {
                    name: info.display_name,
                    image: info.image_ref,
                    local: local_digest,
                    remote: remote_digest,
                    info_url,
                    already_pulled,
                    state,
                }
            }
        }
        Err(e) => CheckResult::Error {
            name: info.display_name,
            image: info.image_ref,
            reason: e.to_string(),
            state,
        },
    }
}
