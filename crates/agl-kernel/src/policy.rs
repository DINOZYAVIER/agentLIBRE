use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use agl_extension::{
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
    pub capability_id: ToolId,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ToolGrantProvenance>,
}

impl ToolGrant {
    pub fn new(capability_id: ToolId, max_operation_kind: OperationKind) -> Self {
        Self {
            capability_id,
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
    pub providers: Vec<ExtensionDescriptor>,
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
        providers: impl IntoIterator<Item = ExtensionDescriptor>,
        baseline: impl IntoIterator<Item = ToolId>,
        tool_mode: ToolAccessMode,
    ) -> Self {
        Self {
            providers: providers.into_iter().collect(),
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
        capabilities: impl IntoIterator<Item = ToolId>,
    ) -> Self {
        self.unavailable_capabilities = capabilities.into_iter().collect();
        self
    }

    pub fn with_function_policy(mut self, policy: FunctionToolPolicy) -> Self {
        self.function_policy = Some(policy);
        self
    }

    pub fn with_authority_ceiling(
        mut self,
        capabilities: impl IntoIterator<Item = ToolId>,
    ) -> Self {
        self.authority_ceiling = Some(capabilities.into_iter().collect());
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
    UnknownCapability,
    ProviderUntrusted,
    ToolModeDenied,
    FunctionAllowDenied,
    SkillDenied,
    FunctionDenied,
    GrantOperationDenied,
    GrantStateEffectDenied,
    GrantSensitiveInputDenied,
    ProviderUnavailable,
    ParentAuthorityDenied,
}

impl ToolExclusionReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotRouted => "not_routed",
            Self::UnknownCapability => "unknown_capability",
            Self::ProviderUntrusted => "provider_untrusted",
            Self::ToolModeDenied => "tool_mode_denied",
            Self::FunctionAllowDenied => "function_allow_denied",
            Self::SkillDenied => "skill_denied",
            Self::FunctionDenied => "function_denied",
            Self::GrantOperationDenied => "grant_operation_denied",
            Self::GrantStateEffectDenied => "grant_state_effect_denied",
            Self::GrantSensitiveInputDenied => "grant_sensitive_input_denied",
            Self::ProviderUnavailable => "provider_unavailable",
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
    pub capability_id: ToolId,
    pub reason: ToolExclusionReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveTool {
    provider_id: ExtensionId,
    provider_trust: ExtensionTrust,
    provider_digest: DeclarationDigest,
    declaration_digest: DeclarationDigest,
    declaration: ToolDeclaration,
    authorized_state_effects: BTreeSet<EffectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant_provenance: Option<ToolGrantProvenance>,
}

impl EffectiveTool {
    pub fn provider_id(&self) -> &ExtensionId {
        &self.provider_id
    }

    pub fn provider_trust(&self) -> ExtensionTrust {
        self.provider_trust
    }

    pub fn declaration_digest(&self) -> &DeclarationDigest {
        &self.declaration_digest
    }

    pub fn provider_digest(&self) -> &DeclarationDigest {
        &self.provider_digest
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
    capabilities: BTreeMap<ToolId, EffectiveTool>,
    exclusions: BTreeMap<ToolId, ToolExclusion>,
}

impl EffectiveToolSet {
    pub fn resolve(input: ToolPolicyInput) -> Result<Self, PolicyResolutionError> {
        let catalog = build_catalog(&input.providers)?;
        let catalog_digest = provider_catalog_digest(&input.providers);
        let mut routed = input.baseline.clone();
        let mut skill_denied = BTreeSet::new();
        for skill in &input.selected_skills {
            routed.extend(skill.allow.iter().cloned());
            skill_denied.extend(skill.deny.iter().cloned());
        }

        let mut grants = BTreeMap::<ToolId, Vec<&ToolGrant>>::new();
        for grant in &input.grants {
            grants
                .entry(grant.capability_id.clone())
                .or_default()
                .push(grant);
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

        let mut capabilities = BTreeMap::new();
        let mut exclusions = BTreeMap::new();
        for capability_id in all_ids {
            let Some((provider, declaration)) = catalog.get(&capability_id).copied() else {
                exclude(
                    &mut exclusions,
                    capability_id,
                    ToolExclusionReason::UnknownCapability,
                );
                continue;
            };

            if input.unavailable_capabilities.contains(&capability_id) {
                exclude(
                    &mut exclusions,
                    capability_id,
                    ToolExclusionReason::ProviderUnavailable,
                );
                continue;
            }
            if input
                .authority_ceiling
                .as_ref()
                .is_some_and(|ceiling| !ceiling.contains(&capability_id))
            {
                exclude(
                    &mut exclusions,
                    capability_id,
                    ToolExclusionReason::ParentAuthorityDenied,
                );
                continue;
            }

            let eligible_grant = grants.get(&capability_id).and_then(|candidates| {
                candidates
                    .iter()
                    .find(|grant| grant.permits(declaration).is_ok())
            });
            let mut reason = if !provider.trust.permits_execution() {
                ToolExclusionReason::ProviderUntrusted
            } else if !input.tool_mode.permits(declaration) {
                ToolExclusionReason::ToolModeDenied
            } else if input
                .function_policy
                .as_ref()
                .is_some_and(|policy| !policy.allow.contains(&capability_id))
            {
                ToolExclusionReason::FunctionAllowDenied
            } else if !declaration.sensitive_inputs.is_empty() && eligible_grant.is_none() {
                grants.get(&capability_id).map_or(
                    ToolExclusionReason::GrantSensitiveInputDenied,
                    |candidates| {
                        candidates
                            .iter()
                            .filter_map(|grant| grant.permits(declaration).err())
                            .min()
                            .unwrap_or(ToolExclusionReason::GrantSensitiveInputDenied)
                    },
                )
            } else if !routed.contains(&capability_id) && eligible_grant.is_none() {
                grants
                    .get(&capability_id)
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
                capabilities.insert(
                    capability_id.clone(),
                    EffectiveTool {
                        provider_id: provider.id.clone(),
                        provider_trust: provider.trust,
                        provider_digest: provider.digest(),
                        declaration_digest: declaration.digest(),
                        declaration: declaration.clone(),
                        authorized_state_effects,
                        grant_provenance: eligible_grant.and_then(|grant| grant.provenance.clone()),
                    },
                );
                continue;
            };

            if skill_denied.contains(&capability_id) {
                reason = ToolExclusionReason::SkillDenied;
            }
            if input
                .function_policy
                .as_ref()
                .is_some_and(|policy| policy.deny.contains(&capability_id))
            {
                reason = ToolExclusionReason::FunctionDenied;
            }
            exclude(&mut exclusions, capability_id, reason);
        }

        // Deny filters apply last, including capabilities admitted above.
        for capability_id in skill_denied {
            if capabilities.remove(&capability_id).is_some() {
                exclude(
                    &mut exclusions,
                    capability_id,
                    ToolExclusionReason::SkillDenied,
                );
            }
        }
        if let Some(policy) = &input.function_policy {
            for capability_id in &policy.deny {
                if capabilities.remove(capability_id).is_some() {
                    exclude(
                        &mut exclusions,
                        capability_id.clone(),
                        ToolExclusionReason::FunctionDenied,
                    );
                }
            }
        }

        #[derive(Serialize)]
        struct HashMaterial<'a> {
            tool_mode: ToolAccessMode,
            providers: BTreeMap<&'a ExtensionId, ProviderHashMaterial<'a>>,
            baseline: &'a BTreeSet<ToolId>,
            selected_skills: BTreeMap<&'a SkillId, SkillHashMaterial<'a>>,
            grants: BTreeSet<&'a ToolGrant>,
            unavailable_capabilities: &'a BTreeSet<ToolId>,
            authority_ceiling: &'a Option<BTreeSet<ToolId>>,
            function_policy: &'a Option<FunctionToolPolicy>,
            capabilities: &'a BTreeMap<ToolId, EffectiveTool>,
            exclusions: &'a BTreeMap<ToolId, ToolExclusion>,
        }
        #[derive(Serialize)]
        struct ProviderHashMaterial<'a> {
            name: &'a str,
            version: &'a str,
            source: agl_extension::ExtensionSource,
            trust: ExtensionTrust,
            hooks: BTreeMap<&'a agl_extension::HookId, &'a agl_extension::HookDeclaration>,
            actions: BTreeMap<&'a ToolId, &'a ToolDeclaration>,
        }
        #[derive(Serialize)]
        struct SkillHashMaterial<'a> {
            allow: &'a BTreeSet<ToolId>,
            requestable: &'a BTreeSet<ToolId>,
            deny: &'a BTreeSet<ToolId>,
        }
        let providers = input
            .providers
            .iter()
            .map(|provider| {
                (
                    &provider.id,
                    ProviderHashMaterial {
                        name: &provider.name,
                        version: &provider.version,
                        source: provider.source,
                        trust: provider.trust,
                        hooks: provider.hooks.iter().map(|hook| (&hook.id, hook)).collect(),
                        actions: provider
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
            providers,
            baseline: &input.baseline,
            selected_skills,
            grants: input.grants.iter().collect(),
            unavailable_capabilities: &input.unavailable_capabilities,
            authority_ceiling: &input.authority_ceiling,
            function_policy: &input.function_policy,
            capabilities: &capabilities,
            exclusions: &exclusions,
        })
        .expect("policy hash material is serializable");
        let policy_hash = PolicyHash::from_json(&material);

        Ok(Self {
            policy_hash,
            catalog_digest,
            tool_mode: input.tool_mode,
            capabilities,
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

    pub fn capabilities(&self) -> impl ExactSizeIterator<Item = &EffectiveTool> {
        self.capabilities.values()
    }

    pub fn capability(&self, id: &ToolId) -> Option<&EffectiveTool> {
        self.capabilities.get(id)
    }

    pub fn contains(&self, id: &ToolId) -> bool {
        self.capabilities.contains_key(id)
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
        current_providers: &'a [ExtensionDescriptor],
    ) -> Result<&'a ToolDeclaration, DispatchDenial> {
        let deny = |code| DispatchDenial {
            capability_id: invocation.capability_id.clone(),
            code,
        };
        if invocation.policy_hash != self.policy_hash {
            return Err(deny(DispatchDenialCode::StalePolicy));
        }
        let effective = self
            .capabilities
            .get(&invocation.capability_id)
            .ok_or_else(|| deny(DispatchDenialCode::CapabilityNotEffective))?;
        if invocation.provider_id != effective.provider_id {
            return Err(deny(DispatchDenialCode::ProviderMismatch));
        }
        let current_provider = current_providers
            .iter()
            .find(|provider| provider.id == effective.provider_id)
            .ok_or_else(|| deny(DispatchDenialCode::CapabilityUnavailable))?;
        if !current_provider.trust.permits_execution() {
            return Err(deny(DispatchDenialCode::ProviderUntrusted));
        }
        if current_provider.trust != effective.provider_trust {
            return Err(deny(DispatchDenialCode::ExtensionTrustChanged));
        }
        let declaration = current_provider
            .tool(&invocation.capability_id)
            .ok_or_else(|| deny(DispatchDenialCode::CapabilityUnavailable))?;
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
        if current_provider.digest() != effective.provider_digest {
            return Err(deny(DispatchDenialCode::ProviderChanged));
        }
        if provider_catalog_digest(current_providers) != self.catalog_digest {
            return Err(deny(DispatchDenialCode::CatalogChanged));
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
    capability_id: ToolId,
    reason: ToolExclusionReason,
) {
    exclusions.insert(
        capability_id.clone(),
        ToolExclusion {
            capability_id,
            reason,
        },
    );
}

type CatalogEntry<'a> = (&'a ExtensionDescriptor, &'a ToolDeclaration);

fn build_catalog(
    providers: &[ExtensionDescriptor],
) -> Result<BTreeMap<ToolId, CatalogEntry<'_>>, PolicyResolutionError> {
    let mut catalog = BTreeMap::new();
    let mut provider_ids = BTreeSet::new();
    for provider in providers {
        provider
            .validate()
            .map_err(PolicyResolutionError::InvalidDeclaration)?;
        if !provider_ids.insert(provider.id.clone()) {
            return Err(PolicyResolutionError::DuplicateProvider {
                id: provider.id.clone(),
            });
        }
        for declaration in &provider.tools {
            if catalog
                .insert(declaration.id.clone(), (provider, declaration))
                .is_some()
            {
                return Err(PolicyResolutionError::DuplicateCapability {
                    id: declaration.id.clone(),
                });
            }
        }
    }
    Ok(catalog)
}

fn provider_catalog_digest(providers: &[ExtensionDescriptor]) -> DeclarationDigest {
    let material = providers
        .iter()
        .map(|provider| (&provider.id, provider.digest()))
        .collect::<BTreeMap<_, _>>();
    let value = serde_json::to_value(material).expect("provider catalog is serializable");
    DeclarationDigest::from_json(&value)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyResolutionError {
    InvalidDeclaration(DeclarationError),
    DuplicateProvider { id: ExtensionId },
    DuplicateCapability { id: ToolId },
    DuplicateSkill { id: SkillId },
}

impl Display for PolicyResolutionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDeclaration(error) => Display::fmt(error, formatter),
            Self::DuplicateProvider { id } => write!(formatter, "duplicate provider ID `{id}`"),
            Self::DuplicateCapability { id } => {
                write!(formatter, "duplicate capability ID `{id}`")
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
    CapabilityNotEffective,
    ProviderMismatch,
    ExtensionTrustChanged,
    ProviderUntrusted,
    ProviderChanged,
    CatalogChanged,
    CapabilityUnavailable,
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
            Self::CapabilityNotEffective => "capability_not_effective",
            Self::ProviderMismatch => "provider_mismatch",
            Self::ExtensionTrustChanged => "provider_trust_changed",
            Self::ProviderUntrusted => "provider_untrusted",
            Self::ProviderChanged => "provider_changed",
            Self::CatalogChanged => "catalog_changed",
            Self::CapabilityUnavailable => "capability_unavailable",
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
    pub capability_id: ToolId,
    pub code: DispatchDenialCode,
}

impl Display for DispatchDenial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "capability `{}` denied: {}",
            self.capability_id,
            self.code.as_str()
        )
    }
}

impl std::error::Error for DispatchDenial {}
