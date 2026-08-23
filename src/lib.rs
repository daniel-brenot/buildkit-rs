//! Build OCI images from Dockerfiles with a pluggable execution backend
//! and filesystem.
//!
//! [`Buildkit`] pulls base images, applies Dockerfile instructions, and writes
//! the result through an [`ImageStore`]. Everything except `RUN` is handled
//! here: unpacking layers, `COPY` / `ADD`, metadata, overlay2 layer cache, and
//! export. `RUN` is delegated to a [`Backend`] you provide. Every create, read,
//! and delete of files goes through [`FileSystem`] (a supertrait of
//! [`ImageStore`]); the default is [`LocalImageStore`] / [`LocalFs`].
//!
//! # Quick start
//!
//! ```no_run
//! use buildkit::{BuildRequest, Buildkit, NoopBackend};
//!
//! # async fn example() -> Result<(), buildkit::Error> {
//! let kit = Buildkit::new(NoopBackend)?;
//! let result = kit
//!     .build(BuildRequest::new(".").tag("myapp:latest"))
//!     .await?;
//! println!("{:?}", result.image_ids);
//! # Ok(())
//! # }
//! ```
//!
//! [`NoopBackend`] succeeds without running anything. Use it in tests or when
//! the Dockerfile has no `RUN` instructions. For real builds, implement
//! [`Backend`].
//!
//! The public `async` APIs are runtime-agnostic. Drive them with Tokio or any
//! other executor; this crate does not depend on Tokio.
//!
//! # Execution backend
//!
//! ```
//! use buildkit::{Backend, RunRequest, RunResult};
//!
//! struct MyRuntime;
//!
//! impl Backend for MyRuntime {
//!     type Error = std::io::Error;
//!
//!     async fn run(&self, request: &RunRequest) -> Result<RunResult, Self::Error> {
//!         // Execute `request.args` with `request.rootfs` as `/`.
//!         let _ = request;
//!         Ok(RunResult::success())
//!     }
//! }
//! ```
//!
//! A non-zero [`RunResult::status`] fails the build.
//!
//! # Image store
//!
//! [`ImageStore`] extends [`FileSystem`]: overlay2 cache, stage rootfs, `COPY`,
//! unpack, and export all call those methods instead of `std::fs`.
//! [`Buildkit::new`] uses [`LocalImageStore::default`]. Pass another directory
//! with [`LocalImageStore::new`], or implement [`FileSystem`] + [`ImageStore`]
//! and construct with [`Buildkit::with_image_store`].
//!
//! # Pulling without a Dockerfile
//!
//! [`ensure_image`] caches a registry image locally. [`ensure_rootfs`] also
//! unpacks layers into a directory. [`Buildkit`] exposes the same operations
//! against its store.
//!
//! # Progress and authentication
//!
//! Implement [`BuildProgress`] or [`PullProgress`] and pass it to
//! [`Buildkit::build_with_progress`] / [`pull_image_with_progress`].
//! [`NullProgress`] discards events.
//!
//! Registry credentials are read from Docker-style `config.json`, then
//! `BUILDKIT_REGISTRY_USER` / `BUILDKIT_REGISTRY_PASSWORD`, then anonymous.
//! Override the config directory with [`set_config_dir`].
//!
//! # Platforms
//!
//! Only `linux/*` images are supported. [`default_pull_platform`] is
//! `linux/arm64` on Apple Silicon and `linux/amd64` elsewhere.
//!
//! # Dockerfile coverage
//!
//! Supported: `FROM` (including `scratch` and multi-stage), `RUN` (shell and
//! exec, `RUN --network=`, heredocs), `COPY` / `ADD` (`COPY --from`, remote
//! HTTP(S) `ADD`, heredocs), `ARG`, `ENV`, `LABEL`, `WORKDIR`, `USER`,
//! `SHELL`, `CMD`, `ENTRYPOINT`, `EXPOSE`, `VOLUME`.
//!
//! Parsed but ignored: `MAINTAINER`, `ONBUILD`, `STOPSIGNAL`, `HEALTHCHECK`.

#![warn(missing_docs)]

mod auth;
mod backend;
mod buildkit;
mod cache;
mod context;
mod error;
mod executor;
mod expand;
mod export;
mod fs;
mod fsutil;
mod materialize;
mod platform;
mod progress;
mod pull;
mod reference;
mod request;
mod rootfs;
mod store;
mod unpack;
mod winpath;

pub use auth::set_config_dir;
pub use backend::Backend;
pub use backend::NetworkMode;
pub use backend::NoopBackend;
pub use backend::RunRequest;
pub use backend::RunResult;
pub use buildkit::Buildkit;
pub use cache::LayerCache;
pub use error::Error;
pub use export::ImageMeta;
pub use fs::FileSystem;
pub use fs::FsDirEntry;
pub use fs::FsMetadata;
pub use fs::LocalFs;
pub use materialize::materialize_rootfs;
pub use materialize::materialize_rootfs_with_progress;
pub use platform::default_pull_platform;
pub use platform::Platform;
pub use progress::BuildEvent;
pub use progress::BuildProgress;
pub use progress::NullProgress;
pub use progress::PullEvent;
pub use progress::PullProgress;
pub use pull::finish_pull_progress;
pub use pull::pull_and_materialize;
pub use pull::pull_image;
pub use pull::pull_image_default;
pub use pull::pull_image_with_progress;
pub use pull::PulledImage;
pub use reference::parse_reference;
pub use request::BuildRequest;
pub use request::BuildResult;
pub use store::ImageStore;
pub use store::LocalImageStore;
pub use store::StoredImage;
pub use store::StoredLayer;

#[doc(inline)]
pub use dockerfile::{Dockerfile, Instruction, Stage};

use oci_distribution::Reference;

use crate::progress::PullEvent as ProgressEvent;

/// Pull `image` into `store` if it is not already cached for `platform`.
///
/// Does not unpack layers. Pass `force_pull` to consult the registry even when
/// a local copy exists. See [`ensure_rootfs`] to also materialize a rootfs.
pub async fn ensure_image<S: ImageStore>(
    store: &S,
    image: &str,
    platform: &Platform,
    force_pull: bool,
) -> Result<Reference, Error> {
    ensure_image_with_progress(store, image, platform, force_pull, &mut NullProgress).await
}

/// [`ensure_image`] that reports download progress to `progress`.
pub async fn ensure_image_with_progress<S: ImageStore>(
    store: &S,
    image: &str,
    platform: &Platform,
    force_pull: bool,
    progress: &mut dyn PullProgress,
) -> Result<Reference, Error> {
    let reference = parse_reference(image)?;
    if force_pull || !store.is_cached(&reference, platform) {
        tracing::debug!(image = %reference, "pulling image");
        pull_image_with_progress(store, &reference, platform, progress).await?;
    }
    Ok(reference)
}

/// Pull `image` if needed and unpack its layers into `dest`.
///
/// Reuses a previous unpack of the same content when the fingerprint still
/// matches. `force_pull` always re-fetches from the registry first.
pub async fn ensure_rootfs<S: ImageStore>(
    store: &S,
    image: &str,
    dest: &std::path::Path,
    platform: &Platform,
    force_pull: bool,
) -> Result<Reference, Error> {
    ensure_rootfs_with_progress(
        store,
        image,
        dest,
        platform,
        force_pull,
        &mut NullProgress,
        false,
    )
    .await
}

/// [`ensure_rootfs`] with Docker-style pull and extract events.
///
/// When `announce_missing` is true and the image is not cached, emits
/// [`PullEvent::UnableToFindLocally`] before pulling.
pub async fn ensure_rootfs_with_progress<S: ImageStore>(
    store: &S,
    image: &str,
    dest: &std::path::Path,
    platform: &Platform,
    force_pull: bool,
    progress: &mut dyn PullProgress,
    announce_missing: bool,
) -> Result<Reference, Error> {
    let reference = parse_reference(image)?;
    let missing = !store.is_cached(&reference, platform);
    let do_pull = force_pull || missing;
    if do_pull {
        if announce_missing && missing {
            progress.event(ProgressEvent::UnableToFindLocally {
                reference: image.to_string(),
            });
        }
        tracing::debug!(image = %reference, "pulling image");
        let pulled = pull_image_with_progress(store, &reference, platform, progress).await?;
        materialize_rootfs_with_progress(store, &reference, platform, dest, progress, true)?;
        finish_pull_progress(image, pulled.digest.as_deref(), pulled.updated, progress);
    } else {
        materialize_rootfs_with_progress(store, &reference, platform, dest, progress, true)?;
    }
    Ok(reference)
}
