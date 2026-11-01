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
use crate::settings::{function_name, ServerFile, ServerKind, UserFile};
use crate::tomlfile;
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
    /// The kind and the model of its server; `from_settings` when the
    /// Function has no `[server]` table and the settings decide.
    pub kind: ServerKind,
    pub model: Option<String>,
    pub from_settings: bool,
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
    pub formats: PathBuf,
    /// The project's `[server]` over the user's, both kinds.
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
            formats: paths.formats(),
            settings: project.server.over(&user.server),
            current: function_name(None, project.function.as_deref(), user.function.as_deref()),
        }
    }

    /// Every Function by name, with its source and its server.
    pub fn entries(&self, lang: Lang) -> Vec<Entry> {
        let workspace = names_in(&self.workspace_dir);
        let user = names_in(&self.user_dir);
        let mut names: Vec<String> = workspace
            .iter()
            .chain(&user)
            .cloned()
            .chain(function::builtin_names().map(str::to_owned))
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
                if function::builtin_names().any(|builtin| builtin == name) {
                    sources.push(Source::BuiltIn);
                }
                let (kind, model, from_settings, error) = match self.server_of(&name) {
                    Ok((kind, model, from_settings)) => (kind, model, from_settings, None),
                    Err(error) => (ServerKind::default(), None, false, Some(error.message(lang))),
                };
                Entry {
                    current: name == self.current,
                    name,
                    source: sources[0],
                    shadows: sources[1..].to_vec(),
                    kind,
                    model,
                    from_settings,
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
                        if entry.from_settings {
                            line.push(' ');
                            line.push_str(tr!(lang, m::FUNCTION_FROM_SETTINGS));
                        }
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

    /// The Function `name` loads and its `[server]` table fits the
    /// settings (AGL-036, D3, D9): what `use` and `/function` check before
    /// they switch.
    pub fn check(&self, name: &str) -> Result<(), Error> {
        self.server_of(name).map(|_| ())
    }

    /// The kind and the model of the Function `name`, and whether the
    /// settings decide them (no `[server]` table).
    fn server_of(&self, name: &str) -> Result<(ServerKind, Option<String>, bool), Error> {
        let function =
            Function::load(&self.workspace_dir, &self.user_dir, &self.formats, name)?;
        let (server, from_settings) = match &function.server {
            Some(table) => (self.settings.effective(table, &function.path)?, false),
            None => (self.settings.selected(), true),
        };
        Ok((server.kind(), super::serve::model_of(&server)?, from_settings))
    }
}

/// The Function names of the `*.toml` files of `dir`; none when it is
/// missing.
fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().is_some_and(|ext| ext == "toml") && path.is_file())
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
                project.dir.join(forge::MARKER_FILE)
            };
            use_function(&path, &name)?;
            outln!("{}", tr!(lang, m::FUNCTION_SET, name = name, path = path.display()));
            // The project's own key wins over the user's in this project.
            if to_user
                && let Some(own) = project.function.as_deref().filter(|own| *own != name)
            {
                outln!(
                    "{}",
                    tr!(
                        lang,
                        m::FUNCTION_PROJECT_WINS,
                        path = project.dir.join(forge::MARKER_FILE).display(),
                        name = own
                    )
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Sets `function = "<name>"` in the settings file `path` by one line
/// (AGL-036, D7); a user file that does not exist is created.
fn use_function(path: &Path, name: &str) -> Result<(), Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(Error::Read {
                path: path.into(),
                source,
            })
        }
    };
    let text = tomlfile::set_top_string(path, &text, "function", name)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| Error::Write {
            path: dir.into(),
            source,
        })?;
    }
    forge::write_atomic(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;

    const LLAMA: &str = "format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"work\"\n\
        [server]\nmodel = \"p.gguf\"\n\
        [state.work]\nanswer = true\n";
    const STRATA_NO_CONFIG: &str = "format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"work\"\n\
        [server]\nkind = \"strata\"\n\
        [state.work]\nanswer = true\n";
    const PLAIN: &str = "format = \"qwen3.8\"\nsystem = \"s\"\ninitial = \"work\"\n\
        [state.work]\nanswer = true\n";

    fn catalog(dir: &Path) -> Catalog {
        Catalog {
            workspace_dir: dir.join("workspace"),
            user_dir: dir.join("user"),
            formats: dir.join("formats"),
            settings: ServerFile {
                llama_url: Some("http://127.0.0.1:18080".into()),
                model: Some("s.gguf".into()),
                ..ServerFile::default()
            },
            current: "mine".into(),
        }
    }

    fn write(dir: &Path, name: &str, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.toml")), text).unwrap();
    }

    #[test]
    fn the_list_has_the_three_sources_with_their_servers() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.workspace_dir, "mine", LLAMA);
        write(&catalog.user_dir, "mine", PLAIN);
        write(&catalog.user_dir, "plain", PLAIN);
        write(&catalog.user_dir, "broken", STRATA_NO_CONFIG);
        fs::write(catalog.user_dir.join("notes.txt"), "x").unwrap();
        let entries = catalog.entries(Lang::En);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["agentLIBRE", "broken", "mine", "minimal", "plain"]);
        let by = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
        // The workspace's file wins over the user's, and says so.
        let mine = by("mine");
        assert_eq!(mine.source, Source::Workspace);
        assert_eq!(mine.shadows, [Source::User]);
        assert!(mine.current);
        assert_eq!((mine.kind, mine.model.as_deref(), mine.from_settings), (ServerKind::Llama, Some("p.gguf"), false));
        // Without a table, the settings decide.
        let plain = by("plain");
        assert_eq!(plain.source, Source::User);
        assert_eq!((plain.model.as_deref(), plain.from_settings), (Some("s.gguf"), true));
        assert_eq!(by("minimal").source, Source::BuiltIn);
        // A Function that does not fit is listed with its error.
        assert!(by("broken").error.as_deref().is_some_and(|e| e.contains("strata_config")));
        let lines = catalog.lines(Lang::En);
        assert_eq!(lines[2], "* mine        workspace  llama   p.gguf over user");
        assert_eq!(lines[4], "  plain       user       llama   s.gguf (the settings' server)");
    }

    #[test]
    fn check_refuses_what_a_run_would_refuse() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.user_dir, "broken", STRATA_NO_CONFIG);
        assert!(catalog.check("minimal").is_ok());
        assert!(matches!(catalog.check("nope"), Err(Error::FunctionNotFound { .. })));
        assert!(matches!(catalog.check("broken"), Err(Error::Function { .. })));
    }

    #[test]
    fn use_writes_one_line_and_creates_a_missing_file() {
        let dir = TempDir::new();
        let path = dir.0.join("config/agentLIBRE.toml");
        use_function(&path, "minimal").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "function = \"minimal\"\n");
        fs::write(&path, "# mine\nfunction = \"a\"\n\n[server.llama]\nurl = \"http://x\"\n").unwrap();
        use_function(&path, "b").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\nfunction = \"b\"\n\n[server.llama]\nurl = \"http://x\"\n"
        );
    }
}
