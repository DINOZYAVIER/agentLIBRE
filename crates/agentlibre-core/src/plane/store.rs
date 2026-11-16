//! The store of a plane (AGL-077): `planes/<name>/` under the data
//! directory, with the parts of a workspace's store (`settings.nix`,
//! `functions/`, `memory/`, `forge/`, D1, D4) and its file, `plane.nix`
//! (D16): its id, which never changes (D13, D17), and its directories, any
//! of which may be in other planes too (D2).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::nix;
use crate::paths::Paths;

/// The file of a plane in its store (D16).
pub const PLANE_FILE: &str = "plane.nix";

/// The only `format` of the file (D16).
const FORMAT: i64 = 1;

/// A plane as its store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaneFile {
    /// Made at `agl plane new`, never changed (D13, D17).
    pub id: String,
    /// What the human types, the name of its store (D5).
    pub name: String,
    /// Its store.
    pub dir: PathBuf,
    /// Its directories, absolute, in the order they were added (D2).
    pub directories: Vec<PathBuf>,
}

impl PlaneFile {
    /// Makes the plane `name` with `directories` (D5): its store and its
    /// file; a name a plane has is refused.
    pub fn create(paths: &Paths, name: &str, directories: &[PathBuf]) -> Result<PlaneFile, Error> {
        check_name(name)?;
        let dir = paths.plane_dir(name);
        if dir.exists() {
            return Err(Error::PlaneExists { name: name.into() });
        }
        let id = crate::session::uuid_v7().map_err(|source| Error::Write {
            path: dir.clone(),
            source,
        })?;
        let mut plane = PlaneFile {
            id,
            name: name.into(),
            dir,
            directories: Vec::new(),
        };
        for directory in directories {
            plane.push(directory)?;
        }
        plane.save()?;
        Ok(plane)
    }

    /// The plane `name`.
    pub fn open(paths: &Paths, name: &str) -> Result<PlaneFile, Error> {
        check_name(name)?;
        let dir = paths.plane_dir(name);
        let file = dir.join(PLANE_FILE);
        if !file.is_file() {
            return Err(Error::PlaneNotFound { name: name.into() });
        }
        let value = nix::eval_file(&file)?;
        let bad = |detail: &str| Error::PlaneFile {
            path: file.clone(),
            detail: detail.into(),
        };
        let format = value["format"].as_i64().ok_or_else(|| bad("no format"))?;
        if format != FORMAT {
            return Err(bad(&format!("format {format}")));
        }
        let id = value["id"].as_str().filter(|id| !id.is_empty()).ok_or_else(|| bad("no id"))?;
        let directories = value["directories"]
            .as_array()
            .ok_or_else(|| bad("no directories"))?
            .iter()
            .map(|directory| directory.as_str().map(PathBuf::from).ok_or_else(|| bad("a directory that is not a string")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PlaneFile {
            id: id.into(),
            name: name.into(),
            dir,
            directories,
        })
    }

    /// Every plane, by name; a store whose file is broken is an error.
    pub fn list(paths: &Paths) -> Result<Vec<PlaneFile>, Error> {
        let root = paths.planes();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::Read { path: root, source }),
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().join(PLANE_FILE).is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| check_name(name).is_ok())
            .collect();
        names.sort();
        names.iter().map(|name| PlaneFile::open(paths, name)).collect()
    }

    /// The planes `directory` is in (D2).
    pub fn of_directory(paths: &Paths, directory: &Path) -> Result<Vec<PlaneFile>, Error> {
        let directory = absolute(directory)?;
        Ok(PlaneFile::list(paths)?
            .into_iter()
            .filter(|plane| plane.directories.contains(&directory))
            .collect())
    }

    /// The plane whose id is `id`, when there is one (D13).
    pub fn with_id(paths: &Paths, id: &str) -> Result<Option<PlaneFile>, Error> {
        Ok(PlaneFile::list(paths)?.into_iter().find(|plane| plane.id == id))
    }

    /// Renames the plane: its store moves, its id stays (D13).
    pub fn rename(paths: &Paths, old: &str, new: &str) -> Result<PlaneFile, Error> {
        let plane = PlaneFile::open(paths, old)?;
        check_name(new)?;
        let dir = paths.plane_dir(new);
        if dir.exists() {
            return Err(Error::PlaneExists { name: new.into() });
        }
        fs::rename(&plane.dir, &dir).map_err(|source| Error::Write {
            path: dir.clone(),
            source,
        })?;
        Ok(PlaneFile {
            name: new.into(),
            dir,
            ..plane
        })
    }

    /// Adds `directory`; false when it was in the plane already.
    pub fn add(&mut self, directory: &Path) -> Result<bool, Error> {
        let added = self.push(directory)?;
        if added {
            self.save()?;
        }
        Ok(added)
    }

    /// Takes `directory` out of the plane; its files are not touched.
    pub fn remove(&mut self, directory: &Path) -> Result<(), Error> {
        let directory = absolute(directory)?;
        let before = self.directories.len();
        self.directories.retain(|known| *known != directory);
        if self.directories.len() == before {
            return Err(Error::PlaneDirectoryNotIn {
                name: self.name.clone(),
                directory,
            });
        }
        self.save()
    }

    /// Removes the plane's store (D14); the sessions keep their files.
    pub fn delete(self) -> Result<(), Error> {
        fs::remove_dir_all(&self.dir).map_err(|source| Error::Write {
            path: self.dir,
            source,
        })
    }

    /// The plane's `settings.nix` (D1).
    pub fn settings_file(&self) -> PathBuf {
        self.dir.join(crate::settings::SETTINGS_FILE)
    }

    /// The plane's settings; none when it has no `settings.nix` (D1, D7).
    pub fn settings(&self) -> Result<crate::settings::ProjectFile, Error> {
        crate::settings::ProjectFile::read(&self.settings_file())
    }

    /// The plane's `functions/` (D1, D7).
    pub fn functions_dir(&self) -> PathBuf {
        self.dir.join("functions")
    }

    /// The plane's `forge/` (D1, D9).
    pub fn forge_dir(&self) -> PathBuf {
        self.dir.join("forge")
    }

    /// The plane's `memory/` (D1, D8).
    pub fn memory_dir(&self) -> PathBuf {
        self.dir.join("memory")
    }

    /// The plane as a session's header records it (D18).
    pub fn of_session(&self) -> crate::session::SessionPlane {
        crate::session::SessionPlane {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    fn push(&mut self, directory: &Path) -> Result<bool, Error> {
        let directory = absolute(directory)?;
        if self.directories.contains(&directory) {
            return Ok(false);
        }
        self.directories.push(directory);
        Ok(true)
    }

    fn save(&self) -> Result<(), Error> {
        let mut map = Map::new();
        map.insert("format".into(), json!(FORMAT));
        map.insert("id".into(), json!(self.id));
        map.insert(
            "directories".into(),
            Value::Array(
                self.directories
                    .iter()
                    .map(|directory| json!(directory.to_string_lossy()))
                    .collect(),
            ),
        );
        nix::write_settings(&self.dir.join(PLANE_FILE), &map)
    }
}

/// A plane's name is its store's directory (D5): not empty, no `/`, not
/// `.` or `..`, and not hidden.
fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') || name.contains('\0') {
        return Err(Error::PlaneName { name: name.into() });
    }
    Ok(())
}

/// `directory` as the plane keeps it: absolute and canonical; it must exist.
fn absolute(directory: &Path) -> Result<PathBuf, Error> {
    fs::canonicalize(directory).map_err(|source| Error::Read {
        path: directory.into(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;

    fn paths(dir: &TempDir) -> Paths {
        Paths {
            config: dir.0.join("config"),
            data: dir.0.join("data"),
            state: dir.0.join("state"),
            runtime: None,
        }
    }

    fn directory(dir: &TempDir, name: &str) -> PathBuf {
        let path = dir.0.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    #[test]
    fn a_plane_is_made_listed_and_opened_by_its_name() {
        let dir = TempDir::new();
        let paths = paths(&dir);
        let a = directory(&dir, "a");
        let made = PlaneFile::create(&paths, "work", std::slice::from_ref(&a)).unwrap();
        assert_eq!(made.id.len(), 36);
        assert_eq!(made.dir, paths.plane_dir("work"));
        let text = fs::read_to_string(made.dir.join(PLANE_FILE)).unwrap();
        assert!(text.starts_with("{ agl, ... }: {\n  directories = ["), "{text}");
        assert_eq!(PlaneFile::open(&paths, "work").unwrap(), made);
        assert_eq!(PlaneFile::list(&paths).unwrap(), std::slice::from_ref(&made));
        assert!(matches!(
            PlaneFile::create(&paths, "work", &[]),
            Err(Error::PlaneExists { .. })
        ));
        assert!(matches!(PlaneFile::open(&paths, "none"), Err(Error::PlaneNotFound { .. })));
        for name in ["", "a/b", ".hidden", ".."] {
            assert!(matches!(PlaneFile::create(&paths, name, &[]), Err(Error::PlaneName { .. })));
        }
    }

    #[test]
    fn directories_and_planes_are_many_to_many() {
        let dir = TempDir::new();
        let paths = paths(&dir);
        let (a, b) = (directory(&dir, "a"), directory(&dir, "b"));
        let mut one = PlaneFile::create(&paths, "one", std::slice::from_ref(&a)).unwrap();
        PlaneFile::create(&paths, "two", &[a.clone(), b.clone()]).unwrap();
        assert!(one.add(&b).unwrap());
        assert!(!one.add(&b).unwrap(), "added once");
        let names = |directory: &Path| -> Vec<String> {
            PlaneFile::of_directory(&paths, directory)
                .unwrap()
                .into_iter()
                .map(|plane| plane.name)
                .collect()
        };
        assert_eq!(names(&a), ["one", "two"]);
        assert_eq!(names(&b), ["one", "two"]);
        one.remove(&a).unwrap();
        assert_eq!(names(&a), ["two"]);
        assert!(a.is_dir(), "its files are not touched");
        assert!(matches!(one.remove(&a), Err(Error::PlaneDirectoryNotIn { .. })));
    }

    #[test]
    fn a_rename_moves_the_store_and_keeps_the_id() {
        let dir = TempDir::new();
        let paths = paths(&dir);
        let made = PlaneFile::create(&paths, "old", &[]).unwrap();
        fs::create_dir_all(made.dir.join("forge")).unwrap();
        let renamed = PlaneFile::rename(&paths, "old", "new").unwrap();
        assert_eq!(renamed.id, made.id);
        assert!(!paths.plane_dir("old").exists());
        assert!(paths.plane_dir("new").join("forge").is_dir());
        assert_eq!(PlaneFile::with_id(&paths, &made.id).unwrap().unwrap().name, "new");
        renamed.delete().unwrap();
        assert!(!paths.plane_dir("new").exists());
        assert!(PlaneFile::list(&paths).unwrap().is_empty());
    }
}
