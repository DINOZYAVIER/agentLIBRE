//! `agl function list|use` (AGL-036, D3, D7, D8): the Functions a run can
//! load, each with its server kind and model, and the choice of the one
//! the runs load.

use std::path::Path;
use std::process::ExitCode;

use crate::error::Error;
use crate::forge;
use crate::function::Function;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::paths::Paths;
use agentlibre_common::settings::{self, UserFile};
use agentlibre_common::tr;
use agentlibre_common::outln;

use super::args::FunctionAction;
use crate::function::catalog::Catalog;

/// Runs `agl function list|use` in the workspace of `cwd`.
pub fn run(
    action: FunctionAction,
    plane: Option<&str>,
    user: &UserFile,
    paths: &Paths,
    cwd: &Path,
    lang: Lang,
) -> Result<ExitCode, Error> {
    let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
    let project = super::forge::project_of(paths, cwd, auto_rehome, lang)?;
    // `--plane`: the plane's Functions and settings win (AGL-077, D7, D11).
    let plane = plane
        .map(|name| crate::plane::PlaneFile::open(paths, name))
        .transpose()?;
    let catalog = match &plane {
        Some(plane) => Catalog::in_plane(paths, &project, user, plane, &plane.settings()?),
        None => Catalog::of(paths, &project, user),
    };
    match action {
        FunctionAction::List => {
            for line in catalog.lines(lang) {
                outln!("{line}");
            }
        }
        // AGL-102, D2: the source and the file it loads from.
        FunctionAction::Show { name } => {
            let text = catalog.text(&name)?;
            let file = catalog.file(&name)?;
            outln!("{text}\n{}", file.path.display());
        }
        // AGL-102, D3: a copy to the workspace's functions/, the workspace
        // registered first, as `use` does.
        FunctionAction::New { name, from } => {
            for line in forge::materialize(&project, lang)? {
                eprintln!("{line}");
            }
            let source = from.as_deref().unwrap_or(&catalog.current);
            let text = catalog.text(source)?;
            let path = catalog.create(&name, &text)?;
            outln!(
                "{}",
                tr!(
                    lang,
                    m::FUNCTION_NEW,
                    name = name,
                    source = source,
                    path = path.display()
                )
            );
        }
        // AGL-109, D5: the load of a file the editor left, or of what the
        // name resolves to; what does not load is the error itself.
        FunctionAction::Check(target) => {
            let path = Path::new(&target);
            let (name, function) = if path.is_file() {
                (
                    path.file_stem()
                        .map_or_else(|| target.clone(), |stem| stem.to_string_lossy().into_owned()),
                    Function::from_value(path, agentlibre_common::nix::eval_file(path)?)?,
                )
            } else {
                (target.clone(), Function::load(&catalog.dirs(), &target)?)
            };
            outln!(
                "{}",
                tr!(
                    lang,
                    m::FUNCTION_CHECK,
                    name = name,
                    kind = function.model.kind().name(),
                    model = crate::engine::model_of(&function.model)?.unwrap_or_default(),
                    context = function.model.context.unwrap_or_default()
                )
            );
        }
        FunctionAction::Use { name, user: to_user } => {
            catalog.check(&name)?;
            let path = if to_user {
                paths.settings_file()
            } else if let Some(plane) = &plane {
                plane.settings_file()
            } else {
                // D8: the workspace is registered first, as a first write.
                for line in forge::materialize(&project, lang)? {
                    eprintln!("{line}");
                }
                project.dir.join(settings::SETTINGS_FILE)
            };
            // The Functions' directories, the plane's first (AGL-077, D7).
            let dirs: Vec<std::path::PathBuf> = plane
                .iter()
                .map(crate::plane::PlaneFile::functions_dir)
                .chain([paths.functions_dir(&project.key), paths.functions()])
                .collect();
            // The file is written anew from its value (AGL-039, D11, D12).
            settings::set_top(&path, "function", serde_json::Value::String(name.clone()))?;
            outln!("{}", tr!(lang, m::FUNCTION_SET, name = name, path = path.display()));
            // The choice is made: what it needs is offered right away, and
            // a no is answered again at the first run (AGL-050, D10).
            if agentlibre_common::tty::is_interactive()
                && project.server.over(&user.server).llama_external != Some(true)
                && let Ok(function) = Function::load(
                    &dirs.iter().map(std::path::PathBuf::as_path).collect::<Vec<_>>(),
                    &name,
                )
            {
                crate::weights::offer(
                    &function.model,
                    &function.sources,
                    &crate::weights::Folders {
                        own: crate::wsl::own_models(paths.models())?,
                        listed: &user.models,
                        user_path: &paths.settings_file(),
                    },
                    &agentlibre_common::tty::ask_line,
                    &|line| eprintln!("{line}"),
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
