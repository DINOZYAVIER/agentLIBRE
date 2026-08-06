use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::{
    DeclarationDigest, DeclarationError, EffectId, ExtensionDescriptor, ExtensionId,
    ExtensionTrust, OperationKind, PolicyHash, SensitiveInput, SkillId, ToolDeclaration,
    ToolGrantProvenance, ToolId, ToolInvocation,
};

#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAccessMode {
    #[default]
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

impl ToolAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
        }
    }

    pub fn permits(self, declaration: &ToolDeclaration) -> bool {
        match self {
            Self::ReadOnly => match declaration.operation_kind {
                OperationKind::Read | OperationKind::Request => true,
                OperationKind::Write
                | OperationKind::Execute
                | OperationKind::Approve
                | OperationKind::Admin => false,
            },
            Self::Write => match declaration.operation_kind {
                OperationKind::Read | OperationKind::Request | OperationKind::Write => true,
                OperationKind::Execute | OperationKind::Approve | OperationKind::Admin => false,
            },
            Self::Execute => match declaration.operation_kind {
                OperationKind::Read
                | OperationKind::Request
                | OperationKind::Write
                | OperationKind::Execute => true,
                OperationKind::Approve | OperationKind::Admin => false,
            },
            Self::Approve => match declaration.operation_kind {
                OperationKind::Read
                | OperationKind::Request
                | OperationKind::Write
                | OperationKind::Execute
                | OperationKind::Approve => true,
                OperationKind::Admin => false,
            },
            Self::Admin => match declaration.operation_kind {
                OperationKind::Read
                | OperationKind::Request
                | OperationKind::Write
                | OperationKind::Execute
                | OperationKind::Approve
                | OperationKind::Admin => true,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionToolPolicy {
    pub allow: BTreeSet<ToolId>,
    pub deny: BTreeSet<ToolId>,
}

impl FunctionToolPolicy {
    pub fn new(
        allow: impl IntoIterator<Item = ToolId>,
        deny: impl IntoIterator<Item = ToolId>,
    ) -> Self {
        Self {
            allow: allow.into_iter().collect(),
            deny: deny.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillToolPolicy {
    pub skill_id: SkillId,
    pub allow: BTreeSet<ToolId>,
    pub requestable: BTreeSet<ToolId>,
    pub deny: BTreeSet<ToolId>,
}

impl SkillToolPolicy {
    pub fn new(skill_id: SkillId, allow: impl IntoIterator<Item = ToolId>) -> Self {
        Self {
            skill_id,
            allow: allow.into_iter().collect(),
            requestable: BTreeSet::new(),
            deny: BTreeSet::new(),
        }
    }

    pub fn with_requestable(mut self, requestable: impl IntoIterator<Item = ToolId>) -> Self {
        self.requestable = requestable.into_iter().collect();
        self
    }

    pub fn with_denied(mut self, deny: impl IntoIterator<Item = ToolId>) -> Self {
        self.deny = deny.into_iter().collect();
        self
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolGrant {
    pub tool_id: ToolId,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ToolGrantProvenance>,
}

impl ToolGrant {
    pub fn new(tool_id: ToolId, max_operation_kind: OperationKind) -> Self {
        Self {
            tool_id,
            max_operation_kind,
            state_effects: BTreeSet::new(),
            sensitive_inputs: BTreeSet::new(),
            provenance: None,
        }
    }

    pub fn with_state_effects(mut self, state_effects: impl IntoIterator<Item = EffectId>) -> Self {
        self.state_effects = state_effects.into_iter().collect();
        self
    }

    pub fn with_sensitive_inputs(
        mut self,
        sensitive_inputs: impl IntoIterator<Item = SensitiveInput>,
    ) -> Self {
        self.sensitive_inputs = sensitive_inputs.into_iter().collect();
        self
    }

    pub fn with_provenance(mut self, provenance: ToolGrantProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    fn permits(&self, declaration: &ToolDeclaration) -> Result<(), ToolExclusionReason> {
        if !self.max_operation_kind.permits(declaration.operation_kind) {
            return Err(ToolExclusionReason::GrantOperationDenied);
        }
        if (!self.state_effects.is_empty() || !declaration.sensitive_inputs.is_empty())
            && !declaration
                .state_effects
                .iter()
                .all(|effect| self.state_effects.contains(effect))
        {
            return Err(ToolExclusionReason::GrantStateEffectDenied);
        }
        if !declaration
            .sensitive_inputs
            .iter()
            .all(|input| self.sensitive_inputs.contains(input))
        {
            return Err(ToolExclusionReason::GrantSensitiveInputDenied);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicyInput {
    pub extensions: Vec<ExtensionDescriptor>,
    pub baseline: BTreeSet<ToolId>,
    pub selected_skills: Vec<SkillToolPolicy>,
    pub grants: Vec<ToolGrant>,
    pub unavailable_capabilities: BTreeSet<ToolId>,
    pub authority_ceiling: Option<BTreeSet<ToolId>>,
    pub function_policy: Option<FunctionToolPolicy>,
    pub tool_mode: ToolAccessMode,
}

impl ToolPolicyInput {
    pub fn new(
        extensions: impl IntoIterator<Item = ExtensionDescriptor>,
        baseline: impl IntoIterator<Item = ToolId>,
        tool_mode: ToolAccessMode,
    ) -> Self {
        Self {
            extensions: extensions.into_iter().collect(),
            baseline: baseline.into_iter().collect(),
            selected_skills: Vec::new(),
            grants: Vec::new(),
            unavailable_capabilities: BTreeSet::new(),
            authority_ceiling: None,
            function_policy: None,
            tool_mode,
        }
    }

    pub fn with_selected_skills(
        mut self,
        selected_skills: impl IntoIterator<Item = SkillToolPolicy>,
    ) -> Self {
        self.selected_skills = selected_skills.into_iter().collect();
        self
    }

    pub fn with_grants(mut self, grants: impl IntoIterator<Item = ToolGrant>) -> Self {
        self.grants = grants.into_iter().collect();
        self
    }

    pub fn with_unavailable_capabilities(
        mut self,
        tools: impl IntoIterator<Item = ToolId>,
    ) -> Self {
        self.unavailable_capabilities = tools.into_iter().collect();
        self
    }

    pub fn with_function_policy(mut self, policy: FunctionToolPolicy) -> Self {
        self.function_policy = Some(policy);
        self
    }

    pub fn with_authority_ceiling(mut self, tools: impl IntoIterator<Item = ToolId>) -> Self {
        self.authority_ceiling = Some(tools.into_iter().collect());
        self
    }

    pub fn resolve(self) -> Result<EffectiveToolSet, PolicyResolutionError> {
        EffectiveToolSet::resolve(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExclusionReason {
    NotRouted,
    UnknownTool,
    ExtensionUntrusted,
    ToolModeDenied,
    FunctionAllowDenied,
    SkillDenied,
    FunctionDenied,
    GrantOperationDenied,
    GrantStateEffectDenied,
    GrantSensitiveInputDenied,
    ExtensionUnavailable,
    ParentAuthorityDenied,
}

impl ToolExclusionReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotRouted => "not_routed",
            Self::UnknownTool => "unknown_tool",
            Self::ExtensionUntrusted => "extension_untrusted",
            Self::ToolModeDenied => "tool_mode_denied",
            Self::FunctionAllowDenied => "function_allow_denied",
            Self::SkillDenied => "skill_denied",
            Self::FunctionDenied => "function_denied",
            Self::GrantOperationDenied => "grant_operation_denied",
            Self::GrantStateEffectDenied => "grant_state_effect_denied",
            Self::GrantSensitiveInputDenied => "grant_sensitive_input_denied",
            Self::ExtensionUnavailable => "extension_unavailable",
            Self::ParentAuthorityDenied => "parent_authority_denied",
        }
    }

    pub fn is_grant_resolvable(self) -> bool {
        matches!(
            self,
            Self::NotRouted
                | Self::GrantOperationDenied
                | Self::GrantStateEffectDenied
                | Self::GrantSensitiveInputDenied
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExclusion {
    pub tool_id: ToolId,
    pub reason: ToolExclusionReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveTool {
    extension_id: ExtensionId,
    extension_trust: ExtensionTrust,
    extension_digest: DeclarationDigest,
    declaration_digest: DeclarationDigest,
    declaration: ToolDeclaration,
    authorized_state_effects: BTreeSet<EffectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant_provenance: Option<ToolGrantProvenance>,
}

impl EffectiveTool {
    pub fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }

    pub fn extension_trust(&self) -> ExtensionTrust {
        self.extension_trust
    }

    pub fn declaration_digest(&self) -> &DeclarationDigest {
        &self.declaration_digest
    }

    pub fn extension_digest(&self) -> &DeclarationDigest {
        &self.extension_digest
    }

    pub fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    pub fn authorized_state_effects(&self) -> &BTreeSet<EffectId> {
        &self.authorized_state_effects
    }

    pub fn grant_provenance(&self) -> Option<&ToolGrantProvenance> {
        self.grant_provenance.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveToolSet {
    policy_hash: PolicyHash,
    catalog_digest: DeclarationDigest,
    tool_mode: ToolAccessMode,
    tools: BTreeMap<ToolId, EffectiveTool>,
    exclusions: BTreeMap<ToolId, ToolExclusion>,
}

impl EffectiveToolSet {
    pub fn resolve(input: ToolPolicyInput) -> Result<Self, PolicyResolutionError> {
        let catalog = build_catalog(&input.extensions)?;
        let catalog_digest = extension_catalog_digest(&input.extensions);
        let mut routed = input.baseline.clone();
        let mut skill_denied = BTreeSet::new();
        for skill in &input.selected_skills {
            routed.extend(skill.allow.iter().cloned());
            skill_denied.extend(skill.deny.iter().cloned());
        }

        let mut grants = BTreeMap::<ToolId, Vec<&ToolGrant>>::new();
        for grant in &input.grants {
            grants.entry(grant.tool_id.clone()).or_default().push(grant);
        }

        let mut all_ids = catalog.keys().cloned().collect::<BTreeSet<_>>();
        all_ids.extend(routed.iter().cloned());
        all_ids.extend(grants.keys().cloned());
        if let Some(policy) = &input.function_policy {
            all_ids.extend(policy.allow.iter().cloned());
            all_ids.extend(policy.deny.iter().cloned());
        }
        for skill in &input.selected_skills {
            all_ids.extend(skill.requestable.iter().cloned());
        }
        all_ids.extend(skill_denied.iter().cloned());

        let mut tools = BTreeMap::new();
        let mut exclusions = BTreeMap::new();
        for tool_id in all_ids {
            let Some((extension, declaration)) = catalog.get(&tool_id).copied() else {
                exclude(&mut exclusions, tool_id, ToolExclusionReason::UnknownTool);
                continue;
            };

            if input.unavailable_capabilities.contains(&tool_id) {
                exclude(
                    &mut exclusions,
                    tool_id,
                    ToolExclusionReason::ExtensionUnavailable,
                );
                continue;
            }
            if input
                .authority_ceiling
                .as_ref()
                .is_some_and(|ceiling| !ceiling.contains(&tool_id))
            {
                exclude(
                    &mut exclusions,
                    tool_id,
                    ToolExclusionReason::ParentAuthorityDenied,
                );
                continue;
            }

            let eligible_grant = grants.get(&tool_id).and_then(|candidates| {
                candidates
                    .iter()
                    .find(|grant| grant.permits(declaration).is_ok())
            });
            let mut reason = if !extension.trust.permits_execution() {
                ToolExclusionReason::ExtensionUntrusted
            } else if !input.tool_mode.permits(declaration) {
                ToolExclusionReason::ToolModeDenied
            } else if input
                .function_policy
                .as_ref()
                .is_some_and(|policy| !policy.allow.contains(&tool_id))
            {
                ToolExclusionReason::FunctionAllowDenied
            } else if !declaration.sensitive_inputs.is_empty() && eligible_grant.is_none() {
                grants.get(&tool_id).map_or(
                    ToolExclusionReason::GrantSensitiveInputDenied,
                    |candidates| {
                        candidates
                            .iter()
                            .filter_map(|grant| grant.permits(declaration).err())
                            .min()
                            .unwrap_or(ToolExclusionReason::GrantSensitiveInputDenied)
                    },
                )
            } else if !routed.contains(&tool_id) && eligible_grant.is_none() {
                grants
                    .get(&tool_id)
                    .map_or(ToolExclusionReason::NotRouted, |candidates| {
                        candidates
                            .iter()
                            .filter_map(|grant| grant.permits(declaration).err())
                            .min()
                            .unwrap_or(ToolExclusionReason::NotRouted)
                    })
            } else {
                let mut authorized_state_effects = declaration.state_effects.clone();
                if let Some(grant) = eligible_grant {
                    authorized_state_effects.extend(
                        grant
                            .state_effects
                            .intersection(&declaration.conditional_state_effects)
                            .cloned(),
                    );
                }
                tools.insert(
                    tool_id.clone(),
                    EffectiveTool {
                        extension_id: extension.id.clone(),
                        extension_trust: extension.trust,
                        extension_digest: extension.digest(),
                        declaration_digest: declaration.digest(),
                        declaration: declaration.clone(),
                        authorized_state_effects,
                        grant_provenance: eligible_grant.and_then(|grant| grant.provenance.clone()),
                    },
                );
                continue;
            };

            if skill_denied.contains(&tool_id) {
                reason = ToolExclusionReason::SkillDenied;
            }
            if input
                .function_policy
                .as_ref()
                .is_some_and(|policy| policy.deny.contains(&tool_id))
            {
                reason = ToolExclusionReason::FunctionDenied;
            }
            exclude(&mut exclusions, tool_id, reason);
        }

        // Deny filters apply last, including tools admitted above.
        for tool_id in skill_denied {
            if tools.remove(&tool_id).is_some() {
                exclude(&mut exclusions, tool_id, ToolExclusionReason::SkillDenied);
            }
        }
        if let Some(policy) = &input.function_policy {
            for tool_id in &policy.deny {
                if tools.remove(tool_id).is_some() {
                    exclude(
                        &mut exclusions,
                        tool_id.clone(),
                        ToolExclusionReason::FunctionDenied,
                    );
                }
            }
        }

        #[derive(Serialize)]
        struct HashMaterial<'a> {
            tool_mode: ToolAccessMode,
            extensions: BTreeMap<&'a ExtensionId, ExtensionHashMaterial<'a>>,
            baseline: &'a BTreeSet<ToolId>,
            selected_skills: BTreeMap<&'a SkillId, SkillHashMaterial<'a>>,
            grants: BTreeSet<&'a ToolGrant>,
            unavailable_capabilities: &'a BTreeSet<ToolId>,
            authority_ceiling: &'a Option<BTreeSet<ToolId>>,
            function_policy: &'a Option<FunctionToolPolicy>,
            tools: &'a BTreeMap<ToolId, EffectiveTool>,
            exclusions: &'a BTreeMap<ToolId, ToolExclusion>,
        }
        #[derive(Serialize)]
        struct ExtensionHashMaterial<'a> {
            name: &'a str,
            version: &'a str,
            source: crate::ExtensionSource,
            trust: ExtensionTrust,
            hooks: BTreeMap<&'a crate::HookId, &'a crate::HookDeclaration>,
            actions: BTreeMap<&'a ToolId, &'a ToolDeclaration>,
        }
        #[derive(Serialize)]
        struct SkillHashMaterial<'a> {
            allow: &'a BTreeSet<ToolId>,
            requestable: &'a BTreeSet<ToolId>,
            deny: &'a BTreeSet<ToolId>,
        }
        let extensions = input
            .extensions
            .iter()
            .map(|extension| {
                (
                    &extension.id,
                    ExtensionHashMaterial {
                        name: &extension.name,
                        version: &extension.version,
                        source: extension.source,
                        trust: extension.trust,
                        hooks: extension
                            .hooks
                            .iter()
                            .map(|hook| (&hook.id, hook))
                            .collect(),
                        actions: extension
                            .tools
                            .iter()
                            .map(|action| (&action.id, action))
                            .collect(),
                    },
                )
            })
            .collect();
        let mut selected_skills = BTreeMap::new();
        for skill in &input.selected_skills {
            if selected_skills
                .insert(
                    &skill.skill_id,
                    SkillHashMaterial {
                        allow: &skill.allow,
                        requestable: &skill.requestable,
                        deny: &skill.deny,
                    },
                )
                .is_some()
            {
                return Err(PolicyResolutionError::DuplicateSkill {
                    id: skill.skill_id.clone(),
                });
            }
        }
        let material = serde_json::to_value(HashMaterial {
            tool_mode: input.tool_mode,
            extensions,
            baseline: &input.baseline,
            selected_skills,
            grants: input.grants.iter().collect(),
            unavailable_capabilities: &input.unavailable_capabilities,
            authority_ceiling: &input.authority_ceiling,
            function_policy: &input.function_policy,
            tools: &tools,
            exclusions: &exclusions,
        })
        .expect("policy hash material is serializable");
        let policy_hash = PolicyHash::from_json(&material);

        Ok(Self {
            policy_hash,
            catalog_digest,
            tool_mode: input.tool_mode,
            tools,
            exclusions,
        })
    }

    pub fn policy_hash(&self) -> &PolicyHash {
        &self.policy_hash
    }

    pub fn catalog_digest(&self) -> &DeclarationDigest {
        &self.catalog_digest
    }

    pub fn tool_mode(&self) -> ToolAccessMode {
        self.tool_mode
    }

    pub fn tools(&self) -> impl ExactSizeIterator<Item = &EffectiveTool> {
        self.tools.values()
    }

    pub fn tool(&self, id: &ToolId) -> Option<&EffectiveTool> {
        self.tools.get(id)
    }

    pub fn contains(&self, id: &ToolId) -> bool {
        self.tools.contains_key(id)
    }

    pub fn exclusions(&self) -> impl ExactSizeIterator<Item = &ToolExclusion> {
        self.exclusions.values()
    }

    pub fn exclusion(&self, id: &ToolId) -> Option<&ToolExclusion> {
        self.exclusions.get(id)
    }

    pub fn authorize<'a>(
        &self,
        invocation: &ToolInvocation,
        current_extensions: &'a [ExtensionDescriptor],
    ) -> Result<&'a ToolDeclaration, DispatchDenial> {
        let deny = |code| DispatchDenial {
            tool_id: invocation.tool_id.clone(),
            code,
        };
        if invocation.policy_hash != self.policy_hash {
            return Err(deny(DispatchDenialCode::StalePolicy));
        }
        let effective = self
            .tools
            .get(&invocation.tool_id)
            .ok_or_else(|| deny(DispatchDenialCode::ToolNotEffective))?;
        if invocation.extension_id != effective.extension_id {
            return Err(deny(DispatchDenialCode::ExtensionMismatch));
        }
        let current_extension = current_extensions
            .iter()
            .find(|extension| extension.id == effective.extension_id)
            .ok_or_else(|| deny(DispatchDenialCode::ToolUnavailable))?;
        if !current_extension.trust.permits_execution() {
            return Err(deny(DispatchDenialCode::ExtensionUntrusted));
        }
        if current_extension.trust != effective.extension_trust {
            return Err(deny(DispatchDenialCode::ExtensionTrustChanged));
        }
        let declaration = current_extension
            .tool(&invocation.tool_id)
            .ok_or_else(|| deny(DispatchDenialCode::ToolUnavailable))?;
        let current_digest = declaration.digest();
        if invocation.declaration_digest != effective.declaration_digest
            || current_digest != effective.declaration_digest
        {
            return Err(deny(DispatchDenialCode::StaleDeclaration));
        }
        if declaration.operation_kind != effective.declaration.operation_kind {
            return Err(deny(DispatchDenialCode::OperationChanged));
        }
        if !self.tool_mode.permits(declaration) {
            return Err(deny(DispatchDenialCode::ToolModeDenied));
        }
        if current_extension.digest() != effective.extension_digest {
            return Err(deny(DispatchDenialCode::ExtensionChanged));
        }
        if !invocation.arguments.is_object() {
            return Err(deny(DispatchDenialCode::InvalidArguments));
        }
        declaration
            .compile_schema()
            .map_err(|_| deny(DispatchDenialCode::StaleDeclaration))?
            .validate(&invocation.arguments)
            .map_err(|_| deny(DispatchDenialCode::InvalidArguments))?;
        Ok(declaration)
    }
}

fn exclude(
    exclusions: &mut BTreeMap<ToolId, ToolExclusion>,
    tool_id: ToolId,
    reason: ToolExclusionReason,
) {
    exclusions.insert(tool_id.clone(), ToolExclusion { tool_id, reason });
}

type CatalogEntry<'a> = (&'a ExtensionDescriptor, &'a ToolDeclaration);

fn build_catalog(
    extensions: &[ExtensionDescriptor],
) -> Result<BTreeMap<ToolId, CatalogEntry<'_>>, PolicyResolutionError> {
    let mut catalog = BTreeMap::new();
    let mut extension_ids = BTreeSet::new();
    for extension in extensions {
        extension
            .validate()
            .map_err(PolicyResolutionError::InvalidDeclaration)?;
        if !extension_ids.insert(extension.id.clone()) {
            return Err(PolicyResolutionError::DuplicateExtension {
                id: extension.id.clone(),
            });
        }
        for declaration in &extension.tools {
            if catalog
                .insert(declaration.id.clone(), (extension, declaration))
                .is_some()
            {
                return Err(PolicyResolutionError::DuplicateTool {
                    id: declaration.id.clone(),
                });
            }
        }
    }
    Ok(catalog)
}

fn extension_catalog_digest(extensions: &[ExtensionDescriptor]) -> DeclarationDigest {
    let material = extensions
        .iter()
        .map(|extension| (&extension.id, extension.digest()))
        .collect::<BTreeMap<_, _>>();
    let value = serde_json::to_value(material).expect("extension catalog is serializable");
    DeclarationDigest::from_json(&value)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyResolutionError {
    InvalidDeclaration(DeclarationError),
    DuplicateExtension { id: ExtensionId },
    DuplicateTool { id: ToolId },
    DuplicateSkill { id: SkillId },
}

impl Display for PolicyResolutionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration(error) => Display::fmt(error, formatter),
            Self::DuplicateExtension { id } => write!(formatter, "duplicate extension ID `{id}`"),
            Self::DuplicateTool { id } => {
                write!(formatter, "duplicate tool ID `{id}`")
            }
            Self::DuplicateSkill { id } => write!(formatter, "duplicate selected skill ID `{id}`"),
        }
    }
}

impl std::error::Error for PolicyResolutionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchDenialCode {
    StalePolicy,
    ToolNotEffective,
    ExtensionMismatch,
    ExtensionTrustChanged,
    ExtensionUntrusted,
    ExtensionChanged,
    CatalogChanged,
    ToolUnavailable,
    StaleDeclaration,
    OperationChanged,
    ToolModeDenied,
    InvalidArguments,
    ConditionalEffectUndeclared,
    ConditionalEffectDenied,
}

impl DispatchDenialCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StalePolicy => "stale_policy",
            Self::ToolNotEffective => "tool_not_effective",
            Self::ExtensionMismatch => "extension_mismatch",
            Self::ExtensionTrustChanged => "extension_trust_changed",
            Self::ExtensionUntrusted => "extension_untrusted",
            Self::ExtensionChanged => "extension_changed",
            Self::CatalogChanged => "catalog_changed",
            Self::ToolUnavailable => "tool_unavailable",
            Self::StaleDeclaration => "stale_declaration",
            Self::OperationChanged => "operation_changed",
            Self::ToolModeDenied => "tool_mode_denied",
            Self::InvalidArguments => "invalid_arguments",
            Self::ConditionalEffectUndeclared => "conditional_effect_undeclared",
            Self::ConditionalEffectDenied => "conditional_effect_denied",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchDenial {
    pub tool_id: ToolId,
    pub code: DispatchDenialCode,
}

impl Display for DispatchDenial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "tool `{}` denied: {}",
            self.tool_id,
            self.code.as_str()
        )
    }
}

impl std::error::Error for DispatchDenial {}
