#[derive(Default)]
pub struct PruneableInfo {
    pub dangling_count: usize,
    pub dangling_bytes: u64,
    pub unused_count: usize,
    pub unused_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemState {
    Running,
    Stopped,
    Image, // locally stored image without any associated container
}

#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub display_name: String, // container name or image ref for standalone images
    pub image_ref: String,
    pub local_digest: Option<String>,
    pub state: ItemState,
}

#[derive(Debug)]
pub enum CheckResult {
    UpToDate {
        name: String,
        image: String,
        state: ItemState,
    },
    UpdateAvailable {
        name: String,
        image: String,
        local: String,
        remote: String,
        info_url: String,
        already_pulled: bool,
        state: ItemState,
    },
    LocalOnly {
        name: String,
        image: String,
        state: ItemState,
    },
    Error {
        name: String,
        image: String,
        reason: String,
        state: ItemState,
    },
}

impl CheckResult {
    pub fn sort_key(&self) -> &str {
        match self {
            Self::UpToDate { name, .. } => name,
            Self::UpdateAvailable { name, .. } => name,
            Self::LocalOnly { name, .. } => name,
            Self::Error { name, .. } => name,
        }
    }
}
