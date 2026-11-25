//! The Functions as the REPL asks for them (AGL-126, D2): by the daemon's
//! requests ([`crate::remote::Stores`]), the tests by a catalog of their own.

use std::path::PathBuf;

use agentlibre_proto::FunctionLoad;

use crate::error::CoreError as Error;
use crate::function::catalog::Entry;

pub trait Functions: Send + Sync {
    /// The Function the session runs.
    fn current(&self) -> &str;
    /// Every Function by name, the session's marked.
    fn entries(&self) -> Result<Vec<Entry>, Error>;
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

        fn entries(&self) -> Result<Vec<Entry>, Error> {
            Ok(Catalog::entries(self, Lang::En))
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
}
