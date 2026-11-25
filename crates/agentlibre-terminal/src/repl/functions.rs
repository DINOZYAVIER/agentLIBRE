//! The Functions as the REPL asks for them (AGL-126, D2): by the daemon's
//! requests ([`crate::remote::Stores`]), the tests by a catalog of their own.

use std::path::PathBuf;

use agentlibre_proto::FunctionLoad;

use crate::error::CoreError as Error;
use agentlibre_proto::FunctionEntry;

pub trait Functions: Send + Sync {
    /// The Function the session runs.
    fn current(&self) -> &str;
    /// Every Function by name, the session's marked.
    fn entries(&self) -> Result<Vec<FunctionEntry>, Error>;
    /// The source of `name` and the file a run loads it from.
    fn show(&self, name: &str) -> Result<(String, PathBuf), Error>;
    /// What `name` runs, when it loads and its model fits the settings.
    fn check(&self, name: &str) -> Result<FunctionLoad, Error>;
    /// A copy of `from` in the workspace's `functions/`: its file.
    fn create(&self, name: &str, from: &str) -> Result<PathBuf, Error>;
    /// The file of `name` deleted.
    fn delete(&self, name: &str) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_common::i18n::Lang;

    use crate::function::Function;
    use crate::function::catalog::Catalog;

    impl Functions for Catalog {
        fn current(&self) -> &str {
            &self.current
        }

        fn entries(&self) -> Result<Vec<FunctionEntry>, Error> {
            Ok(Catalog::entries(self, Lang::En)
                .into_iter()
                .map(crate::plane::wire::function_entry)
                .collect())
        }

        fn show(&self, name: &str) -> Result<(String, PathBuf), Error> {
            Ok((self.text(name)?, self.file(name)?.path))
        }

        fn check(&self, name: &str) -> Result<FunctionLoad, Error> {
            Catalog::check(self, name)?;
            let function = Function::load(&self.dirs(), name)?;
            Ok(FunctionLoad {
                kind: function.model.kind().name().to_owned(),
                model: crate::engine::model_of(&function.model)?,
                context: function.model.context,
            })
        }

        fn create(&self, name: &str, from: &str) -> Result<PathBuf, Error> {
            Catalog::create(self, name, &self.text(from)?)
        }

        fn delete(&self, name: &str) -> Result<(), Error> {
            Catalog::delete(self, name).map(drop)
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
}
