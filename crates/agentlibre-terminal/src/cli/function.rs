//! `agl function list|use` (AGL-036, D3, D7, D8): the Functions a run can
//! load, each with its server kind and model, and the choice of the one
//! the runs load.

use std::path::Path;
use std::process::ExitCode;

use crate::error::Error;
use crate::forge;
use crate::function::Function;
use crate::i18n::{Lang, m};
use crate::paths::Paths;
use crate::settings::{self, UserFile};
use crate::tr;
use crate::outln;

use super::args::FunctionAction;
use crate::function::catalog::Catalog;

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
            if crate::tty::is_interactive()
                && project.server.over(&user.server).llama_external != Some(true)
                && let Ok(function) =
                    Function::load(&paths.functions_dir(&project.key), &paths.functions(), &name)
            {
                crate::weights::offer(
                    &function.model,
                    &function.sources,
                    &crate::weights::Folders {
                        own: crate::wsl::own_models(paths.models())?,
                        listed: &user.models,
                        user_path: &paths.settings_file(),
                    },
                    &crate::engine::ask_line,
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
