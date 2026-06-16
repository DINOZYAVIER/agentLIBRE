use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::{InferenceAttemptId, InferenceRunId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceArtifactRoot {
    root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceArtifactPaths {
    run_dir: PathBuf,
    events_jsonl: PathBuf,
    attempt_dir: PathBuf,
    request_json: PathBuf,
    response_json: PathBuf,
    runtime_log: PathBuf,
}

impl InferenceArtifactRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_dir(&self, run_id: &InferenceRunId) -> PathBuf {
        self.root.join("inference-runs").join(run_id.as_str())
    }

    pub fn events_jsonl(&self, run_id: &InferenceRunId) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    pub fn paths(
        &self,
        run_id: &InferenceRunId,
        attempt_id: &InferenceAttemptId,
    ) -> InferenceArtifactPaths {
        let run_dir = self.run_dir(run_id);
        let attempt_dir = run_dir.join("attempts").join(attempt_id.as_str());
        InferenceArtifactPaths {
            events_jsonl: run_dir.join("events.jsonl"),
            run_dir,
            request_json: attempt_dir.join("request.json"),
            response_json: attempt_dir.join("response.json"),
            runtime_log: attempt_dir.join("runtime.log"),
            attempt_dir,
        }
    }
}

impl InferenceArtifactPaths {
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    pub fn events_jsonl(&self) -> &Path {
        &self.events_jsonl
    }

    pub fn attempt_dir(&self) -> &Path {
        &self.attempt_dir
    }

    pub fn request_json(&self) -> &Path {
        &self.request_json
    }

    pub fn response_json(&self) -> &Path {
        &self.response_json
    }

    pub fn runtime_log(&self) -> &Path {
        &self.runtime_log
    }

    pub fn write_request_json<T>(&self, request: &T) -> Result<&Path>
    where
        T: Serialize + ?Sized,
    {
        write_json_artifact(self.request_json(), request)?;
        Ok(self.request_json())
    }

    pub fn write_response_json<T>(&self, response: &T) -> Result<&Path>
    where
        T: Serialize + ?Sized,
    {
        write_json_artifact(self.response_json(), response)?;
        Ok(self.response_json())
    }

    pub fn write_runtime_log(&self, content: impl AsRef<[u8]>) -> Result<&Path> {
        write_bytes_artifact(self.runtime_log(), content.as_ref())?;
        Ok(self.runtime_log())
    }
}

fn write_json_artifact<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let mut bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize JSON artifact {}", path.display()))?;
    bytes.push(b'\n');
    write_bytes_artifact(path, &bytes)
}

fn write_bytes_artifact(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("artifact path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create inference artifact directory {}",
            parent.display()
        )
    })?;
    std::fs::write(path, bytes)
        .with_context(|| format!("failed to write inference artifact {}", path.display()))
}
