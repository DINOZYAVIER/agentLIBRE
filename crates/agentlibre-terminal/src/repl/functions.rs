//! The Functions as the REPL's tests ask for them (AGL-126, D2): a catalog of
//! their own, where the daemon's requests are the surface crate's
//! (`agentlibre_surface::daemon::Stores`; AGL-127, D2).

use std::path::PathBuf;

use agentlibre_proto::{FunctionEntry, FunctionLoad};
use agentlibre_surface::Error;
use agentlibre_surface::daemon::Functions;

use crate::error::CoreError;

/// A core's error as the surface's: the words it says, as a refusal.
fn said(error: impl Into<CoreError>) -> Error {
    Error::Client(agentlibre_client::Error::Said {
        text: error.into().message(agentlibre_common::i18n::Lang::En),
    })
}

use agentlibre_common::i18n::Lang;

use crate::function::Function;
use crate::function::catalog::Catalog;

/// A catalog standing for the daemon's stores in the tests; the trait is
/// the surface's, so the catalog is wrapped (AGL-127, D2).
pub struct Fixture(pub Catalog);

impl std::ops::Deref for Fixture {
    type Target = Catalog;

    fn deref(&self) -> &Catalog {
        &self.0
    }
}

impl Functions for Fixture {
    fn current(&self) -> &str {
        &self.current
    }

    fn entries(&self) -> Result<Vec<FunctionEntry>, Error> {
        Ok(Catalog::entries(&self.0, Lang::En)
            .into_iter()
            .map(crate::plane::wire::function_entry)
            .collect())
    }

    fn show(&self, name: &str) -> Result<(String, PathBuf), Error> {
        Ok((self.text(name).map_err(said)?, self.file(name).map_err(said)?.path))
    }

    fn check(&self, name: &str) -> Result<FunctionLoad, Error> {
        Catalog::check(&self.0, name).map_err(said)?;
        let function = Function::load(&self.0.dirs(), name).map_err(said)?;
        Ok(FunctionLoad {
            kind: function.model.kind().name().to_owned(),
            model: crate::engine::model_of(&function.model).map_err(said)?,
            context: function.model.context,
        })
    }

    fn create(&self, name: &str, from: &str) -> Result<PathBuf, Error> {
        Catalog::create(&self.0, name, &self.text(from).map_err(said)?).map_err(said)
    }

    fn delete(&self, name: &str) -> Result<(), Error> {
        Catalog::delete(&self.0, name).map(drop).map_err(said)
    }
}

/// The Functions of the list, as the core and as the wire have them.
fn entries() -> (Vec<crate::function::catalog::Entry>, Vec<agentlibre_proto::FunctionEntry>) {
    use crate::function::catalog::{Entry, Source};
    use agentlibre_common::settings::ServerKind;
    let make = || {
        vec![
            Entry {
                name: "minimal".into(),
                source: Source::BuiltIn,
                shadows: vec![Source::User, Source::Workspace],
                kind: ServerKind::Llama,
                model: Some("m.gguf".into()),
                description: Some("the smallest".into()),
                error: None,
                current: true,
            },
            Entry {
                name: "my-own-function".into(),
                source: Source::Plane,
                shadows: Vec::new(),
                kind: ServerKind::Llama,
                model: None,
                description: None,
                error: None,
                current: false,
            },
            Entry {
                name: "broken".into(),
                source: Source::Workspace,
                shadows: Vec::new(),
                kind: ServerKind::Llama,
                model: None,
                description: None,
                error: Some("it does not load".into()),
                current: false,
            },
        ]
    };
    (
        make(),
        make().into_iter().map(crate::plane::wire::function_entry).collect(),
    )
}

#[test]
fn the_functions_of_the_wire_are_listed_as_the_catalog_lists_them() {
    // AGL-130, D10: the same lines in every language.
    let (core, wire) = entries();
    for lang in [Lang::En, Lang::Uk] {
        assert_eq!(
            agentlibre_surface::records::function_lines(&wire, lang),
            crate::function::catalog::lines(&core, lang)
        );
    }
    assert!(agentlibre_surface::records::function_lines(&wire, Lang::En)[0].starts_with("* minimal"));
}
