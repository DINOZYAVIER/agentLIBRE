//! `agl function list|use` (AGL-036, D3, D7, D8): the Functions a run can
//! load, each with its server kind and model, and the choice of the one
//! the runs load.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::error::Error;
use crate::forge::{self, Workspace};
use crate::function::{self, Function};
use crate::i18n::{Lang, m};
use crate::paths::Paths;
use crate::settings::{self, function_name, ServerFile, ServerKind, UserFile};
use crate::tr;
use crate::outln;

use super::args::FunctionAction;

/// Where a Function comes from, in the order a run looks (AGL-028, D3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Workspace,
    User,
    BuiltIn,
}

impl Source {
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Source::Workspace => tr!(lang, m::FUNCTION_SOURCE_WORKSPACE),
            Source::User => tr!(lang, m::FUNCTION_SOURCE_USER),
            Source::BuiltIn => tr!(lang, m::FUNCTION_SOURCE_BUILTIN),
        }
    }
}

/// One Function of the list (AGL-036, D3).
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// The source a run loads it from.
    pub source: Source,
    /// The sources of the same name it hides.
    pub shadows: Vec<Source>,
    /// The kind and the model of its server (AGL-039, D21: always the
    /// Function's).
    pub kind: ServerKind,
    pub model: Option<String>,
    /// Why it does not load, instead of the kind and the model.
    pub error: Option<String>,
    /// The Function a run here loads (`--function` aside).
    pub current: bool,
}

/// The Functions of a workspace (AGL-036, D3): the workspace's, the
/// user's and the built-ins, against the settings of the workspace.
pub struct Catalog {
    pub workspace_dir: PathBuf,
    pub user_dir: PathBuf,
    /// The project's `server` over the user's: the urls and the engine.
    pub settings: ServerFile,
    /// The name of the Function a run here loads.
    pub current: String,
}

impl Catalog {
    /// The catalog of `project`, with the user file `user`.
    pub fn of(paths: &Paths, project: &Workspace, user: &UserFile) -> Catalog {
        Catalog {
            workspace_dir: paths.functions_dir(&project.key),
            user_dir: paths.functions(),
            settings: project.server.over(&user.server),
            current: function_name(None, project.function.as_deref(), user.function.as_deref()),
        }
    }

    /// Every Function by name, with its source and its server.
    pub fn entries(&self, lang: Lang) -> Vec<Entry> {
        let workspace = names_in(&self.workspace_dir);
        let user = names_in(&self.user_dir);
        // Without agl's `nix/` there is no built-in; each Function then
        // says why it does not load.
        let builtins = function::builtin_names().unwrap_or_default();
        let mut names: Vec<String> = workspace
            .iter()
            .chain(&user)
            .chain(&builtins)
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let mut sources = Vec::new();
                if workspace.contains(&name) {
                    sources.push(Source::Workspace);
                }
                if user.contains(&name) {
                    sources.push(Source::User);
                }
                if builtins.contains(&name) {
                    sources.push(Source::BuiltIn);
                }
                let (kind, model, error) = match self.server_of(&name) {
                    Ok((kind, model)) => (kind, model, None),
                    Err(error) => (ServerKind::default(), None, Some(error.message(lang))),
                };
                Entry {
                    current: name == self.current,
                    name,
                    source: sources[0],
                    shadows: sources[1..].to_vec(),
                    kind,
                    model,
                    error,
                }
            })
            .collect()
    }

    /// The list as lines (AGL-036, D3): the current Function marked, the
    /// columns aligned.
    pub fn lines(&self, lang: Lang) -> Vec<String> {
        let entries = self.entries(lang);
        let name_width = entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
        let source_width = entries
            .iter()
            .map(|e| e.source.label(lang).chars().count())
            .max()
            .unwrap_or(0);
        entries
            .iter()
            .map(|entry| {
                let mark = if entry.current { '*' } else { ' ' };
                let mut line = format!(
                    "{mark} {:name_width$}  {:source_width$}  ",
                    entry.name,
                    entry.source.label(lang),
                );
                match &entry.error {
                    Some(error) => line.push_str(error),
                    None => {
                        line.push_str(&format!("{:6}  ", entry.kind.name()));
                        line.push_str(
                            entry
                                .model
                                .as_deref()
                                .unwrap_or(tr!(lang, m::FUNCTION_NO_MODEL)),
                        );
                    }
                }
                if !entry.shadows.is_empty() {
                    let hidden: Vec<&str> = entry.shadows.iter().map(|s| s.label(lang)).collect();
                    line.push(' ');
                    line.push_str(&tr!(lang, m::FUNCTION_SHADOWS, sources = hidden.join(", ")));
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    /// The Function `name` loads and its model fits the settings
    /// (AGL-036, D3, D9): what `use` and `/function` check before they
    /// switch.
    pub fn check(&self, name: &str) -> Result<(), Error> {
        self.server_of(name).map(|_| ())
    }

    /// The kind and the model of the Function `name` (AGL-039, D21).
    fn server_of(&self, name: &str) -> Result<(ServerKind, Option<String>), Error> {
        let function = Function::load(&self.workspace_dir, &self.user_dir, name)?;
        let server = self.settings.effective(&function.model);
        Ok((server.kind(), super::serve::model_of(&server)?))
    }
}

/// The Function names of the `*.nix` files of `dir`; none when it is
/// missing.
fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().is_some_and(|ext| ext == "nix") && path.is_file())
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

/// Runs `agl function list|use` in the workspace of `cwd`.
pub fn run(
    action: FunctionAction,
    user: &UserFile,
    paths: &Paths,
    cwd: &Path,
    lang: Lang,
) -> Result<ExitCode, Error> {
    let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
    let project = super::forge::project_of(paths, cwd, auto_rehome, lang)?;
    match action {
        FunctionAction::List => {
            for line in Catalog::of(paths, &project, user).lines(lang) {
                outln!("{line}");
            }
        }
        FunctionAction::Use { name, user: to_user } => {
            Catalog::of(paths, &project, user).check(&name)?;
            let path = if to_user {
                paths.settings_file()
            } else {
                // D8: the workspace is registered first, as a first write.
                for line in forge::materialize(&project, lang)? {
                    eprintln!("{line}");
                }
                project.dir.join(settings::SETTINGS_FILE)
            };
            // The file is written anew from its value (AGL-039, D11, D12).
            settings::set_top(&path, "function", serde_json::Value::String(name.clone()))?;
            outln!("{}", tr!(lang, m::FUNCTION_SET, name = name, path = path.display()));
            // The choice is made: what it needs is offered right away, and
            // a no is answered again at the first run (AGL-050, D10).
            if crate::repl::term::is_interactive()
                && project.server.over(&user.server).llama_external != Some(true)
                && let Ok(function) =
                    Function::load(&paths.functions_dir(&project.key), &paths.functions(), &name)
            {
                crate::weights::offer(
                    &function.model,
                    &function.sources,
                    &crate::weights::Folders {
                        own: paths.models(),
                        listed: &user.models,
                        user_path: &paths.settings_file(),
                    },
                    &super::serve::ask_line,
                    lang,
                )?;
            }
            // The project's own key wins over the user's in this project.
            if to_user
                && let Some(own) = project.function.as_deref().filter(|own| *own != name)
            {
                outln!(
                    "{}",
                    tr!(
                        lang,
                        m::FUNCTION_PROJECT_WINS,
                        path = project.dir.join(settings::SETTINGS_FILE).display(),
                        name = own
                    )
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;

    /// A Function file on `model` (AGL-039, D8, D17).
    fn file(model: &str) -> String {
        format!(
            r#"{{ agl, ... }}: {{ policy = {{ system = "s"; initial = "work"; state.work.answer = true; }}; model = {model}; }}"#
        )
    }

    const LLAMA: &str = r#"{ format = agl.formats."qwen3.8"; weights = "p.gguf"; }"#;
    const STRATA_NO_CONFIG: &str = r#"{ kind = "strata"; format = agl.formats."qwen3.8"; }"#;

    fn catalog(dir: &Path) -> Catalog {
        Catalog {
            workspace_dir: dir.join("workspace"),
            user_dir: dir.join("user"),
            settings: ServerFile {
                llama_url: Some("http://127.0.0.1:18080".into()),
                ..ServerFile::default()
            },
            current: "mine".into(),
        }
    }

    fn write(dir: &Path, name: &str, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.nix")), text).unwrap();
    }

    #[test]
    fn the_list_has_the_three_sources_with_their_servers() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.workspace_dir, "mine", &file(LLAMA));
        write(&catalog.user_dir, "mine", &file(LLAMA));
        write(&catalog.user_dir, "broken", &file(STRATA_NO_CONFIG));
        fs::write(catalog.user_dir.join("notes.txt"), "x").unwrap();
        let entries = catalog.entries(Lang::En);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "agentLIBRE-cpu-high",
                "agentLIBRE-cpu-low",
                "agentLIBRE-cpu-medium",
                "agentLIBRE-cpu-very-high",
                "agentLIBRE-gpu-high",
                "agentLIBRE-gpu-high-minimal",
                "agentLIBRE-gpu-low",
                "agentLIBRE-gpu-medium",
                "agentLIBRE-gpu-very-high",
                "broken",
                "mine",
            ]
        );
        let by = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
        // The workspace's file wins over the user's, and says so.
        let mine = by("mine");
        assert_eq!(mine.source, Source::Workspace);
        assert_eq!(mine.shadows, [Source::User]);
        assert!(mine.current);
        assert_eq!((mine.kind, mine.model.as_deref()), (ServerKind::Llama, Some("p.gguf")));
        let high = by("agentLIBRE-gpu-high");
        assert_eq!(high.source, Source::BuiltIn);
        assert_eq!(high.model.as_deref(), Some("Qwen3.8-27B-UD-IQ4_XS.gguf"));
        // A Function that does not load is listed with its error.
        assert!(by("broken").error.as_deref().is_some_and(|e| e.contains("strata_config")));
        let lines = catalog.lines(Lang::En);
        let current = lines.iter().find(|line| line.starts_with("* ")).unwrap();
        assert_eq!(current, "* mine                         workspace  llama   p.gguf over user");
    }

    #[test]
    fn check_refuses_what_a_run_would_refuse() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.user_dir, "broken", &file(STRATA_NO_CONFIG));
        assert!(catalog.check("agentLIBRE-gpu-high").is_ok());
        assert!(matches!(catalog.check("nope"), Err(Error::FunctionNotFound { .. })));
        assert!(matches!(catalog.check("broken"), Err(Error::Function { .. })));
    }
}
