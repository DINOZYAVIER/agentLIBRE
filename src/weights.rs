//! The weights files of a run (AGL-039, D22-D24): a file name is looked
//! for in agl's own `models/` folder, then in the folders of the user's
//! `models`; an absolute path is taken as it is. A file found nowhere is
//! asked for in an interactive terminal, and its folder is kept in the
//! user's settings; elsewhere it is an error. agl does not search by
//! itself (D24d).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::settings::{self, ServerFile, WhisperFile};
use crate::tr;

/// What a file found nowhere does.
pub enum Missing<'a> {
    /// Ask the human for its folder (an interactive terminal, D24c): the
    /// answer, `None` when there is none.
    Ask(&'a dyn Fn(&str) -> Option<String>),
    /// Fail with `WeightsNotFound` (a run without a terminal, D24c).
    Fail,
    /// Keep the name: the command does not run the model (`agl server
    /// status`, `stop`).
    Keep,
}

/// The weights folders: agl's own, then the user's list (D23, D24b).
pub struct Folders<'a> {
    pub own: PathBuf,
    pub listed: &'a [PathBuf],
    /// The user's settings file, where an answered folder is kept.
    pub user_path: &'a Path,
}

impl Folders<'_> {
    fn all(&self) -> Vec<PathBuf> {
        std::iter::once(self.own.clone())
            .chain(self.listed.iter().cloned())
            .collect()
    }
}

/// The file `name` in `folders`: an absolute `name` as it is, else the
/// first folder that has it.
pub fn find(name: &str, folders: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    folders.iter().map(|folder| folder.join(name)).find(|path| path.is_file())
}

/// Makes the weights files of `server` (`model`, `draft`, `mmproj`) and of
/// a set-up `whisper` (`url` and `model`) absolute paths (D24e).
pub fn resolve(
    server: &mut ServerFile,
    whisper: &mut WhisperFile,
    folders: &Folders,
    missing: &Missing,
    lang: Lang,
) -> Result<(), Error> {
    let mut found = folders.all();
    let mut listed = folders.listed.to_vec();
    let whisper_set = whisper.url.is_some();
    let files = [
        &mut server.model,
        &mut server.draft,
        &mut server.mmproj,
    ]
    .into_iter()
    .chain(whisper_set.then_some(&mut whisper.model));
    for file in files {
        let Some(name) = file.clone() else {
            continue;
        };
        if let Some(path) = find(&name, &found) {
            *file = Some(path.to_string_lossy().into_owned());
            continue;
        }
        match missing {
            Missing::Keep => {}
            Missing::Fail => {
                return Err(Error::WeightsNotFound { name, folders: found });
            }
            Missing::Ask(ask) => {
                let question = tr!(lang, m::WEIGHTS_ASK, name = name.as_str());
                let folder = ask(&question)
                    .map(|answer| PathBuf::from(answer.trim()))
                    .filter(|folder| folder.is_absolute() && folder.join(&name).is_file());
                let Some(folder) = folder else {
                    return Err(Error::WeightsNotFound { name, folders: found });
                };
                // The folder is kept for the next runs (D24c, D12).
                listed.push(folder.clone());
                let list = listed
                    .iter()
                    .map(|folder| Value::String(folder.to_string_lossy().into_owned()))
                    .collect();
                settings::set_top(folders.user_path, "models", Value::Array(list))?;
                *file = Some(folder.join(&name).to_string_lossy().into_owned());
                found.push(folder);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use std::cell::RefCell;
    use std::fs;

    fn server(model: &str) -> ServerFile {
        ServerFile {
            model: Some(model.into()),
            ..ServerFile::default()
        }
    }

    #[test]
    fn a_name_is_looked_for_in_order_and_a_path_is_taken_as_it_is() {
        let dir = TempDir::new();
        let own = dir.0.join("own");
        let more = dir.0.join("more");
        fs::create_dir_all(&own).unwrap();
        fs::create_dir_all(&more).unwrap();
        fs::write(more.join("a.gguf"), "").unwrap();
        fs::write(own.join("b.gguf"), "").unwrap();
        fs::write(more.join("b.gguf"), "").unwrap();
        let folders = [own.clone(), more.clone()];
        assert_eq!(find("a.gguf", &folders), Some(more.join("a.gguf")));
        // agl's own folder first.
        assert_eq!(find("b.gguf", &folders), Some(own.join("b.gguf")));
        assert_eq!(find("c.gguf", &folders), None);
        assert_eq!(find("/x/c.gguf", &folders), Some(PathBuf::from("/x/c.gguf")));
    }

    #[test]
    fn a_missing_file_fails_without_a_terminal_and_is_kept_by_status() {
        let dir = TempDir::new();
        let folders = Folders {
            own: dir.0.join("own"),
            listed: &[],
            user_path: &dir.0.join("settings.nix"),
        };
        let mut whisper = WhisperFile::default();
        let mut kept = server("m.gguf");
        resolve(&mut kept, &mut whisper, &folders, &Missing::Keep, Lang::En).unwrap();
        assert_eq!(kept.model.as_deref(), Some("m.gguf"));
        let mut failed = server("m.gguf");
        match resolve(&mut failed, &mut whisper, &folders, &Missing::Fail, Lang::En) {
            Err(Error::WeightsNotFound { name, folders }) => {
                assert_eq!(name, "m.gguf");
                assert_eq!(folders, [dir.0.join("own")]);
            }
            other => panic!("{other:?}"),
        }
        // A whisper model is looked for only when the whisper server is set.
        let mut whisper = WhisperFile {
            model: Some("w.bin".into()),
            ..WhisperFile::default()
        };
        resolve(&mut ServerFile::default(), &mut whisper, &folders, &Missing::Fail, Lang::En).unwrap();
        whisper.url = Some("http://127.0.0.1:1".into());
        assert!(resolve(&mut ServerFile::default(), &mut whisper, &folders, &Missing::Fail, Lang::En).is_err());
    }

    #[test]
    fn the_answered_folder_is_kept_in_the_user_settings() {
        let dir = TempDir::new();
        let elsewhere = dir.0.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("m.gguf"), "").unwrap();
        let user_path = dir.0.join("config/settings.nix");
        let listed = [PathBuf::from("/first")];
        let folders = Folders {
            own: dir.0.join("own"),
            listed: &listed,
            user_path: &user_path,
        };
        let asked = RefCell::new(Vec::new());
        let answer = elsewhere.to_string_lossy().into_owned();
        let ask = |question: &str| {
            asked.borrow_mut().push(question.to_owned());
            Some(format!("  {answer}\n"))
        };
        let mut model = server("m.gguf");
        resolve(&mut model, &mut WhisperFile::default(), &folders, &Missing::Ask(&ask), Lang::En).unwrap();
        assert_eq!(model.model, Some(elsewhere.join("m.gguf").to_string_lossy().into_owned()));
        assert_eq!(asked.borrow().len(), 1);
        assert!(asked.borrow()[0].contains("m.gguf"));
        let user = crate::settings::UserFile::load(&user_path).unwrap();
        assert_eq!(user.models, [PathBuf::from("/first"), elsewhere.clone()]);
        // A folder without the file, or no answer, is the error.
        let nothing = |_: &str| None;
        let mut other = server("n.gguf");
        assert!(matches!(
            resolve(&mut other, &mut WhisperFile::default(), &folders, &Missing::Ask(&nothing), Lang::En),
            Err(Error::WeightsNotFound { .. })
        ));
        let wrong = |_: &str| Some(answer.clone());
        assert!(matches!(
            resolve(&mut other, &mut WhisperFile::default(), &folders, &Missing::Ask(&wrong), Lang::En),
            Err(Error::WeightsNotFound { .. })
        ));
    }
}
