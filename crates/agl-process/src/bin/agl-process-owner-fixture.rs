#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use agl_ids::{RunId, StepId};
    use agl_process::{
        EnvironmentOverride, ExecutionAuthorization, ExecutionIo, ExecutionKind, ExecutionLimits,
        ExecutionOwner, ExecutionProfile, ExecutionRequest, FileOutputSpool,
        InMemoryExecutionRepository, ProcessSupervisor, ProcessSupervisorOptions,
    };

    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    pub(super) fn main() {
        if let Err(error) = run() {
            eprintln!("agl-process-owner-fixture: {error}");
            std::process::exit(2);
        }
    }

    fn run() -> Result<(), Box<dyn std::error::Error>> {
        arm_parent_death()?;
        install_shutdown_handlers();
        let arguments = std::env::args().skip(1).collect::<Vec<_>>();
        if arguments.len() != 5 {
            return Err(
                "usage: agl-process-owner-fixture LAUNCHER HELPER ROOT READY EVIDENCE".into(),
            );
        }
        let launcher = canonical(&arguments[0])?;
        let helper = canonical(&arguments[1])?;
        let root = PathBuf::from(&arguments[2]);
        let ready = PathBuf::from(&arguments[3]);
        let evidence = PathBuf::from(&arguments[4]);
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace)?;
        let workspace = workspace.canonicalize()?;
        let options = ProcessSupervisorOptions {
            launcher_path: launcher,
            data_root: root.join("data"),
            state_root: root.join("state"),
            max_active: 2,
            command_capacity: 32,
            poll_interval: Duration::from_millis(2),
            setup_timeout: Duration::from_secs(5),
            termination_grace: Duration::from_millis(50),
            max_input_bytes: 65_536,
            max_result_bytes: 65_536,
            max_spool_bytes: 1024 * 1024,
            termination_output_headroom_bytes: 4_096,
            finished_retention: Duration::from_secs(60),
            runtime_read_only_roots: vec![helper.parent().unwrap().to_path_buf()],
        };
        let repository = Arc::new(InMemoryExecutionRepository::new());
        let spool = Arc::new(FileOutputSpool::new(root.join("spool"))?);
        let supervisor = ProcessSupervisor::start(options, repository, spool)?;
        let handle = supervisor.handle();
        let run_id = RunId::generate();
        let owner = ExecutionOwner::Run {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
        };
        let request = ExecutionRequest {
            owner: owner.clone(),
            creating_run_id: run_id,
            creating_step_id: StepId::generate(),
            kind: ExecutionKind::Argv,
            program: helper.clone(),
            program_digest: None,
            args: vec!["fork-tree".to_owned(), evidence.display().to_string()],
            workspace_root: workspace.clone(),
            cwd: workspace,
            read_only_roots: vec![helper.parent().unwrap().to_path_buf()],
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pipes,
            terminal_size: None,
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            limits: ExecutionLimits {
                timeout_ms: None,
                max_input_bytes: 65_536,
                max_output_bytes: 1024 * 1024,
            },
        };
        let started = handle.start(request)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fs::read_to_string(&evidence)
                .is_ok_and(|contents| contents.lines().any(|line| line == "READY"))
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err("fork-tree evidence did not become ready".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        fs::write(&ready, format!("execution_id={}\n", started.execution_id))?;
        while !SHUTDOWN.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        supervisor.shutdown()?;
        fs::write(root.join("graceful-complete"), b"ok\n")?;
        Ok(())
    }

    fn canonical(value: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        Ok(Path::new(value).canonicalize()?)
    }

    fn arm_parent_death() -> Result<(), Box<dyn std::error::Error>> {
        let parent = unsafe { libc::getppid() };
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::getppid() } != parent {
            return Err("owner fixture parent changed during setup".into());
        }
        Ok(())
    }

    extern "C" fn shutdown(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::Release);
    }

    fn install_shutdown_handlers() {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            unsafe {
                libc::signal(signal, shutdown as *const () as libc::sighandler_t);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("platform_unsupported: process owner fixture is Linux-only");
    std::process::exit(2);
}
