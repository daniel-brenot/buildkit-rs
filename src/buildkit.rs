//! Dockerfile-driven OCI image builder.

use std::path::{Path, PathBuf};

use oci_distribution::Reference;

use crate::backend::Backend;
use crate::cache::LayerCache;
use crate::materialize::{materialize_rootfs, materialize_rootfs_with_progress};
use crate::platform::Platform;
use crate::progress::{BuildProgress, NullProgress, PullProgress};
use crate::pull::{pull_image, pull_image_with_progress, PulledImage};
use crate::reference::parse_reference;
use crate::request::{BuildRequest, BuildResult};
use crate::store::{ImageStore, LocalImageStore};
use crate::Error;

/// Builds OCI images from Dockerfiles.
///
/// `B` is the runtime that executes `RUN` instructions. `S` is the
/// [`ImageStore`] that holds pulled and exported blobs. Pull, unpack, `COPY`,
/// `ADD`, overlay2 layer cache, and image export stay in this crate.
///
/// Images live in `S`. Overlay2 cache and scratch work live under
/// [`ImageStore::root`] unless you override them:
///
/// ```text
/// <store.root()>/
///   images/    pulled and exported image config and layers (LocalImageStore)
///   overlay2/  instruction cache (Docker overlay2 layers)
///   work/      temporary stage rootfs during a build
/// ```
///
/// ```no_run
/// use buildkit::{BuildRequest, Buildkit, NoopBackend};
///
/// # async fn example() -> Result<(), buildkit::Error> {
/// let kit = Buildkit::new(NoopBackend)?;
/// let result = kit
///     .build(BuildRequest::new(".").tag("myapp:latest"))
///     .await?;
/// println!("{:?}", result.image_ids);
/// # Ok(())
/// # }
/// ```
pub struct Buildkit<B, S = LocalImageStore> {
    backend: B,
    store: S,
    cache: LayerCache,
    work_root: PathBuf,
}

impl<B: Default> Buildkit<B, LocalImageStore> {
    /// Construct with `B::default()` and the platform-default Docker overlay2 store.
    pub fn default() -> Result<Self, Error> {
        Self::new(B::default())
    }
}

impl<B> Buildkit<B, LocalImageStore> {
    /// Create a builder with `backend` and the platform-default
    /// [`LocalImageStore`] ([`LocalImageStore::default`]).
    pub fn new(backend: B) -> Result<Self, Error> {
        Self::with_image_store(backend, LocalImageStore::default())
    }
}

impl<B, S: ImageStore> Buildkit<B, S> {
    /// Create a builder with `backend` and a caller-provided [`ImageStore`].
    pub fn with_image_store(backend: B, store: S) -> Result<Self, Error> {
        let cache = LayerCache::open(store.root())?;
        let work_root = store.root().join("work");
        Ok(Self {
            backend,
            store,
            cache,
            work_root,
        })
    }

    /// Override the overlay2 layer cache (defaults to `<store.root()>/overlay2`).
    pub fn with_cache(mut self, cache: LayerCache) -> Self {
        self.cache = cache;
        self
    }

    /// Override the temporary build-work directory (defaults to `<store.root()>/work`).
    pub fn with_work_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.work_root = dir.into();
        self
    }

    /// Image store this builder reads and writes.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Overlay2 layer cache used for instruction snapshots.
    pub fn cache(&self) -> &LayerCache {
        &self.cache
    }

    /// Runtime that executes `RUN` instructions.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Temporary directory for in-progress stage rootfs trees.
    pub fn work_root(&self) -> &Path {
        &self.work_root
    }
}

impl<B: Backend, S: ImageStore> Buildkit<B, S> {
    /// Parse a Dockerfile and execute it against the configured backend.
    ///
    /// Tags the result as `buildkit:latest` when [`BuildRequest::tags`] is empty.
    /// Progress events are discarded; see [`Self::build_with_progress`].
    pub async fn build(&self, request: BuildRequest) -> Result<BuildResult, Error> {
        self.build_with_progress(request, &mut NullProgress).await
    }

    /// [`Self::build`] that reports solve steps, cache hits, and export to `progress`.
    pub async fn build_with_progress(
        &self,
        request: BuildRequest,
        progress: &mut dyn BuildProgress,
    ) -> Result<BuildResult, Error> {
        crate::executor::build(self, request, progress).await
    }

    /// Pull `image` into this builder's store if it is not already cached.
    ///
    /// `force_pull` consults the registry even when a local copy exists.
    pub async fn ensure_image(
        &self,
        image: &str,
        platform: &Platform,
        force_pull: bool,
    ) -> Result<Reference, Error> {
        self.ensure_image_with_progress(image, platform, force_pull, &mut NullProgress)
            .await
    }

    /// [`Self::ensure_image`] that reports download progress to `progress`.
    pub async fn ensure_image_with_progress(
        &self,
        image: &str,
        platform: &Platform,
        force_pull: bool,
        progress: &mut dyn PullProgress,
    ) -> Result<Reference, Error> {
        let reference = parse_reference(image)?;
        if force_pull || !self.store.is_cached(&reference, platform) {
            tracing::debug!(image = %reference, "pulling image");
            pull_image_with_progress(&self.store, &reference, platform, progress).await?;
        }
        Ok(reference)
    }

    /// Pull `reference` from its registry into the local image cache.
    pub async fn pull_image(
        &self,
        reference: &Reference,
        platform: &Platform,
    ) -> Result<PulledImage, Error> {
        pull_image(&self.store, reference, platform).await
    }

    /// Expand cached layers for `image` into `dest`.
    ///
    /// The image must already be in the store; pull it first with
    /// [`Self::ensure_image`] or [`Self::pull_image`].
    pub fn materialize_rootfs(
        &self,
        image: &str,
        platform: &Platform,
        dest: &Path,
    ) -> Result<Reference, Error> {
        let reference = parse_reference(image)?;
        materialize_rootfs(&self.store, &reference, platform, dest)?;
        Ok(reference)
    }

    /// [`Self::materialize_rootfs`] that reports extract progress to `progress`.
    pub fn materialize_rootfs_with_progress(
        &self,
        image: &str,
        platform: &Platform,
        dest: &Path,
        progress: &mut dyn PullProgress,
    ) -> Result<Reference, Error> {
        let reference = parse_reference(image)?;
        materialize_rootfs_with_progress(&self.store, &reference, platform, dest, progress, true)?;
        Ok(reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NoopBackend, RunRequest, RunResult};

    #[tokio::test]
    async fn run_goes_to_backend() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<Vec<String>>>>);

        impl Backend for Capture {
            type Error = std::io::Error;

            async fn run(&self, request: &RunRequest) -> Result<RunResult, Self::Error> {
                self.0.lock().unwrap().push(request.args.clone());
                Ok(RunResult::success())
            }
        }

        let root = std::env::temp_dir().join(format!(
            "buildkit-run-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ctx = root.join("ctx");
        std::fs::create_dir_all(&ctx).unwrap();
        std::fs::write(ctx.join("Dockerfile"), "FROM scratch\nRUN echo hello\n").unwrap();

        let capture = Capture::default();
        let kit =
            Buildkit::with_image_store(capture.clone(), LocalImageStore::new(root.join("store")))
                .unwrap();
        kit.build(BuildRequest::new(&ctx).tag("run-test:latest"))
            .await
            .unwrap();
        let runs = capture.0.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], vec!["/bin/sh", "-c", "echo hello"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn builds_scratch_dockerfile() {
        let root = std::env::temp_dir().join(format!(
            "buildkit-scratch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let ctx = root.join("ctx");
        std::fs::create_dir_all(&ctx).unwrap();
        std::fs::write(
            ctx.join("Dockerfile"),
            "FROM scratch\nENV FOO=bar\nCMD [\"/hello\"]\n",
        )
        .unwrap();

        let kit = Buildkit::with_image_store(NoopBackend, LocalImageStore::new(root.join("store")))
            .unwrap();
        let result = kit
            .build(BuildRequest::new(&ctx).tag("scratch-test:latest"))
            .await
            .unwrap();
        assert_eq!(result.tags, vec!["scratch-test:latest"]);
        assert!(!result.image_ids.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
