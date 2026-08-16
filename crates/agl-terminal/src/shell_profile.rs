mod bash;
mod zsh;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agl_exec::{ExecutionProfile, ProcessError, ProcessErrorCode, Result, ShellProfileSnapshot};
use serde::{Deserialize, Serialize};

use crate::ShellIntegrationToken;
use crate::history::TerminalHistorySeed;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmittedShellKind {
    Bash,
    Zsh,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ShellAdapterCapability {
    AuthenticatedEvents,
    EnvironmentSynchronization,
    ForegroundJobs,
    HistoryIsolation,
    PromptGeneration,
    TypedCommands,
    AuthorizedHostUserRc,
    LoginStartup,
    ArbitraryExecutable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellAdapterDescriptor {
    pub kind: AdmittedShellKind,
    pub executable_basename: &'static str,
    pub version_probe_args: &'static [&'static str],
    pub supported: &'static [ShellAdapterCapability],
    pub unsupported: &'static [ShellAdapterCapability],
}

const MANAGED_CAPABILITIES: &[ShellAdapterCapability] = &[
    ShellAdapterCapability::AuthenticatedEvents,
    ShellAdapterCapability::EnvironmentSynchronization,
    ShellAdapterCapability::ForegroundJobs,
    ShellAdapterCapability::HistoryIsolation,
    ShellAdapterCapability::PromptGeneration,
    ShellAdapterCapability::TypedCommands,
    ShellAdapterCapability::AuthorizedHostUserRc,
];
const UNSUPPORTED_CAPABILITIES: &[ShellAdapterCapability] = &[
    ShellAdapterCapability::LoginStartup,
    ShellAdapterCapability::ArbitraryExecutable,
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedShellProfile {
    pub kind: AdmittedShellKind,
    pub snapshot: ShellProfileSnapshot,
}

impl AdmittedShellProfile {
    pub fn adapter(&self) -> ShellAdapterDescriptor {
        match self.kind {
            AdmittedShellKind::Bash => ShellAdapterDescriptor {
                kind: self.kind,
                executable_basename: "bash",
                version_probe_args: &["--version"],
                supported: MANAGED_CAPABILITIES,
                unsupported: UNSUPPORTED_CAPABILITIES,
            },
            AdmittedShellKind::Zsh => ShellAdapterDescriptor {
                kind: self.kind,
                executable_basename: "zsh",
                version_probe_args: &["--version"],
                supported: MANAGED_CAPABILITIES,
                unsupported: UNSUPPORTED_CAPABILITIES,
            },
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.snapshot.validate()?;
        let executable = self
            .snapshot
            .program
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| unsupported_shell("admitted shell executable has no UTF-8 basename"))?;
        let expected = self.adapter().executable_basename;
        if executable != expected {
            return Err(unsupported_shell(format!(
                "admitted shell kind `{expected}` does not match executable `{executable}`"
            )));
        }
        Ok(())
    }

    pub fn render_startup(
        &self,
        host_startup: &HostStartupPolicy,
        history_seed: &TerminalHistorySeed,
        paths: &ShellStartupPaths,
        token: &ShellIntegrationToken,
        profile: ExecutionProfile,
    ) -> Result<ManagedShellLaunchPlan> {
        self.validate()?;
        host_startup.validate(profile)?;
        let (startup_name, startup, history, args, environment) = match self.kind {
            AdmittedShellKind::Bash => {
                let startup_name = "bashrc";
                let startup_path = paths.startup_directory.join(startup_name);
                (
                    startup_name,
                    bash::render_startup(
                        host_startup,
                        &paths.history_seed,
                        &startup_path,
                        &paths.event_fifo,
                        &paths.control_fifo,
                        token,
                    ),
                    bash::render_history(history_seed),
                    vec![
                        "--noprofile".to_owned(),
                        "--rcfile".to_owned(),
                        startup_path.to_string_lossy().into_owned(),
                        "-i".to_owned(),
                    ],
                    BTreeMap::new(),
                )
            }
            AdmittedShellKind::Zsh => {
                let startup_name = ".zshrc";
                let startup_path = paths.startup_directory.join(startup_name);
                let mut environment = BTreeMap::new();
                environment.insert(
                    "ZDOTDIR".to_owned(),
                    paths.startup_directory.to_string_lossy().into_owned(),
                );
                (
                    startup_name,
                    zsh::render_startup(
                        host_startup,
                        &paths.history_seed,
                        &startup_path,
                        &paths.event_fifo,
                        &paths.control_fifo,
                        token,
                    ),
                    zsh::render_history(history_seed),
                    vec!["-d".to_owned(), "-i".to_owned()],
                    environment,
                )
            }
        };
        Ok(ManagedShellLaunchPlan {
            startup_name,
            startup,
            history,
            args,
            environment,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HostStartupPolicy {
    ManagedOnly,
    SourceUserRc { path: PathBuf },
}

impl HostStartupPolicy {
    pub fn validate(&self, profile: ExecutionProfile) -> Result<()> {
        match self {
            Self::ManagedOnly => Ok(()),
            Self::SourceUserRc { .. } if profile != ExecutionProfile::Host => {
                Err(ProcessError::new(
                    ProcessErrorCode::LoginAuthorityRequired,
                    "user shell rc may be sourced only by an authorized Host terminal",
                ))
            }
            Self::SourceUserRc { path } => {
                let canonical = path.canonicalize().map_err(|error| {
                    ProcessError::new(
                        ProcessErrorCode::InvalidRequest,
                        format!("approved user shell rc cannot be canonicalized: {error}"),
                    )
                })?;
                if &canonical != path || !canonical.is_file() {
                    return Err(ProcessError::new(
                        ProcessErrorCode::InvalidRequest,
                        "approved user shell rc must be an existing canonical regular file",
                    ));
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellStartupPaths {
    pub startup_directory: PathBuf,
    pub history_seed: PathBuf,
    pub event_fifo: PathBuf,
    pub control_fifo: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedShellLaunchPlan {
    pub startup_name: &'static str,
    pub startup: String,
    pub history: Vec<u8>,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

fn shell_quote(path: &Path) -> String {
    let value = path.as_os_str().to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn unsupported_shell(message: impl Into<String>) -> ProcessError {
    ProcessError::new(ProcessErrorCode::InvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(kind: AdmittedShellKind) -> AdmittedShellProfile {
        let executable = match kind {
            AdmittedShellKind::Bash => "/bin/bash",
            AdmittedShellKind::Zsh => "/bin/zsh",
        };
        AdmittedShellProfile {
            kind,
            snapshot: ShellProfileSnapshot {
                program: PathBuf::from(executable),
                command_args: vec!["-c".to_owned()],
                login_command_args: None,
                environment_names: vec!["PATH".to_owned()],
                executable_digest:
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
                config_digest: "sha256:test-shell".to_owned(),
            },
        }
    }

    fn paths() -> ShellStartupPaths {
        ShellStartupPaths {
            startup_directory: PathBuf::from("/private"),
            history_seed: PathBuf::from("/private/history.seed"),
            event_fifo: PathBuf::from("/private/integration.events.fifo"),
            control_fifo: PathBuf::from("/private/integration.controls.fifo"),
        }
    }

    #[test]
    fn admitted_kind_must_match_exact_executable_basename() {
        let mut shell = snapshot(AdmittedShellKind::Bash);
        shell.snapshot.program = PathBuf::from("/bin/zsh");
        assert_eq!(
            shell.validate().unwrap_err().code(),
            ProcessErrorCode::InvalidRequest
        );
    }

    #[test]
    fn adapters_restore_private_history_after_approved_user_rc() {
        let root = std::env::temp_dir().join(format!(
            "agl-shell-profile-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir(&root).unwrap();
        let rc = root.join("user's bashrc");
        std::fs::write(&rc, b"# admitted test rc\n").unwrap();
        let rc = rc.canonicalize().unwrap();
        let policy = HostStartupPolicy::SourceUserRc { path: rc.clone() };
        let token = ShellIntegrationToken::generate().unwrap();
        let seed = TerminalHistorySeed::empty();
        let bash = snapshot(AdmittedShellKind::Bash)
            .render_startup(&policy, &seed, &paths(), &token, ExecutionProfile::Host)
            .unwrap();
        let zsh = snapshot(AdmittedShellKind::Zsh)
            .render_startup(&policy, &seed, &paths(), &token, ExecutionProfile::Host)
            .unwrap();
        let quoted = shell_quote(&rc);
        assert!(bash.startup.contains(&format!("source {quoted}")));
        assert!(zsh.startup.contains(&format!("source {quoted}")));
        assert_eq!(bash.startup.matches("HISTFILE=/dev/null").count(), 2);
        assert_eq!(zsh.startup.matches("HISTFILE=/dev/null").count(), 2);
        assert_eq!(bash.startup.matches("set -m").count(), 2);
        assert_eq!(zsh.startup.matches("MONITOR").count(), 2);
        assert!(
            bash.startup
                .contains("bind 'set enable-bracketed-paste on'")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adapters_return_exact_non_login_launch_arguments() {
        let token = ShellIntegrationToken::generate().unwrap();
        let seed = TerminalHistorySeed::empty();
        let bash = snapshot(AdmittedShellKind::Bash)
            .render_startup(
                &HostStartupPolicy::ManagedOnly,
                &seed,
                &paths(),
                &token,
                ExecutionProfile::Workspace,
            )
            .unwrap();
        let zsh = snapshot(AdmittedShellKind::Zsh)
            .render_startup(
                &HostStartupPolicy::ManagedOnly,
                &seed,
                &paths(),
                &token,
                ExecutionProfile::Workspace,
            )
            .unwrap();
        assert_eq!(bash.startup_name, "bashrc");
        assert_eq!(
            bash.args,
            ["--noprofile", "--rcfile", "/private/bashrc", "-i"]
        );
        assert!(bash.environment.is_empty());
        assert_eq!(zsh.startup_name, ".zshrc");
        assert_eq!(zsh.args, ["-d", "-i"]);
        assert_eq!(zsh.environment["ZDOTDIR"], "/private");
        assert_eq!(
            snapshot(AdmittedShellKind::Bash).adapter().supported,
            MANAGED_CAPABILITIES
        );
        assert_eq!(
            snapshot(AdmittedShellKind::Zsh).adapter().unsupported,
            UNSUPPORTED_CAPABILITIES
        );
    }

    #[test]
    fn zsh_history_preserves_multiline_commands_as_one_extended_entry() {
        let seed =
            TerminalHistorySeed::from_commands(vec!["echo one\necho two".to_owned()]).unwrap();
        let token = ShellIntegrationToken::generate().unwrap();
        let plan = snapshot(AdmittedShellKind::Zsh)
            .render_startup(
                &HostStartupPolicy::ManagedOnly,
                &seed,
                &paths(),
                &token,
                ExecutionProfile::Workspace,
            )
            .unwrap();
        assert_eq!(
            String::from_utf8(plan.history).unwrap(),
            ": 0:0;echo one\\\necho two\n"
        );
    }
}
