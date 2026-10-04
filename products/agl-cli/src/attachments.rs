use agl_core::{ImageRef, ImageSource};
use anyhow::{Context, Result, ensure};
use sha2::{Digest as _, Sha256};
use std::path::Path;

pub(crate) fn image_ref(path: &Path) -> Result<ImageRef> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = path
        .canonicalize()
        .context("failed to resolve image path")?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .context("failed to open image attachment")?;
    ensure!(
        file.metadata()?.is_file(),
        "image attachment must be a regular file"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    ensure!(
        agl_tool_api::image::detect_format(&bytes).is_some(),
        "attachment format is unsupported by the pinned image decoder"
    );
    ImageRef::new(
        ImageSource::File {
            path: path
                .to_str()
                .context("image path must be UTF-8")?
                .to_owned()
                .try_into()
                .map_err(anyhow::Error::msg)?,
        },
        agl_core::agent::PackageDigest::from_bytes(Sha256::digest(&bytes).into()),
    )
    .map_err(anyhow::Error::from)
}

pub(crate) fn attach_command(line: &str, pending: &mut Vec<ImageRef>) -> Result<bool> {
    let command = line.trim();
    if command != "/attach" && !command.starts_with("/attach ") {
        return Ok(false);
    }
    let path = command
        .strip_prefix("/attach")
        .expect("checked prefix")
        .trim();
    ensure!(!path.is_empty(), "usage: /attach PATH");
    pending.push(image_ref(Path::new(path))?);
    println!("image attached for the next message: {path}");
    Ok(true)
}
