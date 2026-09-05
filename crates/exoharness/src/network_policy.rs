use anyhow::{Result, bail};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// A provider-specific domain pattern, such as `api.github.com` or
/// `*.github.com`.
pub type DomainPattern = String;

/// Provider-neutral outbound network policy for a sandbox.
///
/// This policy is limited to rules every supported provider can either enforce
/// or explicitly reject. Provider adapters compile it to their native firewall
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetworkPolicy {
    #[serde(default = "default_deny")]
    pub default_deny: bool,
    #[serde(default)]
    pub allowed_domains: Vec<DomainPattern>,
    #[serde(default)]
    pub allowed_cidrs: Vec<IpNet>,
    #[serde(default)]
    pub denied_domains: Vec<DomainPattern>,
    #[serde(default)]
    pub denied_cidrs: Vec<IpNet>,
}

impl Default for SandboxNetworkPolicy {
    fn default() -> Self {
        Self {
            default_deny: true,
            allowed_domains: Vec::new(),
            allowed_cidrs: Vec::new(),
            denied_domains: Vec::new(),
            denied_cidrs: Vec::new(),
        }
    }
}

impl SandboxNetworkPolicy {
    pub fn from_legacy_enable_networking(enabled: bool) -> Self {
        if enabled {
            Self::allow_all()
        } else {
            Self::deny_all()
        }
    }

    pub fn allow_all() -> Self {
        Self {
            default_deny: false,
            ..Self::default()
        }
    }

    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Whether this policy permits unrestricted outbound access.
    pub fn allows_all(&self) -> bool {
        !self.default_deny
            && self.allowed_domains.is_empty()
            && self.allowed_cidrs.is_empty()
            && self.denied_domains.is_empty()
            && self.denied_cidrs.is_empty()
    }

    pub(crate) fn legacy_enable_networking(&self) -> Option<bool> {
        (self.allowed_domains.is_empty()
            && self.allowed_cidrs.is_empty()
            && self.denied_domains.is_empty()
            && self.denied_cidrs.is_empty())
        .then_some(!self.default_deny)
    }
}

/// Network policy features a backend can enforce, not merely accept in its
/// provider API
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkPolicyCapabilities {
    pub default_deny: bool,
    pub domain_allowlist: bool,
    pub cidr_allowlist: bool,
    pub domain_denylist: bool,
    pub cidr_denylist: bool,
}

/// Validates that `policy` asks only for restrictions the backend can enforce.
pub fn validate_network_policy_capabilities(
    policy: &SandboxNetworkPolicy,
    capabilities: NetworkPolicyCapabilities,
) -> Result<()> {
    if !policy.default_deny
        && (!policy.allowed_domains.is_empty() || !policy.allowed_cidrs.is_empty())
    {
        bail!("a network allowlist requires default_deny");
    }

    if policy.default_deny && !capabilities.default_deny {
        bail!("sandbox backend cannot enforce default-deny network");
    }
    if !policy.allowed_domains.is_empty() && !capabilities.domain_allowlist {
        bail!("sandbox backend cannot enforce domain network allowlists");
    }
    if !policy.allowed_cidrs.is_empty() && !capabilities.cidr_allowlist {
        bail!("sandbox backend cannot enforce CIDR network allowlists");
    }
    if !policy.denied_domains.is_empty() && !capabilities.domain_denylist {
        bail!("sandbox backend cannot enforce domain network denylists");
    }
    if !policy.denied_cidrs.is_empty() && !capabilities.cidr_denylist {
        bail!("sandbox backend cannot enforce CIDR network denylists");
    }

    Ok(())
}

fn default_deny() -> bool {
    true
}
