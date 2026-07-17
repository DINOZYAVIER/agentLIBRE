#![cfg(all(target_os = "linux", feature = "native-test-fixtures"))]

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agl_process::process_platform_diagnostics;

const LAUNCHER: &str = env!("CARGO_BIN_EXE_agl-process-launcher");
const HELPER: &str = env!("CARGO_BIN_EXE_agl-process-test-helper");
const OWNER: &str = env!("CARGO_BIN_EXE_agl-process-owner-fixture");

#[test]
#[ignore = "requires the designated Linux namespace/pidfd owner-death runner"]
fn native_owner_death_and_descendant_cleanup_smoke() {
    let diagnostics = process_platform_diagnostics(LAUNCHER);
    eprintln!(
        "process_platform_diagnostics={}",
        serde_json::to_string(&diagnostics).unwrap()
    );
    assert!(
        diagnostics.supported,
        "designated owner-death smoke cannot skip unsupported isolation: {:?}",
        diagnostics.error_code
    );

    for iteration in 0..3 {
        verify_ready_tree_cleanup(iteration, libc::SIGKILL);
    }
    verify_ready_tree_cleanup(3, libc::SIGTERM);
    for iteration in 0..16 {
        verify_parent_death_setup_race(iteration);
    }
}

fn verify_ready_tree_cleanup(iteration: usize, signal: libc::c_int) {
    let fixture = OwnerFixture::spawn(&format!("tree-{iteration}-{signal}"));
    fixture.wait_ready();
    let namespace = fixture.pid_namespace();
    let namespace_pids = wait_for_namespace_members(&namespace, 4);
    let launcher_pids = direct_children(fixture.pid());
    assert_eq!(
        launcher_pids.len(),
        1,
        "owner must have exactly one private launcher child"
    );
    let mut observed = namespace_pids.clone();
    observed.extend(launcher_pids.iter().copied());
    observed.insert(fixture.pid());
    let pidfds = observed
        .iter()
        .map(|pid| (*pid, pidfd_open(*pid)))
        .collect::<Vec<_>>();
    eprintln!(
        "owner_death_iteration={iteration} signal={signal} owner={} launcher={launcher_pids:?} namespace={namespace} namespace_pids={namespace_pids:?}",
        fixture.pid()
    );

    let status = fixture.signal_and_wait(signal);
    if signal == libc::SIGTERM {
        assert!(status.success(), "graceful owner fixture failed: {status}");
        assert!(fixture.root.join("graceful-complete").is_file());
    } else {
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
    for (pid, pidfd) in pidfds {
        wait_pidfd(&pidfd, Duration::from_secs(5));
        wait_process_absent(pid);
    }
    wait_for_namespace_empty(&namespace);
}

fn verify_parent_death_setup_race(iteration: usize) {
    let fixture = OwnerFixture::spawn(&format!("race-{iteration}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let launcher = loop {
        if let Some(pid) = direct_children(fixture.pid()).into_iter().next() {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "owner fixture did not reach launcher setup race"
        );
        std::thread::yield_now();
    };
    let observed = process_tree(fixture.pid());
    let launcher_pidfd = pidfd_open(launcher);
    let observed_pidfds = observed
        .iter()
        .filter(|pid| **pid != fixture.pid() && **pid != launcher)
        .map(|pid| (*pid, pidfd_open(*pid)))
        .collect::<Vec<_>>();
    let status = fixture.signal_and_wait(libc::SIGKILL);
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    wait_pidfd(&launcher_pidfd, Duration::from_secs(5));
    wait_process_absent(launcher);
    for (pid, pidfd) in observed_pidfds {
        wait_pidfd(&pidfd, Duration::from_secs(5));
        wait_process_absent(pid);
    }
    eprintln!(
        "parent_death_setup_race_iteration={iteration} launcher={launcher} observed={observed:?} cleaned=true"
    );
}

struct OwnerFixture {
    root: PathBuf,
    ready: PathBuf,
    evidence: PathBuf,
    child: std::cell::RefCell<Option<Child>>,
}

impl OwnerFixture {
    fn spawn(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agl-process-owner-death-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("workspace")).unwrap();
        let ready = root.join("ready");
        let evidence = root.join("workspace").join("tree-evidence");
        let child = Command::new(OWNER)
            .args([
                LAUNCHER,
                HELPER,
                root.to_str().unwrap(),
                ready.to_str().unwrap(),
                evidence.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            root,
            ready,
            evidence,
            child: std::cell::RefCell::new(Some(child)),
        }
    }

    fn pid(&self) -> u32 {
        self.child.borrow().as_ref().unwrap().id()
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.ready.is_file() {
            if let Some(status) = self
                .child
                .borrow_mut()
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
            {
                panic!("owner fixture exited before ready: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "owner fixture did not become ready"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn pid_namespace(&self) -> String {
        let contents = fs::read_to_string(&self.evidence).unwrap();
        contents
            .lines()
            .find_map(|line| line.split_once("pid_namespace=").map(|(_, value)| value))
            .unwrap_or_else(|| panic!("missing PID namespace evidence: {contents}"))
            .to_owned()
    }

    fn signal_and_wait(&self, signal: libc::c_int) -> std::process::ExitStatus {
        let mut child = self.child.borrow_mut().take().unwrap();
        let owner_pidfd = pidfd_open(child.id());
        pidfd_signal(&owner_pidfd, signal);
        child.wait().unwrap()
    }
}

impl Drop for OwnerFixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn direct_children(pid: u32) -> BTreeSet<u32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect()
}

fn process_tree(root: u32) -> BTreeSet<u32> {
    let mut found = BTreeSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        for child in direct_children(pid) {
            if found.insert(child) {
                queue.push_back(child);
            }
        }
    }
    found
}

fn namespace_members(namespace: &str) -> BTreeSet<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| {
            fs::read_link(format!("/proc/{pid}/ns/pid"))
                .is_ok_and(|value| value.to_string_lossy() == namespace)
        })
        .collect()
}

fn wait_for_namespace_members(namespace: &str, minimum: usize) -> BTreeSet<u32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let members = namespace_members(namespace);
        if members.len() >= minimum {
            return members;
        }
        assert!(
            Instant::now() < deadline,
            "PID namespace {namespace} exposed only {members:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_namespace_empty(namespace: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let members = namespace_members(namespace);
        if members.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "PID namespace {namespace} still contains {members:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn pidfd_open(pid: u32) -> OwnedFd {
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    assert!(
        descriptor >= 0,
        "pidfd_open({pid}) failed: {}",
        std::io::Error::last_os_error()
    );
    unsafe { OwnedFd::from_raw_fd(descriptor) }
}

fn pidfd_signal(pidfd: &OwnedFd, signal: libc::c_int) {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    assert_eq!(
        result,
        0,
        "pidfd_send_signal failed: {}",
        std::io::Error::last_os_error()
    );
}

fn wait_pidfd(pidfd: &OwnedFd, timeout: Duration) {
    let mut poll = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
    assert!(
        ready > 0 && poll.revents & libc::POLLIN != 0,
        "pidfd did not report process exit: ready={ready} revents={}",
        poll.revents
    );
}

fn wait_process_absent(pid: u32) {
    let path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "observed owner/launcher/namespace process {pid} still exists"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
