//! Inputs and outputs for a local Dockerfile build.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::backend::NetworkMode;
use crate::platform::Platform;

/// Inputs for a local image build.
///
/// Mirrors common `docker build` flags. Construct with [`Self::new`] and chain
/// setters:
///
/// ```
/// use buildkit::{BuildRequest, NetworkMode, Platform};
///
/// let request = BuildRequest::new("./app")
///     .dockerfile("Dockerfile.prod")
///     .tag("myapp:latest")
///     .arg("VERSION", "1.0")
///     .target("runtime")
///     .platform(Platform::linux_amd64())
///     .pull(true)
///     .network(NetworkMode::Host);
/// assert_eq!(request.tags, ["myapp:latest"]);
/// ```
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// Build context directory (`.` in `docker build .`).
    pub context: PathBuf,
    /// Dockerfile path, relative to `context` unless absolute.
    pub dockerfile: PathBuf,
    /// Image tags to write (`name:tag`). Defaults to `buildkit:latest` if empty.
    pub tags: Vec<String>,
    /// `--build-arg` overrides keyed by argument name.
    pub build_args: HashMap<String, String>,
    /// Named or numeric `--target` stage. `None` builds the last stage.
    pub target: Option<String>,
    /// Target platform. Defaults to [`crate::default_pull_platform`].
    pub platform: Option<Platform>,
    /// Always consult the registry for `FROM` images, even if cached locally.
    pub pull: bool,
    /// Skip local layer cache lookups (still writes new cache entries).
    pub no_cache: bool,
    /// Network mode for `RUN` unless a `RUN --network=` flag overrides it.
    pub network: NetworkMode,
}

impl BuildRequest {
    /// Build the context at `context`, using `Dockerfile` in that directory.
    ///
    /// Tags default to `buildkit:latest` when none are added with [`Self::tag`].
    pub fn new(context: impl Into<PathBuf>) -> Self {
        Self {
            context: context.into(),
            dockerfile: PathBuf::from("Dockerfile"),
            tags: Vec::new(),
            build_args: HashMap::new(),
            target: None,
            platform: None,
            pull: false,
            no_cache: false,
            network: NetworkMode::default(),
        }
    }

    /// Set the Dockerfile path (relative to the context unless absolute).
    pub fn dockerfile(mut self, path: impl Into<PathBuf>) -> Self {
        self.dockerfile = path.into();
        self
    }

    /// Append an image tag (`name:tag`) to write on success.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Append several image tags.
    pub fn tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags.extend(tags.into_iter().map(Into::into));
        self
    }

    /// Set a `--build-arg` override.
    pub fn arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.build_args.insert(key.into(), value.into());
        self
    }

    /// Build only this named or numeric stage (`--target`).
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Set the target platform (for example `linux/amd64`).
    pub fn platform(mut self, platform: Platform) -> Self {
        self.platform = Some(platform);
        self
    }

    /// When `true`, always consult the registry for `FROM` images.
    pub fn pull(mut self, pull: bool) -> Self {
        self.pull = pull;
        self
    }

    /// When `true`, skip local layer cache lookups.
    pub fn no_cache(mut self, no_cache: bool) -> Self {
        self.no_cache = no_cache;
        self
    }

    /// Set the default network mode for `RUN`.
    pub fn network(mut self, network: NetworkMode) -> Self {
        self.network = network;
        self
    }
}

/// Result of a successful build.
#[derive(Debug, Clone)]
pub struct BuildResult {
    /// Tags written to the image store (at least one).
    pub tags: Vec<String>,
    /// Local image identifiers recorded for those tags.
    pub image_ids: Vec<String>,
}
