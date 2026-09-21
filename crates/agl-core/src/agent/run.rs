use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use crate::Content;
use crate::package::{PackageId, PackageVersion};
use crate::{AgentRunId, ConversationId, MessageId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{AuthorityGrantSet, ExtensionId, ToolDefinition, ToolId};

use super::{
    AgentEventId, AgentOperationFailureKind, AgentOperationKey, ModelRuntimeSelection, ToolCall,
};

pub const MAX_TOOL_RESULT_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputPresentation {
    pub lines: u32,
    pub chars: u32,
}

impl Default for ToolOutputPresentation {
    fn default() -> Self {
        Self {
            lines: 10,
            chars: 500,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPresentation {
    pub tool_output: ToolOutputPresentation,
    pub tool: ToolPresentation,
    pub colors: PresentationColors,
    pub model_generation: ModelGenerationPresentation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPresentation {
    pub frame: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationColors {
    pub rule: String,
    pub run: String,
    pub run_id: String,
    pub status_success: String,
    pub status_failure: String,
    pub status_pending: String,
    pub operation: String,
    pub tool: String,
    pub ordinal: String,
    pub field: String,
    pub muted: String,
    pub json_key: String,
    pub json_string: String,
    pub json_number: String,
    pub json_boolean: String,
    pub json_null: String,
    pub markdown_heading: String,
    pub markdown_code: String,
    pub markdown_inline_code: String,
    pub markdown_strong: String,
    pub markdown_emphasis: String,
    pub markdown_link: String,
    pub markdown_quote: String,
    pub markdown_bullet: String,
    pub markdown_rule: String,
    pub input_rule: String,
    pub input_background: String,
    pub input_prompt: String,
    pub input_hint: String,
    pub input_text: String,
    pub input_activity: String,
    pub input_selected: String,
}

impl Default for PresentationColors {
    fn default() -> Self {
        Self {
            rule: "dim".into(),
            run: "bold #FF00FF".into(),
            run_id: "bold #FFFFFF".into(),
            status_success: "bold #7BD88F".into(),
            status_failure: "bold #FF0000".into(),
            status_pending: "bold #FFFF00".into(),
            operation: "bold #00FFFF".into(),
            tool: "bold #00FFFF".into(),
            ordinal: "dim".into(),
            field: "bold #8AA2D8".into(),
            muted: "dim".into(),
            json_key: "#00FFFF".into(),
            json_string: "#7BD88F".into(),
            json_number: "#FFFF00".into(),
            json_boolean: "#FF00FF".into(),
            json_null: "dim".into(),
            markdown_heading: "bold #00FFFF".into(),
            markdown_code: "dim".into(),
            markdown_inline_code: "#FFFF00".into(),
            markdown_strong: "bold".into(),
            markdown_emphasis: "italic".into(),
            markdown_link: "#8AA2D8".into(),
            markdown_quote: "dim".into(),
            markdown_bullet: "#00FFFF".into(),
            markdown_rule: "dim".into(),
            input_rule: "oklch(0.439 0 0)".into(),
            input_background: "oklch(0.269 0 0)".into(),
            input_prompt: "bold oklch(0.718 0.202 349.761)".into(),
            input_hint: "dim oklch(0.823 0.12 346.018)".into(),
            input_text: "oklch(0.936 0.032 17.717)".into(),
            input_activity: "oklch(0.823 0.12 346.018)".into(),
            input_selected: "bold oklch(0.518 0.253 323.949)".into(),
        }
    }
}

impl PresentationColors {
    pub fn validate(&self) -> Result<(), String> {
        let fields = [
            ("rule", self.rule.as_str()),
            ("run", self.run.as_str()),
            ("run_id", self.run_id.as_str()),
            ("status_success", self.status_success.as_str()),
            ("status_failure", self.status_failure.as_str()),
            ("status_pending", self.status_pending.as_str()),
            ("operation", self.operation.as_str()),
            ("tool", self.tool.as_str()),
            ("ordinal", self.ordinal.as_str()),
            ("field", self.field.as_str()),
            ("muted", self.muted.as_str()),
            ("json_key", self.json_key.as_str()),
            ("json_string", self.json_string.as_str()),
            ("json_number", self.json_number.as_str()),
            ("json_boolean", self.json_boolean.as_str()),
            ("json_null", self.json_null.as_str()),
            ("markdown_heading", self.markdown_heading.as_str()),
            ("markdown_code", self.markdown_code.as_str()),
            ("markdown_inline_code", self.markdown_inline_code.as_str()),
            ("markdown_strong", self.markdown_strong.as_str()),
            ("markdown_emphasis", self.markdown_emphasis.as_str()),
            ("markdown_link", self.markdown_link.as_str()),
            ("markdown_quote", self.markdown_quote.as_str()),
            ("markdown_bullet", self.markdown_bullet.as_str()),
            ("markdown_rule", self.markdown_rule.as_str()),
            ("input_rule", self.input_rule.as_str()),
            ("input_background", self.input_background.as_str()),
            ("input_prompt", self.input_prompt.as_str()),
            ("input_hint", self.input_hint.as_str()),
            ("input_text", self.input_text.as_str()),
            ("input_activity", self.input_activity.as_str()),
            ("input_selected", self.input_selected.as_str()),
        ];
        for (name, value) in fields {
            validate_presentation_style(value)
                .map_err(|error| format!("presentation.colors.{name}: {error}"))?;
        }
        Ok(())
    }
}

fn validate_presentation_style(value: &str) -> Result<(), String> {
    let value = value.trim();
    if value == "none" {
        return Ok(());
    }
    let mut rest = value;
    loop {
        let mut consumed_attribute = false;
        for attribute in ["bold", "dim", "italic", "underline"] {
            if rest == attribute {
                return Ok(());
            }
            if let Some(next) = rest
                .strip_prefix(attribute)
                .and_then(|next| next.strip_prefix(char::is_whitespace))
            {
                rest = next.trim_start();
                consumed_attribute = true;
                break;
            }
        }
        if !consumed_attribute {
            break;
        }
    }
    if let Some(hex) = rest.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(());
        }
        return Err("expected #RRGGBB".into());
    }
    let Some(payload) = rest
        .strip_prefix("oklch(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return Err("expected none, style attributes, #RRGGBB, or oklch(L C H)".into());
    };
    let values = payload
        .split_whitespace()
        .map(|value| {
            value
                .parse::<f32>()
                .map_err(|_| "OKLCH values must be numbers")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() != 3 || !values.iter().all(|value| value.is_finite()) {
        return Err("OKLCH requires three finite values".into());
    }
    if !(0.0..=1.0).contains(&values[0]) || values[1] < 0.0 {
        return Err("OKLCH lightness must be 0..1 and chroma non-negative".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGenerationPresentation {
    pub details: bool,
}

macro_rules! digest_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "sha256:{}", hex(&self.0))
            }
        }

        impl std::str::FromStr for $name {
            type Err = &'static str;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_digest(value).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                parse_digest(&value)
                    .map(Self)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_digest(value: &str) -> Result<[u8; 32], &'static str> {
    let value = value
        .strip_prefix("sha256:")
        .ok_or("digest requires sha256 prefix")?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("digest requires 64 lowercase hexadecimal characters");
    }
    let mut bytes = [0; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        bytes[index] = u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII checked"), 16)
            .map_err(|_| "invalid digest")?;
    }
    Ok(bytes)
}

digest_type!(PackageDigest);
digest_type!(InstructionDigest);
digest_type!(ToolDefinitionDigest);
digest_type!(ExtensionDefinitionDigest);
digest_type!(InferenceRuntimeProfileDigest);
digest_type!(InferenceEngineBuildDigest);
digest_type!(PhysicalResourceDigest);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinitionRef {
    pub id: PackageId,
    pub version: PackageVersion,
    pub digest: PackageDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDefinitionRef {
    pub id: PackageId,
    pub version: PackageVersion,
    pub digest: PackageDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDefinitionRef {
    pub id: PackageId,
    pub version: PackageVersion,
    pub digest: PackageDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactPackageRef {
    pub id: PackageId,
    pub version: PackageVersion,
    pub digest: PackageDigest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub model: ModelDefinitionRef,
    pub runtime: ModelRuntimeSelection,
    pub reasoning_efforts: Vec<super::ReasoningEffort>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionSource {
    Agent,
    Skill(SkillDefinitionRef),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionBlock {
    pub source: InstructionSource,
    pub content: Content,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionSet {
    pub blocks: Vec<InstructionBlock>,
    pub digest: InstructionDigest,
}

impl InstructionSet {
    pub fn new(blocks: Vec<InstructionBlock>) -> Result<Self, &'static str> {
        if blocks.len() > 256 {
            return Err("instruction set exceeds 256 blocks");
        }
        for block in &blocks {
            block
                .content
                .validate()
                .map_err(|_| "instruction block contains invalid Content")?;
        }
        let encoded = serde_json::to_vec(&blocks).map_err(|_| "instructions are not encodable")?;
        if encoded.len() > 4 * 1024 * 1024 {
            return Err("instruction set exceeds 4 MiB");
        }
        let mut hasher = Sha256::new();
        hasher.update(b"agentlibre.instructions.v1\0");
        hasher.update(encoded);
        Ok(Self {
            blocks,
            digest: InstructionDigest::from_bytes(hasher.finalize().into()),
        })
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let expected = Self::new(self.blocks.clone())?;
        if expected.digest != self.digest {
            return Err("instruction digest does not match ordered blocks");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AbsolutePath(PathBuf);

impl TryFrom<String> for AbsolutePath {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err("path must be absolute");
        }
        Ok(Self(path))
    }
}
impl From<AbsolutePath> for String {
    fn from(value: AbsolutePath) -> Self {
        value.0.to_string_lossy().into_owned()
    }
}
impl AbsolutePath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelativePath(PathBuf);
impl TryFrom<String> for RelativePath {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let path = PathBuf::from(value);
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err("path must be contained and relative");
        }
        Ok(Self(path))
    }
}
impl From<RelativePath> for String {
    fn from(value: RelativePath) -> Self {
        value.0.to_string_lossy().into_owned()
    }
}
impl RelativePath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceScope {
    pub root: AbsolutePath,
    pub working_directory: RelativePath,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDefinitionRef {
    pub id: ExtensionId,
    pub package: ExactPackageRef,
    pub definition_digest: ExtensionDefinitionDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedTool {
    pub definition: ToolDefinition,
    pub extension: ExtensionDefinitionRef,
    pub definition_digest: ToolDefinitionDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunLimits {
    pub deadline_ms: i64,
    pub model_input_tokens: Option<u64>,
    pub model_output_tokens: u64,
    pub model_calls: u64,
    pub correction_input_tokens: u64,
    pub correction_output_tokens: u64,
    pub correction_calls: u64,
    pub tool_calls: u64,
    pub tool_result_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunUsage {
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_calls: u64,
    pub correction_input_tokens: u64,
    pub correction_output_tokens: u64,
    pub correction_calls: u64,
    pub tool_calls: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvalidModelOutputRecoverySelection {
    pub model: ModelSelection,
    pub max_attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunSnapshot {
    pub agent: AgentDefinitionRef,
    pub model: ModelSelection,
    pub invalid_model_output_recovery: Option<InvalidModelOutputRecoverySelection>,
    pub instructions: InstructionSet,
    pub workspace: WorkspaceScope,
    pub tools: Vec<AdmittedTool>,
    pub authority: AuthorityGrantSet,
    pub limits: AgentRunLimits,
    pub presentation: AgentPresentation,
    /// Runtime-owned structured output contract for specialized operations.
    pub response_format: Option<serde_json::Value>,
    /// Restrict tool execution to the planner's read-only workspace policy.
    pub planner_read_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRunOrigin {
    User {
        conversation_id: ConversationId,
        message_id: MessageId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunSpec {
    pub origin: AgentRunOrigin,
    pub input: Content,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<super::ReasoningEffort>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl AgentRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentCheckpoint {
    Ready {
        context: Vec<MessageId>,
        next_operation_ordinal: NonZeroU32,
    },
    Waiting {
        operation: AgentOperationKey,
        context: Vec<MessageId>,
        next_operation_ordinal: NonZeroU32,
        pending_tool_calls: Vec<ToolCall>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunFailureKind {
    Deadline,
    Operation,
    InvalidState,
    LimitsExceeded,
    ToolLoopDetected,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRun {
    pub id: AgentRunId,
    pub origin: AgentRunOrigin,
    pub snapshot: AgentRunSnapshot,
    pub status: AgentRunStatus,
    pub checkpoint: AgentCheckpoint,
    pub usage: AgentRunUsage,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRunView {
    pub id: AgentRunId,
    pub origin: AgentRunOrigin,
    pub status: AgentRunStatus,
    pub usage: AgentRunUsage,
    pub current_operation: Option<AgentOperationKey>,
    pub failure: Option<AgentRunFailureView>,
    pub last_event_id: AgentEventId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentRunFailureView {
    Run {
        kind: AgentRunFailureKind,
    },
    Operation {
        operation: AgentOperationKey,
        kind: AgentOperationFailureKind,
        tool_id: Option<ToolId>,
    },
}

pub(crate) fn checked_next(value: NonZeroU32) -> Option<NonZeroU32> {
    value.get().checked_add(1).and_then(NonZeroU32::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_colors_validate_supported_forms() {
        let colors = PresentationColors {
            tool: "underline italic oklch(0.7 0.15 30)".into(),
            ..Default::default()
        };
        assert!(colors.validate().is_ok());
        assert!(
            !PresentationColors {
                tool: "#1234".into(),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            !PresentationColors {
                tool: "".into(),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn origin_and_run_spec_wire_shapes_are_exact() {
        let origin = AgentRunOrigin::User {
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
        };
        let encoded = serde_json::to_value(&origin).unwrap();
        assert_eq!(
            encoded
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["conversation_id", "message_id", "type"]
        );
        let mut extra = encoded.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("legacy".into(), true.into());
        assert!(serde_json::from_value::<AgentRunOrigin>(extra).is_err());
        let spec = AgentRunSpec {
            reasoning: None,
            origin,
            input: Content::text("prompt").unwrap(),
        };
        let mut spec_value = serde_json::to_value(spec).unwrap();
        assert_eq!(
            spec_value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["input", "origin"]
        );
        spec_value.as_object_mut().unwrap().insert(
            "agent_run_id".into(),
            AgentRunId::generate().to_string().into(),
        );
        assert!(serde_json::from_value::<AgentRunSpec>(spec_value).is_err());
    }

    #[test]
    fn every_digest_has_one_strict_wire_form() {
        let value = format!("sha256:{}", "ab".repeat(32));
        let digest: PackageDigest = value.parse().unwrap();
        assert_eq!(digest.to_string(), value);
        assert_eq!(digest.as_bytes(), &[0xab; 32]);
        assert!(
            format!("sha256:{}", "AB".repeat(32))
                .parse::<PackageDigest>()
                .is_err()
        );
        assert!("ab".repeat(32).parse::<PackageDigest>().is_err());
    }
}
