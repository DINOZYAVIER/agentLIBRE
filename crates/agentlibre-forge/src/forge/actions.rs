//! What the forge's actions do, once (AGL-067, F7): `agl forge` prints what
//! they return and the agent's `forge` tool tells it. The rules of a new
//! name and of a change of status are here and nowhere else.

use std::path::PathBuf;

use super::{naming, Artifact, Header, Kind, Kinds, Workspace};
use agentlibre_common::clock;
use crate::error::Error;
use crate::forge::error::Error as ForgeError;
use agentlibre_common::i18n::Lang;

/// The artifact of the store of `ws` named `name`: the files are parsed
/// one by one, the match is on the `name` field, the file is
/// `forge/<type>/<name>.md`.
pub fn find_artifact(ws: &Workspace, kinds: &Kinds, name: &str) -> Result<Artifact, Error> {
    let mut broken = None;
    for path in super::artifact_files(&ws.dir)? {
        match super::load_artifact(&path, kinds) {
            Ok(artifact) if artifact.name == name => return Ok(artifact),
            Ok(_) => {}
            // A file named after `name` that does not parse says why
            // (AGL-124, D18: a broken description fails its artifacts).
            Err(error) if path.file_stem().is_some_and(|stem| stem == name) => broken = Some(error),
            Err(_) => {}
        }
    }
    Err(broken.unwrap_or_else(|| {
        ForgeError::ArtifactNotFound {
            name: name.to_owned(),
            dir: ws.dir.join("forge"),
        }
        .into()
    }))
}

/// The `name` and the `type` of each artifact of the store of `ws`.
pub fn store_entries(ws: &Workspace, kinds: &Kinds) -> Result<Vec<(String, String)>, Error> {
    let files = super::artifact_files(&ws.dir)?;
    Ok(super::read_headers(&files, kinds)?
        .into_iter()
        .filter_map(|header| match header {
            Header::Artifact(artifact) => Some((artifact.name, artifact.type_name)),
            _ => None,
        })
        .collect())
}

/// The `name`s of the artifacts of the store of `ws`.
pub fn store_names(ws: &Workspace, kinds: &Kinds) -> Result<Vec<String>, Error> {
    Ok(store_entries(ws, kinds)?.into_iter().map(|(name, _)| name).collect())
}

/// A name the human gave with `--name`: non-empty, no path separator.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\')
}

/// An artifact that `new_artifact` wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    pub name: String,
    pub path: PathBuf,
    /// What materializing the store said, when a marker was written.
    pub lines: Vec<String>,
}

/// The scaffold of a new artifact of `kind` in the store of `ws`: named
/// `name` verbatim, or after the numbering of the artifacts of its type
/// and the slug of `title` (AGL-124, D15). Names stay unique across the
/// store.
pub fn new_artifact(
    ws: &Workspace,
    kinds: &Kinds,
    kind: &Kind,
    lang: Lang,
    title: &str,
    name: Option<&str>,
) -> Result<Created, Error> {
    let existing = store_entries(ws, kinds)?;
    let name = match name {
        Some(name) => {
            if !valid_name(name) {
                return Err(agentlibre_common::error::Error::InvalidName(name.to_owned()).into());
            }
            name.to_owned()
        }
        None => {
            let of_type: Vec<String> = existing
                .iter()
                .filter(|(_, type_name)| *type_name == kind.name)
                .map(|(name, _)| name.clone())
                .collect();
            let (prefix, number) = naming::next_number(&of_type, kind.prefix.as_deref());
            format!("{prefix}-{number:03}-{}", naming::slug(title))
        }
    };
    if existing.iter().any(|(taken, _)| *taken == name) {
        return Err(ForgeError::NameTaken {
            name,
            dir: ws.dir.join("forge"),
        }
        .into());
    }
    let path = ws.dir.join("forge").join(&kind.name).join(format!("{name}.md"));
    let mut artifact = kind.scaffold(&name, &clock::today());
    artifact.path = path.clone();
    // The store is materialized on the first write.
    let lines = super::materialize(ws, lang)?;
    super::write_artifact(&artifact)?;
    Ok(Created { name, path, lines })
}

/// An artifact whose status `change_status` moved.
#[derive(Clone, Debug)]
pub struct StatusChanged {
    /// As it was written.
    pub artifact: Artifact,
    /// The status as it was, and as the type names it.
    pub from: String,
    pub to: String,
}

/// Moves the artifact `name` of `ws` to `state` by the edges of its
/// kind (AGL-124, D23): validates the transition, bumps `updated` and
/// rewrites the file with the body byte-identical.
pub fn change_status(ws: &Workspace, kinds: &Kinds, name: &str, state: &str) -> Result<StatusChanged, Error> {
    let artifact = find_artifact(ws, kinds, name)?;
    let kind = kinds.get(&artifact.type_name)?;
    let from = artifact.status.clone();
    if !kind.statuses.contains(&state.to_owned()) {
        return Err(ForgeError::BadState {
            state: state.to_owned(),
            type_name: kind.name.clone(),
            statuses: kind.statuses.clone(),
        }
        .into());
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
