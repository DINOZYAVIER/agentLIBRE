use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::{
    ActionDeclaration, ActionInvocation, CapabilityId, DeclarationDigest, DeclarationError,
    OperationKind, PolicyHash, ProviderDeclaration, ProviderId, ProviderTrust, SensitiveInput,
    SkillId, StateEffect,
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

    pub fn operation_ceiling(self) -> OperationKind {
        match self {
            Self::ReadOnly => OperationKind::Read,
            Self::Write => OperationKind::Write,
            Self::Execute => OperationKind::Execute,
            Self::Approve => OperationKind::Approve,
            Self::Admin => OperationKind::Admin,
        }
    }

    pub fn permits(self, declaration: &ActionDeclaration) -> bool {
        declaration.visibility.visible_in_read_only
            || self.operation_ceiling().permits(declaration.operation_kind)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionToolPolicy {
    pub allow: BTreeSet<CapabilityId>,
    pub deny: BTreeSet<CapabilityId>,
}

impl FunctionToolPolicy {
    pub fn new(
        allow: impl IntoIterator<Item = CapabilityId>,
        deny: impl IntoIterator<Item = CapabilityId>,
    ) -> Self {
        Self {
            allow: allow.into_iter().collect(),
            deny: deny.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCapabilityPolicy {
    pub skill_id: SkillId,
    pub allow: BTreeSet<CapabilityId>,
    pub deny: BTreeSet<CapabilityId>,
}

impl SkillCapabilityPolicy {
    pub fn new(skill_id: SkillId, allow: impl IntoIterator<Item = CapabilityId>) -> Self {
        Self {
            skill_id,
            allow: allow.into_iter().collect(),
            deny: BTreeSet::new(),
        }
    }

    pub fn with_denied(mut self, deny: impl IntoIterator<Item = CapabilityId>) -> Self {
        self.deny = deny.into_iter().collect();
        self
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityGrant {
    pub capability_id: CapabilityId,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<StateEffect>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
}

impl CapabilityGrant {
    pub fn new(capability_id: CapabilityId, max_operation_kind: OperationKind) -> Self {
        Self {
            capability_id,
            max_operation_kind,
            state_effects: BTreeSet::new(),
            sensitive_inputs: BTreeSet::new(),
        }
    }

    pub fn with_state_effects(
        mut self,
        state_effects: impl IntoIterator<Item = StateEffect>,
    ) -> Self {
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

    fn permits(&self, declaration: &ActionDeclaration) -> Result<(), CapabilityExclusionReason> {
        if !self.max_operation_kind.permits(declaration.operation_kind) {
            return Err(CapabilityExclusionReason::GrantOperationDenied);
        }
        if (!self.state_effects.is_empty() || !declaration.sensitive_inputs.is_empty())
            && !declaration
                .state_effects
                .iter()
                .all(|effect| self.state_effects.contains(effect))
        {
            return Err(CapabilityExclusionReason::GrantStateEffectDenied);
        }
        if !declaration
            .sensitive_inputs
            .iter()
            .all(|input| self.sensitive_inputs.contains(input))
        {
            return Err(CapabilityExclusionReason::GrantSensitiveInputDenied);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPolicyInput {
    pub providers: Vec<ProviderDeclaration>,
    pub baseline: BTreeSet<CapabilityId>,
    pub selected_skills: Vec<SkillCapabilityPolicy>,
    pub grants: Vec<CapabilityGrant>,
    pub unavailable_capabilities: BTreeSet<CapabilityId>,
    pub authority_ceiling: Option<BTreeSet<CapabilityId>>,
    pub function_policy: Option<FunctionToolPolicy>,
    pub tool_mode: ToolAccessMode,
}

impl CapabilityPolicyInput {
    pub fn new(
        providers: impl IntoIterator<Item = ProviderDeclaration>,
        baseline: impl IntoIterator<Item = CapabilityId>,
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
        selected_skills: impl IntoIterator<Item = SkillCapabilityPolicy>,
    ) -> Self {
        self.selected_skills = selected_skills.into_iter().collect();
        self
    }

    pub fn with_grants(mut self, grants: impl IntoIterator<Item = CapabilityGrant>) -> Self {
        self.grants = grants.into_iter().collect();
        self
    }

    pub fn with_unavailable_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = CapabilityId>,
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
        capabilities: impl IntoIterator<Item = CapabilityId>,
    ) -> Self {
        self.authority_ceiling = Some(capabilities.into_iter().collect());
        self
    }

    pub fn resolve(self) -> Result<EffectiveCapabilitySet, PolicyResolutionError> {
        EffectiveCapabilitySet::resolve(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityExclusionReason {
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

impl CapabilityExclusionReason {
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityExclusion {
    pub capability_id: CapabilityId,
    pub reason: CapabilityExclusionReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveCapability {
    provider_id: ProviderId,
    provider_trust: ProviderTrust,
    provider_digest: DeclarationDigest,
    declaration_digest: DeclarationDigest,
    declaration: ActionDeclaration,
}

impl EffectiveCapability {
    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    pub fn provider_trust(&self) -> ProviderTrust {
        self.provider_trust
    }

    pub fn declaration_digest(&self) -> &DeclarationDigest {
        &self.declaration_digest
    }

    pub fn provider_digest(&self) -> &DeclarationDigest {
        &self.provider_digest
    }

    pub fn declaration(&self) -> &ActionDeclaration {
        &self.declaration
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveCapabilitySet {
    policy_hash: PolicyHash,
    catalog_digest: DeclarationDigest,
    capabilities: BTreeMap<CapabilityId, EffectiveCapability>,
    exclusions: BTreeMap<CapabilityId, CapabilityExclusion>,
}

impl EffectiveCapabilitySet {
    pub fn resolve(input: CapabilityPolicyInput) -> Result<Self, PolicyResolutionError> {
        let catalog = build_catalog(&input.providers)?;
        let catalog_digest = provider_catalog_digest(&input.providers);
        let mut routed = input.baseline.clone();
        let mut skill_denied = BTreeSet::new();
        for skill in &input.selected_skills {
            routed.extend(skill.allow.iter().cloned());
            skill_denied.extend(skill.deny.iter().cloned());
        }

        let mut grants = BTreeMap::<CapabilityId, Vec<&CapabilityGrant>>::new();
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
        all_ids.extend(skill_denied.iter().cloned());

        let mut capabilities = BTreeMap::new();
        let mut exclusions = BTreeMap::new();
        for capability_id in all_ids {
            let Some((provider, declaration)) = catalog.get(&capability_id).copied() else {
                exclude(
                    &mut exclusions,
                    capability_id,
                    CapabilityExclusionReason::UnknownCapability,
                );
                continue;
            };

            if input.unavailable_capabilities.contains(&capability_id) {
                exclude(
                    &mut exclusions,
                    capability_id,
                    CapabilityExclusionReason::ProviderUnavailable,
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
                    CapabilityExclusionReason::ParentAuthorityDenied,
                );
                continue;
            }

            let eligible_grant = grants.get(&capability_id).and_then(|candidates| {
                candidates
                    .iter()
                    .find(|grant| grant.permits(declaration).is_ok())
            });
            let mut reason = if !declaration.sensitive_inputs.is_empty() && eligible_grant.is_none()
            {
                grants.get(&capability_id).map_or(
                    CapabilityExclusionReason::GrantSensitiveInputDenied,
                    |candidates| {
                        candidates
                            .iter()
                            .filter_map(|grant| grant.permits(declaration).err())
                            .min()
                            .unwrap_or(CapabilityExclusionReason::GrantSensitiveInputDenied)
                    },
                )
            } else if !routed.contains(&capability_id) && eligible_grant.is_none() {
                grants.get(&capability_id).map_or(
                    CapabilityExclusionReason::NotRouted,
                    |candidates| {
                        candidates
                            .iter()
                            .filter_map(|grant| grant.permits(declaration).err())
                            .min()
                            .unwrap_or(CapabilityExclusionReason::NotRouted)
                    },
                )
            } else if !provider.trust.permits_execution() {
                CapabilityExclusionReason::ProviderUntrusted
            } else if !input.tool_mode.permits(declaration) {
                CapabilityExclusionReason::ToolModeDenied
            } else if input
                .function_policy
                .as_ref()
                .is_some_and(|policy| !policy.allow.contains(&capability_id))
            {
                CapabilityExclusionReason::FunctionAllowDenied
            } else {
                capabilities.insert(
                    capability_id.clone(),
                    EffectiveCapability {
                        provider_id: provider.id.clone(),
                        provider_trust: provider.trust,
                        provider_digest: provider.digest(),
                        declaration_digest: declaration.digest(),
                        declaration: declaration.clone(),
                    },
                );
                continue;
            };

            if skill_denied.contains(&capability_id) {
                reason = CapabilityExclusionReason::SkillDenied;
            }
            if input
                .function_policy
                .as_ref()
                .is_some_and(|policy| policy.deny.contains(&capability_id))
            {
                reason = CapabilityExclusionReason::FunctionDenied;
            }
            exclude(&mut exclusions, capability_id, reason);
        }

        // Deny filters apply last, including capabilities admitted above.
        for capability_id in skill_denied {
            if capabilities.remove(&capability_id).is_some() {
                exclude(
                    &mut exclusions,
                    capability_id,
                    CapabilityExclusionReason::SkillDenied,
                );
            }
        }
        if let Some(policy) = &input.function_policy {
            for capability_id in &policy.deny {
                if capabilities.remove(capability_id).is_some() {
                    exclude(
                        &mut exclusions,
                        capability_id.clone(),
                        CapabilityExclusionReason::FunctionDenied,
                    );
                }
            }
        }

        #[derive(Serialize)]
        struct HashMaterial<'a> {
            tool_mode: ToolAccessMode,
            providers: BTreeMap<&'a ProviderId, ProviderHashMaterial<'a>>,
            baseline: &'a BTreeSet<CapabilityId>,
            selected_skills: BTreeMap<&'a SkillId, SkillHashMaterial<'a>>,
            grants: BTreeSet<&'a CapabilityGrant>,
            unavailable_capabilities: &'a BTreeSet<CapabilityId>,
            authority_ceiling: &'a Option<BTreeSet<CapabilityId>>,
            function_policy: &'a Option<FunctionToolPolicy>,
            capabilities: &'a BTreeMap<CapabilityId, EffectiveCapability>,
            exclusions: &'a BTreeMap<CapabilityId, CapabilityExclusion>,
        }
        #[derive(Serialize)]
        struct ProviderHashMaterial<'a> {
            name: &'a str,
            version: &'a str,
            source: crate::ProviderSource,
            trust: ProviderTrust,
            hooks: BTreeMap<&'a crate::HookId, &'a crate::HookDeclaration>,
            actions: BTreeMap<&'a CapabilityId, &'a ActionDeclaration>,
        }
        #[derive(Serialize)]
        struct SkillHashMaterial<'a> {
            allow: &'a BTreeSet<CapabilityId>,
            deny: &'a BTreeSet<CapabilityId>,
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
                            .actions
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

    pub fn capabilities(&self) -> impl ExactSizeIterator<Item = &EffectiveCapability> {
        self.capabilities.values()
    }

    pub fn capability(&self, id: &CapabilityId) -> Option<&EffectiveCapability> {
        self.capabilities.get(id)
    }

    pub fn contains(&self, id: &CapabilityId) -> bool {
        self.capabilities.contains_key(id)
    }

    pub fn exclusions(&self) -> impl ExactSizeIterator<Item = &CapabilityExclusion> {
        self.exclusions.values()
    }

    pub fn exclusion(&self, id: &CapabilityId) -> Option<&CapabilityExclusion> {
        self.exclusions.get(id)
    }

    pub fn authorize<'a>(
        &self,
        invocation: &ActionInvocation,
        current_providers: &'a [ProviderDeclaration],
    ) -> Result<&'a ActionDeclaration, DispatchDenial> {
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
            return Err(deny(DispatchDenialCode::ProviderTrustChanged));
        }
        let declaration = current_provider
            .action(&invocation.capability_id)
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
    exclusions: &mut BTreeMap<CapabilityId, CapabilityExclusion>,
    capability_id: CapabilityId,
    reason: CapabilityExclusionReason,
) {
    exclusions.insert(
        capability_id.clone(),
        CapabilityExclusion {
            capability_id,
            reason,
        },
    );
}

type CatalogEntry<'a> = (&'a ProviderDeclaration, &'a ActionDeclaration);

fn build_catalog(
    providers: &[ProviderDeclaration],
) -> Result<BTreeMap<CapabilityId, CatalogEntry<'_>>, PolicyResolutionError> {
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
        for declaration in &provider.actions {
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

fn provider_catalog_digest(providers: &[ProviderDeclaration]) -> DeclarationDigest {
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
    DuplicateProvider { id: ProviderId },
    DuplicateCapability { id: CapabilityId },
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
    ProviderTrustChanged,
    ProviderUntrusted,
    ProviderChanged,
    CatalogChanged,
    CapabilityUnavailable,
    StaleDeclaration,
    OperationChanged,
    InvalidArguments,
}

impl DispatchDenialCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StalePolicy => "stale_policy",
            Self::CapabilityNotEffective => "capability_not_effective",
            Self::ProviderMismatch => "provider_mismatch",
            Self::ProviderTrustChanged => "provider_trust_changed",
            Self::ProviderUntrusted => "provider_untrusted",
            Self::ProviderChanged => "provider_changed",
            Self::CatalogChanged => "catalog_changed",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::StaleDeclaration => "stale_declaration",
            Self::OperationChanged => "operation_changed",
            Self::InvalidArguments => "invalid_arguments",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchDenial {
    pub capability_id: CapabilityId,
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
