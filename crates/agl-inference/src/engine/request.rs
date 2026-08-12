use std::fs;
use std::io::Write as _;
use std::os::fd::RawFd;
use std::os::unix::fs::{DirBuilderExt as _, symlink};
use std::path::{Path, PathBuf};

use agl_content::ContentPart;
use agl_model::{ModelArtifactRole, ModelExecutionPlan, StructuredGenerationMode};
use agl_oven::RenderedMessageRole;
use serde_json::{Value, json};

use crate::host::descriptors::VerifiedDescriptorSet;
use crate::{InferenceFailure, InferenceRequest, ResolvedMediaAttachment};

use super::process::{ARTIFACT_FD_BASE, SpeculativeReadiness};
use super::transport::{protocol, protocol_io, protocol_io_context};

const MAX_PRIVATE_REQUEST_BYTES: usize = 72 * 1024 * 1024;

pub(super) fn launch_args(
    plan: &ModelExecutionPlan,
    mappings: &[(ModelArtifactRole, PathBuf)],
    slot_root: &Path,
    engine_device_identity: Option<&str>,
) -> Result<Vec<String>, InferenceFailure> {
    let one = |role| {
        mappings
            .iter()
            .find(|(candidate, _)| *candidate == role)
            .map(|(_, path)| path.to_string_lossy().into_owned())
    };
    let main = one(ModelArtifactRole::Main)
        .ok_or_else(|| protocol("execution plan has no main Model artifact"))?;
    let runtime = plan.runtime();
    let mut args = vec![
        "--model".to_owned(),
        main,
        "--ctx-size".to_owned(),
        runtime.context_tokens().to_string(),
        "--batch-size".to_owned(),
        runtime.batch_size().to_string(),
        "--ubatch-size".to_owned(),
        runtime.ubatch_size().to_string(),
        "--threads".to_owned(),
        runtime.threads().to_string(),
        "--threads-batch".to_owned(),
        runtime.threads().to_string(),
        "--gpu-layers".to_owned(),
        runtime.gpu_layers().to_string(),
        "--flash-attn".to_owned(),
        if runtime.flash_attention() {
            "on"
        } else {
            "off"
        }
        .to_owned(),
        "--cache-type-k".to_owned(),
        runtime.key_cache_type().to_owned(),
        "--cache-type-v".to_owned(),
        runtime.value_cache_type().to_owned(),
        if runtime.mmap() {
            "--mmap"
        } else {
            "--no-mmap"
        }
        .to_owned(),
        if runtime.unified_kv() {
            "--kv-unified"
        } else {
            "--no-kv-unified"
        }
        .to_owned(),
        "--fit".to_owned(),
        "off".to_owned(),
        "--parallel".to_owned(),
        "1".to_owned(),
        "--no-cont-batching".to_owned(),
        "--no-context-shift".to_owned(),
        "--no-warmup".to_owned(),
        "--no-ui".to_owned(),
        "--jinja".to_owned(),
        "--reasoning".to_owned(),
        "off".to_owned(),
        "--reasoning-format".to_owned(),
        "none".to_owned(),
        "--slot-prompt-similarity".to_owned(),
        "0".to_owned(),
        "--sleep-idle-seconds".to_owned(),
        "-1".to_owned(),
        "--threads-http".to_owned(),
        "2".to_owned(),
        "--slots".to_owned(),
        "--slot-save-path".to_owned(),
        slot_root.to_string_lossy().into_owned(),
        "--log-disable".to_owned(),
    ];
    if let Some(device) = engine_device_identity {
        args.extend(["--device".to_owned(), device.to_owned()]);
    } else {
        args.extend(["--device".to_owned(), "none".to_owned()]);
    }
    if let Some(projector) = one(ModelArtifactRole::Projector) {
        args.extend(["--mmproj".to_owned(), projector]);
    } else {
        args.push("--no-mmproj".to_owned());
    }
    if let Some(draft) = one(ModelArtifactRole::Draft) {
        args.extend(["--model-draft".to_owned(), draft]);
        let mtp = runtime
            .mtp()
            .ok_or_else(|| protocol("Draft artifact has no exact MTP runtime shape"))?;
        args.extend([
            "--spec-type".to_owned(),
            "draft-simple".to_owned(),
            "--spec-draft-n-max".to_owned(),
            mtp.max_draft_tokens().to_string(),
            "--spec-draft-n-min".to_owned(),
            mtp.min_draft_tokens().to_string(),
            "--spec-draft-p-min".to_owned(),
            format_millionths(mtp.p_min_millionths()),
            "--spec-draft-ngl".to_owned(),
            mtp.gpu_layers().to_string(),
            "--spec-draft-type-k".to_owned(),
            mtp.key_cache_type().to_owned(),
            "--spec-draft-type-v".to_owned(),
            mtp.value_cache_type().to_owned(),
            "--spec-draft-device".to_owned(),
            plan.selected_device()
                .map(|device| device.identity.clone())
                .unwrap_or_else(|| "none".to_owned()),
        ]);
    } else if runtime.mtp().is_some() {
        return Err(protocol("MTP runtime shape has no Draft artifact"));
    }
    Ok(args)
}

pub(super) fn speculative_readiness_matches(
    actual: &SpeculativeReadiness,
    expected: Option<&agl_model::ModelMtpShape>,
) -> bool {
    match expected {
        None => !actual.enabled && actual.kind == "none",
        Some(expected) => {
            actual.enabled
                && actual.kind == "draft-simple"
                && actual.max_draft_tokens == expected.max_draft_tokens()
                && actual.min_draft_tokens == expected.min_draft_tokens()
                && actual.p_min_millionths == expected.p_min_millionths()
                && actual.gpu_layers == i32::try_from(expected.gpu_layers()).unwrap_or(i32::MAX)
                && actual.key_cache_type == expected.key_cache_type()
                && actual.value_cache_type == expected.value_cache_type()
        }
    }
}

pub(super) fn format_millionths(value: u32) -> String {
    format!("{}.{:06}", value / 1_000_000, value % 1_000_000)
}

pub(super) fn descriptor_mappings(
    directory: &Path,
    descriptors: &VerifiedDescriptorSet,
) -> Result<Vec<(ModelArtifactRole, PathBuf)>, InferenceFailure> {
    let artifact_root = directory.join("artifacts");
    let mut builder = fs::DirBuilder::new();
    builder
        .mode(0o700)
        .create(&artifact_root)
        .map_err(|error| protocol_io_context("create descriptor mapping directory", error))?;
    let mut mappings = Vec::with_capacity(descriptors.files.len());
    for (index, artifact) in descriptors.files.iter().enumerate() {
        let path = artifact_root.join(&artifact.basename);
        symlink(
            format!("/proc/self/fd/{}", ARTIFACT_FD_BASE + index as RawFd),
            &path,
        )
        .map_err(|error| protocol_io_context("create descriptor-backed artifact", error))?;
        mappings.push((artifact.role, path));
    }
    Ok(mappings)
}

fn request_json(
    plan: &ModelExecutionPlan,
    request: &InferenceRequest,
) -> Result<Vec<u8>, InferenceFailure> {
    let messages = request
        .rendered
        .messages
        .iter()
        .map(|message| {
            let role = match message.role {
                RenderedMessageRole::System => "system",
                RenderedMessageRole::User => "user",
                RenderedMessageRole::Assistant => "assistant",
                RenderedMessageRole::Tool => "tool",
            };
            let content = match &message.content {
                None => Value::Null,
                Some(content) => {
                    let has_attachments = content
                        .parts
                        .iter()
                        .any(|part| matches!(part, ContentPart::Attachment { .. }));
                    let mut text = String::new();
                    let mut ordered = Vec::new();
                    for part in &content.parts {
                        match part {
                            ContentPart::Text { text: part } => {
                                if has_attachments {
                                    ordered.push(json!({"type": "text", "text": part}));
                                } else {
                                    text.push_str(part);
                                }
                            }
                            ContentPart::Attachment { attachment } => {
                                ordered.push(json!({
                                    "type": "agl_attachment",
                                    "id": attachment.content_attachment_id.as_str(),
                                    "media_type": attachment.media_type.mime(),
                                    "byte_length": attachment.byte_length,
                                }));
                            }
                        }
                    }
                    if has_attachments {
                        Value::Array(ordered)
                    } else {
                        Value::String(text)
                    }
                }
            };
            let mut value = json!({"role": role, "content": content});
            if let Some(name) = &message.name {
                value["name"] = Value::String(name.clone());
            }
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| {
                            json!({
                                "id": format!("call_{index}"),
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments.to_string(),
                                }
                            })
                        })
                        .collect(),
                );
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>, InferenceFailure>>()?;
    let tools = request
        .rendered
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();
    let policy = plan.generation_policy();
    let mut body = json!({
        "agl_attempt_id": request.attempt_id.as_str(),
        "messages": messages,
        "stream": false,
        "temperature": 0.0,
        "top_k": 1,
        "top_p": 1.0,
        "min_p": 0.0,
        "max_tokens": policy.max_output_tokens(),
        "stop": policy.stop_rules(),
        "id_slot": 0,
        "cache_prompt": true,
        "parallel_tool_calls": false,
    });
    match policy.structured_mode() {
        StructuredGenerationMode::Disabled => {}
        StructuredGenerationMode::LazyTool => {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = Value::String("auto".to_owned());
        }
        StructuredGenerationMode::RequiredTool => {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = Value::String("required".to_owned());
        }
    }
    serde_json::to_vec(&body)
        .map_err(|error| protocol(&format!("failed to encode engine request: {error}")))
}

pub(super) struct EngineRequestBody {
    pub(super) bytes: Vec<u8>,
    pub(super) content_type: String,
}

pub(super) fn request_body(
    plan: &ModelExecutionPlan,
    request: &InferenceRequest,
    media: &[ResolvedMediaAttachment],
) -> Result<EngineRequestBody, InferenceFailure> {
    let request_json = request_json(plan, request)?;
    if request_json.len() > MAX_PRIVATE_REQUEST_BYTES {
        return Err(protocol("private engine request exceeds 72 MiB"));
    }
    if media.is_empty() {
        return Ok(EngineRequestBody {
            bytes: request_json,
            content_type: "application/json".to_owned(),
        });
    }
    let boundary = format!("agl-{}", request.attempt_id.as_str());
    let mut bytes = Vec::new();
    write!(
        bytes,
        "--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n"
    )
    .map_err(protocol_io)?;
    bytes.extend_from_slice(&request_json);
    bytes.extend_from_slice(b"\r\n");
    for attachment in media {
        write!(
            bytes,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            attachment.reference.content_attachment_id,
            attachment.reference.content_attachment_id,
            attachment.reference.media_type.mime(),
        )
        .map_err(protocol_io)?;
        bytes.extend_from_slice(&attachment.bytes);
        bytes.extend_from_slice(b"\r\n");
    }
    write!(bytes, "--{boundary}--\r\n").map_err(protocol_io)?;
    if bytes.len() > MAX_PRIVATE_REQUEST_BYTES {
        return Err(protocol("private multipart request exceeds 72 MiB"));
    }
    Ok(EngineRequestBody {
        bytes,
        content_type: format!("multipart/form-data; boundary={boundary}"),
    })
}
