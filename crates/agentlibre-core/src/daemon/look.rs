//! What a client asks of the stores (AGL-126): the past sessions, the planes
//! of a directory and the Functions of a run, which the REPL read off the
//! disk itself.

use super::*;

use agentlibre_common::settings::ProjectFile;

use crate::function::catalog::Catalog;
use crate::plane::PlaneFile;

impl Daemon {
    /// The reply to a request of the stores.
    pub(super) fn look(&self, request: Request, lang: Lang) -> Result<Reply, Error> {
        match request {
            Request::Past { dir, plane, limit } => self.past(dir, plane, limit, lang),
            Request::Planes { dir, name } => self.planes(&dir, name),
            Request::Functions { dir, plane } => Ok(Reply::Functions {
                functions: self
                    .catalog(&dir, plane.as_deref(), lang)?
                    .entries(lang)
                    .into_iter()
                    .map(wire::function_entry)
                    .collect(),
            }),
            Request::Function { dir, plane, verb } => {
                let catalog = self.catalog(&dir, plane.as_deref(), lang)?;
                function(&catalog, verb).map(Reply::Function)
            }
            _ => unreachable!("a request of the stores"),
        }
    }

    /// The project of `dir`, its store not made; a choice of stores is not
    /// asked here.
    fn project(&self, dir: &str, lang: Lang) -> Result<(UserFile, forge::Workspace), Error> {
        let user = UserFile::load(&self.paths.settings_file())?;
        let auto_rehome = user.forge.auto_rehome.unwrap_or(true);
        let project = forge::project_of(&self.paths, Path::new(dir), auto_rehome, lang, &|options| {
            Err(agentlibre_forge::forge::error::Error::RehomeNoStore {
                stores: options[..options.len().saturating_sub(1)].to_vec(),
            }
            .into())
        })?;
        Ok((user, project))
    }

    fn past(
        &self,
        dir: Option<String>,
        plane: Option<String>,
        limit: usize,
        lang: Lang,
    ) -> Result<Reply, Error> {
        let sessions = self.paths.sessions();
        let found = match (plane, dir) {
            (Some(plane), _) => {
                let plane = PlaneFile::open(&self.paths, &plane)?;
                Session::latest_of_plane(&sessions, &plane.id, limit)
            }
            (None, Some(dir)) => {
                let (_, project) = self.project(&dir, lang)?;
                Session::latest(&sessions, &project.project, limit)
            }
            (None, None) => Session::latest_of_all(&sessions, limit),
        };
        Ok(Reply::Past {
            sessions: found.into_iter().map(wire::past).collect(),
        })
    }

    fn planes(&self, dir: &str, name: Option<String>) -> Result<Reply, Error> {
        let planes = match name {
            Some(name) => vec![PlaneFile::open(&self.paths, &name)?],
            None => PlaneFile::of_directory(&self.paths, Path::new(dir))?,
        };
        Ok(Reply::Planes {
            planes: planes.into_iter().map(|plane| plane.name).collect(),
        })
    }

    /// The Functions of a run in `dir` and `plane` (AGL-036, D3; AGL-077, D7).
    fn catalog(&self, dir: &str, plane: Option<&str>, lang: Lang) -> Result<Catalog, Error> {
        let (user, project) = self.project(dir, lang)?;
        let Some(name) = plane else {
            return Ok(Catalog::of(&self.paths, &project, &user));
        };
        let plane = PlaneFile::open(&self.paths, name)?;
        let settings: ProjectFile = plane.settings()?;
        Ok(Catalog::in_plane(&self.paths, &project, &user, &plane, &settings))
    }
}

/// What a verb of a Function of `catalog` says.
fn function(catalog: &Catalog, verb: proto::FunctionVerb) -> Result<proto::FunctionView, Error> {
    let shown = |path: PathBuf| path.to_string_lossy().into_owned();
    match verb {
        proto::FunctionVerb::Show { name } => Ok(proto::FunctionView {
            text: Some(catalog.text(&name)?),
            path: shown(catalog.file(&name)?.path),
            name,
            load: None,
        }),
        proto::FunctionVerb::Check { name } => {
            catalog.check(&name)?;
            let function = Function::load(&catalog.dirs(), &name)?;
            Ok(proto::FunctionView {
                path: shown(catalog.file(&name)?.path),
                load: Some(proto::FunctionLoad {
                    kind: function.model.kind().name().to_owned(),
                    model: crate::engine::model_of(&function.model)?,
                    context: function.model.context,
                }),
                name,
                text: None,
            })
        }
        proto::FunctionVerb::New { name, from } => {
            let text = catalog.text(&from)?;
            Ok(proto::FunctionView {
                path: shown(catalog.create(&name, &text)?),
                name,
                ..proto::FunctionView::default()
            })
        }
        proto::FunctionVerb::Delete { name } => Ok(proto::FunctionView {
            path: shown(catalog.delete(&name)?),
            name,
            ..proto::FunctionView::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::tests::{Client, daemon, open};
    use agentlibre_common::testutil::TempDir;

    const GOOD: &str = r#"{ agl, ... }: { policy = { system = "s"; initial = "work"; state.work.answer = true; }; model = { format = agl.formats."qwen3.8"; weights = "p.gguf"; context = 4096; }; }"#;

    fn text(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// A daemon, its socket, and a project of its user's `fixture-minimal`.
    fn fixture(dir: &TempDir) -> (Paths, PathBuf, PathBuf) {
        let (paths, socket) = daemon(dir);
        std::fs::write(
            paths.settings_file(),
            r#"{ agl, ... }: { function = "fixture-minimal"; server.llama = { url = "http://127.0.0.1:9"; external = true; }; }"#,
        )
        .unwrap();
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        (paths, socket, project)
    }

    fn past(client: &mut Client, dir: Option<&Path>, plane: Option<&str>) -> Vec<proto::PastSession> {
        let reply = client.ask(&Request::Past {
            dir: dir.map(text),
            plane: plane.map(str::to_owned),
            limit: 10,
        });
        let Reply::Past { sessions } = reply else {
            panic!("no past: {reply:?}")
        };
        sessions
    }

    #[test]
    fn the_past_is_of_a_project_of_a_plane_or_of_all() {
        let dir = TempDir::new();
        let (paths, socket, project) = fixture(&dir);
        let other = dir.0.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let plane = PlaneFile::create(&paths, "work", std::slice::from_ref(&other)).unwrap();
        let sessions = paths.sessions();
        let mut one = Session::create(&sessions, "f", &project).unwrap();
        one.append(crate::session::Event::User("first line\nsecond".into())).unwrap();
        one.rename(&project, "named").unwrap();
        // A file's name sorts by the millisecond.
        thread::sleep(std::time::Duration::from_millis(5));
        let two = Session::create_in(&sessions, "f", &other, None, Some(&plane.of_session())).unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let here = past(&mut client, Some(&project), None);
        assert_eq!(here.len(), 1);
        assert_eq!((here[0].id.as_str(), here[0].first.as_str()), (one.id.as_str(), "first line"));
        assert_eq!(here[0].name.as_deref(), Some("named"));
        assert_eq!((here[0].workspace.clone(), here[0].plane.clone()), (text(&project), None));
        let planes = past(&mut client, Some(&project), Some("work"));
        assert_eq!(planes.len(), 1);
        assert_eq!((planes[0].id.as_str(), planes[0].plane.as_deref()), (two.id.as_str(), Some("work")));
        // The newest first, and no session is made by looking.
        let all: Vec<String> = past(&mut client, None, None).into_iter().map(|s| s.id).collect();
        assert_eq!(all, [two.id.clone(), one.id.clone()]);
        assert_eq!(client.ask(&Request::Sessions), Reply::Sessions { sessions: Vec::new() });
        assert!(matches!(
            client.ask(&Request::Past { dir: None, plane: Some("none".into()), limit: 1 }),
            Reply::Refused { .. }
        ));
    }

    #[test]
    fn the_planes_of_a_directory_and_the_plane_of_a_name() {
        let dir = TempDir::new();
        let (paths, socket, project) = fixture(&dir);
        PlaneFile::create(&paths, "work", std::slice::from_ref(&project)).unwrap();
        PlaneFile::create(&paths, "away", &[]).unwrap();
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let mut planes = |name: Option<&str>| {
            client.ask(&Request::Planes {
                dir: text(&project),
                name: name.map(str::to_owned),
            })
        };
        assert_eq!(planes(None), Reply::Planes { planes: vec!["work".into()] });
        assert_eq!(planes(Some("away")), Reply::Planes { planes: vec!["away".into()] });
        assert!(matches!(planes(Some("none")), Reply::Refused { .. }));
    }

    #[test]
    fn the_functions_are_listed_shown_checked_made_and_deleted() {
        let dir = TempDir::new();
        let (paths, socket, project) = fixture(&dir);
        // The project is one, with its store.
        let (mut client, _) = Client::new(&socket, proto::VERSION);
        let Reply::Opened(_) = client.ask(&open(&project, None)) else {
            panic!("not opened")
        };
        std::fs::create_dir_all(paths.functions()).unwrap();
        std::fs::write(paths.functions().join("mine.nix"), GOOD).unwrap();
        std::fs::write(paths.functions().join("broken.nix"), "{ agl, ... }: { nope").unwrap();
        let verb = |client: &mut Client, verb: proto::FunctionVerb| {
            client.ask(&Request::Function { dir: text(&project), plane: None, verb })
        };
        let list = |client: &mut Client| {
            let reply = client.ask(&Request::Functions { dir: text(&project), plane: None });
            let Reply::Functions { functions } = reply else {
                panic!("no functions: {reply:?}")
            };
            functions
        };
        let functions = list(&mut client);
        let named = |name: &str| functions.iter().find(|f| f.name == name).unwrap();
        assert_eq!(named("mine").source, proto::FunctionSource::User);
        assert_eq!((named("mine").kind.as_str(), named("mine").model.as_deref()), ("llama", Some("p.gguf")));
        assert!(named("broken").error.is_some());
        assert!(named("fixture-minimal").current && !named("mine").current);
        let Reply::Function(shown) = verb(&mut client, proto::FunctionVerb::Show { name: "mine".into() }) else {
            panic!("not shown")
        };
        assert_eq!(shown.text.as_deref(), Some(GOOD));
        assert_eq!(shown.path, text(&paths.functions().join("mine.nix")));
        let Reply::Function(checked) = verb(&mut client, proto::FunctionVerb::Check { name: "mine".into() }) else {
            panic!("not checked")
        };
        assert_eq!(checked.load.unwrap().context, Some(4096));
        let refused = |reply: Reply| matches!(reply, Reply::Refused { .. });
        assert!(refused(verb(&mut client, proto::FunctionVerb::Check { name: "broken".into() })));
        let new = proto::FunctionVerb::New { name: "twin".into(), from: "mine".into() };
        let Reply::Function(made) = verb(&mut client, new.clone()) else {
            panic!("not made")
        };
        assert_eq!(std::fs::read_to_string(&made.path).unwrap(), GOOD);
        assert!(refused(verb(&mut client, new)), "a name that is there");
        // A Function is deleted while a session of it may be held (D13); a
        // built-in is not.
        let Reply::Function(gone) = verb(&mut client, proto::FunctionVerb::Delete { name: "twin".into() }) else {
            panic!("not deleted")
        };
        assert_eq!(gone.path, made.path);
        assert!(!Path::new(&made.path).exists());
        assert!(refused(verb(&mut client, proto::FunctionVerb::Delete { name: "fixture-minimal".into() })));
        assert!(!list(&mut client).iter().any(|f| f.name == "twin"));
    }
}
