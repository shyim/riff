//! Unified package-policy configuration.
//!
//! Composer accepts policy settings from the current `config.policy` shape,
//! legacy `config.audit` keys, and a small set of environment overrides. This
//! module normalizes those inputs without mutating process environment, which
//! keeps policy construction deterministic and safe for concurrent requests.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

use crate::advisory::AuditBehavior;

const POLICY_ADVISORIES_BLOCK: &str = "COMPOSER_POLICY_ADVISORIES_BLOCK";
const POLICY_MALWARE_BLOCK: &str = "COMPOSER_POLICY_MALWARE_BLOCK";
const POLICY_ABANDONED_BLOCK: &str = "COMPOSER_POLICY_ABANDONED_BLOCK";
const POLICY_ENABLED: &str = "COMPOSER_POLICY";
const NO_BLOCKING: &str = "COMPOSER_NO_BLOCKING";
const NO_SECURITY_BLOCKING: &str = "COMPOSER_NO_SECURITY_BLOCKING";
const LEGACY_ABANDONED_BLOCK: &str = "COMPOSER_SECURITY_BLOCKING_ABANDONED";
const AUDIT_ABANDONED: &str = "COMPOSER_AUDIT_ABANDONED";

const POLICY_ENVIRONMENT_KEYS: &[&str] = &[
    POLICY_ENABLED,
    NO_BLOCKING,
    NO_SECURITY_BLOCKING,
    POLICY_ADVISORIES_BLOCK,
    POLICY_MALWARE_BLOCK,
    POLICY_ABANDONED_BLOCK,
    LEGACY_ABANDONED_BLOCK,
    AUDIT_ABANDONED,
];

pub const BUILT_IN_POLICY_LISTS: &[&str] = &["advisories", "malware", "abandoned"];
const FUTURE_RESERVED_POLICY_PREFIXES: &[&str] = &["ignore"];
const FUTURE_RESERVED_POLICY_NAMES: &[&str] = &[
    "package",
    "packages",
    "license",
    "licence",
    "licenses",
    "licences",
    "support",
    "maintenance",
    "security",
    "minimum-release-age",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyConfigError {
    #[error("Invalid value for {name}: {value}. Expected 0, 1, false, true, off, or on")]
    InvalidBoolean { name: String, value: String },
    #[error("Invalid value for {name}: {value}. Expected one of ignore, report, fail")]
    InvalidAudit { name: String, value: String },
    #[error("Invalid malware block-scope: {value}. Expected one of update, install, all")]
    InvalidBlockScope { value: String },
    #[error("Invalid 'apply' value for '{subject}': {value}. Expected 'audit', 'block', or 'all'")]
    InvalidApply { subject: String, value: String },
    #[error("Invalid advisory ignore rule for {subject}")]
    InvalidIgnoreRule { subject: String },
    #[error("Unknown ignore-unreachable scope '{scope}'. Expected audit, install, or update")]
    UnknownScope { scope: String },
    #[error("At least one ignore-unreachable scope is required")]
    MissingScope,
    #[error("Invalid version constraint \"{constraint}\" in ignore rule for {subject}: {reason}")]
    InvalidIgnoreConstraint {
        subject: String,
        constraint: String,
        reason: String,
    },
    #[error(transparent)]
    InvalidSource(#[from] PolicySourceError),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicySourceError {
    #[error("Built-in dependency policy \"{name}\" does not support sources")]
    BuiltInList { name: String },
    #[error("\"{name}\" starts with reserved prefix \"{prefix}\"")]
    ReservedPrefix { name: String, prefix: String },
    #[error("\"{name}\" is reserved for future use")]
    ReservedName { name: String },
    #[error("Invalid dependency policy name \"{name}\"")]
    InvalidName { name: String },
    #[error("Source JSON must be an object")]
    SourceMustBeObject,
    #[error("Source configuration is missing the \"type\" field")]
    MissingType,
    #[error("Unsupported source type \"{source_type}\". Only \"url\" is currently supported")]
    UnsupportedType { source_type: String },
    #[error("Source configuration is missing a string \"url\" field")]
    MissingUrl,
    #[error("Source URL for policy list \"{list}\" must start with \"https://\"; got \"{url}\"")]
    InsecureUrl { list: String, url: String },
    #[error("The policy source target must be a JSON object")]
    InvalidDocument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddPolicySourceOutcome {
    Added,
    AlreadyPresent,
}

/// An explicit environment snapshot used while normalizing policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyEnvironment {
    values: BTreeMap<String, String>,
}

impl PolicyEnvironment {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(name.into(), value.into());
        self
    }

    /// Capture only the environment keys which affect dependency policy.
    pub fn from_process() -> Self {
        let values = POLICY_ENVIRONMENT_KEYS
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).into(), value))
            })
            .collect();
        Self { values }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn bool_override(&self, name: &str) -> Result<Option<bool>, PolicyConfigError> {
        let Some(value) = self.get(name).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        match value {
            "1" | "true" | "on" => Ok(Some(true)),
            "0" | "false" | "off" => Ok(Some(false)),
            _ => Err(PolicyConfigError::InvalidBoolean {
                name: name.to_string(),
                value: value.to_string(),
            }),
        }
    }
}

pub fn validate_custom_policy_name(name: &str) -> Result<(), PolicySourceError> {
    if BUILT_IN_POLICY_LISTS.contains(&name) {
        return Err(PolicySourceError::BuiltInList {
            name: name.to_string(),
        });
    }
    if let Some(prefix) = FUTURE_RESERVED_POLICY_PREFIXES
        .iter()
        .find(|prefix| name.starts_with(**prefix))
    {
        return Err(PolicySourceError::ReservedPrefix {
            name: name.to_string(),
            prefix: (*prefix).to_string(),
        });
    }
    if FUTURE_RESERVED_POLICY_NAMES.contains(&name) {
        return Err(PolicySourceError::ReservedName {
            name: name.to_string(),
        });
    }
    if name.is_empty() || name.contains('.') {
        return Err(PolicySourceError::InvalidName {
            name: name.to_string(),
        });
    }
    Ok(())
}

pub fn validate_policy_source(name: &str, source: &Value) -> Result<(), PolicySourceError> {
    let source = source
        .as_object()
        .ok_or(PolicySourceError::SourceMustBeObject)?;
    let source_type = source
        .get("type")
        .and_then(Value::as_str)
        .ok_or(PolicySourceError::MissingType)?;
    if source_type != "url" {
        return Err(PolicySourceError::UnsupportedType {
            source_type: source_type.to_string(),
        });
    }
    let url = source
        .get("url")
        .and_then(Value::as_str)
        .ok_or(PolicySourceError::MissingUrl)?;
    if !url.starts_with("https://") {
        return Err(PolicySourceError::InsecureUrl {
            list: name.to_string(),
            url: url.to_string(),
        });
    }
    Ok(())
}

pub fn add_custom_policy_source(
    document: &mut Value,
    name: &str,
    source: Value,
) -> Result<AddPolicySourceOutcome, PolicySourceError> {
    validate_custom_policy_name(name)?;
    validate_policy_source(name, &source)?;

    let root = document
        .as_object_mut()
        .ok_or(PolicySourceError::InvalidDocument)?;
    let config = object_entry(root, "config");
    let policy = object_entry(config, "policy");
    let list = object_entry(policy, name);
    let sources = list
        .entry("sources".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !sources.is_array() {
        *sources = Value::Array(Vec::new());
    }
    let sources = sources.as_array_mut().expect("source list was normalized");
    let source_type = source.get("type");
    let source_url = source.get("url");
    if sources
        .iter()
        .any(|existing| existing.get("type") == source_type && existing.get("url") == source_url)
    {
        return Ok(AddPolicySourceOutcome::AlreadyPresent);
    }
    sources.push(source);
    Ok(AddPolicySourceOutcome::Added)
}

fn object_entry<'a>(
    object: &'a mut serde_json::Map<String, Value>,
    key: &str,
) -> &'a mut serde_json::Map<String, Value> {
    let value = object
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !value.is_object() {
        *value = Value::Object(serde_json::Map::new());
    }
    value.as_object_mut().expect("entry was normalized")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPolicyConfig {
    pub block: bool,
    pub audit: AuditBehavior,
    pub block_scope: String,
    pub ignore: BTreeMap<String, Vec<AdvisoryIgnoreRule>>,
    pub ignore_source: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyUrlSource {
    pub list: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomListPolicyConfig {
    pub name: String,
    pub block: bool,
    pub audit: AuditBehavior,
    pub ignore: BTreeMap<String, Vec<AdvisoryIgnoreRule>>,
    pub sources: Vec<PolicyUrlSource>,
}

impl CustomListPolicyConfig {
    pub fn from_raw(name: &str, value: &Value) -> Result<Self, PolicyConfigError> {
        if value.as_bool() == Some(false)
            || (!value.is_object() && !value.is_array() && !value.is_boolean())
        {
            return Ok(Self::disabled(name));
        }

        let options = value.as_object();
        let mut sources = Vec::new();
        if let Some(configured_sources) = options
            .and_then(|options| options.get("sources"))
            .and_then(Value::as_array)
        {
            for source in configured_sources
                .iter()
                .filter(|source| source.is_object())
            {
                validate_policy_source(name, source)?;
                let url = source
                    .get("url")
                    .and_then(Value::as_str)
                    .expect("validated policy URL source has a string URL");
                sources.push(PolicyUrlSource {
                    list: name.to_string(),
                    url: url.to_string(),
                });
            }
        }

        let ignore =
            parse_structured_package_ignores(options.and_then(|options| options.get("ignore")))?;
        let parser = riff_semver::VersionParser::new();
        for (subject, rules) in &ignore {
            for rule in rules {
                if let Some(constraint) = &rule.constraint {
                    parser.parse_constraints(constraint).map_err(|error| {
                        PolicyConfigError::InvalidIgnoreConstraint {
                            subject: subject.clone(),
                            constraint: constraint.clone(),
                            reason: error.to_string(),
                        }
                    })?;
                }
            }
        }

        Ok(Self {
            name: name.to_string(),
            block: options
                .and_then(|options| options.get("block"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            audit: options
                .and_then(|options| options.get("audit"))
                .and_then(Value::as_str)
                .and_then(parse_audit)
                .unwrap_or(AuditBehavior::Fail),
            ignore,
            sources,
        })
    }

    pub fn disabled(name: &str) -> Self {
        Self {
            name: name.to_string(),
            block: false,
            audit: AuditBehavior::Ignore,
            ignore: BTreeMap::new(),
            sources: Vec::new(),
        }
    }

    pub fn should_block(&self, scope: PolicyBlockScope) -> bool {
        self.block && scope == PolicyBlockScope::Update
    }

    pub fn package_is_ignored(
        &self,
        package: &str,
        version: &str,
        operation: PolicyOperation,
    ) -> bool {
        package_is_ignored(&self.ignore, package, version, operation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyOperation {
    Block,
    Audit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryIgnoreRule {
    pub constraint: Option<String>,
    pub reason: Option<String>,
    pub on_block: bool,
    pub on_audit: bool,
}

impl AdvisoryIgnoreRule {
    pub fn applies_to(&self, operation: PolicyOperation) -> bool {
        match operation {
            PolicyOperation::Block => self.on_block,
            PolicyOperation::Audit => self.on_audit,
        }
    }

    fn all(reason: Option<String>) -> Self {
        Self {
            constraint: None,
            reason,
            on_block: true,
            on_audit: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoriesPolicyConfig {
    pub block: bool,
    pub audit: AuditBehavior,
    pub ignore: BTreeMap<String, Vec<AdvisoryIgnoreRule>>,
    pub ignore_id: BTreeMap<String, AdvisoryIgnoreRule>,
    pub ignore_severity: BTreeMap<String, AdvisoryIgnoreRule>,
}

impl AdvisoriesPolicyConfig {
    fn disabled() -> Self {
        Self {
            block: false,
            audit: AuditBehavior::Ignore,
            ignore: BTreeMap::new(),
            ignore_id: BTreeMap::new(),
            ignore_severity: BTreeMap::new(),
        }
    }

    pub fn ignore_list_for_operation(
        &self,
        operation: PolicyOperation,
    ) -> BTreeMap<String, Option<String>> {
        let mut result = self
            .ignore_id
            .iter()
            .filter(|(_, rule)| rule.applies_to(operation))
            .map(|(id, rule)| (id.clone(), rule.reason.clone()))
            .collect::<BTreeMap<_, _>>();

        for (package, rules) in &self.ignore {
            for rule in rules.iter().filter(|rule| rule.applies_to(operation)) {
                merge_reason(&mut result, package, rule.reason.clone());
            }
        }
        result
    }

    pub fn ignore_severity_for_operation(
        &self,
        operation: PolicyOperation,
    ) -> BTreeMap<String, Option<String>> {
        self.ignore_severity
            .iter()
            .filter(|(_, rule)| rule.applies_to(operation))
            .map(|(severity, rule)| (severity.clone(), rule.reason.clone()))
            .collect()
    }

    pub fn with_ignore_severity<'a>(&self, severities: impl IntoIterator<Item = &'a str>) -> Self {
        let mut updated = self.clone();
        for severity in severities {
            updated
                .ignore_severity
                .entry(severity.to_string())
                .or_insert_with(|| AdvisoryIgnoreRule {
                    constraint: None,
                    reason: None,
                    on_block: false,
                    on_audit: true,
                });
        }
        updated
    }

    pub fn package_is_ignored(
        &self,
        package: &str,
        version: &str,
        operation: PolicyOperation,
    ) -> bool {
        package_is_ignored(&self.ignore, package, version, operation)
    }

    pub fn identifier_is_ignored(&self, identifier: &str, operation: PolicyOperation) -> bool {
        self.ignore_id.iter().any(|(ignored, rule)| {
            ignored.eq_ignore_ascii_case(identifier) && rule.applies_to(operation)
        })
    }

    pub fn severity_is_ignored(&self, severity: &str, operation: PolicyOperation) -> bool {
        self.ignore_severity.iter().any(|(ignored, rule)| {
            ignored.eq_ignore_ascii_case(severity) && rule.applies_to(operation)
        })
    }
}

impl ListPolicyConfig {
    fn disabled() -> Self {
        Self {
            block: false,
            audit: AuditBehavior::Ignore,
            block_scope: "all".to_string(),
            ignore: BTreeMap::new(),
            ignore_source: Vec::new(),
        }
    }

    pub fn should_block(&self, scope: PolicyBlockScope) -> bool {
        self.block
            && match self.block_scope.as_str() {
                "all" => true,
                "install" => scope == PolicyBlockScope::Install,
                "update" => scope == PolicyBlockScope::Update,
                _ => false,
            }
    }

    pub fn package_is_ignored(
        &self,
        package: &str,
        version: &str,
        operation: PolicyOperation,
    ) -> bool {
        package_is_ignored(&self.ignore, package, version, operation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedPolicyConfig {
    pub block: bool,
    pub audit: AuditBehavior,
    pub ignore: BTreeMap<String, Vec<AdvisoryIgnoreRule>>,
}

impl AbandonedPolicyConfig {
    fn disabled() -> Self {
        Self {
            block: false,
            audit: AuditBehavior::Ignore,
            ignore: BTreeMap::new(),
        }
    }

    pub fn ignore_list_for_operation(
        &self,
        operation: PolicyOperation,
    ) -> BTreeMap<String, Option<String>> {
        let mut result = BTreeMap::new();
        for (package, rules) in &self.ignore {
            for rule in rules.iter().filter(|rule| rule.applies_to(operation)) {
                merge_reason(&mut result, package, rule.reason.clone());
            }
        }
        result
    }

    pub fn package_is_ignored(
        &self,
        package: &str,
        version: &str,
        operation: PolicyOperation,
    ) -> bool {
        package_is_ignored(&self.ignore, package, version, operation)
    }
}

fn package_is_ignored(
    rules: &BTreeMap<String, Vec<AdvisoryIgnoreRule>>,
    package: &str,
    version: &str,
    operation: PolicyOperation,
) -> bool {
    rules.iter().any(|(pattern, rules)| {
        crate::package::package_name_matches(pattern, package)
            && rules.iter().any(|rule| {
                rule.applies_to(operation)
                    && rule.constraint.as_deref().is_none_or(|constraint| {
                        riff_semver::Semver::satisfies(version, constraint)
                    })
            })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyScope {
    Audit,
    Install,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyBlockScope {
    Install,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreUnreachable {
    pub audit: bool,
    pub install: bool,
    pub update: bool,
}

impl Default for IgnoreUnreachable {
    fn default() -> Self {
        Self {
            audit: false,
            install: true,
            update: true,
        }
    }
}

impl IgnoreUnreachable {
    pub fn all() -> Self {
        Self {
            audit: true,
            install: true,
            update: true,
        }
    }

    pub fn none() -> Self {
        Self {
            audit: false,
            install: false,
            update: false,
        }
    }

    fn from_policy(value: &Value) -> Self {
        if let Some(enabled) = value.as_bool() {
            return if enabled { Self::all() } else { Self::none() };
        }
        if let Some(scopes) = value.as_array() {
            return Self {
                audit: contains_scope(scopes, "audit"),
                install: contains_scope(scopes, "install"),
                update: contains_scope(scopes, "update"),
            };
        }
        Self::default()
    }

    pub fn for_block_scope(&self, scope: PolicyBlockScope) -> bool {
        match scope {
            PolicyBlockScope::Install => self.install,
            PolicyBlockScope::Update => self.update,
        }
    }

    pub fn from_legacy_audit(enabled: Option<bool>) -> Self {
        if enabled == Some(true) {
            return Self {
                audit: true,
                install: false,
                update: false,
            };
        }
        Self::default()
    }

    pub fn with_scopes(&self, scopes: &[PolicyScope]) -> Result<Self, PolicyConfigError> {
        if scopes.is_empty() {
            return Err(PolicyConfigError::MissingScope);
        }
        let mut updated = self.clone();
        for scope in scopes {
            match scope {
                PolicyScope::Audit => updated.audit = true,
                PolicyScope::Install => updated.install = true,
                PolicyScope::Update => updated.update = true,
            }
        }
        Ok(updated)
    }

    pub fn with_scope_names(&self, scopes: &[&str]) -> Result<Self, PolicyConfigError> {
        if scopes.is_empty() {
            return Err(PolicyConfigError::MissingScope);
        }
        let scopes = scopes
            .iter()
            .map(|scope| match *scope {
                "audit" => Ok(PolicyScope::Audit),
                "install" => Ok(PolicyScope::Install),
                "update" => Ok(PolicyScope::Update),
                scope => Err(PolicyConfigError::UnknownScope {
                    scope: scope.to_string(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.with_scopes(&scopes)
    }
}

/// Normalized policy used by package selection and audit operations.
#[derive(Debug, Clone, PartialEq)]
pub struct PackagePolicyConfig {
    pub enabled: bool,
    pub advisories: AdvisoriesPolicyConfig,
    pub malware: ListPolicyConfig,
    pub abandoned: AbandonedPolicyConfig,
    pub custom_lists: BTreeMap<String, CustomListPolicyConfig>,
    pub ignore_unreachable: IgnoreUnreachable,
}

impl PackagePolicyConfig {
    pub fn from_raw(
        policy: &Value,
        audit: &Value,
        environment: &PolicyEnvironment,
    ) -> Result<Self, PolicyConfigError> {
        let policy_enabled = environment.bool_override(POLICY_ENABLED)?;
        if policy_enabled == Some(false)
            || (policy_enabled != Some(true) && policy.as_bool() == Some(false))
        {
            return Ok(Self {
                enabled: false,
                advisories: AdvisoriesPolicyConfig::disabled(),
                malware: ListPolicyConfig::disabled(),
                abandoned: AbandonedPolicyConfig::disabled(),
                custom_lists: BTreeMap::new(),
                ignore_unreachable: IgnoreUnreachable::all(),
            });
        }

        let policy = policy.as_object();
        let audit = audit.as_object();
        let advisories = parse_advisories(policy, audit)?;
        let malware = parse_list(
            policy.and_then(|policy| policy.get("malware")),
            true,
            None,
            None,
        )?;
        let abandoned = parse_abandoned(policy, audit)?;

        let custom_lists = policy
            .into_iter()
            .flat_map(|policy| policy.iter())
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "advisories" | "malware" | "abandoned" | "ignore-unreachable"
                )
            })
            .map(|(name, value)| {
                validate_custom_policy_name(name)?;
                CustomListPolicyConfig::from_raw(name, value).map(|config| (name.clone(), config))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        let mut config = Self {
            enabled: true,
            advisories,
            malware,
            abandoned,
            custom_lists,
            ignore_unreachable: parse_ignore_unreachable(policy, audit),
        };

        if let Some(block) = environment.bool_override(POLICY_ADVISORIES_BLOCK)? {
            config.advisories.block = block;
        }
        if let Some(block) = environment.bool_override(POLICY_MALWARE_BLOCK)? {
            config.malware.block = block;
        }
        let abandoned_block = match environment.bool_override(POLICY_ABANDONED_BLOCK)? {
            Some(block) => Some(block),
            None => environment.bool_override(LEGACY_ABANDONED_BLOCK)?,
        };
        if let Some(block) = abandoned_block {
            config.abandoned.block = block;
        }
        if let Some(audit) = environment.get(AUDIT_ABANDONED) {
            config.abandoned.audit =
                parse_audit(audit).ok_or_else(|| PolicyConfigError::InvalidAudit {
                    name: AUDIT_ABANDONED.to_string(),
                    value: audit.to_string(),
                })?;
        }

        if environment.bool_override(NO_BLOCKING)? == Some(true)
            || environment.bool_override(NO_SECURITY_BLOCKING)? == Some(true)
        {
            config = config.with_blocking_disabled();
        }

        Ok(config)
    }

    pub fn with_blocking_disabled(&self) -> Self {
        let mut config = self.clone();
        config.advisories.block = false;
        config.malware.block = false;
        config.abandoned.block = false;
        for custom in config.custom_lists.values_mut() {
            custom.block = false;
        }
        config
    }

    pub fn with_ignore_unreachable(
        &self,
        scopes: &[PolicyScope],
    ) -> Result<Self, PolicyConfigError> {
        let mut updated = self.clone();
        updated.ignore_unreachable = self.ignore_unreachable.with_scopes(scopes)?;
        Ok(updated)
    }
}

fn parse_advisories(
    policy: Option<&serde_json::Map<String, Value>>,
    audit: Option<&serde_json::Map<String, Value>>,
) -> Result<AdvisoriesPolicyConfig, PolicyConfigError> {
    if let Some(value) = policy.and_then(|policy| policy.get("advisories")) {
        if value.as_bool() == Some(false) {
            return Ok(AdvisoriesPolicyConfig::disabled());
        }
        let options = value.as_object();
        return Ok(AdvisoriesPolicyConfig {
            block: options
                .and_then(|options| options.get("block"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            audit: options
                .and_then(|options| options.get("audit"))
                .and_then(Value::as_str)
                .and_then(parse_audit)
                .unwrap_or(AuditBehavior::Fail),
            ignore: parse_structured_package_ignores(
                options.and_then(|options| options.get("ignore")),
            )?,
            ignore_id: parse_structured_scalar_ignores(
                options.and_then(|options| options.get("ignore-id")),
            )?,
            ignore_severity: parse_structured_scalar_ignores(
                options.and_then(|options| options.get("ignore-severity")),
            )?,
        });
    }

    if audit.is_some_and(|audit| !audit.is_empty()) {
        let mut package_ignores = BTreeMap::new();
        let mut id_ignores = BTreeMap::new();
        if let Some(ignore) = audit.and_then(|audit| audit.get("ignore")) {
            for (subject, rule) in parse_legacy_ignores(ignore)? {
                if subject.contains('/') {
                    package_ignores
                        .entry(subject)
                        .or_insert_with(Vec::new)
                        .push(rule);
                } else {
                    id_ignores.insert(subject, rule);
                }
            }
        }
        let ignore_severity = audit
            .and_then(|audit| audit.get("ignore-severity"))
            .map(parse_legacy_ignores)
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .collect();

        return Ok(AdvisoriesPolicyConfig {
            block: legacy_bool(audit, "block-insecure").unwrap_or(true),
            audit: AuditBehavior::Fail,
            ignore: package_ignores,
            ignore_id: id_ignores,
            ignore_severity,
        });
    }

    Ok(AdvisoriesPolicyConfig {
        block: true,
        audit: AuditBehavior::Fail,
        ignore: BTreeMap::new(),
        ignore_id: BTreeMap::new(),
        ignore_severity: BTreeMap::new(),
    })
}

fn parse_structured_package_ignores(
    value: Option<&Value>,
) -> Result<BTreeMap<String, Vec<AdvisoryIgnoreRule>>, PolicyConfigError> {
    let mut result = BTreeMap::new();
    let Some(value) = value else {
        return Ok(result);
    };

    match value {
        Value::Array(packages) => {
            for package in packages {
                let package =
                    package
                        .as_str()
                        .ok_or_else(|| PolicyConfigError::InvalidIgnoreRule {
                            subject: "package list".to_string(),
                        })?;
                result
                    .entry(package.to_string())
                    .or_insert_with(Vec::new)
                    .push(AdvisoryIgnoreRule::all(None));
            }
        }
        Value::Object(packages) => {
            for (package, value) in packages {
                let rules = match value {
                    Value::Array(rules) => rules
                        .iter()
                        .map(|rule| parse_structured_rule(package, rule, true))
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => vec![parse_structured_rule(package, value, true)?],
                };
                result.insert(package.clone(), rules);
            }
        }
        _ => {
            return Err(PolicyConfigError::InvalidIgnoreRule {
                subject: "package list".to_string(),
            });
        }
    }
    Ok(result)
}

fn parse_structured_scalar_ignores(
    value: Option<&Value>,
) -> Result<BTreeMap<String, AdvisoryIgnoreRule>, PolicyConfigError> {
    let mut result = BTreeMap::new();
    let Some(value) = value else {
        return Ok(result);
    };
    match value {
        Value::Array(subjects) => {
            for subject in subjects {
                let subject =
                    subject
                        .as_str()
                        .ok_or_else(|| PolicyConfigError::InvalidIgnoreRule {
                            subject: "ignore list".to_string(),
                        })?;
                result.insert(subject.to_string(), AdvisoryIgnoreRule::all(None));
            }
        }
        Value::Object(subjects) => {
            for (subject, value) in subjects {
                result.insert(
                    subject.clone(),
                    parse_structured_rule(subject, value, false)?,
                );
            }
        }
        _ => {
            return Err(PolicyConfigError::InvalidIgnoreRule {
                subject: "ignore list".to_string(),
            });
        }
    }
    Ok(result)
}

fn parse_structured_rule(
    subject: &str,
    value: &Value,
    include_constraint: bool,
) -> Result<AdvisoryIgnoreRule, PolicyConfigError> {
    let rule = match value {
        Value::Null => AdvisoryIgnoreRule::all(None),
        Value::String(reason) => AdvisoryIgnoreRule::all(Some(reason.clone())),
        Value::Object(options) => AdvisoryIgnoreRule {
            constraint: include_constraint
                .then(|| options.get("constraint").and_then(Value::as_str))
                .flatten()
                .map(str::to_string),
            reason: options
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            on_block: options
                .get("on-block")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            on_audit: options
                .get("on-audit")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        },
        _ => {
            return Err(PolicyConfigError::InvalidIgnoreRule {
                subject: subject.to_string(),
            });
        }
    };
    if let Some(constraint) = &rule.constraint {
        riff_semver::VersionParser::new()
            .parse_constraints(constraint)
            .map_err(|error| PolicyConfigError::InvalidIgnoreConstraint {
                subject: subject.to_string(),
                constraint: constraint.clone(),
                reason: error.to_string(),
            })?;
    }
    Ok(rule)
}

fn parse_legacy_ignores(
    value: &Value,
) -> Result<Vec<(String, AdvisoryIgnoreRule)>, PolicyConfigError> {
    match value {
        Value::Array(subjects) => subjects
            .iter()
            .map(|subject| {
                let subject =
                    subject
                        .as_str()
                        .ok_or_else(|| PolicyConfigError::InvalidIgnoreRule {
                            subject: "legacy ignore list".to_string(),
                        })?;
                Ok((subject.to_string(), AdvisoryIgnoreRule::all(None)))
            })
            .collect(),
        Value::Object(subjects) => subjects
            .iter()
            .map(|(subject, value)| Ok((subject.clone(), parse_legacy_rule(subject, value)?)))
            .collect(),
        _ => Err(PolicyConfigError::InvalidIgnoreRule {
            subject: "legacy ignore list".to_string(),
        }),
    }
}

fn parse_legacy_rule(
    subject: &str,
    value: &Value,
) -> Result<AdvisoryIgnoreRule, PolicyConfigError> {
    match value {
        Value::Null => Ok(AdvisoryIgnoreRule::all(None)),
        Value::String(reason) => Ok(AdvisoryIgnoreRule::all(Some(reason.clone()))),
        Value::Object(options) => {
            let apply = options
                .get("apply")
                .and_then(Value::as_str)
                .unwrap_or("all");
            let (on_block, on_audit) = match apply {
                "all" => (true, true),
                "block" => (true, false),
                "audit" => (false, true),
                value => {
                    return Err(PolicyConfigError::InvalidApply {
                        subject: subject.to_string(),
                        value: value.to_string(),
                    });
                }
            };
            Ok(AdvisoryIgnoreRule {
                constraint: None,
                reason: options
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                on_block,
                on_audit,
            })
        }
        _ => Err(PolicyConfigError::InvalidIgnoreRule {
            subject: subject.to_string(),
        }),
    }
}

fn merge_reason(
    result: &mut BTreeMap<String, Option<String>>,
    subject: &str,
    new_reason: Option<String>,
) {
    let Some(existing) = result.get_mut(subject) else {
        result.insert(subject.to_string(), new_reason);
        return;
    };
    match (existing.as_ref(), new_reason) {
        (_, None) => {}
        (None, Some(reason)) => *existing = Some(reason),
        (Some(current), Some(reason)) if current == &reason => {}
        (Some(current), Some(reason)) if current.split(';').any(|part| part.trim() == reason) => {}
        (Some(current), Some(reason)) => *existing = Some(format!("{current}; {reason}")),
    }
}

fn parse_list(
    value: Option<&Value>,
    default_block: bool,
    legacy_block: Option<bool>,
    legacy_audit_mode: Option<AuditBehavior>,
) -> Result<ListPolicyConfig, PolicyConfigError> {
    if value.and_then(Value::as_bool) == Some(false) {
        return Ok(ListPolicyConfig::disabled());
    }
    let options = value.and_then(Value::as_object);
    let block_scope = options
        .and_then(|options| options.get("block-scope"))
        .and_then(Value::as_str)
        .unwrap_or("all");
    if !matches!(block_scope, "update" | "install" | "all") {
        return Err(PolicyConfigError::InvalidBlockScope {
            value: block_scope.to_string(),
        });
    }
    Ok(ListPolicyConfig {
        block: options
            .and_then(|options| options.get("block"))
            .and_then(Value::as_bool)
            .or(legacy_block)
            .unwrap_or(default_block),
        audit: options
            .and_then(|options| options.get("audit"))
            .and_then(Value::as_str)
            .and_then(parse_audit)
            .or(legacy_audit_mode)
            .unwrap_or(AuditBehavior::Fail),
        block_scope: block_scope.to_string(),
        ignore: parse_structured_package_ignores(
            options.and_then(|options| options.get("ignore")),
        )?,
        ignore_source: options
            .and_then(|options| options.get("ignore-source"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    })
}

fn parse_abandoned(
    policy: Option<&serde_json::Map<String, Value>>,
    audit: Option<&serde_json::Map<String, Value>>,
) -> Result<AbandonedPolicyConfig, PolicyConfigError> {
    if let Some(value) = policy.and_then(|policy| policy.get("abandoned")) {
        if value.as_bool() == Some(false) {
            return Ok(AbandonedPolicyConfig::disabled());
        }
        let options = value.as_object();
        return Ok(AbandonedPolicyConfig {
            block: options
                .and_then(|options| options.get("block"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            audit: options
                .and_then(|options| options.get("audit"))
                .and_then(Value::as_str)
                .and_then(parse_audit)
                .unwrap_or(AuditBehavior::Fail),
            ignore: parse_structured_package_ignores(
                options.and_then(|options| options.get("ignore")),
            )?,
        });
    }

    let mut ignore = BTreeMap::new();
    if let Some(value) = audit.and_then(|audit| audit.get("ignore-abandoned")) {
        for (package, rule) in parse_legacy_ignores(value)? {
            ignore.entry(package).or_insert_with(Vec::new).push(rule);
        }
    }
    Ok(AbandonedPolicyConfig {
        block: legacy_bool(audit, "block-abandoned").unwrap_or(false),
        audit: legacy_audit(audit, "abandoned").unwrap_or(AuditBehavior::Fail),
        ignore,
    })
}

fn parse_ignore_unreachable(
    policy: Option<&serde_json::Map<String, Value>>,
    audit: Option<&serde_json::Map<String, Value>>,
) -> IgnoreUnreachable {
    if let Some(value) = policy.and_then(|policy| policy.get("ignore-unreachable")) {
        return IgnoreUnreachable::from_policy(value);
    }
    IgnoreUnreachable::from_legacy_audit(
        audit
            .and_then(|audit| audit.get("ignore-unreachable"))
            .and_then(Value::as_bool),
    )
}

fn legacy_bool(config: Option<&serde_json::Map<String, Value>>, name: &str) -> Option<bool> {
    config
        .and_then(|config| config.get(name))
        .and_then(Value::as_bool)
}

fn legacy_audit(
    config: Option<&serde_json::Map<String, Value>>,
    name: &str,
) -> Option<AuditBehavior> {
    config
        .and_then(|config| config.get(name))
        .and_then(Value::as_str)
        .and_then(parse_audit)
}

fn parse_audit(value: &str) -> Option<AuditBehavior> {
    match value {
        "ignore" => Some(AuditBehavior::Ignore),
        "report" => Some(AuditBehavior::Report),
        "fail" => Some(AuditBehavior::Fail),
        _ => None,
    }
}

fn contains_scope(scopes: &[Value], expected: &str) -> bool {
    scopes.iter().any(|scope| scope.as_str() == Some(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build(policy: Value, audit: Value, environment: PolicyEnvironment) -> PackagePolicyConfig {
        PackagePolicyConfig::from_raw(&policy, &audit, &environment).unwrap()
    }

    // Ported from Composer\Test\Policy\CustomListPolicyConfigTest::testFromRawConfig.
    #[test]
    fn composer_custom_list_policy_parses_typed_config() {
        let config = CustomListPolicyConfig::from_raw(
            "test",
            &json!({
                "block": false,
                "audit": "report",
                "ignore": {
                    "acme/test": "flagged by mistake",
                    "acme/test2": {"constraint": "1.0"}
                },
                "sources": [{"type": "url", "url": "https://example.com"}]
            }),
        )
        .unwrap();

        assert_eq!(config.name, "test");
        assert!(!config.block);
        assert_eq!(config.audit, AuditBehavior::Report);
        assert_eq!(config.ignore.len(), 2);
        assert_eq!(
            config.ignore["acme/test"][0].reason.as_deref(),
            Some("flagged by mistake")
        );
        assert_eq!(config.ignore["acme/test"][0].constraint, None);
        assert_eq!(
            config.ignore["acme/test2"][0].constraint.as_deref(),
            Some("1.0")
        );
        assert_eq!(config.ignore["acme/test2"][0].reason, None);
        assert_eq!(
            config.sources,
            vec![PolicyUrlSource {
                list: "test".to_string(),
                url: "https://example.com".to_string(),
            }]
        );
    }

    // Ported from Composer\Test\Policy\CustomListPolicyConfigTest::
    // testFromRawConfigRejectsNonHttpsSourceUrl.
    #[test]
    fn composer_custom_list_policy_rejects_non_https_sources() {
        for url in [
            "http://insecure.example.org/list.json",
            "ftp://example.org/list.json",
            "file:///etc/list.json",
            "//example.org/list.json",
        ] {
            let result = CustomListPolicyConfig::from_raw(
                "company-policy",
                &json!({"sources": [{"type": "url", "url": url}]}),
            );
            assert!(matches!(
                result,
                Err(PolicyConfigError::InvalidSource(
                    PolicySourceError::InsecureUrl { .. }
                ))
            ));
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testAllowsIgnoreUnreachableSiblingKey.
    #[test]
    fn composer_policy_config_treats_ignore_unreachable_as_a_sibling_not_a_list() {
        let config = build(
            json!({"ignore-unreachable": true}),
            json!({}),
            PolicyEnvironment::new(),
        );

        assert!(config.ignore_unreachable.update);
        assert!(config.custom_lists.is_empty());
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerPolicyAdvisoriesBlock.
    #[test]
    fn composer_policy_config_applies_advisories_block_environment_override() {
        for (environment, configured, expected) in [("1", false, true), ("0", true, false)] {
            let config = build(
                json!({"advisories": {"block": configured, "audit": "report"}}),
                json!({}),
                PolicyEnvironment::new().with(POLICY_ADVISORIES_BLOCK, environment),
            );
            assert_eq!(config.advisories.block, expected);
            assert_eq!(config.advisories.audit, AuditBehavior::Report);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerSecurityBlockingAbandoned.
    #[test]
    fn composer_policy_config_applies_legacy_abandoned_block_override() {
        for (environment, configured, expected) in [("1", false, true), ("0", true, false)] {
            let config = build(
                json!({"abandoned": {"block": configured, "audit": "report"}}),
                json!({}),
                PolicyEnvironment::new().with(LEGACY_ABANDONED_BLOCK, environment),
            );
            assert_eq!(config.abandoned.block, expected);
            assert_eq!(config.abandoned.audit, AuditBehavior::Report);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::testComposerPolicyAbandonedBlock.
    #[test]
    fn composer_policy_config_applies_canonical_abandoned_block_override() {
        for (environment, configured, expected) in [("1", false, true), ("0", true, false)] {
            let config = build(
                json!({"abandoned": {"block": configured, "audit": "report"}}),
                json!({}),
                PolicyEnvironment::new().with(POLICY_ABANDONED_BLOCK, environment),
            );
            assert_eq!(config.abandoned.block, expected);
            assert_eq!(config.abandoned.audit, AuditBehavior::Report);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerPolicyAbandonedBlockTakesPrecedenceOverLegacyAlias.
    #[test]
    fn composer_policy_config_prefers_canonical_abandoned_override() {
        let config = build(
            json!({}),
            json!({}),
            PolicyEnvironment::new()
                .with(POLICY_ABANDONED_BLOCK, "1")
                .with(LEGACY_ABANDONED_BLOCK, "0"),
        );

        assert!(config.abandoned.block);

        let canonical_does_not_parse_legacy = build(
            json!({}),
            json!({}),
            PolicyEnvironment::new()
                .with(POLICY_ABANDONED_BLOCK, "1")
                .with(LEGACY_ABANDONED_BLOCK, "invalid"),
        );
        assert!(canonical_does_not_parse_legacy.abandoned.block);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testLegacyAbandonedBlockEnvVarStillWorksWhenCanonicalUnset.
    #[test]
    fn composer_policy_config_uses_legacy_abandoned_override_as_fallback() {
        let config = build(
            json!({}),
            json!({}),
            PolicyEnvironment::new().with(LEGACY_ABANDONED_BLOCK, "1"),
        );

        assert!(config.abandoned.block);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerSecurityBlockingAbandonedWithAuditConfig.
    #[test]
    fn composer_policy_config_overrides_legacy_audit_abandoned_block() {
        for (environment, configured, expected) in [("1", false, true), ("0", true, false)] {
            let config = build(
                json!({}),
                json!({"block-abandoned": configured}),
                PolicyEnvironment::new().with(LEGACY_ABANDONED_BLOCK, environment),
            );
            assert_eq!(config.abandoned.block, expected);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerAuditAbandonedSetsAuditMode.
    #[test]
    fn composer_policy_config_overrides_abandoned_audit_mode() {
        for (environment, configured, expected) in [
            ("report", "fail", AuditBehavior::Report),
            ("fail", "report", AuditBehavior::Fail),
        ] {
            let config = build(
                json!({"abandoned": {"audit": configured}}),
                json!({}),
                PolicyEnvironment::new().with(AUDIT_ABANDONED, environment),
            );
            assert_eq!(config.abandoned.audit, expected);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerAuditAbandonedSetsAuditModeWithAuditConfig.
    #[test]
    fn composer_policy_config_overrides_legacy_abandoned_audit_mode() {
        for (environment, configured, expected) in [
            ("report", "fail", AuditBehavior::Report),
            ("fail", "report", AuditBehavior::Fail),
        ] {
            let config = build(
                json!({}),
                json!({"abandoned": configured}),
                PolicyEnvironment::new().with(AUDIT_ABANDONED, environment),
            );
            assert_eq!(config.abandoned.audit, expected);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::testComposerPolicyMalwareBlock.
    #[test]
    fn composer_policy_config_applies_malware_block_environment_override() {
        for (environment, configured, expected) in [("1", false, true), ("0", true, false)] {
            let config = build(
                json!({"malware": {"block": configured, "audit": "report"}}),
                json!({}),
                PolicyEnvironment::new().with(POLICY_MALWARE_BLOCK, environment),
            );
            assert_eq!(config.malware.block, expected);
            assert_eq!(config.malware.audit, AuditBehavior::Report);
        }
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testBothAbandonedEnvVarsApplyIndependently.
    #[test]
    fn composer_policy_config_applies_abandoned_block_and_audit_overrides_independently() {
        let config = build(
            json!({}),
            json!({}),
            PolicyEnvironment::new()
                .with(LEGACY_ABANDONED_BLOCK, "1")
                .with(AUDIT_ABANDONED, "report"),
        );

        assert!(config.abandoned.block);
        assert_eq!(config.abandoned.audit, AuditBehavior::Report);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testAdvisoriesEnvBlockOverridesWhenListExplicitlyDisabled.
    #[test]
    fn composer_policy_config_environment_reenables_disabled_advisories_blocking() {
        let config = build(
            json!({"advisories": false}),
            json!({}),
            PolicyEnvironment::new().with(POLICY_ADVISORIES_BLOCK, "1"),
        );

        assert!(config.advisories.block);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testMalwareEnvBlockOverridesWhenListExplicitlyDisabled.
    #[test]
    fn composer_policy_config_environment_reenables_disabled_malware_blocking() {
        let config = build(
            json!({"malware": false}),
            json!({}),
            PolicyEnvironment::new().with(POLICY_MALWARE_BLOCK, "1"),
        );

        assert!(config.malware.block);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testAbandonedCanonicalEnvBlockOverridesWhenListExplicitlyDisabled.
    #[test]
    fn composer_policy_config_canonical_environment_reenables_disabled_abandoned_blocking() {
        let config = build(
            json!({"abandoned": false}),
            json!({}),
            PolicyEnvironment::new().with(POLICY_ABANDONED_BLOCK, "1"),
        );

        assert!(config.abandoned.block);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testAbandonedLegacyEnvBlockOverridesWhenListExplicitlyDisabled.
    #[test]
    fn composer_policy_config_legacy_environment_reenables_disabled_abandoned_blocking() {
        let config = build(
            json!({"abandoned": false}),
            json!({}),
            PolicyEnvironment::new().with(LEGACY_ABANDONED_BLOCK, "1"),
        );

        assert!(config.abandoned.block);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testComposerAuditAbandonedOverridesWhenAbandonedExplicitlyDisabled.
    #[test]
    fn composer_policy_config_environment_sets_audit_for_disabled_abandoned_list() {
        let config = build(
            json!({"abandoned": false}),
            json!({}),
            PolicyEnvironment::new().with(AUDIT_ABANDONED, "fail"),
        );

        assert_eq!(config.abandoned.audit, AuditBehavior::Fail);
    }

    // Ported from Composer\Test\Policy\PolicyConfigTest::
    // testWithIgnoreUnreachableOnlyAffectsRequestedScope.
    #[test]
    fn composer_policy_config_widens_only_requested_ignore_unreachable_scope() {
        let config = build(
            json!({"ignore-unreachable": ["update"]}),
            json!({}),
            PolicyEnvironment::new(),
        );
        assert_eq!(
            config.ignore_unreachable,
            IgnoreUnreachable {
                audit: false,
                install: false,
                update: true,
            }
        );

        let updated = config
            .with_ignore_unreachable(&[PolicyScope::Audit])
            .unwrap();
        assert_eq!(
            updated.ignore_unreachable,
            IgnoreUnreachable {
                audit: true,
                install: false,
                update: true,
            }
        );
    }

    #[test]
    fn policy_environment_rejects_invalid_boolean_without_process_mutation() {
        let error = PackagePolicyConfig::from_raw(
            &json!({}),
            &json!({}),
            &PolicyEnvironment::new().with(POLICY_MALWARE_BLOCK, "maybe"),
        )
        .unwrap_err();
        assert!(matches!(error, PolicyConfigError::InvalidBoolean { .. }));
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::testFromRawConfig.
    #[test]
    fn composer_advisories_policy_parses_structured_config() {
        let config = build(
            json!({
                "advisories": {
                    "block": true,
                    "audit": "report",
                    "ignore": {
                        "acme/abandoned": "flagged by mistake",
                        "acme/abandoned2": {"constraint": "1.0"}
                    },
                    "ignore-severity": {
                        "low": {"reason": "reason", "on-block": false, "on-audit": false},
                        "high": "ignore"
                    },
                    "ignore-id": {
                        "CVE-2024-1234": "flagged by mistake",
                        "CVE-2024-1235": {"on-block": false}
                    }
                }
            }),
            json!({}),
            PolicyEnvironment::new(),
        );
        let advisories = config.advisories;

        assert!(advisories.block);
        assert_eq!(advisories.audit, AuditBehavior::Report);
        assert_eq!(
            advisories.ignore["acme/abandoned"][0].reason.as_deref(),
            Some("flagged by mistake")
        );
        assert_eq!(
            advisories.ignore["acme/abandoned2"][0]
                .constraint
                .as_deref(),
            Some("1.0")
        );
        assert!(!advisories.ignore_id["CVE-2024-1235"].on_block);
        assert!(!advisories.ignore_severity["low"].on_audit);
        assert_eq!(
            advisories.ignore_severity["high"].reason.as_deref(),
            Some("ignore")
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::testFromAuditConfig.
    #[test]
    fn composer_advisories_policy_parses_legacy_audit_config() {
        let config = build(
            json!({}),
            json!({
                "block": true,
                "ignore": {
                    "acme/abandoned": "flagged by mistake",
                    "acme/abandoned2": {"apply": "block"},
                    "CVE-2024-1234": "flagged by mistake",
                    "CVE-2024-1235": {"apply": "audit"}
                },
                "ignore-severity": {"low": {"apply": "block"}}
            }),
            PolicyEnvironment::new(),
        );
        let advisories = config.advisories;

        assert!(advisories.block);
        assert_eq!(advisories.audit, AuditBehavior::Fail);
        assert!(advisories.ignore["acme/abandoned2"][0].on_block);
        assert!(!advisories.ignore["acme/abandoned2"][0].on_audit);
        assert!(!advisories.ignore_id["CVE-2024-1235"].on_block);
        assert!(advisories.ignore_id["CVE-2024-1235"].on_audit);
        assert!(advisories.ignore_severity["low"].on_block);
        assert!(!advisories.ignore_severity["low"].on_audit);
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreSimpleArray.
    #[test]
    fn composer_advisories_policy_parses_legacy_ignore_id_array_for_both_operations() {
        let advisories = build(
            json!({}),
            json!({"ignore": ["CVE-2024-1234", "CVE-2024-5678"]}),
            PolicyEnvironment::new(),
        )
        .advisories;
        let expected = BTreeMap::from([
            ("CVE-2024-1234".to_string(), None),
            ("CVE-2024-5678".to_string(), None),
        ]);

        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Audit),
            expected
        );
        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Block),
            expected
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreApplyAuditOnly.
    #[test]
    fn composer_advisories_policy_scopes_legacy_ignore_to_audit() {
        let advisories = build(
            json!({}),
            json!({"ignore": {
                "CVE-2024-1234": {"apply": "audit", "reason": "Only ignore for auditing"}
            }}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Audit),
            BTreeMap::from([(
                "CVE-2024-1234".to_string(),
                Some("Only ignore for auditing".to_string())
            )])
        );
        assert!(advisories
            .ignore_list_for_operation(PolicyOperation::Block)
            .is_empty());
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreApplyBlockOnly.
    #[test]
    fn composer_advisories_policy_scopes_legacy_ignore_to_blocking() {
        let advisories = build(
            json!({}),
            json!({"ignore": {
                "CVE-2024-1234": {"apply": "block", "reason": "Only ignore for blocking"}
            }}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert!(advisories
            .ignore_list_for_operation(PolicyOperation::Audit)
            .is_empty());
        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Block),
            BTreeMap::from([(
                "CVE-2024-1234".to_string(),
                Some("Only ignore for blocking".to_string())
            )])
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreMixedFormats.
    #[test]
    fn composer_advisories_policy_parses_mixed_legacy_ignore_formats() {
        let advisories = build(
            json!({}),
            json!({"ignore": {
                "CVE-2024-1234": null,
                "CVE-2024-5678": "Simple reason",
                "CVE-2024-9999": {"apply": "audit", "reason": "Detailed reason"},
                "CVE-2024-8888": {"apply": "block"}
            }}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Audit),
            BTreeMap::from([
                ("CVE-2024-1234".to_string(), None),
                (
                    "CVE-2024-5678".to_string(),
                    Some("Simple reason".to_string())
                ),
                (
                    "CVE-2024-9999".to_string(),
                    Some("Detailed reason".to_string())
                ),
            ])
        );
        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Block),
            BTreeMap::from([
                ("CVE-2024-1234".to_string(), None),
                (
                    "CVE-2024-5678".to_string(),
                    Some("Simple reason".to_string())
                ),
                ("CVE-2024-8888".to_string(), None),
            ])
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreSeveritySimpleArray.
    #[test]
    fn composer_advisories_policy_parses_legacy_severity_array_for_both_operations() {
        let advisories = build(
            json!({}),
            json!({"ignore-severity": ["low", "medium"]}),
            PolicyEnvironment::new(),
        )
        .advisories;
        let expected = BTreeMap::from([("low".to_string(), None), ("medium".to_string(), None)]);

        assert_eq!(
            advisories.ignore_severity_for_operation(PolicyOperation::Audit),
            expected
        );
        assert_eq!(
            advisories.ignore_severity_for_operation(PolicyOperation::Block),
            expected
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreSeverityDetailedFormat.
    #[test]
    fn composer_advisories_policy_scopes_legacy_severity_rules() {
        let advisories = build(
            json!({}),
            json!({"ignore-severity": {
                "low": {"apply": "audit", "reason": "We accept low severity issues"},
                "medium": {"apply": "block"}
            }}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert_eq!(
            advisories.ignore_severity_for_operation(PolicyOperation::Audit),
            BTreeMap::from([(
                "low".to_string(),
                Some("We accept low severity issues".to_string())
            )])
        );
        assert_eq!(
            advisories.ignore_severity_for_operation(PolicyOperation::Block),
            BTreeMap::from([("medium".to_string(), None)])
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testLegacyAuditIgnoreInvalidApplyValue.
    #[test]
    fn composer_advisories_policy_rejects_invalid_legacy_apply_scope() {
        let error = PackagePolicyConfig::from_raw(
            &json!({}),
            &json!({"ignore": {"CVE-2024-1234": {"apply": "invalid"}}}),
            &PolicyEnvironment::new(),
        )
        .unwrap_err();

        assert_eq!(
            error,
            PolicyConfigError::InvalidApply {
                subject: "CVE-2024-1234".to_string(),
                value: "invalid".to_string(),
            }
        );
        assert!(error
            .to_string()
            .contains("Expected 'audit', 'block', or 'all'"));
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testGetIgnoreListForOperationMergesMultiRuleReasons.
    #[test]
    fn composer_advisories_policy_merges_multiple_package_rule_reasons() {
        let advisories = build(
            json!({"advisories": {"ignore": {
                "vendor/multi": [
                    {"constraint": "^1.0", "reason": "v1 patched"},
                    {"constraint": "^2.0", "reason": "v2 mitigated"}
                ]
            }}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories;

        for operation in [PolicyOperation::Audit, PolicyOperation::Block] {
            assert_eq!(
                advisories.ignore_list_for_operation(operation)["vendor/multi"].as_deref(),
                Some("v1 patched; v2 mitigated")
            );
        }
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testGetIgnoreListForOperationPrefersConcreteReasonOverNull.
    #[test]
    fn composer_advisories_policy_prefers_concrete_reason_over_null() {
        let advisories = build(
            json!({"advisories": {"ignore": {
                "vendor/mixed": [
                    {"constraint": "^1.0"},
                    {"constraint": "^2.0", "reason": "v2 mitigated"}
                ]
            }}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert_eq!(
            advisories.ignore_list_for_operation(PolicyOperation::Audit)["vendor/mixed"].as_deref(),
            Some("v2 mitigated")
        );
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testWithIgnoreSeverityAddsAuditScopedRulesForNewSeverities.
    #[test]
    fn composer_advisories_policy_adds_new_severity_rules_for_audit_only() {
        let advisories = AdvisoriesPolicyConfig::disabled().with_ignore_severity(["low", "medium"]);

        assert_eq!(
            advisories.ignore_severity_for_operation(PolicyOperation::Audit),
            BTreeMap::from([("low".to_string(), None), ("medium".to_string(), None)])
        );
        assert!(advisories
            .ignore_severity_for_operation(PolicyOperation::Block)
            .is_empty());
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testPolicyAdvisoriesSetIgnoresLegacyAuditAdvisoriesKeys.
    #[test]
    fn composer_advisories_policy_explicit_config_ignores_legacy_advisory_keys() {
        let advisories = build(
            json!({"advisories": {"block": false, "audit": "report"}}),
            json!({
                "block-insecure": true,
                "ignore": {"CVE-2024-1234": "should be ignored"},
                "ignore-severity": {"low": "should be ignored"}
            }),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert!(!advisories.block);
        assert_eq!(advisories.audit, AuditBehavior::Report);
        assert!(advisories.ignore.is_empty());
        assert!(advisories.ignore_id.is_empty());
        assert!(advisories.ignore_severity.is_empty());
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testPolicyAdvisoriesFalseIgnoresLegacyAuditAdvisoriesKeys.
    #[test]
    fn composer_advisories_policy_disabled_config_ignores_legacy_advisory_keys() {
        let advisories = build(
            json!({"advisories": false}),
            json!({
                "block-insecure": true,
                "ignore": {"CVE-2024-1234": "should be ignored"},
                "ignore-severity": {"low": "should be ignored"}
            }),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert_eq!(advisories, AdvisoriesPolicyConfig::disabled());
    }

    // Ported from Composer\Test\Policy\AdvisoriesPolicyConfigTest::
    // testWithIgnoreSeverityPreservesExistingRulesAndReasons.
    #[test]
    fn composer_advisories_policy_preserves_existing_severity_rules() {
        let advisories = build(
            json!({"advisories": {"ignore-severity": {
                "low": {"reason": "configured low", "on-block": false, "on-audit": true}
            }}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories
        .with_ignore_severity(["low", "medium"]);

        let audit = advisories.ignore_severity_for_operation(PolicyOperation::Audit);
        assert_eq!(audit["low"].as_deref(), Some("configured low"));
        assert_eq!(audit["medium"], None);
    }

    // Ported from Composer\Test\Policy\AbandonedPolicyConfigTest::testFromAuditConfig.
    #[test]
    fn composer_abandoned_policy_parses_legacy_audit_config() {
        let abandoned = build(
            json!({}),
            json!({
                "block-abandoned": true,
                "abandoned": "report",
                "ignore-abandoned": {
                    "acme/abandoned": "flagged by mistake",
                    "acme/abandoned2": {"apply": "block"}
                }
            }),
            PolicyEnvironment::new(),
        )
        .abandoned;

        assert!(abandoned.block);
        assert_eq!(abandoned.audit, AuditBehavior::Report);
        assert_eq!(
            abandoned.ignore["acme/abandoned"][0].reason.as_deref(),
            Some("flagged by mistake")
        );
        assert!(abandoned.ignore["acme/abandoned2"][0].on_block);
        assert!(!abandoned.ignore["acme/abandoned2"][0].on_audit);
    }

    // Ported from Composer\Test\Policy\AbandonedPolicyConfigTest::
    // testLegacyIgnoreAbandonedSimpleArray.
    #[test]
    fn composer_abandoned_policy_parses_legacy_ignore_array_for_both_operations() {
        let abandoned = build(
            json!({}),
            json!({"ignore-abandoned": ["vendor/package1", "vendor/package2"]}),
            PolicyEnvironment::new(),
        )
        .abandoned;
        let expected = BTreeMap::from([
            ("vendor/package1".to_string(), None),
            ("vendor/package2".to_string(), None),
        ]);

        assert_eq!(
            abandoned.ignore_list_for_operation(PolicyOperation::Audit),
            expected
        );
        assert_eq!(
            abandoned.ignore_list_for_operation(PolicyOperation::Block),
            expected
        );
    }

    // Ported from Composer\Test\Policy\AbandonedPolicyConfigTest::
    // testLegacyIgnoreAbandonedDetailedFormat.
    #[test]
    fn composer_abandoned_policy_scopes_legacy_ignore_rules_by_operation() {
        let abandoned = build(
            json!({}),
            json!({"ignore-abandoned": {
                "vendor/package1": {
                    "apply": "audit",
                    "reason": "Report but do not block"
                },
                "vendor/package2": {
                    "apply": "block",
                    "reason": "Block but do not report"
                }
            }}),
            PolicyEnvironment::new(),
        )
        .abandoned;

        assert_eq!(
            abandoned.ignore_list_for_operation(PolicyOperation::Audit),
            BTreeMap::from([(
                "vendor/package1".to_string(),
                Some("Report but do not block".to_string())
            )])
        );
        assert_eq!(
            abandoned.ignore_list_for_operation(PolicyOperation::Block),
            BTreeMap::from([(
                "vendor/package2".to_string(),
                Some("Block but do not report".to_string())
            )])
        );
    }

    // Ported from Composer\Test\Policy\AbandonedPolicyConfigTest::
    // testPolicyAbandonedSetIgnoresLegacyAuditAbandonedKeys.
    #[test]
    fn composer_abandoned_policy_explicit_config_ignores_legacy_audit_keys() {
        let abandoned = build(
            json!({"abandoned": {"block": true, "audit": "report"}}),
            json!({
                "block-abandoned": false,
                "abandoned": "ignore",
                "ignore-abandoned": {"acme/abandoned": "should be ignored"}
            }),
            PolicyEnvironment::new(),
        )
        .abandoned;

        assert!(abandoned.block);
        assert_eq!(abandoned.audit, AuditBehavior::Report);
        assert!(abandoned.ignore.is_empty());
    }

    // Ported from Composer\Test\Policy\AbandonedPolicyConfigTest::
    // testPolicyAbandonedFalseIgnoresLegacyAuditAbandonedKeys.
    #[test]
    fn composer_abandoned_policy_disabled_config_ignores_legacy_audit_keys() {
        let abandoned = build(
            json!({"abandoned": false}),
            json!({
                "block-abandoned": true,
                "abandoned": "fail",
                "ignore-abandoned": {"acme/abandoned": "should be ignored"}
            }),
            PolicyEnvironment::new(),
        )
        .abandoned;

        assert_eq!(abandoned, AbandonedPolicyConfig::disabled());
    }

    // Ported from Composer\Test\Policy\AbandonedPolicyConfigTest::
    // testGetFlatIgnoreForOperationMergesMultiRuleReasons.
    #[test]
    fn composer_abandoned_policy_merges_multiple_rule_reasons() {
        let abandoned = build(
            json!({"abandoned": {"ignore": {
                "vendor/multi-abandoned": [
                    {"constraint": "^1.0", "reason": "fork ready"},
                    {"constraint": "^2.0", "reason": "maintained downstream"}
                ]
            }}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .abandoned;

        assert_eq!(
            abandoned.ignore_list_for_operation(PolicyOperation::Audit)["vendor/multi-abandoned"]
                .as_deref(),
            Some("fork ready; maintained downstream")
        );
    }

    // Ported from Composer\Test\Policy\IgnoreUnreachableTest::testFromRawAuditConfig.
    #[test]
    fn composer_ignore_unreachable_parses_legacy_audit_config() {
        assert_eq!(
            IgnoreUnreachable::from_legacy_audit(Some(true)),
            IgnoreUnreachable {
                audit: true,
                install: false,
                update: false,
            }
        );
    }

    // Ported from Composer\Test\Policy\IgnoreUnreachableTest::testForBlockScope.
    #[test]
    fn composer_ignore_unreachable_selects_the_requested_block_scope() {
        let install = IgnoreUnreachable {
            audit: false,
            install: true,
            update: false,
        };
        assert!(install.for_block_scope(PolicyBlockScope::Install));
        assert!(!install.for_block_scope(PolicyBlockScope::Update));

        let update = IgnoreUnreachable {
            audit: false,
            install: false,
            update: true,
        };
        assert!(!update.for_block_scope(PolicyBlockScope::Install));
        assert!(update.for_block_scope(PolicyBlockScope::Update));
    }

    // Ported from Composer\Test\Policy\IgnoreUnreachableTest::
    // testWithOnlyFlipsRequestedScope.
    #[test]
    fn composer_ignore_unreachable_only_flips_the_requested_scope() {
        let current = IgnoreUnreachable {
            audit: false,
            install: false,
            update: true,
        };

        assert_eq!(
            current.with_scope_names(&["audit"]).unwrap(),
            IgnoreUnreachable {
                audit: true,
                install: false,
                update: true,
            }
        );
    }

    // Ported from Composer\Test\Policy\IgnoreUnreachableTest::
    // testWithAcceptsMultipleScopes.
    #[test]
    fn composer_ignore_unreachable_accepts_multiple_scopes() {
        assert_eq!(
            IgnoreUnreachable::none()
                .with_scope_names(&["audit", "install"])
                .unwrap(),
            IgnoreUnreachable {
                audit: true,
                install: true,
                update: false,
            }
        );
    }

    // Ported from Composer\Test\Policy\IgnoreUnreachableTest::testWithRejectsUnknownScope.
    #[test]
    fn composer_ignore_unreachable_rejects_an_unknown_scope() {
        assert_eq!(
            IgnoreUnreachable::none()
                .with_scope_names(&["not-a-scope"])
                .unwrap_err(),
            PolicyConfigError::UnknownScope {
                scope: "not-a-scope".to_string(),
            }
        );
    }

    // Ported from Composer\Test\Policy\IgnoreUnreachableTest::
    // testWithRequiresAtLeastOneScope.
    #[test]
    fn composer_ignore_unreachable_requires_at_least_one_scope() {
        assert_eq!(
            IgnoreUnreachable::none().with_scope_names(&[]).unwrap_err(),
            PolicyConfigError::MissingScope
        );
    }

    // Ported from Composer\Test\Policy\IgnoreSeverityRuleTest::
    // testParseIgnoreSeverityMapWithEmptyConfig.
    #[test]
    fn composer_ignore_severity_rule_parses_an_empty_config() {
        let advisories = build(
            json!({"advisories": {"ignore-severity": []}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert!(advisories.ignore_severity.is_empty());
    }

    // Ported from Composer\Test\Policy\IgnoreSeverityRuleTest::
    // testParseIgnoreSeverityMapWithIntegerKeyAndStringValue.
    #[test]
    fn composer_ignore_severity_rule_parses_a_list_of_severities() {
        let rules = build(
            json!({"advisories": {"ignore-severity": ["low", "medium"]}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories
        .ignore_severity;

        assert_eq!(rules.len(), 2);
        for severity in ["low", "medium"] {
            let rule = &rules[severity];
            assert_eq!(rule.reason, None);
            assert!(rule.on_block);
            assert!(rule.on_audit);
        }
    }

    // Ported from Composer\Test\Policy\IgnoreSeverityRuleTest::
    // testParseIgnoreSeverityMapWithMultipleMixedEntries.
    #[test]
    fn composer_ignore_severity_rule_parses_mixed_rule_shapes() {
        let rules = build(
            json!({"advisories": {"ignore-severity": {
                "low": "reason",
                "medium": {"on-block": false, "reason": "other reason"},
                "high": null,
                "critical": {"on-audit": false}
            }}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories
        .ignore_severity;

        assert_eq!(rules.len(), 4);
        assert_eq!(rules["low"].reason.as_deref(), Some("reason"));
        assert!(rules["low"].on_block);
        assert!(rules["low"].on_audit);
        assert_eq!(rules["medium"].reason.as_deref(), Some("other reason"));
        assert!(!rules["medium"].on_block);
        assert!(rules["medium"].on_audit);
        assert_eq!(rules["high"].reason, None);
        assert!(rules["high"].on_block);
        assert!(rules["high"].on_audit);
        assert_eq!(rules["critical"].reason, None);
        assert!(rules["critical"].on_block);
        assert!(!rules["critical"].on_audit);
    }

    // Ported from Composer\Test\Policy\IgnoreSeverityRuleTest::
    // testParseIgnoreSeverityMapRejectsUnsupportedShapes.
    #[test]
    fn composer_ignore_severity_rule_rejects_unsupported_shapes() {
        for invalid in [
            json!([null]),
            json!([{"severity": "low"}]),
            json!([true]),
            json!([42]),
            json!({"low": true}),
            json!({"low": 42}),
        ] {
            let result = PackagePolicyConfig::from_raw(
                &json!({"advisories": {"ignore-severity": invalid}}),
                &json!({}),
                &PolicyEnvironment::new(),
            );
            assert!(matches!(
                result,
                Err(PolicyConfigError::InvalidIgnoreRule { .. })
            ));
        }
    }

    // Ported from Composer\Test\Policy\IgnoreIdRuleTest::testParseIgnoreIdMapWithEmptyConfig.
    #[test]
    fn composer_ignore_id_rule_parses_an_empty_config() {
        let advisories = build(
            json!({"advisories": {"ignore-id": []}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories;

        assert!(advisories.ignore_id.is_empty());
    }

    // Ported from Composer\Test\Policy\IgnoreIdRuleTest::
    // testParseIgnoreIdMapWithIntegerKeyAndStringValue.
    #[test]
    fn composer_ignore_id_rule_parses_a_list_of_advisory_ids() {
        let rules = build(
            json!({"advisories": {"ignore-id": ["CVE-123", "GHSA-456"]}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories
        .ignore_id;

        assert_eq!(rules.len(), 2);
        for id in ["CVE-123", "GHSA-456"] {
            let rule = &rules[id];
            assert_eq!(rule.reason, None);
            assert!(rule.on_block);
            assert!(rule.on_audit);
        }
    }

    // Ported from Composer\Test\Policy\IgnoreIdRuleTest::
    // testParseIgnoreIdMapWithMultipleMixedEntries.
    #[test]
    fn composer_ignore_id_rule_parses_mixed_rule_shapes() {
        let rules = build(
            json!({"advisories": {"ignore-id": {
                "CVE-123": "reason",
                "CVE-456": {"on-block": false, "reason": "other reason"},
                "CVE-789": null,
                "CVE-012": {"on-audit": false}
            }}}),
            json!({}),
            PolicyEnvironment::new(),
        )
        .advisories
        .ignore_id;

        assert_eq!(rules.len(), 4);
        assert_eq!(rules["CVE-123"].reason.as_deref(), Some("reason"));
        assert!(rules["CVE-123"].on_block);
        assert!(rules["CVE-123"].on_audit);
        assert_eq!(rules["CVE-456"].reason.as_deref(), Some("other reason"));
        assert!(!rules["CVE-456"].on_block);
        assert!(rules["CVE-456"].on_audit);
        assert_eq!(rules["CVE-789"].reason, None);
        assert!(rules["CVE-789"].on_block);
        assert!(rules["CVE-789"].on_audit);
        assert_eq!(rules["CVE-012"].reason, None);
        assert!(rules["CVE-012"].on_block);
        assert!(!rules["CVE-012"].on_audit);
    }

    // Ported from Composer\Test\Policy\IgnoreIdRuleTest::
    // testParseIgnoreIdMapRejectsUnsupportedShapes.
    #[test]
    fn composer_ignore_id_rule_rejects_unsupported_shapes() {
        for invalid in [
            json!([null]),
            json!([{"id": "CVE-1"}]),
            json!([true]),
            json!([42]),
            json!({"CVE-1": true}),
            json!({"CVE-1": 42}),
        ] {
            let result = PackagePolicyConfig::from_raw(
                &json!({"advisories": {"ignore-id": invalid}}),
                &json!({}),
                &PolicyEnvironment::new(),
            );
            assert!(matches!(
                result,
                Err(PolicyConfigError::InvalidIgnoreRule { .. })
            ));
        }
    }

    #[test]
    fn composer_no_blocking_environment_aliases_disable_every_blocker() {
        for variable in [NO_BLOCKING, NO_SECURITY_BLOCKING] {
            let config = build(
                json!({
                    "advisories": {"block": true},
                    "malware": {"block": true},
                    "abandoned": {"block": true},
                    "company-policy": {"block": true}
                }),
                json!({}),
                PolicyEnvironment::new().with(variable, "1"),
            );
            assert!(config.enabled);
            assert!(!config.advisories.block);
            assert!(!config.malware.block);
            assert!(!config.abandoned.block);
            assert!(!config.custom_lists["company-policy"].block);
        }
    }
}
