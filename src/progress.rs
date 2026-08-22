//! Progress events emitted while pulling images and executing builds.

use std::time::Duration;

/// Events matching Docker's pull progress stream.
#[derive(Debug, Clone)]
pub enum PullEvent {
    /// Printed by `docker run` when the image is not cached locally.
    UnableToFindLocally {
        /// Image reference the caller asked for.
        reference: String,
    },
    /// `{tag}: Pulling from {repository}`
    PullingFrom {
        /// Tag being pulled (`latest` if omitted).
        tag: String,
        /// Repository path without registry prefix handling.
        repository: String,
    },
    /// Layer status without a byte counter (e.g. "Pulling fs layer").
    LayerStatus {
        /// Short layer id (first 12 hex chars of the digest).
        id: String,
        /// Status text (`Pulling fs layer`, `Download complete`, …).
        status: String,
    },
    /// Layer status with byte progress (e.g. "Downloading", "Extracting").
    LayerProgress {
        /// Short layer id (first 12 hex chars of the digest).
        id: String,
        /// Status text (`Downloading`, `Extracting`, …).
        status: String,
        /// Bytes transferred so far.
        current: u64,
        /// Total layer size from the manifest.
        total: u64,
    },
    /// `Digest: sha256:...`
    Digest {
        /// Full manifest digest.
        digest: String,
    },
    /// `Status: Downloaded newer image for ...`
    Status {
        /// Human-readable status line.
        message: String,
    },
    /// Printed when unpacking cached layers into a rootfs (no registry pull).
    Unpacking {
        /// Image reference being unpacked.
        reference: String,
        /// Number of layer blobs to extract.
        layers: usize,
    },
}

/// Receives pull and unpack progress.
pub trait PullProgress: Send {
    /// Handle one progress event (layer download, extract, digest, …).
    fn event(&mut self, event: PullEvent);
}

/// A single build progress event.
#[derive(Debug, Clone)]
pub enum BuildEvent {
    /// Build is starting.
    BuildStart {
        /// Builder name (`buildkit`).
        builder: String,
    },
    /// A new solve vertex / step began.
    VertexStart {
        /// Vertex id allocated for this build.
        id: u32,
        /// Display name, for example `[stage-0 2/5] RUN echo hi`.
        name: String,
    },
    /// Transient status for the active vertex.
    VertexStatus {
        /// Vertex this status belongs to.
        id: u32,
        /// Short status (`running`, `copying to /app`, …).
        status: String,
    },
    /// A log line associated with a vertex (for example `RUN` output).
    VertexLog {
        /// Vertex this log belongs to.
        id: u32,
        /// One line of output, without a trailing newline.
        line: String,
    },
    /// Vertex finished successfully.
    VertexDone {
        /// Vertex that finished.
        id: u32,
        /// Whether the step was satisfied from the local layer cache.
        cached: bool,
        /// Wall time spent on this vertex.
        duration: Duration,
    },
    /// Vertex failed.
    VertexError {
        /// Vertex that failed.
        id: u32,
        /// Error message.
        error: String,
        /// Wall time spent before failure.
        duration: Duration,
    },
    /// Image export / tagging phase.
    Exporting,
    /// Build finished; image refs/ids for quiet mode and final display.
    Finished {
        /// Local image identifiers written to the store.
        image_ids: Vec<String>,
    },
}

/// Receives build progress events (solve vertices, cache hits, export).
pub trait BuildProgress: Send {
    /// Handle one build event.
    fn on_event(&mut self, event: BuildEvent);
}

/// Discard all progress events.
///
/// Used by the `*_with_progress` APIs' quiet counterparts.
#[derive(Debug, Default)]
pub struct NullProgress;

impl PullProgress for NullProgress {
    fn event(&mut self, _event: PullEvent) {}
}

impl BuildProgress for NullProgress {
    fn on_event(&mut self, _event: BuildEvent) {}
}

/// Helper used by the executor to allocate vertex ids and emit events.
pub(crate) struct ProgressEmitter<'a> {
    reporter: &'a mut dyn BuildProgress,
    next_id: u32,
}

impl<'a> ProgressEmitter<'a> {
    pub fn new(reporter: &'a mut dyn BuildProgress) -> Self {
        Self {
            reporter,
            next_id: 1,
        }
    }

    pub fn emit(&mut self, event: BuildEvent) {
        self.reporter.on_event(event);
    }

    pub fn start(&mut self, name: impl Into<String>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.emit(BuildEvent::VertexStart {
            id,
            name: name.into(),
        });
        id
    }

    pub fn status(&mut self, id: u32, status: impl Into<String>) {
        self.emit(BuildEvent::VertexStatus {
            id,
            status: status.into(),
        });
    }

    pub fn done(&mut self, id: u32, duration: Duration) {
        self.emit(BuildEvent::VertexDone {
            id,
            cached: false,
            duration,
        });
    }

    pub fn cached(&mut self, id: u32, duration: Duration) {
        self.emit(BuildEvent::VertexDone {
            id,
            cached: true,
            duration,
        });
    }

    pub fn error(&mut self, id: u32, error: impl Into<String>, duration: Duration) {
        self.emit(BuildEvent::VertexError {
            id,
            error: error.into(),
            duration,
        });
    }
}

/// Short layer id used by Docker (first 12 hex characters of the digest).
pub fn short_layer_id(digest: &str) -> String {
    let hex = digest
        .strip_prefix("sha256:")
        .or_else(|| digest.strip_prefix("sha512:"))
        .unwrap_or(digest);
    hex.chars().take(12).collect()
}
