//! The Functions of a workspace as a list (AGL-036, D3): where each comes
//! from, which server it needs, which one a run loads. `agl function` and
//! the REPL's `/function` read it (AGL-067, F4).

use std::fs;
use std::path::{Path, PathBuf};

use super::{check_name, Function};
use crate::error::Error;
use super::error::Error as FunctionError;
use crate::forge::Workspace;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::nix;
use agentlibre_common::paths::Paths;
use agentlibre_common::settings::{ServerFile, ServerKind, UserFile, function_name};
use agentlibre_common::tr;
use agentlibre_common::error::Error as CommonError;

/// Where a Function comes from, in the order a run looks (AGL-028, D3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The plane's `functions/` (AGL-077, D7).
    Plane,
    Workspace,
    User,
    BuiltIn,
}

impl Source {
    /// The source's word: `built-in`, `user`, the workspace's, the plane's.
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Source::Plane => tr!(lang, m::FUNCTION_SOURCE_PLANE),
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
    /// What it is made for (AGL-089, D12).
    pub description: Option<String>,
    /// Why it does not load, instead of the kind and the model.
    pub error: Option<String>,
    /// The Function a run here loads (`--function` aside).
    pub current: bool,
}

/// The source file of a Function (AGL-102, D2): where it is, and which
/// source wins with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFile {
    pub path: PathBuf,
    pub source: Source,
}

impl FunctionFile {
    /// The Function's name: the stem of its file.
    pub fn stem(&self) -> String {
        self.path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned())
    }
}

/// The Functions of a workspace (AGL-036, D3): the workspace's, the
/// user's and the built-ins, against the settings of the workspace.
pub struct Catalog {
    /// The plane's Functions, which win over the others (AGL-077, D7).
    pub plane_dir: Option<PathBuf>,
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
            plane_dir: None,
            workspace_dir: paths.functions_dir(&project.key),
            user_dir: paths.functions(),
            settings: project.server.over(&user.server),
            current: function_name(None, project.function.as_deref(), user.function.as_deref()),
        }
    }

    /// The catalog of `project` in `plane`, whose settings are `planes`
    /// (AGL-077, D7): the plane's Functions and settings win.
    pub fn in_plane(
        paths: &Paths,
        project: &Workspace,
        user: &UserFile,
        plane: &crate::plane::PlaneFile,
        settings: &agentlibre_common::settings::ProjectFile,
    ) -> Catalog {
        Catalog {
            plane_dir: Some(plane.functions_dir()),
            settings: settings.server.over(&project.server).over(&user.server),
            current: function_name(
                None,
                settings.function.as_deref().or(project.function.as_deref()),
                user.function.as_deref(),
            ),
            ..Catalog::of(paths, project, user)
        }
    }

    /// The directories a Function is looked for in, in their order.
    pub fn dirs(&self) -> Vec<&Path> {
        self.plane_dir
            .iter()
            .map(PathBuf::as_path)
            .chain([self.workspace_dir.as_path(), self.user_dir.as_path()])
            .collect()
    }

    /// Every Function by name, with its source and its server.
    pub fn entries(&self, lang: Lang) -> Vec<Entry> {
        let plane = self.plane_dir.as_deref().map(names_in).unwrap_or_default();
        let workspace = names_in(&self.workspace_dir);
        let user = names_in(&self.user_dir);
        // Without agl's `nix/` there is no built-in; each Function then
        // says why it does not load.
        let builtins = super::builtin_names().unwrap_or_default();
        let mut names: Vec<String> = plane
            .iter()
            .chain(&workspace)
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
                if plane.contains(&name) {
                    sources.push(Source::Plane);
                }
                if workspace.contains(&name) {
                    sources.push(Source::Workspace);
                }
                if user.contains(&name) {
                    sources.push(Source::User);
                }
                if builtins.contains(&name) {
                    sources.push(Source::BuiltIn);
                }
                let (kind, model, description, error) = match self.server_of(&name) {
                    Ok((kind, model, description)) => (kind, model, description, None),
                    Err(error) => (ServerKind::default(), None, None, Some(error.message(lang))),
                };
                Entry {
                    current: name == self.current,
                    name,
                    source: sources[0],
                    shadows: sources[1..].to_vec(),
                    kind,
                    model,
                    description,
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
        let no_model = tr!(lang, m::FUNCTION_NO_MODEL);
        let model_width = entries
            .iter()
            .filter(|e| e.error.is_none())
            .map(|e| e.model.as_deref().unwrap_or(no_model).chars().count())
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
                        let model = entry.model.as_deref().unwrap_or(no_model);
                        // What it is made for (AGL-089, D12, D14), in a
                        // column after the models.
                        match &entry.description {
                            Some(description) => {
                                line.push_str(&format!("{model:model_width$}  {description}"));
                            }
                            None => line.push_str(model),
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

    /// The Function `name` loads and its model fits the settings
    /// (AGL-036, D3, D9): what `use` and `/function` check before they
    /// switch.
    pub fn check(&self, name: &str) -> Result<(), Error> {
        self.server_of(name).map(|_| ())
    }

    /// The file of `name` whose source a run loads (AGL-102, D2): the
    /// winning one, as `load` resolves it; a built-in answers with its
    /// file in the Functions' repository.
    pub fn file(&self, name: &str) -> Result<FunctionFile, Error> {
        check_name(name)?;
        for dir in self.dirs() {
            let path = dir.join(format!("{name}.nix"));
            if path.is_file() {
                let source = self.source_at(&path);
                return Ok(FunctionFile { path, source });
            }
        }
        // The repository's `functions/` (AGL-053): `agl.functions` is
        // built from it by its default.nix; no file of that name means
        // no Function at all.
        let path = nix::functions_dir()?.join("functions").join(format!("{name}.nix"));
        if !path.is_file() {
            return Err(FunctionError::NotFound {
                name: name.into(),
                paths: self.dirs().iter().map(|dir| dir.to_path_buf()).collect(),
            }
            .into());
        }
        Ok(FunctionFile { path, source: Source::BuiltIn })
    }

    /// The source text of `name`: the bytes of the file `file` names
    /// (AGL-102, D2).
    pub fn text(&self, name: &str) -> Result<String, Error> {
        let file = self.file(name)?;
        fs::read_to_string(&file.path).map_err(|source| Error::from(CommonError::Read { path: file.path, source }))
    }

    /// The Function `name` of the workspace's `functions/`, whatever
    /// wins over it (AGL-102, D3).
    pub fn workspace_file(&self, name: &str) -> PathBuf {
        self.workspace_dir.join(format!("{name}.nix"))
    }

    /// True when `name` has a file in any source or is a built-in
    /// (AGL-102, D3: `new` refuses it).
    pub fn exists(&self, name: &str) -> bool {
        self.dirs()
            .iter()
            .any(|dir| dir.join(format!("{name}.nix")).is_file())
            || super::builtin_names()
                .map(|names| names.iter().any(|b| b == name))
                .unwrap_or(false)
    }

    /// The source of the file `path` of this catalog (AGL-102, D2).
    fn source_at(&self, path: &Path) -> Source {
        if path.parent() == Some(self.workspace_dir.as_path()) {
            Source::Workspace
        } else if self.plane_dir.as_deref().is_some_and(|dir| path.parent() == Some(dir)) {
            Source::Plane
        } else {
            Source::User
        }
    }

    /// Writes `text` to the workspace's `<name>.nix` as a first write,
    /// validated through the same load as `check` before the rename
    /// (AGL-102, D3, D4): on a failure the directory keeps what it had.
    /// A built-in of that name is never written.
    pub fn create(&self, name: &str, text: &str) -> Result<PathBuf, Error> {
        check_name(name)?;
        if self.exists(name) {
            return Err(FunctionError::Exists { name: name.into() }.into());
        }
        write_validated(&self.workspace_file(name), &self.workspace_dir, name, text)
    }

    /// Saves the edited `text` over `file` (AGL-102, D4): atomic,
    /// validated through the same load as `check` before the rename; on
    /// a failure the file keeps its previous text.
    pub fn save(&self, file: &FunctionFile, text: &str) -> Result<(), Error> {
        if file.source == Source::BuiltIn {
            return Err(FunctionError::BuiltIn { name: file.stem() }.into());
        }
        write_validated(&file.path, file.path.parent().unwrap_or(Path::new(".")), &file.stem(), text)
            .map(|_| ())
    }

    /// The kind, the model and the description of the Function `name`
    /// (AGL-039, D21; AGL-089, D12).
    fn server_of(&self, name: &str) -> Result<(ServerKind, Option<String>, Option<String>), Error> {
        let function = Function::load(&self.dirs(), name)?;
        let server = self.settings.effective(&function.model);
        Ok((server.kind(), crate::engine::model_of(&server)?, function.description))
    }
}

/// Writes `text` to `target` as a temp file next to it, loads that file
/// alone the way `Catalog::check` loads it, and only then renames it over
/// `target` (AGL-102, D3, D4): a text that does not load never reaches
/// the name.
fn write_validated(target: &Path, dir: &Path, name: &str, text: &str) -> Result<PathBuf, Error> {
    fs::create_dir_all(dir).map_err(|source| Error::from(CommonError::Write {
        path: dir.to_path_buf(),
        source,
    }))?;
    let temp = dir.join(format!("{name}.nix.tmp"));
    fs::write(&temp, text).map_err(|source| Error::from(CommonError::Write { path: temp.clone(), source }))?;
    match nix::eval_file(&temp).map_err(Into::into).and_then(|value| Function::from_value(&temp, value)) {
        Ok(_) => fs::rename(&temp, target)
            .map(|()| target.to_path_buf())
            .map_err(|source| Error::from(CommonError::Write { path: target.to_path_buf(), source })),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_common::testutil::TempDir;

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
            plane_dir: None,
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
                "broken",
                "fixture-gemma",
                "fixture-high",
                "fixture-minimal",
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
        let high = by("fixture-high");
        assert_eq!(high.source, Source::BuiltIn);
        assert_eq!(high.model.as_deref(), Some("Qwen3.8-27B-UD-IQ4_XS.gguf"));
        // A Function that does not load is listed with its error.
        assert!(by("broken").error.as_deref().is_some_and(|e| e.contains("needs `strata`")));
        let lines = catalog.lines(Lang::En);
        let current = lines.iter().find(|line| line.starts_with("* ")).unwrap();
        assert_eq!(current, "* mine             workspace  llama   p.gguf over user");
    }

    #[test]
    fn a_description_is_a_column_after_the_models() {
        // AGL-089, D12, D14.
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        let described = |text: &str| file(LLAMA).replacen("policy =", &format!("description = \"{text}\"; policy ="), 1);
        write(&catalog.user_dir, "mine", &described("for a GPU with 10 GB of VRAM and more"));
        write(&catalog.user_dir, "other", &file(LLAMA));
        let entries = catalog.entries(Lang::En);
        let by = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
        assert_eq!(by("mine").description.as_deref(), Some("for a GPU with 10 GB of VRAM and more"));
        assert_eq!(by("other").description, None);
        let lines = catalog.lines(Lang::En);
        let line = |name: &str| lines.iter().find(|l| l[2..].starts_with(name)).unwrap().clone();
        let mine = line("mine");
        let column = mine.find("for a GPU").unwrap();
        // The column starts after the longest model of the list.
        assert_eq!(&mine[column - 2..column], "  ");
        assert!(mine[..column].contains("p.gguf "), "{mine}");
        assert!(line("other").ends_with("llama   p.gguf"), "{}", line("other"));
    }

    #[test]
    fn check_refuses_what_a_run_would_refuse() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.user_dir, "broken", &file(STRATA_NO_CONFIG));
        assert!(catalog.check("fixture-high").is_ok());
        assert!(matches!(catalog.check("nope"), Err(Error::Function(FunctionError::NotFound { .. }))));
        assert!(matches!(catalog.check("broken"), Err(Error::Function(FunctionError::Invalid { .. }))));
    }

    /// AGL-102, D2: the file of a Function is the one that wins; a name
    /// with no file answers with the built-in's file of the repository.
    #[test]
    fn the_winning_file_and_its_source() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.user_dir, "mine", &file(LLAMA));
        let f = catalog.file("mine").unwrap();
        assert_eq!(f.source, Source::User);
        assert_eq!(f.path, catalog.user_dir.join("mine.nix"));
        assert_eq!(catalog.text("mine").unwrap(), file(LLAMA));
        write(&catalog.workspace_dir, "mine", &file(LLAMA));
        assert_eq!(catalog.file("mine").unwrap().source, Source::Workspace);
        // A name with no file is the built-in's.
        let f = catalog.file("fixture-minimal").unwrap();
        assert_eq!(f.source, Source::BuiltIn);
        assert!(catalog.text("fixture-minimal").unwrap().contains("policy"));
    }

    /// AGL-102, D3: `create` writes the workspace's `functions/`, refuses
    /// a name that exists and a text that does not load.
    #[test]
    fn create_writes_the_workspace_and_refuses() {
        let dir = TempDir::new();
        let catalog = catalog(&dir.0);
        write(&catalog.user_dir, "mine", &file(LLAMA));
        assert!(catalog.exists("mine"));
        assert!(catalog.exists("fixture-minimal"));
        assert!(!catalog.exists("twin"));
        let path = catalog.create("twin", &file(LLAMA)).unwrap();
        assert_eq!(path, catalog.workspace_file("twin"));
        assert!(catalog.check("twin").is_ok());
        // A name of any source, a built-in included, is refused.
        assert!(matches!(catalog.create("mine", &file(LLAMA)), Err(Error::Function(FunctionError::Exists { .. }))));
        assert!(matches!(
            catalog.create("fixture-minimal", &file(LLAMA)),
            Err(Error::Function(FunctionError::Exists { .. }))
        ));
        // A text that does not load never reaches the name.
        assert!(matches!(catalog.create("twin2", "{ nope"), Err(Error::Common(agentlibre_common::error::Error::Nix(nix::Error::Eval { .. })))));
        assert!(!catalog.workspace_file("twin2").exists());
    }
}
