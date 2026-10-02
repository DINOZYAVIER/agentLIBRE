//! `agl function-family`: list, inspect and select Function Family profiles.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use agl_runtime::function::{FamilyProfile, FunctionFamily, discover_function_families};
use anyhow::{Context, Result, bail};

use crate::config;
use crate::workspace::ApplicationRoots;

/// Profile minimums are declared in GB; one GB is 10^9 bytes.
const BYTES_PER_GB: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fit {
    Fits,
    BelowMinimum,
    Unknown,
}

impl Fit {
    fn label(self) -> &'static str {
        match self {
            Self::Fits => "fits",
            Self::BelowMinimum => "below minimum",
            Self::Unknown => "unknown",
        }
    }
}

/// Detected machine memory in bytes; `None` when it could not be detected.
#[derive(Clone, Copy, Debug, Default)]
struct Machine {
    ram_bytes: Option<u64>,
    vram_bytes: Option<u64>,
}

impl Machine {
    /// RAM comes from `MemTotal` in `/proc/meminfo`. VRAM is the largest
    /// `mem_info_vram_total` that a DRM device reports in sysfs (amdgpu exposes
    /// it); devices without that file leave VRAM unknown.
    fn detect() -> Self {
        Self {
            ram_bytes: std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|meminfo| parse_mem_total(&meminfo)),
            vram_bytes: largest_vram(std::path::Path::new("/sys/class/drm")),
        }
    }
}

fn largest_vram(drm: &std::path::Path) -> Option<u64> {
    std::fs::read_dir(drm)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|card| {
            std::fs::read_to_string(card.path().join("device/mem_info_vram_total")).ok()
        })
        .filter_map(|bytes| bytes.trim().parse::<u64>().ok())
        .max()
}

fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let mut fields = line.split_whitespace().skip(1);
    let value: u64 = fields.next()?.parse().ok()?;
    match fields.next() {
        Some("kB") => value.checked_mul(1024),
        _ => None,
    }
}

fn fit(minimum_gb: Option<u64>, detected: Option<u64>) -> Option<Fit> {
    let minimum = minimum_gb?.saturating_mul(BYTES_PER_GB);
    Some(match detected {
        None => Fit::Unknown,
        Some(bytes) if bytes >= minimum => Fit::Fits,
        Some(_) => Fit::BelowMinimum,
    })
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / BYTES_PER_GB as f64)
}

fn detected(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "unknown (could not be detected)".to_owned(), gb)
}

fn minimums(profile: &FamilyProfile) -> String {
    let mut parts = Vec::new();
    if let Some(ram) = profile.min_ram_gb {
        parts.push(format!("min RAM {ram} GB"));
    }
    if let Some(vram) = profile.min_vram_gb {
        parts.push(format!("min VRAM {vram} GB"));
    }
    if parts.is_empty() {
        "no minimum".to_owned()
    } else {
        parts.join(", ")
    }
}

fn find<'a>(families: &'a [FunctionFamily], id: &str) -> Result<&'a FunctionFamily> {
    families.iter().find(|family| family.id == id).with_context(|| {
        format!(
            "Function Family {id:?} is not declared; run `agl function-family list` (declared: {})",
            families
                .iter()
                .map(|family| family.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The `chat.default_function` string of the human config, if the file exists.
fn selected_function(roots: &ApplicationRoots) -> Result<Option<String>> {
    let path = roots.config.join(config::CONFIG_FILE_NAME);
    if !path.try_exists()? {
        return Ok(None);
    }
    let bytes = crate::workspace::read_regular_file(&path, 4 * 1024 * 1024)?;
    let document: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("invalid {}", path.display()))?;
    Ok(document
        .get("chat")
        .and_then(|chat| chat.get("default_function"))
        .and_then(|value| value.as_str())
        .map(str::to_owned))
}

pub(crate) fn list(roots: &ApplicationRoots) -> Result<()> {
    let families = discover_function_families()?;
    let selected = selected_function(roots)?;
    if families.is_empty() {
        println!("no Function Family is declared in the AGL workspace");
    }
    for family in &families {
        println!("{}  {}", family.id, family.name);
        for profile in &family.profiles {
            let mut marks = Vec::new();
            if profile.id == family.default {
                marks.push("default");
            }
            if selected.as_deref() == Some(profile.function.as_str()) {
                marks.push("selected");
            }
            let marks = if marks.is_empty() {
                String::new()
            } else {
                format!("  [{}]", marks.join(", "))
            };
            println!(
                "  {}  {}  {}  ({}){marks}",
                profile.id,
                profile.name,
                profile.function,
                minimums(profile)
            );
        }
    }
    Ok(())
}

pub(crate) fn show(roots: &ApplicationRoots, family_id: &str) -> Result<()> {
    let families = discover_function_families()?;
    let family = find(&families, family_id)?;
    let selected = selected_function(roots)?;
    let machine = Machine::detect();
    println!("{}  {}", family.id, family.name);
    if let Some(description) = &family.description {
        println!("{description}");
    }
    println!("default profile: {}", family.default);
    println!("detected RAM:  {}", detected(machine.ram_bytes));
    println!("detected VRAM: {}", detected(machine.vram_bytes));
    for profile in &family.profiles {
        let selected_mark = if selected.as_deref() == Some(profile.function.as_str()) {
            "  [selected]"
        } else {
            ""
        };
        println!(
            "  {}  {}  {}{selected_mark}",
            profile.id, profile.name, profile.function
        );
        for (label, minimum, detected) in [
            ("RAM", profile.min_ram_gb, machine.ram_bytes),
            ("VRAM", profile.min_vram_gb, machine.vram_bytes),
        ] {
            if let Some(result) = fit(minimum, detected) {
                println!(
                    "    {label}: minimum {} GB -> {}",
                    minimum.expect("fit exists only with a minimum"),
                    result.label()
                );
            }
        }
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Print resource warnings; return whether to continue.
fn check_resources(profile: &FamilyProfile, machine: Machine) -> Result<bool> {
    let mut warned = false;
    for (label, minimum, detected) in [
        ("RAM", profile.min_ram_gb, machine.ram_bytes),
        ("VRAM", profile.min_vram_gb, machine.vram_bytes),
    ] {
        match fit(minimum, detected) {
            Some(Fit::BelowMinimum) => {
                warned = true;
                println!(
                    "warning: profile {} needs at least {} GB {label} but this machine has {}",
                    profile.id,
                    minimum.expect("fit exists only with a minimum"),
                    gb(detected.expect("below minimum implies detected")),
                );
            }
            Some(Fit::Unknown) => println!(
                "note: {label} could not be detected; profile {} needs at least {} GB",
                profile.id,
                minimum.expect("fit exists only with a minimum"),
            ),
            _ => {}
        }
    }
    if warned && interactive() {
        return confirm("Select this profile anyway?");
    }
    Ok(true)
}

pub(crate) async fn select(
    roots: &ApplicationRoots,
    family_id: &str,
    profile_id: &str,
) -> Result<()> {
    let families = discover_function_families()?;
    let family = find(&families, family_id)?;
    let profile = family.profile(profile_id).with_context(|| {
        format!(
            "profile {profile_id:?} is not declared in {}; declared: {}",
            family.id,
            family
                .profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let requirement = profile.requirement()?;
    if !check_resources(profile, Machine::detect())? {
        println!("selection cancelled; configuration unchanged");
        return Ok(());
    }
    config::set_default_function(&roots.config, &profile.function)?;
    println!(
        "chat.default_function = {} (profile {} of {}) written to {}",
        profile.function,
        profile.id,
        family.id,
        roots.config.join(config::CONFIG_FILE_NAME).display()
    );
    println!("run `agl config apply` to activate the selection");

    let command = format!("agl function-family select {family_id} {profile_id}");
    match ensure_model(roots, &requirement).await? {
        ModelStatus::Present | ModelStatus::Downloaded => {}
        ModelStatus::Declined => {
            println!("download skipped; selection kept. Run `{command}` later to download it.")
        }
        ModelStatus::NonInteractive => println!(
            "download skipped (not an interactive terminal); selection kept. \
             Run `{command}` in a terminal to download it."
        ),
    }
    Ok(())
}

enum ModelStatus {
    Present,
    Downloaded,
    Declined,
    NonInteractive,
}

/// Shows the pinned model size and asks before downloading it (spec F5.6).
/// Nothing is fetched without an interactive yes.
async fn ensure_model(
    roots: &ApplicationRoots,
    requirement: &agl_runtime::function::FunctionRequirement,
) -> Result<ModelStatus> {
    let model = agl_runtime::resolve_function_model(&requirement.id, &requirement.version)?;
    let digest = model
        .artifact
        .sha256
        .to_string()
        .strip_prefix("sha256:")
        .context("model digest has no sha256 prefix")?
        .to_owned();
    let destination = roots
        .data
        .join("runtime/models")
        .join(format!("{digest}.gguf"));
    if model_present(&destination, model.artifact.bytes)? {
        return Ok(ModelStatus::Present);
    }
    println!(
        "model {} is not downloaded ({}, {})",
        model.id,
        gb(model.artifact.bytes),
        model.artifact.url
    );
    if !interactive() {
        return Ok(ModelStatus::NonInteractive);
    }
    if !confirm("Download the model now?")? {
        return Ok(ModelStatus::Declined);
    }
    download(&model, &destination).await?;
    Ok(ModelStatus::Downloaded)
}

/// Before a chat starts, make sure the chat Function's model is present, asking
/// for consent to download it. Directory locators are activated as they are.
pub(crate) async fn ensure_chat_model(
    roots: &ApplicationRoots,
    function: Option<&str>,
) -> Result<()> {
    let locator = match function {
        Some(locator) => locator.to_owned(),
        None => match selected_function(roots)? {
            Some(locator) => locator,
            None => return Ok(()),
        },
    };
    if !locator.starts_with("function:") {
        return Ok(());
    }
    let requirement = agl_runtime::function::parse_function_requirement(&locator)?;
    match ensure_model(roots, &requirement).await? {
        ModelStatus::Present | ModelStatus::Downloaded => Ok(()),
        ModelStatus::Declined => bail!("the chat model is not downloaded; chat was not started"),
        ModelStatus::NonInteractive => bail!(
            "the chat model is not downloaded; run `agl chat` in an interactive terminal to download it"
        ),
    }
}

fn model_present(path: &Path, expected_bytes: u64) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == expected_bytes => Ok(true),
        Ok(_) => bail!(
            "{} exists but is not a regular file of the pinned size; remove it and retry",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn download(model: &agl_runtime::model::ModelManifest, destination: &Path) -> Result<()> {
    let request = model.fetch_request(destination.to_owned())?;
    let cancelled = AtomicBool::new(false);
    let mut last_percent = None;
    let imported = agl_runtime::model::fetch_model(&request, &cancelled, move |progress| {
        if let Some(total) = progress.total_bytes.filter(|total| *total > 0) {
            let percent = progress.downloaded_bytes * 100 / total;
            if last_percent != Some(percent) && percent % 5 == 0 {
                last_percent = Some(percent);
                eprintln!("downloaded {percent}%");
            }
        }
    })
    .await?;
    println!(
        "downloaded and verified {} ({})",
        imported.path().display(),
        gb(imported.bytes())
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_total_is_converted_from_kib() {
        assert_eq!(
            parse_mem_total("MemFree: 1 kB\nMemTotal:       16384 kB\n"),
            Some(16384 * 1024)
        );
        assert_eq!(parse_mem_total("MemTotal: 5\n"), None);
        assert_eq!(parse_mem_total("nothing\n"), None);
    }

    #[test]
    fn fit_uses_decimal_gigabytes() {
        assert_eq!(fit(None, Some(1)), None);
        assert_eq!(fit(Some(16), None), Some(Fit::Unknown));
        assert_eq!(fit(Some(16), Some(16 * BYTES_PER_GB)), Some(Fit::Fits));
        assert_eq!(
            fit(Some(16), Some(16 * BYTES_PER_GB - 1)),
            Some(Fit::BelowMinimum)
        );
    }

    #[test]
    fn largest_vram_takes_the_biggest_reporting_device() {
        let root = std::env::temp_dir().join(format!("agl-drm-{}", std::process::id()));
        for (card, vram) in [
            ("card0", Some("536870912\n")),
            ("card1", Some("25753026560\n")),
            ("card2", None),
        ] {
            std::fs::create_dir_all(root.join(card).join("device")).unwrap();
            if let Some(vram) = vram {
                std::fs::write(root.join(card).join("device/mem_info_vram_total"), vram).unwrap();
            }
        }
        assert_eq!(largest_vram(&root), Some(25_753_026_560));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(largest_vram(&root), None);
    }
}
