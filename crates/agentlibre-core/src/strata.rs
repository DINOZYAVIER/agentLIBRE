//! The Strata engines (AGL-089): the package of a Function's variant, the
//! first preparation of its model under agl's models folder, and the config
//! the package's server reads, all from the Function (D5).
//!
//! The packages come from `programs.agl.strata`, whose wrapper names their
//! folder in `AGENTLIBRE_STRATA`: `<folder>/<variant>` (D20). A model's data
//! is `<models>/strata/<name>/`: its pack (D9), the config and the logs; the
//! MTP head, the same for every Flash-Next, is `<models>/strata/mtp/` (D13).

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::error;
use crate::i18n::{Lang, Msg, m};
use crate::settings::ServerFile;
use crate::tr;

/// The folder of the Strata packages, by variant (D20).
pub const STRATA_VAR: &str = "AGENTLIBRE_STRATA";

/// What can go wrong with a Strata engine (AGL-089, D20; AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// The Strata package of the Function's variant is not installed: no
    /// `<AGENTLIBRE_STRATA>/<variant>` (AGL-089, D20).
    NotInstalled { variant: String },
    /// A step of the first preparation of a Strata model failed: the pack,
    /// the MTP head (AGL-089, D9, D13); its output is in `log`.
    Prepare { step: String, log: PathBuf },
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::NotInstalled { variant } => tr!(lang, NOT_INSTALLED, variant = variant),
            Self::Prepare { step, log } => {
                tr!(lang, PREPARE, step = step, log = log.display())
            }
        }
    }
}

/// `Error::NotInstalled`.
const NOT_INSTALLED: Msg = Msg {
    en: "the Strata engine `{variant}` is not installed: on NixOS, add it to `programs.agl.strata` (e.g. programs.agl.strata = [ \"{variant}\" ];) and rebuild",
    uk: "рушій Strata `{variant}` не встановлено: на NixOS додай його в `programs.agl.strata` (наприклад, programs.agl.strata = [ \"{variant}\" ];) і перебудуй систему",
};

/// `Error::Prepare`.
const PREPARE: Msg = Msg {
    en: "the preparation of the Strata model failed at {step}; its output is in {log}",
    uk: "підготовка моделі Strata не вдалася на кроці «{step}»; її вивід у {log}",
};

/// The package of `variant`, from `AGENTLIBRE_STRATA`.
pub fn package(variant: &str) -> Result<PathBuf, error::Error> {
    Ok(package_in(std::env::var_os(STRATA_VAR).map(PathBuf::from).as_deref(), variant)?)
}

/// The package of `variant` in the folder `dir`: one with its server.
fn package_in(dir: Option<&Path>, variant: &str) -> Result<PathBuf, Error> {
    dir.filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(variant))
        .filter(|package| package.join("bin/strata-server").is_file())
        .ok_or_else(|| Error::NotInstalled {
            variant: variant.to_owned(),
        })
}

/// Where a model's data is under agl's models folder.
#[derive(Debug, PartialEq, Eq)]
pub struct Data {
    /// `<models>/strata/<name>`: the config, the logs.
    pub dir: PathBuf,
    /// Its pack (`iq_pack.py`).
    pub pack: PathBuf,
    /// The MTP head's folder, the engine's `--mtp` being its `rt`.
    pub mtp: PathBuf,
}

impl Data {
    pub fn of(models: &Path, name: &str) -> Data {
        let root = models.join("strata");
        let dir = root.join(name);
        Data {
            pack: dir.join("pack"),
            mtp: root.join("mtp"),
            dir,
        }
    }

    /// Whether the pack is made: its index is the last file `iq_pack.py`
    /// writes into a folder that is renamed into place once whole.
    fn pack_ready(&self) -> bool {
        self.pack.join("index.txt").is_file()
    }

    fn mtp_ready(&self) -> bool {
        self.mtp.join("rt/experts.bin").is_file()
    }
}

/// The model's name and its first shard: the Function loader requires both
/// of the strata kind (D5).
fn name_and_first_shard(settings: &ServerFile) -> (&str, &str) {
    let name = settings
        .strata_name
        .as_deref()
        .expect("a strata Function has a name");
    let shard = settings
        .gguf
        .as_deref()
        .and_then(<[String]>::first)
        .expect("a strata Function has its shards");
    (name, shard)
}

/// Makes what a first run of the model needs and is not there yet (D9,
/// D13): the pack of its shards, the MTP head. `settings` has its shards
/// resolved to absolute paths. Each step says what it does by `say`; its
/// output goes to `prepare.log` beside the model's data.
pub fn prepare(
    settings: &ServerFile,
    models: &Path,
    package: &Path,
    say: &mut dyn FnMut(&str),
    lang: Lang,
) -> Result<Data, error::Error> {
    let (name, shard) = name_and_first_shard(settings);
    let data = Data::of(models, name);
    let created = |path: &Path| {
        fs::create_dir_all(path).map_err(|source| error::Error::Write {
            path: path.into(),
            source,
        })
    };
    created(&data.dir)?;
    let log = data.dir.join("prepare.log");
    let python = package.join("bin/strata-python");
    let tools = package.join("share/strata/tools");
    if !data.pack_ready() {
        let step = tr!(lang, m::STRATA_STEP_PACK);
        say(&tr!(lang, m::STRATA_PREPARING, step = step));
        let part = data.dir.join("pack.part");
        let _ = fs::remove_dir_all(&part);
        run(
            Command::new(package.join("bin/strata-iq-pack"))
                .arg("--gguf")
                .arg(shard)
                .arg("--out")
                .arg(&part)
                .arg("--compat-bf16"),
            &log,
            step,
        )?;
        replace(&part, &data.pack)?;
    }
    if !data.mtp_ready() {
        created(&data.mtp)?;
        let fetch = tr!(lang, m::STRATA_STEP_MTP_FETCH);
        say(&tr!(lang, m::STRATA_PREPARING, step = fetch));
        // Resumable: a run again continues where the last one stopped.
        run(
            Command::new(&python)
                .arg(tools.join("mtp_fetch.py"))
                .arg("fetch")
                .arg("--out")
                .arg(&data.mtp),
            &log,
            fetch,
        )?;
        let build = tr!(lang, m::STRATA_STEP_MTP_BUILD);
        say(&tr!(lang, m::STRATA_PREPARING, step = build));
        let gguf = data.mtp.join("mtp-q2_0.gguf");
        run(
            Command::new(&python)
                .arg(tools.join("mtp_pack.py"))
                .arg("--src")
                .arg(&data.mtp)
                .arg("--experts")
                .arg("q2_0")
                .arg("--out")
                .arg(&gguf),
            &log,
            build,
        )?;
        let part = data.mtp.join("rt.part");
        let _ = fs::remove_dir_all(&part);
        run(
            Command::new(&python)
                .arg(tools.join("mtp_rt.py"))
                .arg("--gguf")
                .arg(&gguf)
                .arg("--out")
                .arg(&part),
            &log,
            build,
        )?;
        // The draft layer's token subset setup.py puts there, its default
        // (`data/draft_vocab.bin`, AGL-086's D6 runs on it).
        let vocab = package.join("share/strata/data/draft_vocab.bin");
        fs::copy(&vocab, part.join("draft_vocab.bin")).map_err(|source| error::Error::Write {
            path: part.join("draft_vocab.bin"),
            source,
        })?;
        replace(&part, &data.mtp.join("rt"))?;
    }
    Ok(data)
}

/// Runs a step of the preparation, its output appended to `log`.
fn run(command: &mut Command, log: &Path, step: &str) -> Result<(), error::Error> {
    let open = || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|source| error::Error::Write {
                path: log.into(),
                source,
            })
    };
    let failed = || Error::Prepare {
        step: step.to_owned(),
        log: log.into(),
    };
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(open()?))
        .stderr(Stdio::from(open()?))
        .status()
        .map_err(|_| failed())?;
    if status.success() { Ok(()) } else { Err(failed().into()) }
}

/// `part` renamed to `done`, an older `done` gone first.
fn replace(part: &Path, done: &Path) -> Result<(), error::Error> {
    let _ = fs::remove_dir_all(done);
    fs::rename(part, done).map_err(|source| error::Error::Write {
        path: done.into(),
        source,
    })
}

/// The config the package's server reads (D5), written beside the model's
/// data: the engine of the package, its arguments with the paths agl fills
/// (the pack, the first shard, the MTP head, the expert profile, the
/// context), the environment with the hipBLASLt tuning of the package's
/// hipBLASLt; its path.
pub fn write_config(settings: &ServerFile, data: &Data, package: &Path) -> Result<PathBuf, error::Error> {
    let config = config(settings, data, package);
    let variant = settings.strata.as_deref().unwrap_or_default();
    let path = data.dir.join(format!("config-{variant}.json"));
    let mut text = serde_json::to_string_pretty(&config).expect("a JSON value serializes");
    text.push('\n');
    fs::write(&path, text).map_err(|source| error::Error::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

fn config(settings: &ServerFile, data: &Data, package: &Path) -> Value {
    let (name, shard) = name_and_first_shard(settings);
    let share = package.join("share/strata");
    let text = |path: PathBuf| path.to_string_lossy().into_owned();
    let mut args = vec![
        "--pack".to_owned(),
        text(data.pack.clone()),
        "--native".to_owned(),
        shard.to_owned(),
        "--expert-profile".to_owned(),
        text(share.join("data/expert-profile.bin")),
        "--mtp".to_owned(),
        text(data.mtp.join("rt")),
    ];
    if let Some(context) = settings.context {
        args.push("--max-context".to_owned());
        args.push(context.to_string());
    }
    args.extend(settings.args.clone().unwrap_or_default());
    let mut env = settings.env.clone().unwrap_or_default();
    let tuning = share.join("hipblaslt-tuning.txt");
    if tuning.exists() {
        env.entry("STRATA_HIPBLASLT_TUNING".to_owned())
            .or_insert_with(|| text(tuning));
    }
    json!({
        "exe": text(package.join("bin/strata")),
        "cwd": text(data.dir.clone()),
        "tokenizer": text(data.pack.join("tokenizer")),
        "model_name": name,
        "backend": "hip",
        "gpu": "0",
        "layer_split": "auto",
        "host": "127.0.0.1",
        "open_browser": false,
        "engine_silence_s": 1800,
        "lib_dirs": [],
        "log": text(data.dir.join("engine.log")),
        "args": args,
        "env": env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;

    fn settings() -> ServerFile {
        ServerFile {
            kind: Some("strata".into()),
            strata: Some("rdna3".into()),
            strata_name: Some("tc".into()),
            gguf: Some(vec!["/m/tc-00001-of-00002.gguf".into(), "/m/tc-00002-of-00002.gguf".into()]),
            context: Some(262144),
            args: Some(vec!["--spec".into(), "4".into()]),
            env: Some([("STRATA_GR_RDNA".to_owned(), "1".to_owned())].into()),
            ..ServerFile::default()
        }
    }

    #[test]
    fn a_variant_is_found_by_its_server_in_the_folder() {
        let dir = TempDir::new();
        let bin = dir.0.join("rdna3/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("strata-server"), "").unwrap();
        assert_eq!(package_in(Some(&dir.0), "rdna3").unwrap(), dir.0.join("rdna3"));
        for (folder, variant) in [(Some(dir.0.as_path()), "upstream"), (None, "rdna3"), (Some(Path::new("rel")), "rdna3")] {
            assert!(matches!(
                package_in(folder, variant),
                Err(Error::NotInstalled { variant: v }) if v == variant
            ));
        }
    }

    #[test]
    fn the_data_of_a_model_is_under_the_models_folder() {
        let data = Data::of(Path::new("/models"), "tc");
        assert_eq!(data.dir, Path::new("/models/strata/tc"));
        assert_eq!(data.pack, Path::new("/models/strata/tc/pack"));
        // The MTP head is every Flash-Next's (D13).
        assert_eq!(data.mtp, Path::new("/models/strata/mtp"));
    }

    #[test]
    fn the_config_is_the_functions_with_the_paths_agl_fills() {
        let dir = TempDir::new();
        let package = dir.0.join("pkg");
        fs::create_dir_all(package.join("share/strata")).unwrap();
        fs::write(package.join("share/strata/hipblaslt-tuning.txt"), "").unwrap();
        let data = Data::of(&dir.0.join("models"), "tc");
        fs::create_dir_all(&data.dir).unwrap();
        let path = write_config(&settings(), &data, &package).unwrap();
        assert_eq!(path, data.dir.join("config-rdna3.json"));
        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let p = |path: PathBuf| Value::String(path.to_string_lossy().into_owned());
        assert_eq!(config["exe"], p(package.join("bin/strata")));
        assert_eq!(config["cwd"], p(data.dir.clone()));
        assert_eq!(config["model_name"], "tc");
        assert_eq!(config["tokenizer"], p(data.pack.join("tokenizer")));
        let args: Vec<&str> = config["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        let pack = data.pack.to_string_lossy().into_owned();
        let profile = package.join("share/strata/data/expert-profile.bin").to_string_lossy().into_owned();
        let mtp = data.mtp.join("rt").to_string_lossy().into_owned();
        assert_eq!(
            args,
            [
                "--pack", &pack, "--native", "/m/tc-00001-of-00002.gguf", "--expert-profile", &profile,
                "--mtp", &mtp, "--max-context", "262144", "--spec", "4",
            ]
        );
        assert_eq!(config["env"]["STRATA_GR_RDNA"], "1");
        assert_eq!(
            config["env"]["STRATA_HIPBLASLT_TUNING"],
            p(package.join("share/strata/hipblaslt-tuning.txt"))
        );
        assert_eq!(config["lib_dirs"], json!([]));
    }

    #[test]
    fn a_tuning_the_function_names_wins() {
        let dir = TempDir::new();
        let package = dir.0.join("pkg");
        fs::create_dir_all(package.join("share/strata")).unwrap();
        fs::write(package.join("share/strata/hipblaslt-tuning.txt"), "").unwrap();
        let mut settings = settings();
        settings.env = Some([("STRATA_HIPBLASLT_TUNING".to_owned(), "/t.txt".to_owned())].into());
        let config = config(&settings, &Data::of(&dir.0, "tc"), &package);
        assert_eq!(config["env"]["STRATA_HIPBLASLT_TUNING"], "/t.txt");
    }

    #[test]
    fn a_failed_step_names_its_log() {
        let dir = TempDir::new();
        let log = dir.0.join("prepare.log");
        let error = run(Command::new("/bin/sh").args(["-c", "echo out; exit 3"]), &log, "the pack").unwrap_err();
        assert!(matches!(error, error::Error::Strata(Error::Prepare { ref step, log: ref l }) if step == "the pack" && *l == log));
        assert_eq!(fs::read_to_string(&log).unwrap(), "out\n");
    }
}
