use std::path::{Path, PathBuf};

use agl_artifact::{
    ArtifactCandidate, ArtifactPackageView, ArtifactRelativePath, ArtifactSourceKind,
    ArtifactSourceTier,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

#[cfg(test)]
use crate::adapter::validate_function_model_contract;
use crate::locator::{FunctionPackageLocation, FunctionPackageSource};
use crate::manifest::{
    AgentFunctionFrontMatter, FUNCTION_FILE_NAME, FUNCTION_SYSTEM_PROMPT_FILE_NAME,
};
use crate::subagent::{
    SubagentFrontMatter, load_declared_subagents, load_declared_subagents_from_view,
};
use crate::validation::validate_relative_function_file_path;
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MarkdownSection {
    pub title: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoadedFunction {
    pub locator: FunctionPackageLocation,
    pub front_matter: AgentFunctionFrontMatter,
    pub system_prompt_path: PathBuf,
    pub system_prompt: String,
    pub system_prompt_sections: Vec<MarkdownSection>,
    pub inference_config_path: Option<PathBuf>,
    pub inference_config_toml: Option<String>,
    pub subagents: Vec<LoadedSubagent>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoadedSubagent {
    pub path: PathBuf,
    pub front_matter: SubagentFrontMatter,
    pub body: String,
    pub sections: Vec<MarkdownSection>,
    pub source_digest: String,
}

pub(crate) fn load_function(locator: FunctionPackageLocation) -> Result<LoadedFunction> {
    let builtin = if locator.source == FunctionPackageSource::Builtin {
        Some(resolve_builtin_package(&locator.reference)?)
    } else {
        None
    };
    let content = if let Some(function) = builtin {
        let file = function_file(function, FUNCTION_FILE_NAME)?;
        std::str::from_utf8(file.bytes)
            .with_context(|| format!("builtin function `{}` is not UTF-8", function.id))?
            .to_string()
    } else {
        std::fs::read_to_string(&locator.path)
            .with_context(|| format!("failed to read function {}", locator.path.display()))?
    };
    let (front_matter, body) = parse_function_document(&content)
        .with_context(|| format!("failed to parse function {}", locator.path.display()))?;
    front_matter.validate()?;
    ensure!(
        body.trim().is_empty(),
        "FUNCTION.md body is not supported; put system instructions in SYSTEM.md"
    );
    if !matches!(
        locator.source,
        FunctionPackageSource::Explicit | FunctionPackageSource::Builtin
    ) {
        let directory_id = locator
            .root_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        ensure!(
            directory_id == front_matter.id(),
            "function id `{}` does not match directory `{directory_id}`",
            front_matter.id()
        );
    }
    let subagents = load_declared_subagents(&locator.root_dir, &front_matter)?;
    let (system_prompt_path, system_prompt) =
        load_function_system_prompt(&locator.root_dir, builtin)?;
    let system_prompt_sections = markdown_sections(&system_prompt);
    let (inference_config_path, inference_config_toml) =
        load_function_inference_config(&locator.root_dir, &front_matter, builtin)?;
    #[cfg(test)]
    validate_function_model_contract(&front_matter, inference_config_toml.as_deref(), &locator)?;
    Ok(LoadedFunction {
        locator,
        front_matter,
        system_prompt_path,
        system_prompt,
        system_prompt_sections,
        inference_config_path,
        inference_config_toml,
        subagents,
    })
}

pub fn load_function_candidate(candidate: &ArtifactCandidate) -> Result<LoadedFunction> {
    let content = read_package_text(candidate.view(), FUNCTION_FILE_NAME)?;
    let (front_matter, body) =
        parse_function_document(&content).context("failed to parse resolved Function payload")?;
    front_matter.validate()?;
    ensure!(
        body.trim().is_empty(),
        "FUNCTION.md body is not supported; put system instructions in SYSTEM.md"
    );
    ensure!(
        front_matter.id() == candidate.package_id.as_str(),
        "function id `{}` does not match resolved candidate `{}`",
        front_matter.id(),
        candidate.package_id
    );
    let source = match (candidate.tier, candidate.kind) {
        (ArtifactSourceTier::Explicit, _) => FunctionPackageSource::Explicit,
        (ArtifactSourceTier::Workspace, _) => FunctionPackageSource::Workspace,
        (_, ArtifactSourceKind::Embedded) => FunctionPackageSource::Builtin,
        _ => FunctionPackageSource::Global,
    };
    let root_dir = candidate.package_root.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "artifact/function/{}@{}",
            candidate.package_id, candidate.version
        ))
    });
    let locator = FunctionPackageLocation {
        reference: format!("function:{}@{}", candidate.package_id, candidate.version),
        source,
        path: root_dir.join(FUNCTION_FILE_NAME),
        root_dir: root_dir.clone(),
    };
    let system_prompt = read_package_text(candidate.view(), FUNCTION_SYSTEM_PROMPT_FILE_NAME)?;
    ensure!(
        !system_prompt.trim().is_empty(),
        "function system prompt cannot be empty"
    );
    let system_prompt_path = root_dir.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME);
    let inference_config_toml = front_matter
        .model_config_path()
        .map(|path| read_package_text(candidate.view(), path))
        .transpose()?;
    let inference_config_path = front_matter
        .model_config_path()
        .map(|path| root_dir.join(path));
    let subagents = load_declared_subagents_from_view(candidate.view(), &root_dir, &front_matter)?;
    Ok(LoadedFunction {
        locator,
        front_matter,
        system_prompt_sections: markdown_sections(&system_prompt),
        system_prompt,
        system_prompt_path,
        inference_config_path,
        inference_config_toml,
        subagents,
    })
}

fn read_package_text(package: &dyn ArtifactPackageView, path: &str) -> Result<String> {
    let relative = ArtifactRelativePath::new(path.to_owned())?;
    let bytes = package.read_file(&relative)?;
    String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))
}

pub(crate) fn load_function_system_prompt(
    function_root: &Path,
    builtin: Option<&'static agl_assets::BuiltinArtifactPackage>,
) -> Result<(PathBuf, String)> {
    if let Some(function) = builtin {
        let file = function_file(function, FUNCTION_SYSTEM_PROMPT_FILE_NAME)?;
        let content = std::str::from_utf8(file.bytes).with_context(|| {
            format!(
                "builtin function `{}` system prompt is not UTF-8",
                function.id
            )
        })?;
        ensure!(
            !content.trim().is_empty(),
            "function system prompt cannot be empty: {}",
            file.path
        );
        return Ok((
            PathBuf::from(format!("builtin:function/{}/{}", function.id, file.path)),
            content.to_string(),
        ));
    }
    let path = resolve_function_relative_path(function_root, FUNCTION_SYSTEM_PROMPT_FILE_NAME)?;
    ensure!(
        path.is_file(),
        "function system prompt file not found: {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read function system prompt {}", path.display()))?;
    ensure!(
        !content.trim().is_empty(),
        "function system prompt cannot be empty: {}",
        path.display()
    );

    Ok((path, content))
}

pub(crate) fn load_function_inference_config(
    function_root: &Path,
    front_matter: &AgentFunctionFrontMatter,
    builtin: Option<&'static agl_assets::BuiltinArtifactPackage>,
) -> Result<(Option<PathBuf>, Option<String>)> {
    let Some(relative) = front_matter.model_config_path() else {
        return Ok((None, None));
    };
    if let Some(function) = builtin {
        ensure!(
            relative == "inference.toml",
            "builtin function `{}` can only load model.config: inference.toml",
            function.id
        );
        let file = function_file(function, relative)?;
        let content = std::str::from_utf8(file.bytes).with_context(|| {
            format!(
                "builtin function `{}` inference config is not UTF-8",
                function.id
            )
        })?;
        ensure!(
            !content.trim().is_empty(),
            "function inference config cannot be empty: {}",
            file.path
        );
        return Ok((
            Some(PathBuf::from(format!(
                "builtin:function/{}/{}",
                function.id, file.path
            ))),
            Some(content.to_string()),
        ));
    }

    let path = resolve_function_relative_path(function_root, relative)?;
    ensure!(
        path.is_file(),
        "function inference config file not found: {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read function inference config {}",
            path.display()
        )
    })?;
    ensure!(
        !content.trim().is_empty(),
        "function inference config cannot be empty: {}",
        path.display()
    );
    Ok((Some(path), Some(content)))
}

pub(crate) fn resolve_builtin_package(
    reference: &str,
) -> Result<&'static agl_assets::BuiltinArtifactPackage> {
    agl_assets::builtin_artifact_package(reference)
        .with_context(|| format!("builtin function `{reference}` is not embedded"))
}

fn function_file<'a>(
    function: &'a agl_assets::BuiltinArtifactPackage,
    path: &str,
) -> Result<&'a agl_assets::BuiltinArtifactFile> {
    function
        .files
        .iter()
        .find(|file| file.path == path)
        .with_context(|| format!("builtin function `{}` is missing {path}", function.id))
}

pub(crate) fn resolve_function_relative_path(
    function_root: &Path,
    relative: &str,
) -> Result<PathBuf> {
    validate_relative_function_file_path("function file path", relative)?;
    let path = function_root.join(relative);
    ensure!(
        path.starts_with(function_root),
        "function file path escapes function root: {}",
        path.display()
    );
    Ok(path)
}

pub(crate) fn parse_function_document(content: &str) -> Result<(AgentFunctionFrontMatter, String)> {
    let (yaml, body) = split_front_matter(content)?;
    let front_matter = serde_yaml::from_str::<AgentFunctionFrontMatter>(&yaml)
        .context("failed to parse function YAML front matter")?;
    Ok((front_matter, body))
}

pub(crate) fn parse_subagent_document(content: &str) -> Result<(SubagentFrontMatter, String)> {
    let (yaml, body) = split_front_matter(content)?;
    let front_matter = serde_yaml::from_str::<SubagentFrontMatter>(&yaml)
        .context("failed to parse subagent YAML front matter")?;
    Ok((front_matter, body))
}

pub(crate) fn split_front_matter(content: &str) -> Result<(String, String)> {
    let mut lines = content.lines();
    let Some(first) = lines.next() else {
        bail!("document is empty");
    };
    ensure!(
        first.trim_end_matches('\r') == "---",
        "document must start with YAML front matter"
    );

    let mut yaml = String::new();
    let mut closed = false;
    for line in &mut lines {
        if line.trim_end_matches('\r') == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    ensure!(closed, "YAML front matter is not closed");
    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((yaml, body))
}

pub(crate) fn markdown_sections(body: &str) -> Vec<MarkdownSection> {
    let mut sections = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_content = String::new();
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("# ") {
            if let Some(title) = current_title.take() {
                sections.push(MarkdownSection {
                    title,
                    content: current_content.trim().to_string(),
                });
                current_content.clear();
            }
            current_title = Some(title.trim().to_string());
        } else if current_title.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }
    if let Some(title) = current_title {
        sections.push(MarkdownSection {
            title,
            content: current_content.trim().to_string(),
        });
    }
    sections
}
