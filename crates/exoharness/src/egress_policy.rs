use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// A provider-specific domain pattern, such as `api.github.com` or
/// `*.github.com`.
pub type DomainPattern = String;

/// Provider-neutral outbound network policy for a sandbox.
///
/// This network policy is limited to rules every supported provider can either
/// enforce or explicitly reject. Provider adapters compile it to their native
/// firewall configuration
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressPolicy {
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

impl Default for EgressPolicy {
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

impl EgressPolicy {
    /// Whether this policy permits unrestricted outbound access.
    pub fn permits_unrestricted_egress(&self) -> bool {
        !self.default_deny
    }
}

/// Egress-policy features a backend can enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressCapabilities {
    /// Enforces `EgressPolicy::default_deny`.
    pub default_deny: bool,
    /// Enforces `EgressPolicy::allowed_domains`.
    pub domain_allowlist: bool,
    /// Enforces `EgressPolicy::allowed_cidrs`.
    pub cidr_allowlist: bool,
    /// Enforces `EgressPolicy::denied_domains`.
    pub domain_denylist: bool,
    /// Enforces `EgressPolicy::denied_cidrs`.
    pub cidr_denylist: bool,
    /// Applies changes to a running sandbox.
    pub live_updates: bool,
}

impl Default for EgressCapabilities {
    fn default() -> Self {
        Self {
            default_deny: false,
            domain_allowlist: false,
            cidr_allowlist: false,
            domain_denylist: false,
            cidr_denylist: false,
            live_updates: false,
        }
    }
}

/// Validates that `policy` asks only for restrictions the backend can enforce.
///
/// An unrestricted policy (`default_deny: false` with no allowlists) is valid
/// for every backend. Backends must reject every other policy unless their
/// reported capabilities cover it; accepting a policy that is silently weaker
/// than requested would be a security bug.
pub fn validate_egress_policy_capabilities(
    policy: &EgressPolicy,
    capabilities: EgressCapabilities,
) -> anyhow::Result<()> {
    if !policy.default_deny
        && (!policy.allowed_domains.is_empty() || !policy.allowed_cidrs.is_empty())
    {
        anyhow::bail!("an egress allowlist requires default_deny");
    }

    if policy.default_deny && !capabilities.default_deny {
        anyhow::bail!("sandbox backend cannot enforce default-deny egress");
    }
    if !policy.allowed_domains.is_empty() && !capabilities.domain_allowlist {
        anyhow::bail!("sandbox backend cannot enforce domain egress allowlists");
    }
    if !policy.allowed_cidrs.is_empty() && !capabilities.cidr_allowlist {
        anyhow::bail!("sandbox backend cannot enforce CIDR egress allowlists");
    }
    if !policy.denied_domains.is_empty() && !capabilities.domain_denylist {
        anyhow::bail!("sandbox backend cannot enforce domain egress denylists");
    }
    if !policy.denied_cidrs.is_empty() && !capabilities.cidr_denylist {
        anyhow::bail!("sandbox backend cannot enforce CIDR egress denylists");
    }

    Ok(())
}

fn default_deny() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_denied_egress() {
        assert_eq!(EgressPolicy::default().default_deny, true);
        let decoded: EgressPolicy = serde_json::from_str("{}").expect("deserialize policy");
        assert_eq!(decoded, EgressPolicy::default());
    }

    #[test]
    fn rejects_rules_a_backend_cannot_enforce() {
        let capabilities = EgressCapabilities {
            default_deny: true,
            ..EgressCapabilities::default()
        };

        validate_egress_policy_capabilities(&EgressPolicy::default(), capabilities)
            .expect("default deny is supported");
        assert!(
            validate_egress_policy_capabilities(
                &EgressPolicy {
                    allowed_domains: vec!["api.github.com".to_string()],
                    ..EgressPolicy::default()
                },
                capabilities,
            )
            .is_err()
        );
        assert!(
            validate_egress_policy_capabilities(
                &EgressPolicy {
                    default_deny: false,
                    denied_cidrs: vec!["192.0.2.0/24".parse().expect("valid CIDR")],
                    ..EgressPolicy::default()
                },
                capabilities,
            )
            .is_err()
        );
    }
}
