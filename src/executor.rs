//! Execute a parsed Dockerfile against a local context.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dockerfile::{Command, Dockerfile, Instruction, Stage};

use crate::backend::{Backend, NetworkMode, RunRequest};
use crate::buildkit::Buildkit;
use crate::cache::{self, LayerCache};
use crate::context::{copy_into, BuildContext};
use crate::expand;
use crate::export::{self, ImageMeta};
use crate::fsutil::{copy_tree, guest_to_host, join_workdir};
use crate::materialize::materialize_rootfs;
use crate::platform::default_pull_platform;
use crate::platform::Platform;
use crate::progress::{BuildEvent, BuildProgress, ProgressEmitter};
use crate::reference::parse_reference;
use crate::request::{BuildRequest, BuildResult};
use crate::store::ImageStore;
use crate::Error;

const DEFAULT_TAG: &str = "buildkit:latest";

#[derive(Clone)]
struct StageState {
    rootfs: PathBuf,
    /// When true, `rootfs` points into the layer cache and must be copied before mutation.
    rootfs_shared: bool,
    /// Writable rootfs directory for this stage (under the build work dir).
    work_rootfs: PathBuf,
    meta: ImageMeta,
    args: HashMap<String, String>,
    /// Current chain id (for export blob reuse).
    layer_id: String,
}

pub async fn build<B: Backend, S: ImageStore>(
    kit: &Buildkit<B, S>,
    request: BuildRequest,
    progress: &mut dyn BuildProgress,
) -> Result<BuildResult, Error> {
    let mut progress = ProgressEmitter::new(progress);
    progress.emit(BuildEvent::BuildStart {
        builder: "buildkit".into(),
    });

    let t0 = Instant::now();
    let id_df = progress.start("[internal] load build definition from Dockerfile");
    let context = BuildContext::open(&request.context)?;
    let dockerfile_path = if request.dockerfile.is_absolute() {
        request.dockerfile.clone()
    } else {
        context.root().join(&request.dockerfile)
    };
    let df = fs::read_to_string(&dockerfile_path).map_err(|e| {
        Error::io(
            dockerfile_path.clone(),
            std::io::Error::new(e.kind(), format!("failed to read Dockerfile: {e}")),
        )
    })?;
    progress.status(id_df, format!("transferring dockerfile: {}B", df.len()));
    progress.done(id_df, t0.elapsed());

    let t0 = Instant::now();
    let id_ctx = progress.start("[internal] load build context");
    progress.status(id_ctx, format!("context: {}", request.context.display()));
    progress.done(id_ctx, t0.elapsed());

    let dockerfile = Dockerfile::parse(&df)?;
    if dockerfile.stages.is_empty() {
        return Err(Error::other("dockerfile: no FROM instruction found"));
    }
    let global_args = resolve_global_args(&dockerfile, &request.build_args);
    let stages = &dockerfile.stages;
    let platform = request
        .platform
        .clone()
        .unwrap_or_else(default_pull_platform);
    let tags = if request.tags.is_empty() {
        vec![DEFAULT_TAG.to_string()]
    } else {
        request.tags.clone()
    };

    let target_idx = select_stage(stages, request.target.as_deref())?;
    let build_id = format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let work = export::work_dir(kit.work_root(), &build_id);
    fs::create_dir_all(&work)?;

    let mut completed: HashMap<String, StageState> = HashMap::new();
    let mut last_state: Option<StageState> = None;

    for (idx, stage) in stages.iter().enumerate() {
        let stage_dir = work.join(format!("stage-{idx}"));
        let rootfs = stage_dir.join("rootfs");
        fs::create_dir_all(&rootfs)?;

        let base = expand::expand(&stage.image.as_str(), &global_args);
        let base_as_scratch = base.eq_ignore_ascii_case("scratch");
        let stage_platform = stage_platform(stage, &platform)?;

        let step_total = stage.instructions.len() + 1;
        let stage_label = stage
            .name
            .as_deref()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("stage-{idx}"));

        let t0 = Instant::now();
        let from_name = if base_as_scratch {
            format!("[{stage_label} 1/{step_total}] FROM scratch")
        } else {
            format!("[{stage_label} 1/{step_total}] FROM {base}")
        };
        let id_from = progress.start(from_name);

        if !base_as_scratch {
            if let Err(e) = kit.ensure_image(&base, &stage_platform, request.pull).await {
                progress.error(id_from, e.to_string(), t0.elapsed());
                return Err(e);
            }
        }

        let from_key = cache::from_cache_key(kit.store(), &base, base_as_scratch, &stage_platform);
        let mut parent_id = String::new();
        let from_id = cache::chain_id(&parent_id, &from_key);
        let mut cache_busted = request.no_cache;

        let mut state = {
            let try_cache = !cache_busted && kit.cache().has(&from_id);
            let loaded = if try_cache {
                match (
                    kit.cache().load_meta(&from_id),
                    kit.cache().resolve_rootfs(&from_id),
                ) {
                    (Ok((meta, args)), Ok(snap)) => {
                        progress.cached(id_from, std::time::Duration::ZERO);
                        Some(StageState {
                            rootfs: snap,
                            rootfs_shared: true,
                            work_rootfs: rootfs.clone(),
                            meta,
                            args,
                            layer_id: from_id.clone(),
                        })
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        tracing::warn!(error = %e, "FROM cache load failed; rebuilding");
                        None
                    }
                }
            } else {
                None
            };

            if let Some(s) = loaded {
                parent_id = from_id.clone();
                s
            } else {
                if !base_as_scratch {
                    progress.status(id_from, format!("resolve {base}"));
                    let bundle = stage_dir.join("base-rootfs");
                    if let Err(e) = materialize_rootfs(
                        kit.store(),
                        &parse_reference(&base)?,
                        &stage_platform,
                        &bundle,
                    ) {
                        progress.error(id_from, e.to_string(), t0.elapsed());
                        return Err(e);
                    }
                }
                match init_stage_from_image(
                    kit.store(),
                    &stage_platform,
                    &base,
                    base_as_scratch,
                    &stage_dir,
                    &rootfs,
                    &request.build_args,
                ) {
                    Ok(mut s) => {
                        s.work_rootfs = rootfs.clone();
                        s.layer_id = from_id.clone();
                        let _ = kit
                            .cache()
                            .save(&from_id, "", &from_key, &s.meta, &s.args, &s.rootfs, true);
                        progress.done(id_from, t0.elapsed());
                        parent_id = from_id.clone();
                        s
                    }
                    Err(e) => {
                        progress.error(id_from, e.to_string(), t0.elapsed());
                        return Err(e);
                    }
                }
            }
        };

        for (inst_idx, inst) in stage.instructions.iter().enumerate() {
            let step = inst_idx + 2;
            let name = format!(
                "[{stage_label} {step}/{step_total}] {}",
                instruction_summary(inst, &state)
            );
            let t0 = Instant::now();
            let id = progress.start(name);

            let donor_map: HashMap<String, (PathBuf, String)> = completed
                .iter()
                .map(|(k, v)| (k.clone(), (v.rootfs.clone(), v.meta.working_dir.clone())))
                .collect();
            let (ikey, fs_changed) = match cache::instruction_cache_key(
                inst,
                &state.meta,
                &state.args,
                &context,
                &donor_map,
                request.network.as_str(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    progress.error(id, e.to_string(), t0.elapsed());
                    return Err(e);
                }
            };
            let layer_id = cache::chain_id(&parent_id, &ikey);

            if !cache_busted && kit.cache().has(&layer_id) {
                match (
                    kit.cache().load_meta(&layer_id),
                    kit.cache().resolve_rootfs(&layer_id),
                ) {
                    (Ok((meta, args)), Ok(snap)) => {
                        state.meta = meta;
                        state.args = args;
                        state.rootfs = snap;
                        state.rootfs_shared = true;
                        state.layer_id = layer_id.clone();
                        progress.cached(id, std::time::Duration::ZERO);
                        parent_id = layer_id;
                        continue;
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        tracing::warn!(error = %e, "build cache load failed; rebuilding");
                    }
                }
            }

            if instruction_needs_writable(inst) {
                if let Err(e) = prepare_writable(&mut state) {
                    progress.error(id, e.to_string(), t0.elapsed());
                    return Err(e);
                }
            }

            if let Err(e) = apply_instruction(
                kit,
                &mut state,
                inst,
                &context,
                &completed,
                &global_args,
                request.network,
                &mut progress,
                id,
            )
            .await
            {
                progress.error(id, e.to_string(), t0.elapsed());
                return Err(e);
            }
            let _ = kit.cache().save(
                &layer_id,
                &parent_id,
                &ikey,
                &state.meta,
                &state.args,
                &state.rootfs,
                fs_changed,
            );
            state.layer_id = layer_id.clone();
            progress.done(id, t0.elapsed());
            parent_id = layer_id;
            cache_busted = true;
        }

        let snapshot = state.clone();
        completed.insert(idx.to_string(), snapshot.clone());
        if let Some(name) = &stage.name {
            completed.insert(name.clone(), snapshot);
        }

        last_state = Some(state);
        if idx == target_idx {
            break;
        }
    }

    let final_state = last_state.ok_or_else(|| Error::other("no build stage produced"))?;
    let t0 = Instant::now();
    let id_export = progress.start("[internal] exporting to image");
    let refs = export_final(
        kit.store(),
        kit.cache(),
        &final_state,
        &tags,
        &platform,
        &mut progress,
        id_export,
        t0,
    )?;
    progress.done(id_export, t0.elapsed());

    let _ = fs::remove_dir_all(&work);

    let image_ids: Vec<String> = refs.iter().map(|r| r.to_string()).collect();
    progress.emit(BuildEvent::Finished {
        image_ids: image_ids.clone(),
    });

    Ok(BuildResult { tags, image_ids })
}

fn export_final<S: ImageStore>(
    store: &S,
    cache: &LayerCache,
    final_state: &StageState,
    tags: &[String],
    platform: &Platform,
    progress: &mut ProgressEmitter<'_>,
    id_export: u32,
    t0: Instant,
) -> Result<Vec<oci_distribution::Reference>, Error> {
    if cache.has_layer_blob(&final_state.layer_id) {
        if let Some(blob_path) = cache.layer_blob_path(&final_state.layer_id) {
            let digest = match cache.layer_blob_digest(&final_state.layer_id) {
                Ok(d) => d,
                Err(e) => {
                    progress.error(id_export, e.to_string(), t0.elapsed());
                    return Err(e);
                }
            };
            progress.status(id_export, format!("naming to {}", tags.join(", ")));
            match export::export_image_layer_file(
                store,
                &blob_path,
                &digest,
                &final_state.meta,
                tags,
                platform,
                "buildkit",
            ) {
                Ok(r) => Ok(r),
                Err(e) => {
                    progress.error(id_export, e.to_string(), t0.elapsed());
                    Err(e)
                }
            }
        } else {
            let layer = match cache.read_layer_blob(&final_state.layer_id) {
                Ok(bytes) => bytes,
                Err(e) => {
                    progress.error(id_export, e.to_string(), t0.elapsed());
                    return Err(e);
                }
            };
            progress.status(id_export, format!("naming to {}", tags.join(", ")));
            match export::export_image_layer(
                store,
                &layer,
                &final_state.meta,
                tags,
                platform,
                "buildkit",
            ) {
                Ok(r) => Ok(r),
                Err(e) => {
                    progress.error(id_export, e.to_string(), t0.elapsed());
                    Err(e)
                }
            }
        }
    } else {
        progress.status(id_export, "exporting layers");
        let layer = match export::pack_rootfs(&final_state.rootfs) {
            Ok(bytes) => {
                let _ = cache.write_layer_blob(&final_state.layer_id, &bytes);
                bytes
            }
            Err(e) => {
                progress.error(id_export, e.to_string(), t0.elapsed());
                return Err(e);
            }
        };
        progress.status(id_export, format!("naming to {}", tags.join(", ")));
        match export::export_image_layer(
            store,
            &layer,
            &final_state.meta,
            tags,
            platform,
            "buildkit",
        ) {
            Ok(r) => Ok(r),
            Err(e) => {
                progress.error(id_export, e.to_string(), t0.elapsed());
                Err(e)
            }
        }
    }
}

fn stage_platform(stage: &Stage, default: &Platform) -> Result<Platform, Error> {
    match &stage.platform {
        None => Ok(default.clone()),
        Some(spec) => Platform::parse(spec).map_err(Error::other),
    }
}

fn instruction_summary(inst: &Instruction, state: &StageState) -> String {
    let vars = merged_vars(state);
    match inst {
        Instruction::Run(run) => format!("RUN {}", command_display(&run.command, &vars)),
        Instruction::Copy(copy) => {
            let dest = expand::expand(&copy.destination, &vars);
            let srcs: Vec<_> = copy
                .sources
                .iter()
                .map(|s| expand::expand(s, &vars))
                .collect();
            match copy
                .flags
                .iter()
                .find(|f| f.is("from"))
                .and_then(|f| f.value.as_deref())
            {
                Some(f) => format!("COPY --from={f} {} {dest}", srcs.join(" ")),
                None => format!("COPY {} {dest}", srcs.join(" ")),
            }
        }
        Instruction::Add(add) => {
            let dest = expand::expand(&add.destination, &vars);
            let srcs: Vec<_> = add
                .sources
                .iter()
                .map(|s| expand::expand(s, &vars))
                .collect();
            format!("ADD {} {dest}", srcs.join(" "))
        }
        Instruction::Workdir(wd) => format!("WORKDIR {}", expand::expand(&wd.path, &vars)),
        Instruction::Env(env) => {
            let body: Vec<_> = env
                .pairs
                .iter()
                .map(|p| format!("{}={}", p.key, expand::expand(&p.value, &vars)))
                .collect();
            format!("ENV {}", body.join(" "))
        }
        Instruction::Arg(arg) => {
            let body: Vec<_> = arg
                .args
                .iter()
                .map(|a| match &a.default {
                    Some(d) => format!("{}={}", a.name, expand::expand(d, &vars)),
                    None => a.name.clone(),
                })
                .collect();
            format!("ARG {}", body.join(" "))
        }
        Instruction::Label(label) => {
            let body: Vec<_> = label
                .pairs
                .iter()
                .map(|p| format!("{}={}", p.key, expand::expand(&p.value, &vars)))
                .collect();
            format!("LABEL {}", body.join(" "))
        }
        Instruction::Entrypoint(ep) => {
            format!("ENTRYPOINT {}", command_display(&ep.command, &vars))
        }
        Instruction::Cmd(cmd) => format!("CMD {}", command_display(&cmd.command, &vars)),
        Instruction::User(user) => format!("USER {}", expand::expand(&user.spec, &vars)),
        Instruction::Expose(ex) => {
            let ports: Vec<_> = ex
                .ports
                .iter()
                .map(|p| expand::expand(&p.to_string(), &vars))
                .collect();
            format!("EXPOSE {}", ports.join(" "))
        }
        Instruction::Volume(vol) => {
            let vols: Vec<_> = vol.paths.iter().map(|v| expand::expand(v, &vars)).collect();
            format!("VOLUME {}", vols.join(" "))
        }
        other => other.keyword().to_string(),
    }
}

fn command_display(command: &Command, vars: &HashMap<String, String>) -> String {
    match command {
        Command::Shell(s) => expand::expand(s, vars),
        Command::Exec(args) => format!("{:?}", expand::expand_vec(args, vars)),
    }
}

fn select_stage(stages: &[Stage], target: Option<&str>) -> Result<usize, Error> {
    match target {
        None => Ok(stages.len() - 1),
        Some(name) => {
            if let Ok(idx) = name.parse::<usize>() {
                if idx < stages.len() {
                    return Ok(idx);
                }
            }
            stages
                .iter()
                .position(|s| s.name.as_deref() == Some(name))
                .ok_or_else(|| Error::other(format!("unknown build target stage '{name}'")))
        }
    }
}

fn resolve_global_args(
    dockerfile: &Dockerfile,
    cli_args: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for decl in &dockerfile.args {
        if let Some(v) = cli_args.get(&decl.name) {
            out.insert(decl.name.clone(), v.clone());
        } else if let Some(default) = &decl.default {
            let expanded = expand::expand(default, &out);
            out.insert(decl.name.clone(), expanded);
        }
    }
    for (k, v) in cli_args {
        out.entry(k.clone()).or_insert_with(|| v.clone());
    }
    out
}

fn init_stage_from_image<S: ImageStore>(
    store: &S,
    platform: &Platform,
    base: &str,
    base_as_scratch: bool,
    stage_dir: &Path,
    rootfs: &Path,
    cli_args: &HashMap<String, String>,
) -> Result<StageState, Error> {
    let mut meta = ImageMeta::new();
    let args = cli_args.clone();

    if !base_as_scratch {
        let bundle = stage_dir.join("base-rootfs");
        if bundle.is_dir() {
            if rootfs.exists() {
                let _ = fs::remove_dir_all(rootfs);
            }
            fs::create_dir_all(rootfs)?;
            copy_tree(&bundle, rootfs)?;
        }
        if let Ok(reference) = parse_reference(base) {
            if let Ok(cfg) = store.image_config(&reference, platform) {
                if let Some(c) = cfg.config {
                    if let Some(env) = c.env {
                        meta.env = env;
                    }
                    if let Some(wd) = c.working_dir {
                        if !wd.is_empty() {
                            meta.working_dir = wd;
                        }
                    }
                    meta.user = c.user;
                    meta.entrypoint = c.entrypoint;
                    meta.cmd = c.cmd;
                    if let Some(labels) = c.labels {
                        meta.labels = labels;
                    }
                }
            }
        }
    }

    Ok(StageState {
        rootfs: rootfs.to_path_buf(),
        rootfs_shared: false,
        work_rootfs: rootfs.to_path_buf(),
        meta,
        args,
        layer_id: String::new(),
    })
}

fn instruction_needs_writable(inst: &Instruction) -> bool {
    matches!(
        inst,
        Instruction::Run(_) | Instruction::Copy(_) | Instruction::Add(_) | Instruction::Workdir(_)
    )
}

fn prepare_writable(state: &mut StageState) -> Result<(), Error> {
    if !state.rootfs_shared {
        return Ok(());
    }
    let dest = state.work_rootfs.clone();
    if dest.exists() {
        fs::remove_dir_all(&dest)?;
    }
    fs::create_dir_all(&dest)?;
    copy_tree(&state.rootfs, &dest)?;
    state.rootfs = dest;
    state.rootfs_shared = false;
    Ok(())
}

async fn apply_instruction<B: Backend, S: ImageStore>(
    kit: &Buildkit<B, S>,
    state: &mut StageState,
    inst: &Instruction,
    context: &BuildContext,
    completed: &HashMap<String, StageState>,
    global_args: &HashMap<String, String>,
    network: NetworkMode,
    progress: &mut ProgressEmitter<'_>,
    vertex_id: u32,
) -> Result<(), Error> {
    match inst {
        Instruction::Arg(arg) => {
            for decl in &arg.args {
                if !state.args.contains_key(&decl.name) {
                    if let Some(v) = &decl.default {
                        state
                            .args
                            .insert(decl.name.clone(), expand::expand(v, &state.args));
                    } else if let Some(v) = global_args.get(&decl.name) {
                        state.args.insert(decl.name.clone(), v.clone());
                    }
                }
            }
        }
        Instruction::Env(env) => {
            for pair in &env.pairs {
                let val = expand::expand(&pair.value, &merged_vars(state));
                state.args.insert(pair.key.clone(), val.clone());
                state.meta.set_env(&pair.key, &val);
            }
        }
        Instruction::Label(label) => {
            for pair in &label.pairs {
                let val = expand::expand(&pair.value, &merged_vars(state));
                state.meta.labels.insert(pair.key.clone(), val);
            }
        }
        Instruction::Workdir(wd) => {
            let path = expand::expand(&wd.path, &merged_vars(state));
            let abs = join_workdir(&state.meta.working_dir, &path);
            state.meta.working_dir = abs.clone();
            let host = guest_to_host(&state.rootfs, &abs);
            fs::create_dir_all(&host)?;
        }
        Instruction::User(user) => {
            state.meta.user = Some(expand::expand(&user.spec, &merged_vars(state)));
        }
        Instruction::Entrypoint(ep) => {
            state.meta.entrypoint = Some(command_to_args(
                &ep.command,
                &state.meta.shell,
                &merged_vars(state),
            ));
        }
        Instruction::Cmd(cmd) => {
            state.meta.cmd = Some(command_to_args(
                &cmd.command,
                &state.meta.shell,
                &merged_vars(state),
            ));
        }
        Instruction::Expose(ex) => {
            for p in &ex.ports {
                state
                    .meta
                    .exposed_ports
                    .push(expand::expand(&p.to_string(), &merged_vars(state)));
            }
        }
        Instruction::Volume(vol) => {
            for v in &vol.paths {
                state
                    .meta
                    .volumes
                    .push(expand::expand(v, &merged_vars(state)));
            }
        }
        Instruction::Shell(sh) => {
            state.meta.shell = sh.args.clone();
        }
        Instruction::Copy(copy) => {
            let dest = expand::expand(&copy.destination, &merged_vars(state));
            let dest_guest = join_workdir(&state.meta.working_dir, &dest);
            progress.status(vertex_id, format!("copying to {dest_guest}"));
            let from = copy
                .flags
                .iter()
                .find(|f| f.is("from"))
                .and_then(|f| f.value.as_deref());
            apply_copy(state, from, &copy.sources, &dest_guest, context, completed)?;
            write_heredocs(state, &copy.heredocs, &dest_guest)?;
        }
        Instruction::Add(add) => {
            let dest = expand::expand(&add.destination, &merged_vars(state));
            let dest_guest = join_workdir(&state.meta.working_dir, &dest);
            progress.status(vertex_id, format!("adding to {dest_guest}"));
            apply_add(state, &add.sources, &dest_guest, context, completed).await?;
            write_heredocs(state, &add.heredocs, &dest_guest)?;
        }
        Instruction::Run(run) => {
            let vars = merged_vars(state);
            let mut args = command_to_args(&run.command, &state.meta.shell, &vars);
            if !run.heredocs.is_empty() {
                let mut body = match &run.command {
                    Command::Shell(s) => expand::expand(s, &vars),
                    Command::Exec(_) => String::new(),
                };
                for h in &run.heredocs {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&h.body);
                }
                args = {
                    let mut shell = state.meta.shell.clone();
                    shell.push(body);
                    shell
                };
            }
            let run_network = run
                .flags
                .iter()
                .find(|f| f.is("network"))
                .and_then(|f| f.value.as_deref())
                .map(NetworkMode::parse)
                .transpose()?
                .unwrap_or(network);
            progress.status(vertex_id, "running");
            run_in_rootfs(kit, state, args, run_network).await?;
        }
        Instruction::From(_)
        | Instruction::Maintainer(_)
        | Instruction::OnBuild(_)
        | Instruction::StopSignal(_)
        | Instruction::Healthcheck(_)
        | Instruction::Unknown { .. } => {}
    }
    Ok(())
}

fn command_to_args(
    command: &Command,
    shell: &[String],
    vars: &HashMap<String, String>,
) -> Vec<String> {
    match command {
        Command::Shell(s) => {
            let mut args = shell.to_vec();
            args.push(expand::expand(s, vars));
            args
        }
        Command::Exec(args) => expand::expand_vec(args, vars),
    }
}

fn write_heredocs(
    state: &StageState,
    heredocs: &[dockerfile::Heredoc],
    dest_guest: &str,
) -> Result<(), Error> {
    if heredocs.is_empty() {
        return Ok(());
    }
    let dest_host = guest_to_host(&state.rootfs, dest_guest);
    if dest_guest.ends_with('/') || dest_host.is_dir() {
        fs::create_dir_all(&dest_host)?;
        for h in heredocs {
            fs::write(dest_host.join(&h.delimiter), &h.body)?;
        }
    } else {
        if let Some(parent) = dest_host.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        for h in heredocs {
            body.push_str(&h.body);
        }
        fs::write(&dest_host, body)?;
    }
    Ok(())
}

fn apply_copy(
    state: &mut StageState,
    from: Option<&str>,
    sources: &[String],
    dest_guest: &str,
    context: &BuildContext,
    completed: &HashMap<String, StageState>,
) -> Result<(), Error> {
    let dest_host = guest_to_host(&state.rootfs, dest_guest);
    let dest_is_dir = dest_guest.ends_with('/');

    for src in sources {
        let src = expand::expand(src, &merged_vars(state));
        if let Some(stage_name) = from {
            let donor = completed
                .get(stage_name)
                .ok_or_else(|| Error::other(format!("unknown COPY --from stage '{stage_name}'")))?;
            let src_host = if src.starts_with('/') {
                guest_to_host(&donor.rootfs, &src)
            } else {
                guest_to_host(&donor.rootfs, &join_workdir(&donor.meta.working_dir, &src))
            };
            let target = copy_dest_path(&dest_host, dest_is_dir, sources.len() > 1, &src_host)?;
            copy_into(context, &src_host, &target)?;
        } else {
            let src_host = context.resolve(&src)?;
            let target = copy_dest_path(&dest_host, dest_is_dir, sources.len() > 1, &src_host)?;
            copy_into(context, &src_host, &target)?;
        }
    }
    Ok(())
}

async fn apply_add(
    state: &mut StageState,
    sources: &[String],
    dest_guest: &str,
    context: &BuildContext,
    completed: &HashMap<String, StageState>,
) -> Result<(), Error> {
    let dest_host = guest_to_host(&state.rootfs, dest_guest);
    let dest_is_dir = dest_guest.ends_with('/');
    let multi = sources.len() > 1;
    let mut local = Vec::new();

    for src in sources {
        let src = expand::expand(src, &merged_vars(state));
        if is_remote_url(&src) {
            if multi && !dest_is_dir && !dest_host.is_dir() {
                return Err(Error::other(
                    "when ADD has multiple sources, destination must be a directory",
                ));
            }
            let target =
                url_dest_path(&dest_host, dest_is_dir || multi || dest_host.is_dir(), &src)?;
            download_url(&src, &target).await?;
        } else {
            local.push(src);
        }
    }

    if !local.is_empty() {
        apply_copy(state, None, &local, dest_guest, context, completed)?;
    }
    Ok(())
}

fn is_remote_url(src: &str) -> bool {
    let lower = src.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn url_filename(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let name = without_query.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() {
        "download".into()
    } else {
        name.to_string()
    }
}

fn url_dest_path(dest_host: &Path, as_dir: bool, url: &str) -> Result<PathBuf, Error> {
    if as_dir {
        fs::create_dir_all(dest_host)?;
        Ok(dest_host.join(url_filename(url)))
    } else {
        if let Some(parent) = dest_host.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(dest_host.to_path_buf())
    }
}

async fn download_url(url: &str, dest: &Path) -> Result<(), Error> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("buildkit/0.1")
        .build()
        .map_err(|e| Error::other(format!("ADD: http client error: {e}")))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::other(format!("ADD: failed to fetch '{url}': {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::other(format!(
            "ADD: failed to fetch '{url}': HTTP {status}"
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::other(format!("ADD: failed to read '{url}': {e}")))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, &bytes)
        .map_err(|e| Error::other(format!("ADD: failed to write '{}': {e}", dest.display())))?;
    Ok(())
}

fn copy_dest_path(
    dest_host: &Path,
    dest_is_dir: bool,
    multi_src: bool,
    src_host: &Path,
) -> Result<PathBuf, Error> {
    if dest_is_dir || multi_src || dest_host.is_dir() {
        fs::create_dir_all(dest_host)?;
        let name = src_host
            .file_name()
            .ok_or_else(|| Error::other("invalid COPY source"))?;
        Ok(dest_host.join(name))
    } else {
        if let Some(parent) = dest_host.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(dest_host.to_path_buf())
    }
}

async fn run_in_rootfs<B: Backend, S: ImageStore>(
    kit: &Buildkit<B, S>,
    state: &StageState,
    args: Vec<String>,
    network: NetworkMode,
) -> Result<(), Error> {
    let mut env = state.meta.env.clone();
    for (k, v) in &state.args {
        let prefix = format!("{k}=");
        if !env.iter().any(|e| e.starts_with(&prefix)) {
            env.push(format!("{k}={v}"));
        }
    }

    let request = RunRequest {
        rootfs: state.rootfs.clone(),
        args,
        env,
        cwd: state.meta.working_dir.clone(),
        user: state.meta.user.clone(),
        network,
    };
    let result = kit.backend().run(&request).await.map_err(Error::backend)?;
    if !result.is_success() {
        return Err(Error::other(format!(
            "RUN failed with exit code {}",
            result.status
        )));
    }
    Ok(())
}

fn merged_vars(state: &StageState) -> HashMap<String, String> {
    let mut vars = state.args.clone();
    for entry in &state.meta.env {
        if let Some((k, v)) = entry.split_once('=') {
            vars.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
    }
    vars
}
