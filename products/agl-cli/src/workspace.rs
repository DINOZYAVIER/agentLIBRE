use super::*;

pub(super) struct ApplicationRoots {
    pub(super) config: PathBuf,
    pub(super) data: PathBuf,
    pub(super) state: PathBuf,
}

pub(super) fn application_roots() -> Result<ApplicationRoots> {
    if let Some(root) = nonempty_env_path("AGL_HOME") {
        let root = absolute_root(root, "AGL_HOME")?;
        return Ok(ApplicationRoots {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        });
    }
    let home = nonempty_env_path("HOME");
    let config = nonempty_env_path("XDG_CONFIG_HOME")
        .or_else(|| home.as_ref().map(|path| path.join(".config")))
        .context("HOME or XDG_CONFIG_HOME is required")?;
    let data = nonempty_env_path("XDG_DATA_HOME")
        .or_else(|| home.as_ref().map(|path| path.join(".local/share")))
        .context("HOME or XDG_DATA_HOME is required")?;
    let state = nonempty_env_path("XDG_STATE_HOME")
        .or_else(|| home.map(|path| path.join(".local/state")))
        .context("HOME or XDG_STATE_HOME is required")?;
    Ok(ApplicationRoots {
        config: absolute_root(config, "configuration root")?.join("agentLIBRE"),
        data: absolute_root(data, "data root")?.join("agentLIBRE"),
        state: absolute_root(state, "state root")?.join("agentLIBRE"),
    })
}

pub(super) fn resolve_function_locator(
    locator: &config::FunctionLocator,
    active: &config::ActiveConfig,
) -> Result<PathBuf> {
    match locator {
        config::FunctionLocator::Directory(path) => active
            .functions
            .iter()
            .find(|function| function.directory == *path)
            .map(|_| path.clone())
            .context("explicit Function has not been activated; run `agl config apply --function <FUNCTION>`"),
        config::FunctionLocator::Requirement(requirement) => {
            let mut matches = active
                .functions
                .iter()
                .filter_map(|function| {
                    let id = agl_runtime::package::PackageId::new(function.id.clone()).ok()?;
                    if id != requirement.id {
                        return None;
                    }
                    let version = semver::Version::parse(&function.version).ok()?;
                    requirement.version.matches(&version).then_some((version, function))
                })
                .collect::<Vec<_>>();
            matches.sort_by(|left, right| right.0.cmp(&left.0));
            let best = matches.first().context(
                "typed Function is not active; run `agl config apply --function <FUNCTION>`",
            )?;
            ensure!(
                matches.iter().filter(|item| item.0 == best.0).count() == 1,
                "typed Function resolves ambiguously in active configuration"
            );
            Ok(best.1.directory.clone())
        }
    }
}

pub(super) fn print_config_plan(plan: &config::ConfigPlan) {
    println!("source_digest={}", plan.source_digest);
    println!("executable={}", plan.executable.display());
    println!(
        "keep_free_ram={}",
        plan.keep_free_ram
            .map_or_else(|| "automatic".into(), |value| value.to_string())
    );
    println!(
        "keep_free_vram={}",
        plan.keep_free_vram
            .map_or_else(|| "automatic".into(), |value| value.to_string())
    );
    for function in &plan.functions {
        println!(
            "function={}@{}\t{}\tlock_entities={}",
            function.id,
            function.version,
            function.directory.display(),
            function.digest
        );
    }
}

pub(super) fn apply_config(
    roots: &ApplicationRoots,
    socket: &Path,
    plan: &config::ConfigPlan,
) -> Result<()> {
    let unfinished = agl_daemon::unfinished_agent_runs(&roots.data)?;
    ensure!(
        unfinished.is_empty(),
        "configuration is busy with unfinished Runs: {}",
        unfinished
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let active_path = config::active_path(&roots.data);
    let old_active = std::fs::read(&active_path).ok();
    let result = (|| {
        config::write_active(&roots.data, &config::active(plan))?;
        controlled_reload(socket)?;
        println!("applied {}", active_path.display());
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(error) = result {
        match old_active {
            Some(bytes) => std::fs::write(active_path, bytes)?,
            None if active_path.try_exists()? => std::fs::remove_file(active_path)?,
            None => {}
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn controlled_reload(socket: &Path) -> Result<()> {
    if !socket.try_exists()? {
        return Ok(());
    }
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "reload-or-restart", "agentlibre-daemon.service"])
        .status()
        .context("daemon is active but systemctl --user is unavailable")?;
    ensure!(
        status.success(),
        "controlled daemon reload failed; generated state remains on disk"
    );
    Ok(())
}

pub(super) fn print_doctor(roots: &ApplicationRoots) -> Result<()> {
    let active = config::read_active(&roots.data)
        .context("no generated configuration; run `agl config apply` first")?;
    println!("active_source_digest={}", active.source_digest);
    println!("active_executable={}", active.executable.display());
    println!("active_functions={}", active.functions.len());
    for function in &active.functions {
        println!(
            "function={}@{}\t{}\tdigest={}",
            function.id,
            function.version,
            function.directory.display(),
            function.digest
        );
    }
    let unfinished = agl_daemon::unfinished_agent_runs(&roots.data)?;
    println!("unfinished_runs={}", unfinished.len());
    for run in unfinished {
        println!("run={run}");
    }
    Ok(())
}

pub(super) fn read_regular_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} must be a regular non-symlink file",
        path.display()
    );
    ensure!(metadata.len() <= maximum, "{} is oversized", path.display());
    std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(super) fn absolute_root(path: PathBuf, name: &str) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "{name} must be absolute");
    Ok(path)
}

pub(super) fn utf8_path(path: &Path, name: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{name} is not valid UTF-8"))
}
