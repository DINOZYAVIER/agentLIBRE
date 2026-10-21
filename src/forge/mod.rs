//! The forge: the per-workspace store of typed artifacts (AGL-007).
//!
//! One store per project (AGL-010: the nearest registered ancestor of
//! the cwd), at `agentLIBRE/<project>/forge/<type>/` under
//! XDG_DATA_HOME: the project's `agentLIBRE.toml` (the list of the
//! identities the store was re-homed from, the last one the current
//! home, and the project settings) next to the `forge/` directory, and
//! one file per artifact (TOML front-matter + Markdown body). The
//! `forge/v1` envelope lives here; the `task-spec` type is in
//! [`task_spec`].

pub mod task_spec;

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use toml::{Table, Value};

use crate::error::Error;
use crate::settings::ServerFile;
use crate::tomlfile::quote as toml_string;
use crate::tomlfile::Section;

/// The name of the per-workspace marker file, in the data directory
/// next to `forge/`.
pub const MARKER_FILE: &str = "agentLIBRE.toml";

/// The schema of the artifact front-matter (the v1 envelope).
pub const SCHEMA: &str = "forge/v1";

/// The `sha256:<hex>` identity of a project: the sha256 of its
/// canonical directory path.
pub fn identity_of(project: &Path) -> String {
    let digest = sha256(project.as_os_str().as_bytes());
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

/// Checks that `value` is a `sha256:<64 lowercase hex>` identity.
pub fn is_identity(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

/// The sha256 of `data` (FIPS 180-4); the workspace identity and the
/// media of a session need it and the forge adds no dependencies.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut buf = data.to_vec();
    let len = (buf.len() as u64).wrapping_mul(8);
    buf.push(0x80);
    while buf.len() % 64 != 56 {
        buf.push(0);
    }
    buf.extend_from_slice(&len.to_be_bytes());
    for chunk in buf.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for t in 0..16 {
            w[t] = u32::from_be_bytes([
                chunk[4 * t],
                chunk[4 * t + 1],
                chunk[4 * t + 2],
                chunk[4 * t + 3],
            ]);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for t in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[t])
                .wrapping_add(w[t]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (a, b, c, d, e, f, g, hh) = (
                t1.wrapping_add(t2),
                a,
                b,
                c,
                d.wrapping_add(t1),
                e,
                f,
                g,
            );
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Runs `git <args>` in `cwd` and returns the trimmed stdout; a spawn
/// failure or a nonzero exit is an error.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Process::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|source| Error::GitSpawn { source })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(Error::GitFailed {
            args: args.join(" "),
            detail,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether `dir` is inside a git work tree (a repository or a
/// directory of one).
pub fn in_repository(dir: &Path) -> bool {
    // Outside a repository the check exits with a failure, in a bare
    // repository it answers false.
    matches!(git(dir, &["rev-parse", "--is-inside-work-tree"]), Ok(answer) if answer == "true")
}

/// The canonical git toplevel of `cwd`; an error outside a repository.
pub fn toplevel(cwd: &Path) -> Result<PathBuf, Error> {
    if !in_repository(cwd) {
        return Err(Error::NotInRepository { cwd: cwd.to_path_buf() });
    }
    let path = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    fs::canonicalize(&path).map_err(|source| Error::GitFailed {
        args: "rev-parse --show-toplevel".into(),
        detail: source.to_string(),
    })
}

/// The project's `agentLIBRE.toml` (AGL-010): the format and the list
/// of the identities the store carries, the last one the current home,
/// and the project settings.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Marker {
    pub hashes: Vec<String>,
    /// The project's Function (D56, moved here by AGL-010).
    pub function: Option<String>,
    /// The project's `[server]` (AGL-015); wins per key over the user
    /// file's.
    pub server: ServerFile,
}

impl Marker {
    /// Parses `text` (from `path`) strictly: `format = 1`, the
    /// `workspace_sha256` list of `sha256:<64-hex>` identities and the
    /// project settings.
    pub fn parse(path: &Path, text: &str) -> Result<Marker, Error> {
        let mut file = Section::new(path, crate::tomlfile::parse(path, text)?);
        file.check_keys(&["function", "format", "workspace_sha256", "server"])?;
        let format = file.required_integer("format")?;
        if format != 1 {
            return Err(Error::MarkerFormat {
                path: path.into(),
                format,
            });
        }
        let hashes = file.required_strings("workspace_sha256")?;
        for value in &hashes {
            if !is_identity(value) {
                return Err(Error::MarkerIdentity {
                    path: path.into(),
                    value: value.to_string(),
                });
            }
        }
        let function = file.string("function")?;
        let server = ServerFile::parse(file.section("server")?, path)?;
        Ok(Marker {
            hashes,
            function,
            server,
        })
    }

    /// Renders the marker the way it is written: the settings first.
    pub fn render(&self) -> String {
        let hashes = self
            .hashes
            .iter()
            .map(|hash| toml_string(hash))
            .collect::<Vec<_>>()
            .join(", ");
        let function = self
            .function
            .as_deref()
            .map(|function| format!("function = {}\n", toml_string(function)))
            .unwrap_or_default();
        format!(
            "{function}format = 1\nworkspace_sha256 = [{hashes}]\n{}",
            self.server.render()
        )
    }

    /// Writes the marker to `path`: a temp file in the same directory
    /// plus a rename.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        write_atomic(path, &self.render())
    }
}

/// A project store found under the data directory: its directory, the
/// identities its marker lists and its project settings.
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
    pub hashes: Vec<String>,
    pub function: Option<String>,
    pub server: ServerFile,
}

impl Store {
    /// The directory name: the label, stale after a move by design.
    pub fn name(&self) -> String {
        self.dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Every store under `data`: the subdirectories carrying a marker, by
/// name. A marker that does not parse is an error.
pub fn stores(data: &Path) -> Result<Vec<Store>, Error> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(data) else {
        return Ok(found);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let marker_path = dir.join(MARKER_FILE);
        let text = match fs::read_to_string(&marker_path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::Read { path: marker_path, source }),
        };
        let marker = Marker::parse(&marker_path, &text)?;
        found.push(Store {
            dir,
            hashes: marker.hashes,
            function: marker.function,
            server: marker.server,
        });
    }
    found.sort_by_key(Store::name);
    Ok(found)
}

/// The resolved project of the cwd: its directory and the store
/// directory under the data directory.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The canonical project directory.
    pub project: PathBuf,
    /// The `sha256:<hex>` identity of the project path.
    pub identity: String,
    /// The store directory name: `<basename>-<12 hex>` (the 64-hex
    /// identity as the name fallback when that name is held by a
    /// different store).
    pub key: String,
    /// The store directory under the data directory.
    pub dir: PathBuf,
    /// The project's Function from the store's `agentLIBRE.toml`; none
    /// for a store not yet written.
    pub function: Option<String>,
    /// The project's `[server]` from the same file (AGL-015); empty for
    /// a store not yet written.
    pub server: ServerFile,
    /// The first identity of the store's marker, the one the project had
    /// at its start; `None` for a project that is not registered. The
    /// name of the project's llama-server unit is built from it
    /// (AGL-015, D12).
    pub origin: Option<String>,
}

impl Workspace {
    /// The part of the unit name that names the project: the first 12 hex
    /// of [`Workspace::origin`].
    pub fn unit_key(&self) -> Option<String> {
        self.origin
            .as_deref()
            .and_then(|origin| origin.strip_prefix("sha256:"))
            .map(|hex| hex[..12].to_owned())
    }
}

/// The name of the store of `basename` and `identity`; the 64-hex
/// identity when the 12-hex name is held by a different store.
fn store_key(basename: &str, identity: &str, found: &[Store]) -> String {
    let hex = &identity["sha256:".len()..];
    let key = format!("{basename}-{}", &hex[..12]);
    if found
        .iter()
        .any(|store| store.name() == key && !store.hashes.iter().any(|h| h == identity))
    {
        return format!("{basename}-{hex}");
    }
    key
}

/// Makes the identity the last entry of `store`'s marker, renames the
/// store to `key` under `data`, and returns the notification line
/// naming the old and the new directory.
fn rehomed(
    data: &Path,
    store: &Store,
    key: &str,
    identity: &str,
    lang: crate::i18n::Lang,
) -> Result<(PathBuf, String), Error> {
    use crate::i18n::m;
    use crate::tr;
    let new = data.join(key);
    let mut hashes = store.hashes.clone();
    hashes.retain(|h| h != identity);
    hashes.push(identity.to_owned());
    let marker = Marker {
        hashes,
        function: store.function.clone(),
        server: store.server.clone(),
    };
    if store.dir != new {
        fs::rename(&store.dir, &new)
            .map_err(|source| Error::Write { path: new.clone(), source })?;
    }
    marker.write(&new.join(MARKER_FILE))?;
    Ok((
        new.clone(),
        tr!(lang, m::FORGE_REHOMED, old = store.dir.display(), new = new.display()),
    ))
}

/// The final component of `path` as text; empty for the root.
fn basename_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether `name` is a store of `basename`: exactly
/// `<basename>-<12 hex>` or `<basename>-<64 hex>`, the two forms of
/// [`store_key`].
fn is_store_name(name: &str, basename: &str) -> bool {
    let Some(hex) = name
        .strip_prefix(basename)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    matches!(hex.len(), 12 | 64) && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Resolves the project of `cwd` against the stores under `data`
/// (AGL-010): the nearest ancestor of `cwd` (itself included) whose
/// identity a store carries; else the nearest ancestor with a store
/// named exactly like it (a move or a rename); else `cwd`, registered
/// at its first write. Syncs the store when it moved or was renamed.
/// Returns the workspace and the lines to print before the command
/// output. `ask` is shown the store names plus a new store when the
/// choice is the human's; it returns the index of the choice.
pub fn resolve(
    data: &Path,
    cwd: &Path,
    lang: crate::i18n::Lang,
    auto_rehome: bool,
    ask: &dyn Fn(&[String]) -> Result<usize, Error>,
) -> Result<(Workspace, Vec<String>), Error> {
    let cwd = fs::canonicalize(cwd).map_err(|source| Error::Read {
        path: cwd.to_path_buf(),
        source,
    })?;
    let projects: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    resolve_among(data, &projects, lang, auto_rehome, ask)
}

/// The project for `forge rehome`: the nearest ancestor of `cwd` whose
/// identity a store carries, else `cwd` itself. It only looks: it never
/// asks, adopts or renames, because `rehome` is the explicit way to
/// do those, and it must work where the lookup of [`resolve`] cannot
/// choose (no terminal, `auto_rehome` off).
pub fn locate(data: &Path, cwd: &Path) -> Result<Workspace, Error> {
    let cwd = fs::canonicalize(cwd).map_err(|source| Error::Read {
        path: cwd.to_path_buf(),
        source,
    })?;
    let found = stores(data)?;
    for project in cwd.ancestors() {
        let identity = identity_of(project);
        let mut mine = found.iter().filter(|store| store.hashes.contains(&identity));
        if let Some(store) = mine.next() {
            let key = store_key(&basename_of(project), &identity, &found);
            return Ok(Workspace {
                project: project.to_path_buf(),
                identity,
                key,
                dir: store.dir.clone(),
                function: store.function.clone(),
                server: store.server.clone(),
                origin: store.hashes.first().cloned(),
            });
        }
    }
    let identity = identity_of(&cwd);
    let key = store_key(&basename_of(&cwd), &identity, &found);
    Ok(Workspace {
        dir: data.join(&key),
        project: cwd,
        identity,
        key,
        function: None,
        server: ServerFile::default(),
        origin: None,
    })
}

/// [`resolve`] for `agl init` (AGL-010): `dir` alone may be the
/// project, an ancestor is not looked at.
pub fn resolve_dir(
    data: &Path,
    dir: &Path,
    lang: crate::i18n::Lang,
    auto_rehome: bool,
    ask: &dyn Fn(&[String]) -> Result<usize, Error>,
) -> Result<(Workspace, Vec<String>), Error> {
    let dir = fs::canonicalize(dir).map_err(|source| Error::Read {
        path: dir.to_path_buf(),
        source,
    })?;
    resolve_among(data, &[dir], lang, auto_rehome, ask)
}

/// [`resolve`] over `projects`, nearest first: the directories that may
/// be the project. The first one is the project when nothing is found.
fn resolve_among(
    data: &Path,
    projects: &[PathBuf],
    lang: crate::i18n::Lang,
    auto_rehome: bool,
    ask: &dyn Fn(&[String]) -> Result<usize, Error>,
) -> Result<(Workspace, Vec<String>), Error> {
    use crate::i18n::m;
    use crate::tr;
    let found = stores(data)?;
    let workspace = |project: &Path,
                     identity: &str,
                     key: &str,
                     dir: PathBuf,
                     store: Option<&Store>| Workspace {
        project: project.to_path_buf(),
        identity: identity.to_owned(),
        key: key.to_owned(),
        dir,
        function: store.and_then(|store| store.function.clone()),
        server: store.map(|store| store.server.clone()).unwrap_or_default(),
        origin: store.and_then(|store| store.hashes.first().cloned()),
    };
    // A project not registered yet: its store is created at the first
    // write. A hint at the other stores covers the pure-rename case.
    let fresh = |project: &Path| {
        let identity = identity_of(project);
        let key = store_key(&basename_of(project), &identity, &found);
        let lines = if found.is_empty() {
            Vec::new()
        } else {
            let names = found
                .iter()
                .map(Store::name)
                .collect::<Vec<_>>()
                .join(", ");
            vec![tr!(
                lang,
                m::FORGE_FRESH_STORE,
                dir = data.join(&key).display(),
                names = names
            )]
        };
        (
            workspace(project, &identity, &key, data.join(&key), None),
            lines,
        )
    };
    // By identity over all the ancestors first: the nearest wins.
    for project in projects {
        let identity = identity_of(project);
        let mine: Vec<&Store> = found
            .iter()
            .filter(|store| store.hashes.contains(&identity))
            .collect();
        let key = store_key(&basename_of(project), &identity, &found);
        match mine.len() {
            0 => continue,
            // The identity is the marker's last entry and the name is
            // current: nothing to sync.
            1 if mine[0].name() == key && mine[0].hashes.last() == Some(&identity) => {
                let store = mine[0];
                return Ok((
                    workspace(project, &identity, &key, store.dir.clone(), Some(store)),
                    Vec::new(),
                ));
            }
            // A move or a rename, back or forth: the old directory
            // becomes the new one, the artifacts are untouched.
            1 => {
                let (dir, line) = rehomed(data, mine[0], &key, &identity, lang)?;
                return Ok((
                    workspace(project, &identity, &key, dir, Some(mine[0])),
                    vec![line],
                ));
            }
            // The identity is in two markers: a store copied by hand.
            _ => {
                return Err(Error::WorkspaceAmbiguous {
                    dirs: mine.iter().map(|store| store.name()).collect(),
                });
            }
        }
    }
    // Then by exact name over all of them: the nearest ancestor that
    // has a same-named store.
    for project in projects {
        let basename = basename_of(project);
        if basename.is_empty() {
            continue;
        }
        let candidates: Vec<&Store> = found
            .iter()
            .filter(|store| is_store_name(&store.name(), &basename))
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let identity = identity_of(project);
        let key = store_key(&basename, &identity, &found);
        let adopt = |store: &Store| {
            let (dir, _line) = rehomed(data, store, &key, &identity, lang)?;
            let line = tr!(
                lang,
                m::FORGE_ADOPTED,
                old = store.dir.display(),
                new = dir.display()
            );
            Ok((
                workspace(project, &identity, &key, dir, Some(store)),
                vec![line],
            ))
        };
        // The unique same-named store: adopted, notified.
        if candidates.len() == 1 && auto_rehome {
            return adopt(candidates[0]);
        }
        // Several candidates, or auto_rehome off: never a guess. The
        // human chooses a store or a new one.
        let mut options: Vec<String> = found.iter().map(Store::name).collect();
        options.push(tr!(lang, m::FORGE_NEW_STORE).to_string());
        let choice = ask(&options)?;
        return if choice + 1 == options.len() {
            Ok(fresh(project))
        } else {
            adopt(&found[choice])
        };
    }
    // Nothing registered: the cwd is the project, registered at the
    // first write.
    Ok(fresh(&projects[0]))
}

/// [`resolve`] for a caller that cannot ask (the in-turn `forge` tool):
/// the store-choice ask becomes the `RehomeNoStore` error pointing at
/// `forge rehome`.
pub fn resolve_store(
    data: &Path,
    cwd: &Path,
    lang: crate::i18n::Lang,
    auto_rehome: bool,
) -> Result<(Workspace, Vec<String>), Error> {
    resolve(data, cwd, lang, auto_rehome, &|options| {
        Err(Error::RehomeNoStore {
            stores: options[..options.len() - 1].to_vec(),
        })
    })
}

/// Creates the store directory and its single-hash marker when they
/// are missing: the workspace is materialized on its first write. A
/// directory pre-created without a marker is adopted. Returns the line
/// to print when a marker was written.
pub fn materialize(ws: &Workspace, lang: crate::i18n::Lang) -> Result<Vec<String>, Error> {
    use crate::i18n::m;
    use crate::tr;
    let marker_path = ws.dir.join(MARKER_FILE);
    if marker_path.is_file() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(&ws.dir)
        .map_err(|source| Error::Write { path: ws.dir.clone(), source })?;
    Marker {
        hashes: vec![ws.identity.clone()],
        function: None,
        server: ServerFile::default(),
    }
    .write(&marker_path)?;
    Ok(vec![tr!(lang, m::FORGE_MARKER_WRITTEN, path = marker_path.display())])
}

/// `forge rehome <store>`: make the current identity the last entry of
/// the named store's marker (old entries kept) and rename it to the
/// current key. Returns the notification line.
pub fn rehome_store(
    data: &Path,
    ws: &Workspace,
    store_name: &str,
    lang: crate::i18n::Lang,
) -> Result<String, Error> {
    let found = stores(data)?;
    let target = found
        .iter()
        .find(|store| store.name() == store_name)
        .ok_or_else(|| Error::StoreNotFound {
            name: store_name.to_string(),
            stores: found.iter().map(Store::name).collect(),
        })?;
    let (_, line) = rehomed(data, target, &ws.key, &ws.identity, lang)?;
    Ok(line)
}

/// `forge rehome --new`: a fresh single-hash store at the current key.
/// Returns the line naming it.
pub fn rehome_new(ws: &Workspace, lang: crate::i18n::Lang) -> Result<String, Error> {
    use crate::i18n::m;
    use crate::tr;
    fs::create_dir_all(&ws.dir)
        .map_err(|source| Error::Write { path: ws.dir.clone(), source })?;
    Marker {
        hashes: vec![ws.identity.clone()],
        function: None,
        server: ServerFile::default(),
    }
    .write(&ws.dir.join(MARKER_FILE))?;
    Ok(tr!(lang, m::FORGE_STORE_NEW, dir = ws.dir.display()))
}

/// Writes `text` to `path`: a temp file in the same directory plus a
/// rename, so a crash leaves no half-written file.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), Error> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.{}.{}.tmp", std::process::id(), n));
    fs::write(&tmp, text).map_err(|source| Error::Write {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| Error::Write { path: path.into(), source })?;
    Ok(())
}

/// A parsed artifact: the `forge/v1` envelope, the raw `[<type>]`
/// table, and the body, verbatim.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// The file's path.
    pub path: PathBuf,
    /// The `type` field.
    pub type_name: String,
    /// The `name` field.
    pub name: String,
    /// The `status` field (validated for the known types).
    pub status: String,
    /// The `created` date (`YYYY-MM-DD`).
    pub created: String,
    /// The `updated` date (`YYYY-MM-DD`).
    pub updated: String,
    pub deps: Vec<String>,
    pub supersedes: Vec<String>,
    /// `[verification] cmds`, when the table is present.
    pub cmds: Option<Vec<String>>,
    /// The `[<type>]` table, carried unparsed.
    pub type_table: Value,
    /// The body, verbatim.
    pub body: String,
}

/// The header of a store file for `list`: parsed (a known or an
/// unknown type), an unknown schema (never parsed), or unparseable.
#[derive(Debug)]
pub enum Header {
    Artifact(Box<Artifact>),
    UnknownSchema { schema: String },
    Broken { detail: String },
}

/// Splits `text` into the front-matter and the body: the text between
/// the opening and the closing `---` line, and the rest, verbatim.
fn split_front_matter(path: &Path, text: &str) -> Result<(String, String), Error> {
    let bad = || Error::ArtifactFrontMatter { path: path.into() };
    if !text.starts_with("---\n") {
        return Err(bad());
    }
    let rest = &text["---\n".len()..];
    let mut front = String::new();
    let mut body_offset = None;
    for line in rest.split_inclusive('\n') {
        if line == "---\n" {
            body_offset = Some(front.len() + line.len());
            break;
        }
        if line == "---" {
            body_offset = Some(front.len() + line.len());
            break;
        }
        front.push_str(line);
    }
    let Some(body_offset) = body_offset else {
        return Err(bad());
    };
    Ok((front, rest[body_offset..].to_string()))
}

/// Whether `text` is a run of ASCII digits.
fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Checks that `value` is a calendar date, `YYYY-MM-DD`.
pub fn check_date(key: &str, value: &str, path: &Path) -> Result<(), Error> {
    let ok = value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && digits(&value[..4])
        && digits(&value[5..7])
        && digits(&value[8..])
        && (1..=12).contains(&value[5..7].parse::<u8>().unwrap_or(0))
        && {
            let day = value[8..].parse::<u8>().unwrap_or(0);
            let month = value[5..7].parse::<u8>().unwrap_or(0);
            let leap = value[..4].parse::<u16>().map(|y| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)).unwrap_or(false);
            let last = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
                .get(month as usize - 1)
                .copied()
                .unwrap_or(0);
            (1..=last).contains(&day)
        };
    if ok {
        Ok(())
    } else {
        Err(Error::ArtifactDate {
            path: path.into(),
            key: key.to_string(),
            value: value.to_string(),
        })
    }
}

/// Parses and validates the front-matter of `path` strictly: the
/// `forge/v1` envelope, the `[<type>]` table carried unparsed (and
/// validated by the type when it is known), and the body.
pub fn parse_artifact(path: &Path, text: &str) -> Result<Artifact, Error> {
    let (front, body) = split_front_matter(path, text)?;
    let table = crate::tomlfile::parse(path, &front)?;
    let mut file = Section::new(path, table);
    let schema = file.required_string("schema")?;
    if schema != SCHEMA {
        return Err(Error::ArtifactUnknownSchema {
            path: path.into(),
            schema,
        });
    }
    let type_name = file.required_string("type")?;
    let name = file.required_string("name")?;
    let status = file.required_string("status")?;
    let created = file.required_string("created")?;
    check_date("created", &created, path)?;
    let updated = file.required_string("updated")?;
    check_date("updated", &updated, path)?;
    let deps = file.strings("deps")?.unwrap_or_default();
    let supersedes = file.strings("supersedes")?.unwrap_or_default();
    let cmds = match file.section("verification")? {
        None => None,
        Some(mut verification) => {
            verification.check_keys(&["cmds"])?;
            Some(verification.strings("cmds")?.unwrap_or_default())
        }
    };
    let type_table = file
        .section(&type_name)?
        .ok_or_else(|| Error::ArtifactTypeTableMissing {
            path: path.into(),
            type_name: type_name.clone(),
        })?
        .into_value();
    file.check_empty()?;
    let artifact = Artifact {
        path: path.into(),
        type_name,
        name,
        status,
        created,
        updated,
        deps,
        supersedes,
        cmds,
        type_table,
        body,
    };
    if artifact.type_name == task_spec::TYPE {
        task_spec::check(&artifact)?;
    }
    Ok(artifact)
}

/// Reads the header of the store file `path` for `list`: it does not
/// fail on an unknown schema or an unparseable file, it marks them.
pub fn read_header(path: &Path) -> Result<Header, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.into(),
        source,
    })?;
    let (front, _body) = match split_front_matter(path, &text) {
        Ok(split) => split,
        Err(error) => {
            return Ok(Header::Broken {
                detail: error.to_string(),
            })
        }
    };
    let table = match crate::tomlfile::parse(path, &front) {
        Ok(table) => table,
        Err(error) => {
            return Ok(Header::Broken {
                detail: error.to_string(),
            })
        }
    };
    let schema = table
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if schema != SCHEMA {
        return Ok(Header::UnknownSchema { schema });
    }
    match parse_artifact(path, &text) {
        Ok(artifact) => Ok(Header::Artifact(Box::new(artifact))),
        Err(error) => Ok(Header::Broken {
            detail: error.to_string(),
        }),
    }
}

/// A TOML key: bare when possible, quoted otherwise.
fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare {
        key.to_string()
    } else {
        toml_string(key)
    }
}

/// A TOML value that can be written on one line.
fn toml_inline(value: &Value) -> String {
    match value {
        Value::String(text) => toml_string(text),
        Value::Integer(n) => n.to_string(),
        Value::Float(f) => {
            if f.is_nan() {
                "nan".to_string()
            } else if f.is_infinite() {
                format!("{}inf", if f.is_sign_negative() { "-" } else { "" })
            } else {
                let text = f.to_string();
                if text.contains(['.', 'e']) {
                    text
                } else {
                    format!("{text}.0")
                }
            }
        }
        Value::Boolean(b) => b.to_string(),
        Value::Datetime(date) => date.to_string(),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(toml_inline)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Table(_) => unreachable!("tables are rendered as sections"),
    }
}

/// Renders the entries of `table`: the one-line values first (key
/// order), then each sub-table as a `[prefix.key]` section. `prefix`
/// is the table's own name, empty at the top level.
fn toml_table(out: &mut String, prefix: &str, table: &Table) {
    let mut subs = Vec::new();
    for (key, value) in table {
        match value {
            Value::Table(sub) => subs.push((key, sub)),
            value => out.push_str(&format!(
                "{} = {}\n",
                toml_key(key),
                toml_inline(value)
            )),
        }
    }
    for (key, sub) in subs {
        out.push('\n');
        let name = if prefix.is_empty() {
            toml_key(key)
        } else {
            format!("{prefix}.{}", toml_key(key))
        };
        out.push_str(&format!("[{name}]\n"));
        toml_table(out, &name, sub);
    }
}

/// Renders an artifact: the front-matter in the envelope key order,
/// `---`, and the body, the way the file is written.
pub fn render(artifact: &Artifact) -> String {
    let scalar = |front: &mut String, key: &str, value: &str| {
        front.push_str(&format!("{key} = {}\n", toml_string(value)));
    };
    let array = |front: &mut String, key: &str, items: &[String]| {
        let items = items
            .iter()
            .map(|item| toml_string(item))
            .collect::<Vec<_>>()
            .join(", ");
        front.push_str(&format!("{key} = [{items}]\n"));
    };
    let mut front = String::new();
    scalar(&mut front, "schema", SCHEMA);
    scalar(&mut front, "type", &artifact.type_name);
    scalar(&mut front, "name", &artifact.name);
    scalar(&mut front, "status", &artifact.status);
    scalar(&mut front, "created", &artifact.created);
    scalar(&mut front, "updated", &artifact.updated);
    array(&mut front, "deps", &artifact.deps);
    array(&mut front, "supersedes", &artifact.supersedes);
    let Value::Table(table) = &artifact.type_table else {
        unreachable!("a parsed artifact always carries a table")
    };
    let header = toml_key(&artifact.type_name);
    front.push_str(&format!("\n[{header}]\n"));
    toml_table(&mut front, &header, table);
    if let Some(cmds) = &artifact.cmds {
        front.push('\n');
        front.push_str("[verification]\n");
        array(&mut front, "cmds", cmds);
    }
    format!("---\n{front}---\n{}", artifact.body)
}

/// Reads and parses the artifact file `path`.
pub fn load_artifact(path: &Path) -> Result<Artifact, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.into(),
        source,
    })?;
    parse_artifact(path, &text)
}

/// Rewrites the artifact file: a temp file in the same directory plus
/// a rename, the body byte-identical; the type directory is created
/// when it is not there yet (the first write of a type).
pub fn write_artifact(artifact: &Artifact) -> Result<(), Error> {
    if let Some(parent) = artifact.path.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    write_atomic(&artifact.path, &render(artifact))
}

/// The artifact files of the store at `dir`: every `<type>/*.md`
/// under its `forge/`, by type then by name.
pub fn artifact_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let forge = dir.join("forge");
    let mut type_names = Vec::new();
    if let Ok(types) = fs::read_dir(&forge) {
        for entry in types.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(name) = path.file_name()
            {
                type_names.push(name.to_string_lossy().into_owned());
            }
        }
    }
    type_names.sort();
    let mut files = Vec::new();
    for type_name in type_names {
        let type_dir = forge.join(&type_name);
        let Ok(entries) = fs::read_dir(&type_dir) else {
            continue;
        };
        let mut named: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        named.sort();
        files.extend(named);
    }
    Ok(files)
}

/// Test helpers of the forge tree: one counter for all the modules, so
/// the temporary directories never collide between modules.
#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory removed when dropped.
    #[derive(Debug)]
    pub struct TempDir(pub std::path::PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("agentLIBRE-forge-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TempDir;
    use super::*;
    use crate::i18n::Lang;
    use std::sync::{Arc, Mutex};

    const EN: Lang = Lang::En;

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    /// A git repository at `root/name` with `count` commits.
    fn git_repo(root: &Path, name: &str, count: u32) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Process::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        for i in 0..count {
            fs::write(dir.join("f.txt"), format!("{i}\n")).unwrap();
            run(&["add", "f.txt"]);
            run(&["commit", "-q", "-m", &format!("commit {i}")]);
        }
        dir
    }

    /// A valid `sha256:<64-hex>` identity, distinct per `d`.
    fn identity(d: u8) -> String {
        format!("sha256:{:064x}", ((d as u128) + 1) << 120)
    }

    /// The 12 hex of the identity of `toplevel`.
    fn short(toplevel: &Path) -> String {
        identity_of(toplevel)["sha256:".len()..19].to_string()
    }

    /// Resolves with auto-rehome on; panics when a choice would be
    /// needed.
    fn auto(data: &Path, cwd: &Path) -> (Workspace, Vec<String>) {
        resolve(data, cwd, EN, true, &|options| {
            panic!("no store to choose: {options:?}")
        })
        .unwrap()
    }

    /// Like [`auto`], keeping the error.
    fn auto_err(data: &Path, cwd: &Path) -> Result<(Workspace, Vec<String>), Error> {
        resolve(data, cwd, EN, true, &|options| {
            panic!("no store to choose: {options:?}")
        })
    }

    /// Resolves and materializes the store of `repo`; the workspace.
    fn fresh_store(data: &Path, repo: &Path) -> Workspace {
        let (ws, lines) = auto(data, repo);
        assert!(lines.len() <= 1);
        materialize(&ws, EN).unwrap();
        ws
    }

    /// The marker at `dir/agentLIBRE.toml`.
    fn marker(dir: &Path) -> Marker {
        let path = dir.join(MARKER_FILE);
        let text = fs::read_to_string(&path).unwrap();
        Marker::parse(&path, &text).unwrap()
    }

    /// Moves `from` to `to` (a new parent, the same last component).
    fn move_repo(from: &Path, to: &Path) {
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(from, to).unwrap();
    }

    #[test]
    fn sha256_matches_sha256sum() {
        let hex = |digest: [u8; 32]| {
            digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        };
        // The FIPS 180-4 test vectors.
        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Message lengths at the padding boundaries, digested with
        // sha256sum.
        for (len, expect) in [
            (55, "f34d5a0f80c0cbf84c8c0b90218c22637abd199965249da736a20143c8c9c9d9"),
            (56, "83aa034bda83e458a0dc9cbce0d4e354716aa0ff770ed37ac0ed2b292052e4af"),
            (57, "2f599f9dd7fac80e26892a008adcf28c0c9aaa11cf3df4ceacd07d0f1aef4c76"),
            (63, "074f6e9ac301d5d1b6df6f1dfb8c6f89c187ea945d352ce6a29279a9c630680b"),
            (64, "9674d9e078535b7cec43284387a6ee39956188e735a85452b0050b55341cda56"),
            (65, "52774b57c10e45040a61c14d35c1c8ebefe880082313aa0a21ebb077734cd067"),
        ] {
            let message = "0123456789"
                .repeat(len / 10 + 1)
                .chars()
                .take(len)
                .collect::<String>();
            assert_eq!(hex(sha256(message.as_bytes())), expect);
        }
        // A million characters: many blocks.
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(sha256(&million)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn the_identity_of_a_known_toplevel() {
        // `printf '%s' /home/dinozyavier/repos/agl-lite | sha256sum`.
        assert_eq!(
            identity_of(Path::new("/home/dinozyavier/repos/agl-lite")),
            "sha256:60af300abea6735a002395903b4dd0b3fe2c20e9ab20c8b96a9690adb39f883c"
        );
    }

    #[test]
    fn the_marker_round_trips_and_is_strict() {
        let path = Path::new("/d/agentLIBRE.toml");
        let marker = Marker {
            hashes: vec![identity(1), identity(2)],
            function: Some("coder".into()),
            server: ServerFile {
                url: Some("http://127.0.0.1:18081".into()),
                model: Some("q.gguf".into()),
                context: Some(65536),
                args: Some(vec!["-ngl".into(), "999".into()]),
            },
        };
        assert_eq!(Marker::parse(path, &marker.render()).unwrap(), marker);
        // The table comes after the top-level keys, so the file parses.
        assert!(marker.render().contains("workspace_sha256 = ["));
        assert!(marker.render().find("[server]") > marker.render().find("workspace_sha256"));
        // A marker without it has an empty one, and renders none.
        let plain = Marker {
            server: ServerFile::default(),
            ..marker
        };
        assert!(!plain.render().contains("[server]"));
        assert_eq!(Marker::parse(path, &plain.render()).unwrap(), plain);
        assert!(matches!(
            Marker::parse(
                path,
                "format = 1\nworkspace_sha256 = []\n[server]\nport = 1\n"
            ),
            Err(Error::UnknownKey { key, .. }) if key == "server.port"
        ));
        assert!(matches!(
            Marker::parse(path, "format = 2\nworkspace_sha256 = []\n"),
            Err(Error::MarkerFormat { format: 2, .. })
        ));
        assert!(matches!(
            Marker::parse(path, "format = 1\nworkspace_sha256 = [\"sha256:xyz\"]\n"),
            Err(Error::MarkerIdentity { .. })
        ));
        assert!(matches!(
            Marker::parse(path, "format = 1\nworkspace_sha256 = []\nnote = \"x\"\n"),
            Err(Error::UnknownKey { .. })
        ));
        assert!(matches!(
            Marker::parse(path, "format = 1\n"),
            Err(Error::MissingKey { key, .. }) if key == "workspace_sha256"
        ));
    }

    #[test]
    fn stores_walks_only_the_marked_directories() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        // Unmarked trees are not stores (the old product's
        // forge/agentLIBRE/ shape).
        fs::create_dir_all(data.join("forge/agentLIBRE")).unwrap();
        fs::create_dir_all(data.join("plain")).unwrap();
        let store = data.join("ws-0123456789ab");
        fs::create_dir_all(&store).unwrap();
        Marker { hashes: vec![identity(1)], function: None, server: ServerFile::default() }
            .write(&store.join(MARKER_FILE))
            .unwrap();
        let found = stores(&data).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir, store);
        assert_eq!(found[0].hashes, vec![identity(1)]);
        // A marker that does not parse is an error.
        fs::create_dir_all(data.join("broken")).unwrap();
        fs::write(data.join("broken/agentLIBRE.toml"), "format = one\n").unwrap();
        assert!(matches!(stores(&data), Err(Error::Parse { .. })));
    }

    #[test]
    fn a_fresh_store_is_created_lazily() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let (ws, lines) = auto(&data, &repo);
        assert!(lines.is_empty());
        assert_eq!(ws.project, fs::canonicalize(&repo).unwrap());
        assert_eq!(ws.identity, identity_of(&repo));
        assert_eq!(ws.key, format!("proj-{}", short(&repo)));
        assert_eq!(ws.dir, data.join(&ws.key));
        assert!(!ws.dir.exists());
        // Materialize: the directory and a single-hash marker.
        let lines = materialize(&ws, EN).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(marker(&ws.dir).hashes, vec![ws.identity.clone()]);
        // A second materialize changes nothing.
        assert!(materialize(&ws, EN).unwrap().is_empty());
        // The same workspace resolves again, quietly.
        let (again, lines) = auto(&data, &repo);
        assert!(lines.is_empty());
        assert_eq!(again.dir, ws.dir);
    }

    #[test]
    fn a_pre_created_store_directory_is_adopted() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let (ws, _) = auto(&data, &repo);
        fs::create_dir_all(&ws.dir).unwrap();
        let lines = materialize(&ws, EN).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(marker(&ws.dir).hashes, vec![ws.identity.clone()]);
    }

    #[test]
    fn a_stale_store_name_is_synced() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let ws = fresh_store(&data, &repo);
        let stale = data.join("proj-old-label");
        fs::rename(&ws.dir, &stale).unwrap();
        let (ws, lines) = auto(&data, &repo);
        assert_eq!(lines.len(), 1);
        assert_eq!(ws.dir, data.join(format!("proj-{}", short(&repo))));
        assert!(!stale.exists());
        assert_eq!(marker(&ws.dir).hashes, vec![ws.identity.clone()]);
    }

    #[test]
    fn a_moved_checkout_is_adopted_and_the_round_trip_is_quiet() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = git_repo(dir.path(), "a/proj", 1);
        let ws = fresh_store(&data, &from);
        // An artifact in the store.
        let artifact = ws.dir.join("forge/task-spec/agl-001-x.md");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "body").unwrap();
        let old_identity = identity_of(&from);
        let old_dir = ws.dir.clone();
        // Move the checkout: the same basename, a new path.
        let to = dir.path().join("b/proj");
        move_repo(&from, &to);
        let (ws, lines) = auto(&data, &to);
        assert_eq!(lines.len(), 1);
        let new_identity = identity_of(&to);
        assert_ne!(old_identity, new_identity);
        assert_eq!(ws.dir, data.join(format!("proj-{}", short(&to))));
        // The marker keeps the old home and names the new one last.
        assert_eq!(
            marker(&ws.dir).hashes,
            vec![old_identity.clone(), new_identity.clone()]
        );
        assert!(!old_dir.exists());
        // The artifact is untouched.
        assert_eq!(
            fs::read_to_string(ws.dir.join("forge/task-spec/agl-001-x.md")).unwrap(),
            "body"
        );
        // Back to the original path: no prompt, the old identity is the
        // marker's last entry again.
        let back = dir.path().join("a/proj");
        move_repo(&to, &back);
        let (ws, lines) = auto(&data, &back);
        assert_eq!(lines.len(), 1);
        assert_eq!(ws.dir, data.join(format!("proj-{}", short(&back))));
        assert_eq!(marker(&ws.dir).hashes, vec![new_identity, old_identity]);
    }

    #[test]
    fn auto_rehome_off_offers_the_stores() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = git_repo(dir.path(), "a/proj", 1);
        let ws = fresh_store(&data, &from);
        let old_name = ws.dir.file_name().unwrap().to_string_lossy().into_owned();
        let to = dir.path().join("b/proj");
        move_repo(&from, &to);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let (ws, lines) = resolve(&data, &to, EN, false, &|options| {
            recorded.lock().unwrap().push(options.to_vec());
            Ok(0)
        })
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], vec![old_name, "a new store".to_string()]);
        assert_eq!(lines.len(), 1);
        assert!(marker(&ws.dir).hashes.last() == Some(&identity_of(&to)));
    }

    #[test]
    fn the_offer_can_choose_a_new_store() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = git_repo(dir.path(), "a/proj", 1);
        let ws = fresh_store(&data, &from);
        let taken = ws.dir.clone();
        let to = dir.path().join("b/proj");
        move_repo(&from, &to);
        let (ws, lines) = resolve(&data, &to, EN, false, &|options| Ok(options.len() - 1))
            .unwrap();
        // The fresh store takes the current key; the old store is
        // untouched, and the hint names it.
        assert_eq!(ws.dir, data.join(format!("proj-{}", short(&to))));
        assert_ne!(ws.dir, taken);
        assert!(taken.exists());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&taken.file_name().unwrap().to_string_lossy().into_owned()));
    }

    #[test]
    fn the_ask_error_propagates() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = git_repo(dir.path(), "a/proj", 1);
        fresh_store(&data, &from);
        let to = dir.path().join("b/proj");
        move_repo(&from, &to);
        let error = resolve(&data, &to, EN, false, &|_| {
            Err(Error::RehomeNoStore { stores: vec![] })
        })
        .unwrap_err();
        assert!(matches!(error, Error::RehomeNoStore { .. }));
    }

    #[test]
    fn several_same_named_stores_are_never_guessed() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let first = git_repo(dir.path(), "a/proj", 1);
        let ws1 = fresh_store(&data, &first);
        // The second same-named store, chosen over the offer.
        let second = git_repo(dir.path(), "b/proj", 1);
        let (ws2, _) = resolve(&data, &second, EN, false, &|options| Ok(options.len() - 1))
            .unwrap();
        materialize(&ws2, EN).unwrap();
        // A third checkout, the same basename, a new identity.
        let third = git_repo(dir.path(), "c/proj", 1);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let (ws, _) = resolve(&data, &third, EN, true, &|options| {
            recorded.lock().unwrap().push(options.to_vec());
            Ok(0)
        })
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        // The two stores by name, then the new store.
        let mut names = vec![
            ws1.dir.file_name().unwrap().to_string_lossy().into_owned(),
            ws2.dir.file_name().unwrap().to_string_lossy().into_owned(),
        ];
        names.sort();
        names.push("a new store".to_string());
        assert_eq!(seen[0], names);
        // The first store in name order was adopted and re-homed to
        // the current key; the other store is untouched.
        let mut dirs = [ws1.dir.clone(), ws2.dir.clone()];
        dirs.sort();
        assert_eq!(ws.dir, data.join(format!("proj-{}", short(&third))));
        assert!(!dirs[0].exists());
        assert!(dirs[1].exists());
        assert!(marker(&ws.dir).hashes.last() == Some(&identity_of(&third)));
    }

    #[test]
    fn a_fresh_name_hints_the_other_stores() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let first = git_repo(dir.path(), "a/proj", 1);
        let ws1 = fresh_store(&data, &first);
        let other = git_repo(dir.path(), "c/other", 1);
        let (ws, lines) = auto(&data, &other);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&ws1.dir.file_name().unwrap().to_string_lossy().into_owned()));
        assert_eq!(ws.dir, data.join(format!("other-{}", short(&other))));
        assert!(!ws.dir.exists());
    }

    #[test]
    fn an_identity_in_two_markers_is_an_error() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let ws = fresh_store(&data, &repo);
        // A store copied by hand.
        let copy = data.join("proj-copy");
        fs::create_dir_all(&copy).unwrap();
        fs::write(
            copy.join(MARKER_FILE),
            fs::read_to_string(ws.dir.join(MARKER_FILE)).unwrap(),
        )
        .unwrap();
        let error = auto_err(&data, &repo).unwrap_err();
        assert!(matches!(
            error,
            Error::WorkspaceAmbiguous { dirs } if dirs.len() == 2
        ));
    }

    #[test]
    fn rehome_moves_the_identity_last_and_renames() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let first = git_repo(dir.path(), "a/first", 1);
        let ws1 = fresh_store(&data, &first);
        let second = git_repo(dir.path(), "b/second", 1);
        let (ws2, _) = auto(&data, &second);
        // The store is named: no offer.
        let name = ws1.dir.file_name().unwrap().to_string_lossy().into_owned();
        let line = rehome_store(&data, &ws2, &name, EN).unwrap();
        assert!(!line.is_empty());
        assert_eq!(ws2.dir, data.join(format!("second-{}", short(&second))));
        assert!(!ws1.dir.exists());
        let hashes = marker(&ws2.dir).hashes;
        assert_eq!(hashes, vec![identity_of(&first), identity_of(&second)]);
        // The re-homed store resolves again, quietly.
        let (again, lines) = auto(&data, &second);
        assert!(lines.is_empty());
        assert_eq!(again.dir, ws2.dir);
    }

    #[test]
    fn rehome_names_a_missing_store() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let (ws, _) = auto(&data, &repo);
        let error = rehome_store(&data, &ws, "nope", EN).unwrap_err();
        assert!(matches!(
            error,
            Error::StoreNotFound { name, .. } if name == "nope"
        ));
    }

    #[test]
    fn rehome_new_creates_a_fresh_single_hash_store() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let other = git_repo(dir.path(), "a/proj", 1);
        fresh_store(&data, &other);
        let repo = git_repo(dir.path(), "b/other", 1);
        let (ws, _) = auto(&data, &repo);
        let line = rehome_new(&ws, EN).unwrap();
        assert!(!line.is_empty());
        assert_eq!(marker(&ws.dir).hashes, vec![ws.identity.clone()]);
        // The other store is untouched.
        assert_eq!(stores(&data).unwrap().len(), 2);
    }

    #[test]
    fn the_store_name_falls_back_to_the_full_identity() {
        let held = Store {
            dir: PathBuf::from("/d/proj-0123456789ab"),
            hashes: vec![identity(2)],
            function: None,
            server: ServerFile::default(),
        };
        let mine = format!("sha256:0123456789abcdef{}", "f".repeat(52));
        // No collision: the 12-hex name.
        assert_eq!(
            store_key("proj", &identity(1), &[]),
            format!("proj-{}", &identity(1)["sha256:".len()..19])
        );
        // A store holds the 12-hex name without this identity: the full
        // 64-hex name.
        assert_eq!(
            store_key("proj", &mine, std::slice::from_ref(&held)),
            format!("proj-{}", &mine["sha256:".len()..]),
        );
        // A store holds the name and the identity: the 12-hex name stays.
        let mine_store = Store {
            dir: PathBuf::from("/d/proj-0123456789ab"),
            hashes: vec![mine.to_string()],
            function: None,
            server: ServerFile::default(),
        };
        assert_eq!(store_key("proj", &mine, &[mine_store]), "proj-0123456789ab");
    }

    #[test]
    fn a_taken_store_name_uses_the_full_identity() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let mine = identity_of(&repo);
        let taken = data.join(format!("proj-{}", &mine["sha256:".len()..19]));
        fs::create_dir_all(&taken).unwrap();
        Marker { hashes: vec![identity(7)], function: None, server: ServerFile::default() }
            .write(&taken.join(MARKER_FILE))
            .unwrap();
        let (ws, _) = resolve(&data, &repo, EN, false, &|options| Ok(options.len() - 1)).unwrap();
        assert_eq!(ws.dir, data.join(format!("proj-{}", &mine["sha256:".len()..])));
        assert_ne!(ws.dir, taken);
    }

    /// A plain directory (no git) `rel` under `root`, canonical.
    fn plain_dir(root: &Path, rel: &str) -> PathBuf {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn a_directory_outside_git_is_a_project() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let plain = plain_dir(dir.path(), "plain");
        let ws = fresh_store(&data, &plain);
        assert_eq!(ws.project, plain);
        assert!(!in_repository(&plain));
        assert_eq!(marker(&ws.dir).hashes, vec![identity_of(&plain)]);
    }

    #[test]
    fn the_nearest_registered_ancestor_is_the_project() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let root = plain_dir(dir.path(), "root");
        let outer = fresh_store(&data, &root);
        let deep = plain_dir(dir.path(), "root/src/deep");
        // Found from a nested cwd, quietly; nothing is registered for
        // the cwd.
        let (ws, lines) = auto(&data, &deep);
        assert!(lines.is_empty());
        assert_eq!(ws.dir, outer.dir);
        assert_eq!(ws.project, root);
        assert_eq!(stores(&data).unwrap().len(), 1);
        // A nearer project wins.
        let inner_dir = plain_dir(dir.path(), "root/src");
        let (inner, _) = resolve_dir(&data, &inner_dir, EN, true, &|options| {
            panic!("no store to choose: {options:?}")
        })
        .unwrap();
        materialize(&inner, EN).unwrap();
        assert_ne!(inner.dir, outer.dir);
        let (ws, _) = auto(&data, &deep);
        assert_eq!(ws.dir, inner.dir);
        assert_eq!(ws.project, inner_dir);
        let (ws, _) = auto(&data, &root);
        assert_eq!(ws.dir, outer.dir);
    }

    #[test]
    fn an_unregistered_cwd_is_the_project_even_inside_a_repository() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let repo = git_repo(dir.path(), "proj", 1);
        let sub = plain_dir(dir.path(), "proj/src");
        let (ws, lines) = auto(&data, &sub);
        assert!(lines.is_empty());
        assert_eq!(ws.project, sub);
        assert_eq!(ws.key, format!("src-{}", short(&sub)));
        assert!(!ws.dir.exists());
        // The git toplevel is not the project; git is still readable
        // from the project directory.
        assert_ne!(ws.project, fs::canonicalize(&repo).unwrap());
        assert!(in_repository(&ws.project));
    }

    #[test]
    fn a_registered_parent_wins_over_a_same_named_child() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let parent = plain_dir(dir.path(), "a/proj");
        let ws = fresh_store(&data, &parent);
        // The child has the parent's basename, so the parent's store
        // is also a same-named candidate for it: the identity of the
        // parent is looked up first.
        let child = plain_dir(dir.path(), "a/proj/inner/proj");
        let (found, lines) = resolve(&data, &child, EN, false, &|options| {
            panic!("no store to choose: {options:?}")
        })
        .unwrap();
        assert!(lines.is_empty());
        assert_eq!(found.dir, ws.dir);
        assert_eq!(found.project, parent);
    }

    #[test]
    fn an_ancestor_is_adopted_by_exact_name_only() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = plain_dir(dir.path(), "x/proj");
        let ws = fresh_store(&data, &from);
        // The project moved; the cwd is below its new home.
        let to = dir.path().join("y/proj");
        move_repo(&from, &to);
        let to = fs::canonicalize(&to).unwrap();
        let cwd = plain_dir(dir.path(), "y/proj/src");
        let (found, lines) = auto(&data, &cwd);
        assert_eq!(lines.len(), 1);
        assert_eq!(found.project, to);
        assert_eq!(found.dir, data.join(format!("proj-{}", short(&to))));
        assert!(!ws.dir.exists());
        assert_eq!(marker(&found.dir).hashes, vec![identity_of(&from), identity_of(&to)]);
        // A longer name that merely starts with the basename is not a
        // candidate.
        let other = plain_dir(dir.path(), "z/pro");
        let (fresh, _) = auto(&data, &other);
        assert_eq!(fresh.dir, data.join(format!("pro-{}", short(&other))));
        assert!(!fresh.dir.exists());
    }

    #[test]
    fn the_project_function_travels_with_the_store() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = plain_dir(dir.path(), "a/proj");
        let ws = fresh_store(&data, &from);
        assert_eq!(ws.function, None);
        // The human sets the Function in the store's file.
        Marker {
            hashes: vec![identity_of(&from)],
            function: Some("coder".into()),
            server: ServerFile::default(),
        }
        .write(&ws.dir.join(MARKER_FILE))
        .unwrap();
        let (found, _) = auto(&data, &from);
        assert_eq!(found.function.as_deref(), Some("coder"));
        assert!(
            fs::read_to_string(found.dir.join(MARKER_FILE))
                .unwrap()
                .starts_with("function = \"coder\"\n")
        );
        // A move keeps it.
        let to = dir.path().join("b/proj");
        move_repo(&from, &to);
        let (moved, lines) = auto(&data, &to);
        assert_eq!(lines.len(), 1);
        assert_eq!(moved.function.as_deref(), Some("coder"));
        assert_eq!(marker(&moved.dir).function.as_deref(), Some("coder"));
    }

    #[test]
    fn the_project_server_and_its_unit_key_travel_with_the_store() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let from = plain_dir(dir.path(), "a/proj");
        let ws = fresh_store(&data, &from);
        // Not registered yet: no settings, no unit key.
        assert_eq!(ws.server, ServerFile::default());
        assert_eq!(ws.origin, None);
        assert_eq!(ws.unit_key(), None);
        let server = ServerFile {
            url: Some("http://127.0.0.1:18081".into()),
            context: Some(65536),
            ..ServerFile::default()
        };
        let origin = identity_of(&from);
        Marker {
            hashes: vec![origin.clone()],
            function: None,
            server: server.clone(),
        }
        .write(&ws.dir.join(MARKER_FILE))
        .unwrap();
        let (found, _) = auto(&data, &from);
        assert_eq!(found.server, server);
        assert_eq!(found.origin.as_deref(), Some(origin.as_str()));
        let key = found.unit_key().unwrap();
        assert_eq!(key, origin["sha256:".len()..][..12]);
        // A move renames the store and appends the new identity, but keeps
        // the settings and the first identity, so the unit name stays.
        let to = dir.path().join("b/proj");
        move_repo(&from, &to);
        let (moved, lines) = auto(&data, &to);
        assert_eq!(lines.len(), 1);
        assert_eq!(moved.server, server);
        assert_eq!(marker(&moved.dir).server, server);
        assert_eq!(moved.origin.as_deref(), Some(origin.as_str()));
        assert_eq!(moved.unit_key(), Some(key));
        // `locate` (for `forge rehome`) reads the same.
        let located = locate(&data, &to).unwrap();
        assert_eq!(located.server, server);
        assert_eq!(located.origin.as_deref(), Some(origin.as_str()));
    }

    #[test]
    fn resolve_dir_looks_at_the_directory_alone() {
        let dir = TempDir::new();
        let data = dir.path().join("data");
        let root = plain_dir(dir.path(), "root");
        let outer = fresh_store(&data, &root);
        let sub = plain_dir(dir.path(), "root/sub");
        let (ws, lines) = resolve_dir(&data, &sub, EN, true, &|options| {
            panic!("no store to choose: {options:?}")
        })
        .unwrap();
        assert!(lines.len() <= 1);
        assert_eq!(ws.project, sub);
        assert_ne!(ws.dir, outer.dir);
        // Registered, it is found from below as the nearer project.
        materialize(&ws, EN).unwrap();
        let (below, _) = auto(&data, &plain_dir(dir.path(), "root/sub/deeper"));
        assert_eq!(below.dir, ws.dir);
    }

    fn task_spec_text() -> &'static str {
        r#"---
schema = "forge/v1"
type = "task-spec"
name = "agl-001-x"
status = "open"
created = "2026-10-15"
updated = "2026-10-15"
deps = ["agl-000-a"]
supersedes = []

[task-spec]
anchor = ""
last = ""

[verification]
cmds = ["true"]
---
# AGL-001

body line
"#
    }

    #[test]
    fn the_envelope_round_trips() {
        let path = Path::new("/s/agl-001-x.md");
        let text = task_spec_text();
        let artifact = parse_artifact(path, text).unwrap();
        assert_eq!(artifact.type_name, "task-spec");
        assert_eq!(artifact.name, "agl-001-x");
        assert_eq!(artifact.status, "open");
        assert_eq!(artifact.created, "2026-10-15");
        assert_eq!(artifact.updated, "2026-10-15");
        assert_eq!(artifact.deps, vec!["agl-000-a".to_string()]);
        assert!(artifact.supersedes.is_empty());
        assert_eq!(artifact.cmds, Some(vec!["true".to_string()]));
        let Value::Table(table) = &artifact.type_table else {
            panic!("the type table is carried")
        };
        assert_eq!(table.get("anchor").and_then(Value::as_str), Some(""));
        assert_eq!(table.get("last").and_then(Value::as_str), Some(""));
        // The body, verbatim.
        assert_eq!(artifact.body, "# AGL-001\n\nbody line\n");
        // Render and parse again: the file is byte-identical.
        assert_eq!(render(&artifact), text);
        assert_eq!(
            parse_artifact(path, &render(&artifact)).unwrap().body,
            artifact.body
        );
    }

    #[test]
    fn the_envelope_is_strict() {
        let path = Path::new("/s/agl-001-x.md");
        // An unknown key at the top level.
        let text = task_spec_text().replace(
            "schema = \"forge/v1\"\n",
            "schema = \"forge/v1\"\nextra = 1\n",
        );
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::ArtifactKey { key, .. }) if key == "extra"
        ));
        // A missing required key.
        let text = task_spec_text().replace("status = \"open\"\n", "");
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::MissingKey { key, .. }) if key == "status"
        ));
        // A missing type table.
        let text = task_spec_text().replace("[task-spec]\nanchor = \"\"\nlast = \"\"\n\n", "");
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::ArtifactTypeTableMissing { type_name, .. })
                if type_name == "task-spec"
        ));
        // An unknown key in the type table (the task-spec check).
        let text = task_spec_text().replace("[task-spec]\n", "[task-spec]\nextra = 1\n");
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::UnknownKey { key, .. }) if key == "task-spec.extra"
        ));
        // A wrong type for a key.
        let text = task_spec_text().replace("deps = [\"agl-000-a\"]", "deps = 1");
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::WrongType { key, .. }) if key == "deps"
        ));
        // A state outside the lifecycle (the task-spec check).
        let text = task_spec_text().replace("status = \"open\"", "status = \"draft\"");
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::BadState { state, .. }) if state == "draft"
        ));
        // The dates are calendar dates.
        for (value, ok) in [
            ("2026-10-15", true),
            ("2024-02-29", true),
            ("2026-02-29", false),
            ("2026-02-30", false),
            ("2026-13-01", false),
            ("2026-1-15", false),
            ("20261015", false),
        ] {
            let text = task_spec_text().replace("created = \"2026-10-15\"", &format!("created = \"{value}\""));
            let broken = parse_artifact(path, &text).is_err();
            assert_eq!(broken, !ok, "created = {value}");
        }
    }

    #[test]
    fn an_unknown_type_is_carried_unparsed() {
        let path = Path::new("/s/note-1.md");
        let text = r#"---
schema = "forge/v1"
type = "note"
name = "note-1"
status = "whatever"
created = "2026-10-15"
updated = "2026-10-15"
deps = []
supersedes = []

[note]
mood = "fine"
n = 3

[verification]
cmds = []
---
a note
"#;
        let artifact = parse_artifact(path, text).unwrap();
        assert_eq!(artifact.type_name, "note");
        assert_eq!(artifact.status, "whatever");
        let Value::Table(table) = &artifact.type_table else {
            panic!("the type table is carried")
        };
        assert_eq!(table.get("mood").and_then(Value::as_str), Some("fine"));
        assert_eq!(table.get("n").and_then(Value::as_integer), Some(3));
        // And the round trip.
        assert_eq!(parse_artifact(path, &render(&artifact)).unwrap().body, "a note\n");
    }

    #[test]
    fn an_unknown_schema_is_declined() {
        let path = Path::new("/s/old.md");
        let text = task_spec_text().replace("schema = \"forge/v1\"", "schema = \"forge/v0\"");
        assert!(matches!(
            parse_artifact(path, &text),
            Err(Error::ArtifactUnknownSchema { schema, .. }) if schema == "forge/v0"
        ));
        // `list` marks it instead of failing.
        let dir = TempDir::new();
        let file = dir.path().join("old.md");
        fs::write(&file, &text).unwrap();
        match read_header(&file).unwrap() {
            Header::UnknownSchema { schema, .. } => assert_eq!(schema, "forge/v0"),
            other => panic!("expected an unknown schema, got {other:?}"),
        }
    }

    #[test]
    fn list_marks_the_files_it_cannot_parse() {
        let dir = TempDir::new();
        let no_matter = dir.path().join("no-matter.md");
        fs::write(&no_matter, "no front matter at all\n").unwrap();
        match read_header(&no_matter).unwrap() {
            Header::Broken { .. } => {}
            other => panic!("expected a broken header, got {other:?}"),
        }
        let bad_toml = dir.path().join("bad-toml.md");
        fs::write(&bad_toml, "---\nschema = forge/v1\n---\nbody\n").unwrap();
        match read_header(&bad_toml).unwrap() {
            Header::Broken { .. } => {}
            other => panic!("expected a broken header, got {other:?}"),
        }
    }

    #[test]
    fn a_status_change_keeps_the_body_byte_identical() {
        let dir = TempDir::new();
        let path = dir.path().join("agl-001-x.md");
        fs::write(&path, task_spec_text()).unwrap();
        let mut artifact = load_artifact(&path).unwrap();
        let body = artifact.body.clone();
        artifact.status = "in-progress".into();
        artifact.updated = "2026-10-16".into();
        write_artifact(&artifact).unwrap();
        let reread = load_artifact(&path).unwrap();
        assert_eq!(reread.status, "in-progress");
        assert_eq!(reread.updated, "2026-10-16");
        assert_eq!(reread.body, body);
        // The file text outside the two lines is untouched.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("status = \"in-progress\""));
        assert!(text.contains("updated = \"2026-10-16\""));
        assert!(!text.contains("2026-10-15\nupdated"));
    }

    #[test]
    fn the_store_walk_lists_the_artifacts_by_type_then_name() {
        let dir = TempDir::new();
        let store = dir.path().join("store");
        fs::create_dir_all(store.join("forge/note")).unwrap();
        fs::create_dir_all(store.join("forge/task-spec")).unwrap();
        // Not an artifact: no .md.
        fs::write(store.join("forge/task-spec/scratch.txt"), "x").unwrap();
        fs::write(store.join("forge/note/b.md"), "b").unwrap();
        fs::write(store.join("forge/note/a.md"), "a").unwrap();
        fs::write(store.join("forge/task-spec/agl-002-b.md"), "b").unwrap();
        fs::write(store.join("forge/task-spec/agl-001-a.md"), "a").unwrap();
        // A type directory with no files is fine.
        fs::create_dir_all(store.join("forge/empty")).unwrap();
        let files = artifact_files(&store).unwrap();
        assert_eq!(
            files,
            vec![
                store.join("forge/note/a.md"),
                store.join("forge/note/b.md"),
                store.join("forge/task-spec/agl-001-a.md"),
                store.join("forge/task-spec/agl-002-b.md"),
            ]
        );
        // A store without forge/ is empty.
        let none = dir.path().join("none");
        fs::create_dir_all(&none).unwrap();
        assert!(artifact_files(&none).unwrap().is_empty());
    }

    #[test]
    fn write_atomic_leaves_no_temp_files() {
        let dir = TempDir::new();
        let path = dir.path().join("x.txt");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        for entry in fs::read_dir(dir.path()).unwrap() {
            assert_eq!(entry.unwrap().file_name().to_string_lossy().as_ref(), "x.txt");
        }
    }
}
