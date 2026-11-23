//! What the forge's actions do, once (AGL-067, F7): `agl forge` prints what
//! they return and the agent's `forge` tool tells it. The rules of a new
//! name and of a change of status are here and nowhere else.

use std::path::PathBuf;

use super::task_spec;
use super::{Artifact, Header, Workspace};
use agentlibre_common::clock;
use crate::error::Error;
use crate::forge::error::Error as ForgeError;
use agentlibre_common::i18n::Lang;

/// The artifact of the store of `ws` named `name`: the files are parsed
/// one by one, the match is on the `name` field, the file is
/// `forge/<type>/<name>.md`.
pub fn find_artifact(ws: &Workspace, name: &str) -> Result<Artifact, Error> {
    for path in super::artifact_files(&ws.dir)? {
        if let Ok(artifact) = super::load_artifact(&path)
            && artifact.name == name
        {
            return Ok(artifact);
        }
    }
    Err(ForgeError::ArtifactNotFound {
        name: name.to_owned(),
        dir: ws.dir.join("forge"),
    }
    .into())
}

/// The `name`s of the artifacts of the store of `ws`.
pub fn store_names(ws: &Workspace) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for path in super::artifact_files(&ws.dir)? {
        if let Header::Artifact(artifact) = super::read_header(&path)? {
            names.push(artifact.name);
        }
    }
    Ok(names)
}

/// A name the human gave with `--name`: non-empty, no path separator.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\')
}

/// A task-spec that `new_task_spec` wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    pub name: String,
    pub path: PathBuf,
    /// What materializing the store said, when a marker was written.
    pub lines: Vec<String>,
}

/// The scaffold of a new task-spec in the store of `ws`: named `name`
/// verbatim, or after the store's numbering and the slug of `title`.
pub fn new_task_spec(
    ws: &Workspace,
    lang: Lang,
    title: &str,
    name: Option<&str>,
) -> Result<Created, Error> {
    let existing = store_names(ws)?;
    let name = match name {
        Some(name) => {
            if !valid_name(name) {
                return Err(agentlibre_common::error::Error::InvalidName(name.to_owned()).into());
            }
            name.to_owned()
        }
        None => {
            let (prefix, number) = task_spec::next_number(&existing);
            format!("{prefix}-{number:03}-{}", task_spec::slug(title))
        }
    };
    if existing.contains(&name) {
        return Err(ForgeError::NameTaken {
            name,
            dir: ws.dir.join("forge"),
        }
        .into());
    }
    let kind = super::type_of(task_spec::TYPE).expect("task-spec is in the registry");
    let path = ws
        .dir
        .join("forge")
        .join(kind.name())
        .join(format!("{name}.md"));
    let mut artifact = kind.scaffold(&name, &clock::today());
    artifact.path = path.clone();
    // The store is materialized on the first write.
    let lines = super::materialize(ws, lang)?;
    super::write_artifact(&artifact)?;
    Ok(Created { name, path, lines })
}

/// A task-spec whose status `change_status` moved.
#[derive(Clone, Debug)]
pub struct StatusChanged {
    /// As it was written.
    pub artifact: Artifact,
    /// The status as it was, and as the type names it.
    pub from: String,
    pub to: String,
}

/// Moves the task-spec `name` of `ws` to `state`: validates the
/// transition, bumps `updated` and rewrites the file with the body
/// byte-identical.
pub fn change_status(ws: &Workspace, name: &str, state: &str) -> Result<StatusChanged, Error> {
    let artifact = find_artifact(ws, name)?;
    let kind = super::type_of(&artifact.type_name).ok_or_else(|| Error::from(ForgeError::NotTaskSpec {
        name: name.to_owned(),
        type_name: artifact.type_name.clone(),
    }))?;
    let from = artifact.status.clone();
    if !kind.statuses().contains(&from.as_str()) {
        return Err(ForgeError::BadState { state: from }.into());
    }
    if !kind.statuses().contains(&state) {
        return Err(ForgeError::BadState { state: state.to_owned() }.into());
    }
    if from == state || !kind.allows(&from, state) {
        if kind.terminal(&from) {
            return Err(ForgeError::TerminalState {
                name: name.to_owned(),
                from,
            }
            .into());
        }
        return Err(ForgeError::BadTransition {
            from,
            to: state.to_owned(),
        }
        .into());
    }
    let mut artifact = artifact;
    artifact.status = state.to_owned();
    artifact.updated = clock::today();
    super::write_artifact(&artifact)?;
    Ok(StatusChanged { artifact, from, to: state.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_human_name_is_valid() {
        assert!(valid_name("agl-008-x"));
        assert!(!valid_name(""));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a\\b"));
    }
}
