//! Pluggable execution of Dockerfile `RUN` instructions.

use std::path::PathBuf;

/// How `RUN` instructions should reach the network.
///
/// Matches Docker's `--network` values. The backend is responsible for
/// enforcing the policy; this crate only records what was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Isolated / NAT networking (Docker `bridge`).
    #[default]
    Bridge,
    /// Share the host network namespace (Docker `host`).
    Host,
    /// No network access (Docker `none`).
    None,
}

impl NetworkMode {
    /// Parse a Docker-style network name (`bridge`, `host`, `none`).
    ///
    /// Empty string, `bridge`, and `default` all map to [`Self::Bridge`].
    pub fn parse(name: &str) -> Result<Self, crate::Error> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "bridge" | "default" => Ok(Self::Bridge),
            "host" => Ok(Self::Host),
            "none" => Ok(Self::None),
            other => Err(crate::Error::other(format!(
                "unknown network mode '{other}' (expected bridge, host, or none)"
            ))),
        }
    }

    /// Canonical Docker name for this mode (`bridge`, `host`, or `none`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Host => "host",
            Self::None => "none",
        }
    }
}

impl std::fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A command the backend should execute inside a stage rootfs.
///
/// Paths in [`Self::rootfs`] are on the host. [`Self::cwd`] and [`Self::user`]
/// are guest values from the image config / Dockerfile.
#[derive(Debug, Clone)]
pub struct RunRequest {
    /// Host path of the writable stage root filesystem. Treat this directory as
    /// the container's `/`.
    pub rootfs: PathBuf,
    /// Process arguments (already expanded; shell form is `[shell..., command]`).
    pub args: Vec<String>,
    /// `KEY=value` environment entries, including image `ENV` and in-scope `ARG`.
    pub env: Vec<String>,
    /// Guest working directory (`WORKDIR`), typically starting with `/`.
    pub cwd: String,
    /// Image `USER` spec (`uid`, `name`, `uid:gid`, …), if set.
    pub user: Option<String>,
    /// Network policy for this `RUN`.
    pub network: NetworkMode,
}

/// Outcome of a backend `RUN`.
///
/// A non-zero [`Self::status`] fails the build. Runtime failures (could not
/// spawn, I/O, …) should be returned as `Err` from [`Backend::run`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// Process exit status. Non-zero fails the build.
    pub status: i32,
}

impl RunResult {
    /// Successful run (`status == 0`).
    pub fn success() -> Self {
        Self { status: 0 }
    }

    /// Whether [`Self::status`] is zero.
    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

/// Executes `RUN` instructions. Everything else (`FROM`, `COPY`, `ENV`, …)
/// stays in this crate; only the command that mutates the rootfs at build time
/// is delegated here so any runtime can be plugged in.
///
/// # Example
///
/// ```
/// use buildkit::{Backend, RunRequest, RunResult};
///
/// struct MyRuntime;
///
/// impl Backend for MyRuntime {
///     type Error = std::io::Error;
///
///     async fn run(&self, request: &RunRequest) -> Result<RunResult, Self::Error> {
///         let _ = request;
///         Ok(RunResult::success())
///     }
/// }
/// ```
#[allow(async_fn_in_trait)]
pub trait Backend: Send + Sync {
    /// Error type returned when the runtime cannot start or wait for the process.
    ///
    /// Displayed as [`crate::Error::Backend`]. Does not include a non-zero
    /// exit status; report that via [`RunResult::status`].
    type Error: std::fmt::Display + Send + Sync + 'static;

    /// Run `request.args` with `request.rootfs` as `/`.
    ///
    /// Honor [`RunRequest::env`], [`RunRequest::cwd`], [`RunRequest::user`],
    /// and [`RunRequest::network`]. Return [`RunResult::success`] only when
    /// the process exits 0.
    async fn run(&self, request: &RunRequest) -> Result<RunResult, Self::Error>;
}

/// A backend that ignores `RUN` and always succeeds.
///
/// Useful in tests and for Dockerfiles that have no `RUN` steps. It does not
/// mutate the rootfs.
#[derive(Debug, Default)]
pub struct NoopBackend;

impl Backend for NoopBackend {
    type Error = std::io::Error;

    async fn run(&self, _request: &RunRequest) -> Result<RunResult, Self::Error> {
        Ok(RunResult::success())
    }
}
