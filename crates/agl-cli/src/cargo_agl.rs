use std::path::PathBuf;
use std::process::{Command, ExitCode};

use agl_package::{ArtifactPackageView, DirectoryPackageView};
use anyhow::{Context, Result, bail};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo agl: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.first().is_some_and(|argument| argument == "agl") {
        arguments.remove(0);
    }
    if arguments.first().is_none_or(|argument| argument != "build") {
        bail!(
            "usage: cargo agl build --exporter <cargo-bin> --source <directory> --output <directory>"
        );
    }
    arguments.remove(0);

    let mut exporter = None;
    let mut source = None;
    let mut output = None;
    while !arguments.is_empty() {
        let option = arguments.remove(0);
        let value = arguments
            .first()
            .cloned()
            .with_context(|| format!("missing value for `{}`", option.to_string_lossy()))?;
        arguments.remove(0);
        match option.to_string_lossy().as_ref() {
            "--exporter" => exporter = Some(value),
            "--source" => source = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            unknown => bail!("unknown option `{unknown}`"),
        }
    }
    let exporter = exporter.context("--exporter is required")?;
    let source = source.context("--source is required")?;
    let output = output.context("--output is required")?;

    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["run", "--quiet", "--bin"])
        .arg(exporter)
        .arg("--")
        .arg(&source)
        .arg(&output)
        .status()
        .context("failed to run the explicit Extension exporter")?;
    if !status.success() {
        bail!("Extension exporter failed with {status}");
    }

    let view = DirectoryPackageView::new(&output)?;
    let package = agl_extension::package::ExtensionPackage::parse(&view)?;
    let definition = package.definition()?;
    let files = view.files()?.len();
    println!(
        "built extension:{}@{} declaration={} package={} files={files}",
        definition.id,
        definition.version,
        definition.digest(),
        package.package_tree_digest()
    );
    Ok(())
}
