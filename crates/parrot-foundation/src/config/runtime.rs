use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use parrot_domain::values::config::{
    PathPlatform, PlatformPath, SkillConfigEntry, SkillConfiguration,
};

use super::environment;
use super::node::{
    Kind, Node, Style, boolean, mapping, nonnegative_integer, positive_integer, scalar_or_empty,
    validate_keys,
};
use super::{ConfigError, Result};

/// Configuration collection exposing the source's read-only list API.
///
/// ```compile_fail
/// use parrot_foundation::config::runtime::ReadOnlyCollection;
/// let mut prefixes = ReadOnlyCollection::new(vec!["git status".to_owned()]);
/// prefixes.push("git reset".to_owned());
/// ```
///
/// ```compile_fail
/// use parrot_foundation::config::runtime::ReadOnlyCollection;
/// let mut prefixes = ReadOnlyCollection::new(vec!["git status".to_owned()]);
/// prefixes[0] = "git reset".to_owned();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadOnlyCollection<T> {
    values: Vec<T>,
}

impl<T> ReadOnlyCollection<T> {
    pub fn new(values: Vec<T>) -> Self {
        Self { values }
    }

    pub fn as_slice(&self) -> &[T] {
        &self.values
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.values.iter()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl<T> std::ops::Deref for ReadOnlyCollection<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

pub const PROFILE_IDS: [&str; 9] = [
    "coordinator",
    "build",
    "plan",
    "query",
    "explorer",
    "review",
    "worker",
    "agent-task-payload",
    "thinker",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxRuleAction {
    AllowWrite,
    DenyRead,
    AllowRead,
    DenyWrite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRule {
    pub path: String,
    pub action: SandboxRuleAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileConfig {
    pub prompt: String,
    pub usage: String,
    pub allowed_tools: Option<Vec<String>>,
    pub max_turns: i32,
    pub recursion_limit: i32,
    pub read_only: bool,
    pub is_user_selectable: bool,
    pub is_agent_selectable: bool,
    pub sandbox_rules: Vec<SandboxRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliUtilityCandidates {
    pub expected: Vec<String>,
    pub optional: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionConfig {
    pub trigger_percent: i32,
    pub target_percent: i32,
    pub maximum_input_tokens: i32,
    pub summary_output_tokens: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTaskForkHistoryMode {
    Dependency,
    Parent,
    Empty,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTaskConfig {
    pub maximum_attempts: i32,
    pub maximum_response_repairs: i32,
    pub fork_history_mode: AgentTaskForkHistoryMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestLimitsConfig {
    pub image_bytes_per_tool_cycle: i32,
    pub image_bytes_per_read: i32,
    pub provider_request_bytes: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageUploadConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub key_prefix: String,
    pub public_base_url: String,
    pub access_key_env: String,
    pub secret_key_env: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredToolDefinition {
    pub description: String,
    pub parameters_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSettings {
    pub profiles: BTreeMap<String, ProfileConfig>,
    pub default_profile: String,
    pub cli_utilities: CliUtilityCandidates,
    pub read_only_exec_command_prefixes: ReadOnlyCollection<String>,
    pub disabled_tools: BTreeSet<String>,
    pub sandbox_rules: Vec<SandboxRule>,
    pub sandbox_enabled: bool,
    pub sandbox_dev_bind: ReadOnlyCollection<String>,
    pub inline_diff: bool,
    pub compaction: CompactionConfig,
    pub agent_tasks: AgentTaskConfig,
    pub agent_send_to_parent: bool,
    pub tools: BTreeMap<String, ConfiguredToolDefinition>,
    pub live_buffer_rows: i32,
    /// -1 denotes the source's infinite timeout; all other values are positive milliseconds.
    pub user_input_timeout_ms: i32,
    pub request_limits: RequestLimitsConfig,
    pub web_fetch_allow_private: bool,
    pub image_upload: Option<ImageUploadConfig>,
    pub skills: SkillConfiguration,
    directories: Vec<(String, String)>,
}

impl RuntimeSettings {
    pub fn read(
        root: &Node,
        user_root: &Node,
        environment: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut directories = Vec::new();
        // Match source loading order: profiles validate before the remaining runtime settings.
        let profiles = read_profiles(root, environment, &mut directories)?;
        let inline_diff = root
            .get("inline_diff")
            .map(|v| boolean(Some(v), "inline_diff"))
            .transpose()?
            .unwrap_or(true);
        let web_fetch_allow_private = root
            .get("web_fetch")
            .and_then(|v| v.get("allow_private"))
            .and_then(Node::as_str)
            == Some("true");
        let request_limits = read_request_limits(root)?;
        let image_upload = read_image_upload(root)?;
        let sandbox_rules =
            read_sandbox_rules(root, "sandbox_rules", environment, &mut directories)?;
        let (sandbox_enabled, sandbox_dev_bind) = read_sandbox(root)?;
        let disabled_tools = read_disabled_tools(root)?;
        let default_profile = nonempty_scalar(root, "default_profile", "default_profile")?;
        if !profiles
            .get(&default_profile)
            .is_some_and(|profile| profile.is_user_selectable)
        {
            return Err(error("default_profile must name a user-selectable profile"));
        }
        let cli_utilities = read_cli_utilities(root)?;
        let read_only_exec_command_prefixes = read_prefixes(root)?;
        let user_input_timeout_ms = read_input_timeout(root, user_root)?;
        let live_buffer_rows = positive_integer(root, "live_buffer_rows", "live_buffer_rows")?;
        let compaction = read_compaction(root)?;
        let agent_tasks = read_agent_tasks(root)?;
        let agent_send_to_parent =
            boolean(root.get("agent_send_to_parent"), "agent_send_to_parent")?;
        let tools = read_tool_definitions(root)?;
        let skills = read_skills(root)?;
        Ok(Self {
            profiles,
            default_profile,
            cli_utilities,
            read_only_exec_command_prefixes: ReadOnlyCollection::new(
                read_only_exec_command_prefixes,
            ),
            disabled_tools,
            sandbox_rules,
            sandbox_enabled,
            sandbox_dev_bind: ReadOnlyCollection::new(sandbox_dev_bind),
            inline_diff,
            compaction,
            agent_tasks,
            agent_send_to_parent,
            tools,
            live_buffer_rows,
            user_input_timeout_ms,
            request_limits,
            web_fetch_allow_private,
            image_upload,
            skills,
            directories,
        })
    }

    pub fn snapshot(&self) -> serde_json::Value {
        use serde_json::json;
        let rules = |rules: &[SandboxRule]| -> Vec<serde_json::Value> {
            rules
                .iter()
                .map(|rule| json!({"path":rule.path,"action":format!("{:?}",rule.action)}))
                .collect()
        };
        let profiles: serde_json::Map<String,serde_json::Value> = self.profiles.iter().map(|(id,p)| (id.clone(),json!({
            "prompt":p.prompt,"usage":p.usage,"allowedTools":p.allowed_tools,"maxTurns":p.max_turns,
            "recursionLimit":p.recursion_limit,"readOnly":p.read_only,"isUserSelectable":p.is_user_selectable,
            "isAgentSelectable":p.is_agent_selectable,"sandboxRules":rules(&p.sandbox_rules)
        }))).collect();
        let tools: serde_json::Map<String, serde_json::Value> = self
            .tools
            .iter()
            .map(|(name, t)| {
                (
                    name.clone(),
                    json!({"description":t.description,"parametersJson":t.parameters_json}),
                )
            })
            .collect();
        let upload=self.image_upload.as_ref().map(|u|json!({"endpoint":u.endpoint,"bucket":u.bucket,"region":u.region,"keyPrefix":u.key_prefix,"publicBaseUrl":u.public_base_url,"accessKeyEnv":u.access_key_env,"secretKeyEnv":u.secret_key_env}));
        json!({
            "profiles":profiles,"defaultProfile":self.default_profile,
            "cliUtilities":{"expected":self.cli_utilities.expected,"optional":self.cli_utilities.optional},
            "readOnlyExecCommandPrefixes":self.read_only_exec_command_prefixes.as_slice(),"disabledTools":self.disabled_tools,
            "sandboxRules":rules(&self.sandbox_rules),"sandboxEnabled":self.sandbox_enabled,"sandboxDevBind":self.sandbox_dev_bind.as_slice(),
            "inlineDiff":self.inline_diff,"compaction":{"triggerPercent":self.compaction.trigger_percent,"targetPercent":self.compaction.target_percent,"maximumInputTokens":self.compaction.maximum_input_tokens,"summaryOutputTokens":self.compaction.summary_output_tokens},
            "agentTasks":{"maximumAttempts":self.agent_tasks.maximum_attempts,"maximumResponseRepairs":self.agent_tasks.maximum_response_repairs,"forkHistoryMode":format!("{:?}",self.agent_tasks.fork_history_mode)},
            "agentSend":{"toParent":self.agent_send_to_parent},"toolDefinitions":tools,
            "liveBufferRows":self.live_buffer_rows,"userInputTimeoutMs":self.user_input_timeout_ms,
            "requestLimits":{"imageBytesPerToolCycle":self.request_limits.image_bytes_per_tool_cycle,"imageBytesPerRead":self.request_limits.image_bytes_per_read,"providerRequestBytes":self.request_limits.provider_request_bytes},
            "webFetch":{"allowPrivate":self.web_fetch_allow_private},"imageUpload":upload,
            "skills":{"enabled":self.skills.enabled,"entries":self.skills.entries.iter().map(|entry|json!({"path":entry.path,"enabled":entry.enabled})).collect::<Vec<_>>()}
        })
    }

    /// Provision only directories admitted by validated `allow_write` rules.
    /// Call after all configuration sections have validated, as in the source loader.
    pub fn provision_directories(&self) -> Result<()> {
        for (path, field) in &self.directories {
            std::fs::create_dir_all(path)
                .map_err(|_| error(format!("{field} could not be created as a directory")))?;
        }
        Ok(())
    }
}

fn error(message: impl Into<String>) -> ConfigError {
    ConfigError::invalid_data(message.into())
}

fn section<'a>(root: &'a Node, key: &str) -> Result<&'a Node> {
    root.get(key)
        .filter(|v| v.as_mapping().is_some())
        .ok_or_else(|| error(format!("{key} must be a mapping")))
}

fn sequence<'a>(node: Option<&'a Node>, message: &str) -> Result<&'a [Node]> {
    node.and_then(Node::as_sequence)
        .ok_or_else(|| error(message))
}

fn plain_null(node: &Node) -> bool {
    node.style == Style::Plain && matches!(node.as_str(), Some("null" | "Null" | "NULL" | "~"))
}

pub fn nonempty_scalar(parent: &Node, key: &str, path: &str) -> Result<String> {
    parent
        .get(key)
        .filter(|node| !plain_null(node))
        .and_then(Node::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| error(format!("{path} must be a non-empty string")))
}

pub fn validate_profiles(root: &Node, environment: &BTreeMap<String, String>) -> Result<()> {
    read_profiles(root, environment, &mut Vec::new()).map(|_| ())
}

fn read_profiles(
    root: &Node,
    environment: &BTreeMap<String, String>,
    directories: &mut Vec<(String, String)>,
) -> Result<BTreeMap<String, ProfileConfig>> {
    let profiles = section(root, "profiles")?;
    for (id, profile) in mapping(profiles, "profiles")? {
        let id = id
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| error("each profile must be a named mapping"))?;
        if profile.as_mapping().is_none() {
            return Err(error("each profile must be a named mapping"));
        }
        if !PROFILE_IDS.contains(&id) {
            return Err(error(format!("profiles.{id} is not supported")));
        }
        // `status` is removed from the source's effective profile before validation.
        let mut profile = profile.clone();
        profile.remove("status");
        validate_keys(
            &profile,
            &format!("profiles.{id}"),
            &[
                "prompt",
                "usage",
                "allowed_tools",
                "max_turns",
                "recursion_limit",
                "read_only",
                "is_user_selectable",
                "is_agent_selectable",
                "sandbox_rules",
            ],
        )?;
    }
    let mut result = BTreeMap::new();
    for id in PROFILE_IDS {
        let path = format!("profiles.{id}");
        let profile = profiles
            .get(id)
            .filter(|node| node.as_mapping().is_some())
            .ok_or_else(|| error(format!("{path} must be a mapping")))?;
        result.insert(
            id.to_owned(),
            ProfileConfig {
                prompt: nonempty_scalar(profile, "prompt", &format!("{path}.prompt"))?,
                usage: nonempty_scalar(profile, "usage", &format!("{path}.usage"))?,
                allowed_tools: read_allowed_tools(profile, &format!("{path}.allowed_tools"))?,
                max_turns: positive_integer(profile, "max_turns", &format!("{path}.max_turns"))?,
                recursion_limit: nonnegative_integer(
                    profile,
                    "recursion_limit",
                    &format!("{path}.recursion_limit"),
                )?,
                read_only: boolean(profile.get("read_only"), &format!("{path}.read_only"))?,
                is_user_selectable: boolean(
                    profile.get("is_user_selectable"),
                    &format!("{path}.is_user_selectable"),
                )?,
                is_agent_selectable: boolean(
                    profile.get("is_agent_selectable"),
                    &format!("{path}.is_agent_selectable"),
                )?,
                sandbox_rules: read_sandbox_rules(
                    profile,
                    &format!("{path}.sandbox_rules"),
                    environment,
                    directories,
                )?,
            },
        );
    }
    Ok(result)
}

fn read_allowed_tools(parent: &Node, path: &str) -> Result<Option<Vec<String>>> {
    let Some(node) = parent.get("allowed_tools") else {
        return Ok(None);
    };
    if node.as_str() == Some("null") {
        return Ok(None);
    }
    let items = sequence(
        Some(node),
        &format!("{path} must be a string sequence or null"),
    )?;
    let mut result = Vec::new();
    for item in items {
        let value = item
            .as_str()
            .filter(|s| !s.is_empty() && s.trim() == *s && !result.iter().any(|v| v == s))
            .ok_or_else(|| {
                error(format!(
                    "{path} must contain unique nonblank untrimmed tool names"
                ))
            })?;
        result.push(value.to_owned());
    }
    Ok(Some(result))
}

fn read_cli_utilities(root: &Node) -> Result<CliUtilityCandidates> {
    let utilities = section(root, "cli_utilities")?;
    validate_keys(utilities, "cli_utilities", &["expected", "optional"])?;
    let read = |key: &str| -> Result<Vec<String>> {
        let path = format!("cli_utilities.{key}");
        let items = sequence(
            utilities.get(key),
            &format!("{path} must be a string sequence"),
        )?;
        let mut result = Vec::new();
        for item in items {
            let name = item.as_str().filter(|name| !name.is_empty() && !name.chars().any(char::is_whitespace)
                && !name.contains(['/', '\\']) && !result.iter().any(|existing|existing == name))
                .ok_or_else(||error(format!("{path} must contain unique nonempty utility names without whitespace or path separators")))?;
            result.push(name.to_owned());
        }
        Ok(result)
    };
    let expected = read("expected")?;
    let optional = read("optional")?
        .into_iter()
        .filter(|name| !expected.contains(name))
        .collect();
    Ok(CliUtilityCandidates { expected, optional })
}

fn read_prefixes(root: &Node) -> Result<Vec<String>> {
    let key = "read_only_exec_command_prefixes";
    let items = sequence(root.get(key), &format!("{key} must be a string sequence"))?;
    let mut result = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let value = item.as_str().filter(|s|!s.trim().is_empty() && s.trim()==*s && !plain_null(item) && !result.iter().any(|v|v==s))
            .ok_or_else(||error(format!("{key}[{index}] must be a unique nonblank string without leading or trailing whitespace")))?;
        result.push(value.to_owned());
    }
    Ok(result)
}

fn read_disabled_tools(root: &Node) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    let Some(node) = root.get("disabled_tools") else {
        return Ok(result);
    };
    for (name, value) in mapping(node, "disabled_tools")? {
        let name = name
            .as_str()
            .filter(|name| !name.is_empty() && name.trim() == *name)
            .ok_or_else(|| error("disabled_tools keys must be nonblank trimmed tool names"))?;
        if boolean(Some(value), &format!("disabled_tools.{name}"))? {
            result.insert(name.to_owned());
        }
    }
    Ok(result)
}

fn read_sandbox_rules(
    parent: &Node,
    path: &str,
    environment: &BTreeMap<String, String>,
    directories: &mut Vec<(String, String)>,
) -> Result<Vec<SandboxRule>> {
    let Some(node) = parent.get("sandbox_rules") else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for (index, item) in sequence(Some(node), &format!("{path} must be a sequence"))?
        .iter()
        .enumerate()
    {
        let required = format!("{path}.{index} requires scalar path and rule fields");
        if item.as_mapping().is_none() {
            return Err(error(required));
        }
        validate_keys(
            item,
            &format!("{path}.{index}"),
            &["path", "rule", "create_if_not_exist"],
        )?;
        let rule_path = item
            .get("path")
            .and_then(Node::as_str)
            .ok_or_else(|| error(&required))?;
        let action = item
            .get("rule")
            .and_then(Node::as_str)
            .ok_or_else(|| error(&required))?;
        let field = format!("{path}.{index}.path");
        let expanded = environment::resolve(rule_path, &field, environment)?;
        if expanded.trim().is_empty() || !expanded.starts_with('/') {
            return Err(error(format!(
                "{field} must resolve to a nonblank fully qualified path"
            )));
        }
        let action = match action {
            "allow_write" => SandboxRuleAction::AllowWrite,
            "deny_read" => SandboxRuleAction::DenyRead,
            "allow_read" => SandboxRuleAction::AllowRead,
            "deny_write" => SandboxRuleAction::DenyWrite,
            _ => {
                return Err(error(format!(
                    "{path}.{index}.rule has invalid action {action}"
                )));
            }
        };
        let create = item
            .get("create_if_not_exist")
            .map(|value| boolean(Some(value), &format!("{path}.{index}.create_if_not_exist")))
            .transpose()?
            .unwrap_or(false);
        if create && action != SandboxRuleAction::AllowWrite {
            return Err(error(format!(
                "{path}.{index}.create_if_not_exist is supported only for allow_write rules"
            )));
        }
        if !Path::new(&expanded).exists() && action == SandboxRuleAction::AllowWrite {
            if !create {
                continue;
            }
            directories.push((expanded.clone(), field));
        }
        result.push(SandboxRule {
            path: expanded,
            action,
        });
    }
    Ok(result)
}

fn native_platform() -> PathPlatform {
    if cfg!(target_os = "macos") {
        PathPlatform::MacOs
    } else {
        PathPlatform::Unix
    }
}

pub fn read_sandbox(root: &Node) -> Result<(bool, Vec<String>)> {
    let Some(sandbox) = root.get("sandbox") else {
        return Ok((true, Vec::new()));
    };
    mapping(sandbox, "sandbox")?;
    validate_keys(sandbox, "sandbox", &["enabled", "dev_bind"])?;
    let enabled = boolean(sandbox.get("enabled"), "sandbox.enabled")?;
    let mut paths = Vec::new();
    for (index, node) in sequence(
        sandbox.get("dev_bind"),
        "sandbox.dev_bind must be a sequence",
    )?
    .iter()
    .enumerate()
    {
        let invalid = || {
            error(format!(
                "sandbox.dev_bind[{index}] must be an absolute device path under /dev"
            ))
        };
        let path = node
            .as_str()
            .filter(|value| !value.trim().is_empty() && value.starts_with('/') && !plain_null(node))
            .ok_or_else(invalid)?;
        // Source uses Path.GetFullPath here, not PlatformPath's macOS aliases.
        let normalized =
            PlatformPath::normalize(path, "/", PathPlatform::Unix).map_err(|_| invalid())?;
        if normalized != "/dev" && !normalized.starts_with("/dev/") {
            return Err(invalid());
        }
        paths.push(normalized);
    }
    Ok((enabled, paths))
}

fn read_compaction(root: &Node) -> Result<CompactionConfig> {
    let node = section(root, "compaction")?;
    validate_keys(
        node,
        "compaction",
        &[
            "trigger_percent",
            "target_percent",
            "maximum_input_tokens",
            "summary_output_tokens",
        ],
    )?;
    let trigger_percent = positive_integer(node, "trigger_percent", "compaction.trigger_percent")?;
    let target_percent = positive_integer(node, "target_percent", "compaction.target_percent")?;
    if target_percent >= trigger_percent || trigger_percent > 99 {
        return Err(error(
            "compaction requires 1 <= target_percent < trigger_percent <= 99",
        ));
    }
    Ok(CompactionConfig {
        trigger_percent,
        target_percent,
        maximum_input_tokens: positive_integer(
            node,
            "maximum_input_tokens",
            "compaction.maximum_input_tokens",
        )?,
        summary_output_tokens: positive_integer(
            node,
            "summary_output_tokens",
            "compaction.summary_output_tokens",
        )?,
    })
}

fn read_agent_tasks(root: &Node) -> Result<AgentTaskConfig> {
    let node = section(root, "agent_tasks")?;
    if node.get("fork_parent_history").is_some() {
        return Err(error(
            "agent_tasks.fork_parent_history has been replaced by agent_tasks.fork_history_mode; use parent for true, empty for false, or dependency for the new default behavior",
        ));
    }
    validate_keys(
        node,
        "agent_tasks",
        &[
            "maximum_attempts",
            "maximum_response_repairs",
            "fork_history_mode",
        ],
    )?;
    let fork_history_mode = match node.get("fork_history_mode").and_then(Node::as_str) {
        Some("dependency") => AgentTaskForkHistoryMode::Dependency,
        Some("parent") => AgentTaskForkHistoryMode::Parent,
        Some("empty") => AgentTaskForkHistoryMode::Empty,
        _ => {
            return Err(error(
                "agent_tasks.fork_history_mode must be dependency, parent, or empty",
            ));
        }
    };
    Ok(AgentTaskConfig {
        maximum_attempts: positive_integer(
            node,
            "maximum_attempts",
            "agent_tasks.maximum_attempts",
        )?,
        maximum_response_repairs: positive_integer(
            node,
            "maximum_response_repairs",
            "agent_tasks.maximum_response_repairs",
        )?,
        fork_history_mode,
    })
}

fn read_input_timeout(root: &Node, user: &Node) -> Result<i32> {
    let (parent, key) = if user.get("user_input_timeout_ms").is_some() {
        (user, "user_input_timeout_ms")
    } else if user.get("permission_request_timeout_ms").is_some() {
        (user, "permission_request_timeout_ms")
    } else {
        (root, "user_input_timeout_ms")
    };
    if parent.get(key).and_then(Node::as_str) == Some("-1") {
        return Ok(-1);
    }
    positive_integer(parent, key, key)
        .map_err(|_| error(format!("{key} must be a positive integer or -1")))
}

fn read_request_limits(root: &Node) -> Result<RequestLimitsConfig> {
    let node = section(root, "request_limits")?;
    validate_keys(
        node,
        "request_limits",
        &[
            "image_bytes_per_tool_cycle",
            "image_bytes_per_read",
            "provider_request_bytes",
        ],
    )?;
    Ok(RequestLimitsConfig {
        image_bytes_per_tool_cycle: positive_integer(
            node,
            "image_bytes_per_tool_cycle",
            "request_limits.image_bytes_per_tool_cycle",
        )?,
        image_bytes_per_read: positive_integer(
            node,
            "image_bytes_per_read",
            "request_limits.image_bytes_per_read",
        )?,
        provider_request_bytes: positive_integer(
            node,
            "provider_request_bytes",
            "request_limits.provider_request_bytes",
        )?,
    })
}

fn read_image_upload(root: &Node) -> Result<Option<ImageUploadConfig>> {
    let Some(node) = root.get("image_upload") else {
        return Ok(None);
    };
    mapping(node, "image_upload")?;
    validate_keys(
        node,
        "image_upload",
        &[
            "endpoint",
            "bucket",
            "region",
            "key_prefix",
            "public_base_url",
            "access_key_env",
            "secret_key_env",
        ],
    )?;
    let bucket = scalar_or_empty(node, "bucket");
    if !(3..=63).contains(&bucket.len())
        || !bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        || !bucket
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !bucket
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(error("image_upload.bucket must be a valid S3 bucket name"));
    }
    let key_prefix = if node.get("key_prefix").is_some() {
        scalar_or_empty(node, "key_prefix")
    } else {
        "parrot/".to_owned()
    };
    if !key_prefix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'.' | b'-'))
    {
        return Err(error(
            "image_upload.key_prefix may contain only letters, digits, '/', '_', '.', and '-'",
        ));
    }
    let endpoint = read_https_url(node, "endpoint")?;
    let public_base_url = if scalar_or_empty(node, "public_base_url").is_empty() {
        String::new()
    } else {
        read_https_url(node, "public_base_url")?
    };
    Ok(Some(ImageUploadConfig {
        endpoint,
        bucket,
        region: if node.get("region").is_some() {
            scalar_or_empty(node, "region")
        } else {
            "us-east-1".to_owned()
        },
        key_prefix,
        public_base_url,
        access_key_env: scalar_or_empty(node, "access_key_env"),
        secret_key_env: scalar_or_empty(node, "secret_key_env"),
    }))
}

fn read_https_url(node: &Node, key: &str) -> Result<String> {
    let value = scalar_or_empty(node, key);
    // WHATWG URL parsing repairs missing authority slashes and drops tabs in
    // hosts; System.Uri rejects those source inputs instead.
    let authority = value
        .trim_matches([' ', '\t', '\r', '\n'])
        .split_once(':')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("https"))
        .and_then(|(_, remainder)| remainder.strip_prefix("//"))
        .map(|remainder| remainder.split(['/', '\\', '?', '#']).next().unwrap_or(""));
    let authority_is_valid = authority.is_some_and(|host| {
        !host.is_empty() && !host.chars().any(|c| c.is_whitespace() || c.is_control())
    });
    let parsed = url::Url::parse(&value).ok();
    if !authority_is_valid
        || !parsed.as_ref().is_some_and(|uri| {
            uri.scheme() == "https"
                && uri.host_str().is_some_and(|host| !host.is_empty())
                && uri.query().is_none()
                && uri.fragment().is_none()
                && uri.username().is_empty()
                && uri.password().is_none()
        })
    {
        return Err(error(format!("image_upload.{key} must be an https URL")));
    }
    Ok(value)
}

pub fn canonical_skill_path(value: &str, field: &str) -> Result<String> {
    if !value.starts_with('/') {
        return Err(error(format!("{field} must be an absolute path")));
    }
    let normalized = PlatformPath::normalize(value, "/", native_platform())
        .map_err(|_| error(format!("{field} must be a valid absolute path")))?;
    Ok(if normalized == "/" {
        normalized
    } else {
        normalized.trim_end_matches('/').to_owned()
    })
}

pub fn read_skills(root: &Node) -> Result<SkillConfiguration> {
    let Some(node) = root.get("skills") else {
        return Ok(SkillConfiguration::default());
    };
    mapping(node, "skills")?;
    validate_keys(node, "skills", &["enabled", "entries"])?;
    let enabled = node
        .get("enabled")
        .map(|n| boolean(Some(n), "skills.enabled"))
        .transpose()?
        .unwrap_or(true);
    let mut entries: Vec<SkillConfigEntry> = Vec::new();
    if let Some(configured) = node.get("entries") {
        for (index, item) in sequence(Some(configured), "skills.entries must be a sequence")?
            .iter()
            .enumerate()
        {
            let path = format!("skills.entries[{index}]");
            mapping(item, &path)?;
            validate_keys(item, &path, &["path", "enabled"])?;
            let value = nonempty_scalar(item, "path", &format!("{path}.path"))?;
            let canonical = canonical_skill_path(&value, &format!("{path}.path"))?;
            let enabled = boolean(item.get("enabled"), &format!("{path}.enabled"))?;
            entries.retain(|entry| entry.path != canonical);
            entries.push(SkillConfigEntry {
                path: canonical,
                enabled,
            });
        }
    }
    Ok(SkillConfiguration::new(enabled, entries))
}

pub fn read_tool_definitions(root: &Node) -> Result<BTreeMap<String, ConfiguredToolDefinition>> {
    let tools = section(root, "tools")?;
    let mut result = BTreeMap::new();
    for (name, node) in mapping(tools, "tools")? {
        let name = name
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| error("each tools entry must be a named mapping"))?;
        if node.as_mapping().is_none() {
            return Err(error("each tools entry must be a named mapping"));
        }
        let path = format!("tools.{name}");
        validate_keys(node, &path, &["description", "parameters"])?;
        let parameters = node
            .get("parameters")
            .filter(|node| node.as_mapping().is_some())
            .ok_or_else(|| error(format!("{path}.parameters must be a mapping")))?;
        validate_schema_descriptions(parameters, &format!("{path}.parameters"), false)?;
        let description = nonempty_scalar(node, "description", &format!("{path}.description"))?;
        let parameters_json = schema_json(parameters, &format!("{path}.parameters"))?;
        result.insert(
            name.to_owned(),
            ConfiguredToolDefinition {
                description,
                parameters_json,
            },
        );
    }
    Ok(result)
}

fn schema_map_keyword(name: &str) -> bool {
    matches!(
        name,
        "properties" | "patternProperties" | "dependentSchemas" | "$defs" | "definitions"
    )
}
fn schema_keyword(name: &str) -> bool {
    schema_map_keyword(name)
        || matches!(
            name,
            "items"
                | "additionalProperties"
                | "not"
                | "if"
                | "then"
                | "else"
                | "contains"
                | "propertyNames"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "allOf"
                | "anyOf"
                | "oneOf"
                | "prefixItems"
        )
}

fn validate_schema_descriptions(node: &Node, path: &str, property_map: bool) -> Result<()> {
    match &node.kind {
        Kind::Mapping(entries) => {
            for (name, value) in entries {
                let name = name
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        error(format!("{path} must use non-empty scalar property names"))
                    })?;
                if !property_map
                    && name == "description"
                    && value.as_str().is_none_or(|s| s.is_empty())
                {
                    return Err(error(format!(
                        "{path}.description must be a non-empty scalar"
                    )));
                }
                if property_map || schema_keyword(name) {
                    validate_schema_descriptions(
                        value,
                        &format!("{path}.{name}"),
                        !property_map && schema_map_keyword(name),
                    )?;
                }
            }
        }
        Kind::Sequence(items) => {
            for (index, item) in items.iter().enumerate() {
                validate_schema_descriptions(item, &format!("{path}[{index}]"), false)?;
            }
        }
        Kind::Scalar(_) => {}
    }
    Ok(())
}

/// JSON schema conversion is intentionally distinct from provider-preference YamlJson:
/// quoting fixes strings, while unquoted JSON numbers retain their original lexemes.
pub fn schema_json(node: &Node, path: &str) -> Result<String> {
    match &node.kind {
        Kind::Mapping(entries) => {
            let mut values = Vec::new();
            for (key, value) in entries {
                let key = key.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                    error(format!("{path} must use non-empty scalar property names"))
                })?;
                values.push(format!(
                    "{}:{}",
                    json_string(key),
                    schema_json(value, &format!("{path}.{key}"))?
                ));
            }
            Ok(format!("{{{}}}", values.join(",")))
        }
        Kind::Sequence(items) => {
            let values: Result<Vec<_>> = items
                .iter()
                .enumerate()
                .map(|(index, item)| schema_json(item, &format!("{path}[{index}]")))
                .collect();
            Ok(format!("[{}]", values?.join(",")))
        }
        Kind::Scalar(value) => {
            let value = value.as_deref();
            if node.style != Style::Plain {
                return Ok(json_string(value.unwrap_or("")));
            }
            match value {
                None | Some("null" | "Null" | "NULL" | "~") => Ok("null".to_owned()),
                Some("true" | "True" | "TRUE") => Ok("true".to_owned()),
                Some("false" | "False" | "FALSE") => Ok("false".to_owned()),
                Some(value) if is_json_number(value) => Ok(value.to_owned()),
                Some(
                    ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF" | "-.inf" | "-.Inf"
                    | "-.INF" | "+.inf" | "+.Inf" | "+.INF",
                ) => Err(error(format!("{path} must be representable as JSON"))),
                Some(value) => Ok(json_string(value)),
            }
        }
    }
}

fn is_json_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = usize::from(bytes.first() == Some(&b'-'));
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if start == index {
            return false;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if start == index {
            return false;
        }
    }
    index == bytes.len()
}

pub fn json_string(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{0008}' => result.push_str("\\b"),
            '\u{000c}' => result.push_str("\\f"),
            character
                if !((' '..='~').contains(&character))
                    || matches!(character, '"' | '\'' | '&' | '+' | '<' | '>' | '`') =>
            {
                for unit in character.encode_utf16(&mut [0; 2]).iter() {
                    result.push_str(&format!("\\u{unit:04X}"));
                }
            }
            character => result.push(character),
        }
    }
    result.push('"');
    result
}
