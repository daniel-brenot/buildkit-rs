# Buildkit

Build OCI images from Dockerfiles in Rust, with a pluggable execution backend.

This crate pulls base images, applies Dockerfile instructions, and writes the result to a local image store. `RUN` is delegated to a [`Backend`](src/backend.rs) you provide, so any runtime can execute build steps.

## Features

- Multi-stage Dockerfiles, including `COPY --from`
- Local overlay2 layer cache
- Pluggable [`FileSystem`](src/fs.rs) for every file operation
- `.dockerignore` filtering
- Registry pulls with Docker-style progress events
- Registry auth from `~/.docker/config.json` or environment variables
- Export of gzipped OCI layers and image config into an on-disk store

## Requirements

- Rust 1.85 or later
- An async runtime to drive the public `async` APIs (this crate does not depend on Tokio)
- A `Backend` implementation if your Dockerfile contains `RUN` instructions

Only `linux/*` platforms are supported. The default pull platform is `linux/amd64`, or `linux/arm64` on Apple Silicon.

## Usage

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
buildkit = "0.1"
```

Build a context directory that contains a `Dockerfile`:

```rust
use buildkit::{BuildRequest, Buildkit, NoopBackend};

#[tokio::main]
async fn main() -> Result<(), buildkit::Error> {
    let kit = Buildkit::new(NoopBackend)?;

    let result = kit
        .build(
            BuildRequest::new(".")
                .tag("myapp:latest")
                .arg("VERSION", "1.0")
                .target("runtime"),
        )
        .await?;

    println!("{:?}", result.image_ids);
    Ok(())
}
```

`NoopBackend` always succeeds without running anything. Use it for tests and for Dockerfiles that have no `RUN` steps. For real builds, implement [`Backend`](#implementing-a-backend).

### Build options

`BuildRequest` mirrors common `docker build` flags:

| Method | Purpose |
| --- | --- |
| `.dockerfile(path)` | Dockerfile path (default `Dockerfile` in the context) |
| `.tag("name:tag")` | Image tag (default `buildkit:latest`) |
| `.arg(key, value)` | `--build-arg` |
| `.target("stage")` | Named or numeric `--target` stage |
| `.platform(platform)` | Target platform (`linux/amd64`, …) |
| `.pull(true)` | Always consult the registry for `FROM` images |
| `.no_cache(true)` | Skip local layer cache lookups |
| `.network(NetworkMode::Host)` | Network mode for `RUN` (`bridge`, `host`, `none`) |

### Implementing a backend

Only command execution is delegated. Pull, unpack, `COPY`, `ADD`, metadata, and export stay in this crate.

```rust
use buildkit::{Backend, RunRequest, RunResult};

struct MyRuntime;

impl Backend for MyRuntime {
    type Error = std::io::Error;

    async fn run(&self, request: &RunRequest) -> Result<RunResult, Self::Error> {
        // Execute `request.args` with `request.rootfs` as `/`.
        // Honor `request.env`, `request.cwd`, `request.user`, and `request.network`.
        let _ = request;
        Ok(RunResult::success())
    }
}
```

`RunResult.status` must be `0` or the build fails.

### Pulling and unpacking images

You can pull and materialize a rootfs without a full Dockerfile build:

```rust
use buildkit::{default_pull_platform, ensure_rootfs, ImageStore};

async fn pull_alpine() -> Result<(), buildkit::Error> {
    let store = ImageStore::new("/var/lib/buildkit".into());
    ensure_rootfs(
        &store,
        "alpine:3.19",
        std::path::Path::new("/tmp/alpine-rootfs"),
        &default_pull_platform(),
        false,
    )
    .await?;
    Ok(())
}
```

`Buildkit` also exposes `ensure_image`, `pull_image`, and `materialize_rootfs`.

### Progress

Pass a `BuildProgress` or `PullProgress` implementation to receive Docker-style events (`VertexStart`, layer download progress, cache hits, export, and so on). `NullProgress` discards events.

```rust
use buildkit::{BuildEvent, BuildProgress, BuildRequest};

struct Printer;

impl BuildProgress for Printer {
    fn on_event(&mut self, event: BuildEvent) {
        println!("{event:?}");
    }
}
```

Then call `kit.build_with_progress(request, &mut Printer).await`.

### Filesystem

Every file create, read, and delete during a build goes through `FileSystem`. `ImageStore` uses that implementation for image blobs and the overlay2 layout; the on-disk layout is not itself a plug-in. `Buildkit::new` uses `ImageStore::default` (host `std::fs` under Docker's data-root). Implement `FileSystem` to control how files are written.

```rust
use buildkit::{Buildkit, FileSystem, ImageStore, LocalFs, NoopBackend};

fn with_custom_fs(fs: impl FileSystem) -> Result<(), buildkit::Error> {
    let _kit = Buildkit::with_fs(NoopBackend, fs, "/var/lib/buildkit")?;
    Ok(())
}

fn with_local_dir() -> Result<(), buildkit::Error> {
    let _kit = Buildkit::with_store(
        NoopBackend,
        ImageStore::new("/var/lib/buildkit".into()),
    )?;
    Ok(())
}

fn with_default_host_fs() -> Result<(), buildkit::Error> {
    let _kit = Buildkit::with_fs(NoopBackend, LocalFs, "/var/lib/buildkit")?;
    Ok(())
}
```

### Registry authentication

Credentials are resolved in this order:

1. `config.json` under `DOCKER_CONFIG`, `BUILDKIT_CONFIG`, or `~/.docker`
2. `BUILDKIT_REGISTRY_USER` / `BUILDKIT_REGISTRY_PASSWORD`
3. Anonymous

Override the config directory with `buildkit::set_config_dir`. Docker itself does not need to be installed.

## Store layout

`ImageStore` keeps pulled images, layer cache, and scratch work under a single root:

```
<store>/
  images/     # pulled / exported image config and layers
  overlay2/   # instruction cache (Docker overlay2: diff/, lower, link)
  work/       # temporary stage rootfs during a build
```

## Supported instructions

| Instruction | Notes |
| --- | --- |
| `FROM` | Including `scratch` and multi-stage |
| `RUN` | Shell and exec form; `RUN --network=`; heredocs |
| `COPY` / `ADD` | `COPY --from`; `ADD` of remote `http(s)` URLs; heredocs |
| `ARG` / `ENV` / `LABEL` | Build-arg substitution |
| `WORKDIR` / `USER` / `SHELL` | Applied to later `RUN` / `COPY` |
| `CMD` / `ENTRYPOINT` | Written into image config |
| `EXPOSE` / `VOLUME` | Written into image config |

## License

Apache-2.0
