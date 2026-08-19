//! Configuration.
//!
//! Three layers, each overriding the one before it:
//!
//!   1. the defaults in this file, so Genesis runs with no configuration at all
//!   2. `genesis.toml` (or `$GENESIS_CONFIG`), the normal place to change things
//!   3. environment variables, for per-run overrides and secrets
//!
//! The file is parsed with `deny_unknown_fields`: a mistyped key is an error at
//! startup rather than a setting that silently does nothing.
//!
//! The OpenRouter key may be set in the file or in the environment, with the
//! environment winning. It is held in a `Secret`, whose `Debug` prints nothing,
//! so it cannot reach a log through an incidental `{:?}`.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::slot::Slot;

// ---------------------------------------------------------------------------
// Runtime configuration
// ---------------------------------------------------------------------------

/// A value that must never be printed.
///
/// `Debug` is implemented by hand: deriving it anywhere up the tree — on
/// `Config`, or on something holding a `Config` — would otherwise be enough to
/// spill the key into a log line.
#[derive(Clone, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "Secret(empty)" } else { "Secret(***)" })
    }
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ModeSpec {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Whether tools that change things are withheld in this mode. Carried to
    /// the agent so a new mode needs no agent code.
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub artifacts: PathBuf,
    pub skills: PathBuf,
    pub templates: PathBuf,
    pub wit: PathBuf,
    /// The agent's source tree.
    pub agent: PathBuf,
    /// Directory holding gateway crates, and the prefix their names carry.
    pub gateways: PathBuf,
    pub gateway_prefix: String,
    pub tools: PathBuf,
    pub tool_prefix: String,
}

#[derive(Debug, Clone)]
pub struct BuildSettings {
    pub command: String,
    pub target: String,
    pub profile: String,
    /// Shared cargo target directory, so guests compile their dependencies once.
    pub target_dir: PathBuf,
    /// Pass `--locked` when a lockfile exists, keeping resolution reproducible.
    pub locked: bool,
    pub extra_args: Vec<String>,
    /// How long a single cargo invocation may run before it is killed.
    ///
    /// Builds hold a process-wide lock, so one that never returns - a stalled
    /// crates.io fetch, a dependency's `build.rs` waiting on input - would wedge
    /// every future build permanently. This is the backstop for that.
    pub timeout: Duration,
    /// Crates a guest may depend on. Empty means no restriction.
    pub allowed_crates: Vec<String>,
}

/// What the WASI sandbox actually hands to a guest.
///
/// Guests run on WASI preview 2, which is capability-based: the interfaces are
/// linked either way, but they do nothing at all until a capability is granted.
/// Before this existed the context was built empty, so a tool could link
/// `reqwest`, compile it, and then find no sockets underneath at runtime.
#[derive(Debug, Clone)]
pub struct WasiSettings {
    /// Outbound sockets and `wasi:http`. Any tool calling a web API needs it.
    pub network: bool,
    /// Name resolution. Without it a guest can only reach literal addresses.
    pub dns: bool,
    /// The host's environment variables.
    pub env: bool,
    /// The host's stdin, stdout and stderr.
    pub stdio: bool,
    /// Directories handed to guests as preopens, readable and writable.
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchdogSettings {
    pub failure_window: Duration,
    pub failure_threshold: usize,
    pub probe_interval: Duration,
    /// How long the file watcher ignores a slot the orchestrator just wrote to.
    pub watch_suppression: Duration,
    pub debounce: Duration,
}

#[derive(Debug, Clone)]
pub struct DevkitSettings {
    pub enabled: bool,
    /// Files that decide what runs during a build; a host-side build executes
    /// them, so guests may not edit them.
    pub protected_files: Vec<String>,
    pub protected_dirs: Vec<String>,
}

/// How a provider's prompt cache is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStrategy {
    /// The provider caches long prefixes on its own; marking would only cost
    /// writes. Keeping the prefix stable is all that helps.
    Automatic,
    /// Nothing is cached unless a breakpoint says so.
    Breakpoints,
}

#[derive(Debug, Clone)]
pub struct CacheSettings {
    pub enabled: bool,
    /// Cache lifetime, e.g. "5m" or "1h". A longer one costs more to write.
    pub ttl: String,
    /// How far apart the stable anchors sit in the message list. Smaller means
    /// more resilience to turns that add many blocks, at the cost of more
    /// frequent writes.
    pub anchor_stride: usize,
    /// Vendors that need explicit breakpoints. Anything else is left automatic.
    pub explicit_vendors: Vec<String>,
}

impl CacheSettings {
    pub fn strategy_for(&self, vendor: &str) -> CacheStrategy {
        if self
            .explicit_vendors
            .iter()
            .any(|v| v.eq_ignore_ascii_case(vendor))
        {
            CacheStrategy::Breakpoints
        } else {
            CacheStrategy::Automatic
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilesystemSettings {
    pub enabled: bool,
    /// Every path the agent touches must resolve inside one of these.
    pub roots: Vec<PathBuf>,
    pub max_read_bytes: usize,
    /// Names that may not be written or deleted anywhere under a root. This
    /// protects the system's own state from an accidental `rm -rf`, and is not
    /// a security boundary — a terminal session can reach them regardless.
    pub protected: Vec<String>,
    pub allow_delete: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalSettings {
    pub enabled: bool,
    pub shell: String,
    pub shell_args: Vec<String>,
    pub max_sessions: usize,
    pub default_timeout: Duration,
    pub max_output_bytes: usize,
    pub idle_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ControlSettings {
    pub allow_restart: bool,
    /// A restart is refused before this much uptime, so a misbehaving agent
    /// cannot put the process into a restart loop.
    pub min_uptime: Duration,
    /// How long to wait after the call so the turn can finish and the user can
    /// read why.
    pub restart_delay: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Project root: the directory holding the config file and the source trees.
    pub root: PathBuf,
    /// The file this was loaded from, and the one edits are written back to.
    /// It need not exist: absent means everything came from defaults.
    pub config_path: PathBuf,
    /// Per-tool settings, keyed by tool name, from `[tools.<name>]`. Each tool
    /// is handed its own block and never sees another's.
    pub tools: std::collections::BTreeMap<String, toml::Value>,
    pub paths: Paths,
    pub bind_addr: SocketAddr,
    /// Gateway slot that serves the browser UI.
    pub primary_gateway: String,
    pub admin_enabled: bool,

    // --- llm ---------------------------------------------------------------
    pub openrouter_api_key: Option<Secret>,
    pub openrouter_base: String,
    pub model: String,
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub models: Vec<ModelSpec>,

    // --- agent loop --------------------------------------------------------
    pub system_prompt: String,
    pub max_iterations: u32,
    pub modes: Vec<ModeSpec>,
    pub default_mode: String,

    // --- budgets -----------------------------------------------------------
    pub turn_budget: Duration,
    pub wasm_slice: Duration,
    pub tool_budget: Duration,
    pub probe_budget: Duration,

    // --- ceilings ----------------------------------------------------------
    pub agent_memory_bytes: usize,
    pub tool_memory_bytes: usize,
    pub gateway_memory_bytes: usize,
    pub session_spend_limit_usd: f64,
    pub max_tool_output_bytes: usize,
    pub max_attachment_bytes: usize,
    pub max_attachments: usize,

    pub cache: CacheSettings,
    pub build: BuildSettings,
    pub wasi: WasiSettings,
    pub watchdog: WatchdogSettings,
    pub devkit: DevkitSettings,
    pub filesystem: FilesystemSettings,
    pub terminal: TerminalSettings,
    pub control: ControlSettings,
    pub sandbox_available: bool,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.paths.data.join("genesis.redb")
    }

    /// Source directory for a slot.
    pub fn slot_source_dir(&self, slot: &Slot) -> PathBuf {
        match slot {
            Slot::Agent => self.paths.agent.clone(),
            Slot::Gateway(name) => self
                .paths
                .gateways
                .join(format!("{}{name}", self.paths.gateway_prefix)),
            Slot::Tool(name) => self
                .paths
                .tools
                .join(format!("{}{name}", self.paths.tool_prefix)),
        }
    }

    /// Cargo package name of the crate backing a slot, taken from its directory
    /// so the convention lives in configuration rather than in code.
    pub fn slot_crate_name(&self, slot: &Slot) -> String {
        self.slot_source_dir(slot)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| slot.key())
    }

    /// Filename cargo emits for a slot's component.
    pub fn slot_wasm_filename(&self, slot: &Slot) -> String {
        format!("{}.wasm", self.slot_crate_name(slot).replace('-', "_"))
    }

    pub fn slot_artifact_dir(&self, slot: &Slot, revision: u64) -> PathBuf {
        self.paths
            .artifacts
            .join(slot.artifact_subdir())
            .join(format!("r{revision:04}"))
    }

    /// One tool's settings as JSON, or `{}` when it has none configured.
    ///
    /// Scoped deliberately: a tool is handed its own block and cannot read
    /// another's, nor anything else in the configuration.
    pub fn tool_config_json(&self, tool: &str) -> String {
        self.tools
            .get(tool)
            .and_then(|v| serde_json::to_string(v).ok())
            .unwrap_or_else(|| "{}".to_string())
    }

    pub fn mode(&self, id: &str) -> Option<&ModeSpec> {
        self.modes.iter().find(|m| m.id == id)
    }

    /// Directories the file watcher follows.
    pub fn watched_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.paths.agent.clone(),
            self.paths.gateways.clone(),
            self.paths.tools.clone(),
            self.paths.wit.clone(),
        ]
    }
}

// ---------------------------------------------------------------------------
// File shape
// ---------------------------------------------------------------------------

mod spec {
    use serde::Deserialize;

    // No `Debug` on this or on `Llm`: both hold the raw API key, and a derived
    // Debug anywhere above them would be enough to print it.
    #[derive(Default, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct File {
        pub server: Server,
        pub paths: Paths,
        pub llm: Llm,
        pub agent: Agent,
        pub models: Vec<Model>,
        pub modes: Vec<Mode>,
        pub budgets: Budgets,
        pub limits: Limits,
        pub cache: Cache,
        pub build: Build,
        pub watchdog: Watchdog,
        pub devkit: Devkit,
        pub sandbox: Sandbox,
        pub filesystem: Filesystem,
        pub terminal: Terminal,
        pub control: Control,
        pub wasi: Wasi,
        /// Free-form per-tool settings. Shapes are up to each tool, so this is
        /// carried as-is rather than being given a schema here.
        pub tools: std::collections::BTreeMap<String, toml::Value>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Server {
        pub bind: String,
        pub primary_gateway: String,
        pub admin_enabled: bool,
    }
    impl Default for Server {
        fn default() -> Self {
            Self {
                bind: "127.0.0.1:7777".into(),
                primary_gateway: "web".into(),
                admin_enabled: true,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Paths {
        pub data: String,
        pub artifacts: String,
        pub skills: String,
        pub templates: String,
        pub wit: String,
        pub agent: String,
        pub gateways: String,
        pub gateway_prefix: String,
        pub tools: String,
        pub tool_prefix: String,
    }
    impl Default for Paths {
        fn default() -> Self {
            Self {
                data: "data".into(),
                artifacts: "artifacts".into(),
                skills: "skills".into(),
                templates: "templates".into(),
                wit: "wit".into(),
                agent: "agents/agent-core".into(),
                gateways: "gateways".into(),
                gateway_prefix: "gateway-".into(),
                tools: "tools".into(),
                tool_prefix: "".into(),
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Llm {
        pub base_url: String,
        pub model: String,
        pub api_key: String,
        pub request_timeout_secs: u64,
        pub max_retries: u32,
    }
    impl Default for Llm {
        fn default() -> Self {
            Self {
                base_url: "https://openrouter.ai/api/v1".into(),
                model: "anthropic/claude-sonnet-4.5".into(),
                api_key: String::new(),
                request_timeout_secs: 180,
                max_retries: 3,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Agent {
        pub max_iterations: u32,
        pub default_mode: String,
        /// Inline prompt. Ignored when `system_prompt_file` is set.
        pub system_prompt: String,
        /// Path to a prompt file, relative to the project root.
        pub system_prompt_file: String,
    }
    impl Default for Agent {
        fn default() -> Self {
            Self {
                max_iterations: 32,
                default_mode: "agent".into(),
                system_prompt: String::new(),
                system_prompt_file: String::new(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Model {
        pub id: String,
        #[serde(default)]
        pub label: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Mode {
        pub id: String,
        #[serde(default)]
        pub label: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub read_only: bool,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Budgets {
        pub turn_secs: u64,
        pub wasm_slice_secs: u64,
        pub tool_secs: u64,
        pub probe_secs: u64,
    }
    impl Default for Budgets {
        fn default() -> Self {
            Self {
                turn_secs: 300,
                wasm_slice_secs: 10,
                tool_secs: 30,
                probe_secs: 5,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Limits {
        pub agent_memory_mb: usize,
        pub tool_memory_mb: usize,
        pub gateway_memory_mb: usize,
        pub session_spend_limit_usd: f64,
        pub max_tool_output_bytes: usize,
        pub max_attachment_bytes: usize,
        pub max_attachments: usize,
    }
    impl Default for Limits {
        fn default() -> Self {
            Self {
                agent_memory_mb: 512,
                tool_memory_mb: 128,
                gateway_memory_mb: 128,
                session_spend_limit_usd: 0.0,
                max_tool_output_bytes: 32_768,
                max_attachment_bytes: 8_388_608,
                max_attachments: 8,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Cache {
        pub enabled: bool,
        pub ttl: String,
        pub anchor_stride: usize,
        pub explicit_vendors: Vec<String>,
    }
    impl Default for Cache {
        fn default() -> Self {
            Self {
                enabled: true,
                ttl: "5m".into(),
                anchor_stride: 8,
                // Anthropic caches nothing without being told to. OpenAI and
                // Google do it themselves, and explicit marks there bill writes
                // for prefixes that move every turn.
                explicit_vendors: vec!["anthropic".into()],
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Build {
        pub command: String,
        pub target: String,
        pub profile: String,
        pub target_dir: String,
        pub locked: bool,
        pub extra_args: Vec<String>,
        pub timeout_secs: u64,
        pub allowed_crates: Vec<String>,
    }
    impl Default for Build {
        fn default() -> Self {
            Self {
                command: "cargo".into(),
                target: "wasm32-wasip2".into(),
                profile: "release".into(),
                target_dir: "target-wasm".into(),
                locked: true,
                extra_args: Vec::new(),
                // Generous: a cold build that fetches a large dependency tree
                // is slow but legitimate. This only catches genuine hangs.
                timeout_secs: 900,
                allowed_crates: Vec::new(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Wasi {
        pub network: bool,
        pub dns: bool,
        pub env: bool,
        pub stdio: bool,
        pub dirs: Vec<String>,
    }
    impl Default for Wasi {
        fn default() -> Self {
            Self {
                // Permissive: a tool that cannot reach the network cannot be a
                // web tool, and the component boundary is still the sandbox.
                network: true,
                dns: true,
                // Deliberately not permissive. The host environment is where
                // the API keys live, and no guest has a reason to read them.
                env: false,
                stdio: false,
                dirs: vec!["workspace".into()],
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Watchdog {
        pub failure_window_secs: u64,
        pub failure_threshold: usize,
        pub probe_interval_secs: u64,
        pub watch_suppression_secs: u64,
        pub debounce_ms: u64,
    }
    impl Default for Watchdog {
        fn default() -> Self {
            Self {
                failure_window_secs: 120,
                failure_threshold: 3,
                probe_interval_secs: 30,
                watch_suppression_secs: 5,
                debounce_ms: 500,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Devkit {
        pub enabled: bool,
        pub protected_files: Vec<String>,
        pub protected_dirs: Vec<String>,
    }
    impl Default for Devkit {
        fn default() -> Self {
            Self {
                enabled: true,
                protected_files: ["Cargo.toml", "Cargo.lock", "build.rs"]
                    .map(String::from)
                    .to_vec(),
                protected_dirs: [".cargo", "target"].map(String::from).to_vec(),
            }
        }
    }

    #[derive(Debug, Default, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Sandbox {
        pub enabled: bool,
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Filesystem {
        pub enabled: bool,
        pub roots: Vec<String>,
        pub max_read_bytes: usize,
        pub protected: Vec<String>,
        pub allow_delete: bool,
    }
    impl Default for Filesystem {
        fn default() -> Self {
            Self {
                enabled: true,
                // Empty means "the project root", resolved at load time.
                roots: Vec::new(),
                max_read_bytes: 1_048_576,
                protected: ["data", "artifacts", ".git"].map(String::from).to_vec(),
                allow_delete: true,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Terminal {
        pub enabled: bool,
        pub shell: String,
        pub shell_args: Vec<String>,
        pub max_sessions: usize,
        pub default_timeout_ms: u64,
        pub max_output_bytes: usize,
        pub idle_timeout_secs: u64,
    }
    impl Default for Terminal {
        fn default() -> Self {
            Self {
                enabled: true,
                // Empty means "whatever suits this platform".
                shell: String::new(),
                shell_args: Vec::new(),
                max_sessions: 4,
                default_timeout_ms: 30_000,
                max_output_bytes: 65_536,
                idle_timeout_secs: 1_800,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Control {
        pub allow_restart: bool,
        pub min_uptime_secs: u64,
        pub restart_delay_ms: u64,
    }
    impl Default for Control {
        fn default() -> Self {
            Self {
                allow_restart: true,
                min_uptime_secs: 20,
                restart_delay_ms: 1_500,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str, current: T) -> T {
    env_string(key).and_then(|v| v.parse().ok()).unwrap_or(current)
}

/// Walks up from `start` looking for the marker that identifies a project root.
fn discover_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("genesis.toml").is_file() || dir.join("wit").is_dir())
        .map(Path::to_path_buf)
}

/// Joins a configured path onto the root unless it is already absolute.
/// `genesis.toml` -> `genesis.local.toml`, keeping any other stem intact.
fn local_overlay_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "genesis".to_string());
    config_path.with_file_name(format!("{stem}.local.toml"))
}

fn read_toml(path: &Path) -> Result<toml::Value> {
    if !path.is_file() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Deep-merges `overlay` into `base`, with the overlay winning.
///
/// Recursive on tables so an overlay can set one key of one tool without
/// restating the whole section. Arrays are replaced rather than concatenated:
/// appending would make it impossible to shorten a list from the overlay.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn resolve(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir().context("resolving the current directory")?;
        let root = match env_string("GENESIS_ROOT") {
            Some(r) => PathBuf::from(r),
            None => discover_root(&cwd).unwrap_or(cwd),
        };

        let config_path = match env_string("GENESIS_CONFIG") {
            Some(p) => resolve(&root, &p),
            None => root.join("genesis.toml"),
        };

        let mut merged = read_toml(&config_path)?;

        // `genesis.toml` is committed; the local overlay beside it is not.
        // Secrets that belong to a tool - an API key for a service it calls -
        // have nowhere else to go, since a tool's settings are read from the
        // config file rather than the environment.
        let local_path = local_overlay_path(&config_path);
        if local_path.is_file() {
            merge_toml(&mut merged, read_toml(&local_path)?);
            tracing::debug!(overlay = %local_path.display(), "applied local config overlay");
        }

        let file: spec::File = merged
            .try_into()
            .with_context(|| format!("parsing {}", config_path.display()))?;

        Self::assemble(root, config_path, file)
    }

    /// The overlay that sits beside a config file: `genesis.toml` becomes
    /// `genesis.local.toml`.
    pub fn local_overlay(&self) -> PathBuf {
        local_overlay_path(&self.config_path)
    }

    /// Checks that a candidate config file would load, without applying it.
    ///
    /// Genesis refuses to start on a bad configuration, so writing one is the
    /// one mistake that cannot be undone from inside the running system. Every
    /// edit goes through here first.
    pub fn validate(text: &str, root: &Path) -> Result<()> {
        let file: spec::File = toml::from_str(text).context("the file is not valid TOML")?;
        Self::assemble(
            root.to_path_buf(),
            root.join("genesis.toml"),
            file,
        )?;
        Ok(())
    }

    fn assemble(root: PathBuf, config_path: PathBuf, file: spec::File) -> Result<Self> {
        let paths = Paths {
            data: resolve(&root, &file.paths.data),
            artifacts: resolve(&root, &file.paths.artifacts),
            skills: resolve(&root, &file.paths.skills),
            templates: resolve(&root, &file.paths.templates),
            wit: resolve(&root, &file.paths.wit),
            agent: resolve(&root, &file.paths.agent),
            gateways: resolve(&root, &file.paths.gateways),
            gateway_prefix: file.paths.gateway_prefix,
            tools: resolve(&root, &file.paths.tools),
            tool_prefix: file.paths.tool_prefix,
        };

        let bind_raw = env_string("GENESIS_BIND").unwrap_or(file.server.bind);
        let bind_addr: SocketAddr = bind_raw
            .parse()
            .with_context(|| format!("`{bind_raw}` is not a valid host:port"))?;

        // A prompt file wins over an inline prompt; neither means the built-in.
        let system_prompt = match env_string("GENESIS_SYSTEM_PROMPT") {
            Some(p) => p,
            None if !file.agent.system_prompt_file.is_empty() => {
                let path = resolve(&root, &file.agent.system_prompt_file);
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading the system prompt at {}", path.display()))?
            }
            None if !file.agent.system_prompt.is_empty() => file.agent.system_prompt,
            None => default_system_prompt().to_string(),
        };

        let models = match env_string("GENESIS_MODELS") {
            Some(raw) => parse_models_env(&raw),
            None if !file.models.is_empty() => file
                .models
                .into_iter()
                .map(|m| ModelSpec {
                    label: if m.label.is_empty() { m.id.clone() } else { m.label },
                    id: m.id,
                })
                .collect(),
            None => builtin_models(),
        };

        let modes: Vec<ModeSpec> = if file.modes.is_empty() {
            builtin_modes()
        } else {
            file.modes
                .into_iter()
                .map(|m| ModeSpec {
                    label: if m.label.is_empty() { m.id.clone() } else { m.label },
                    id: m.id,
                    description: m.description,
                    read_only: m.read_only,
                })
                .collect()
        };

        let default_mode = env_string("GENESIS_DEFAULT_MODE").unwrap_or(file.agent.default_mode);
        if !modes.iter().any(|m| m.id == default_mode) {
            anyhow::bail!(
                "default_mode `{default_mode}` is not one of the configured modes ({})",
                modes
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let config = Self {
            paths,
            bind_addr,
            primary_gateway: env_string("GENESIS_GATEWAY")
                .unwrap_or(file.server.primary_gateway),
            admin_enabled: env_parse("GENESIS_ADMIN", file.server.admin_enabled),

            openrouter_api_key: resolve_api_key(
                env_string("OPENROUTER_API_KEY"),
                &file.llm.api_key,
            ),
            openrouter_base: env_string("OPENROUTER_BASE_URL").unwrap_or(file.llm.base_url),
            model: env_string("GENESIS_MODEL").unwrap_or(file.llm.model),
            request_timeout: Duration::from_secs(env_parse(
                "GENESIS_REQUEST_TIMEOUT_SECS",
                file.llm.request_timeout_secs,
            )),
            max_retries: env_parse("GENESIS_MAX_RETRIES", file.llm.max_retries),
            models,

            system_prompt,
            max_iterations: env_parse("GENESIS_MAX_ITERATIONS", file.agent.max_iterations),
            modes,
            default_mode,

            turn_budget: Duration::from_secs(env_parse(
                "GENESIS_TURN_BUDGET_SECS",
                file.budgets.turn_secs,
            )),
            wasm_slice: Duration::from_secs(env_parse(
                "GENESIS_WASM_SLICE_SECS",
                file.budgets.wasm_slice_secs,
            )),
            tool_budget: Duration::from_secs(env_parse(
                "GENESIS_TOOL_BUDGET_SECS",
                file.budgets.tool_secs,
            )),
            probe_budget: Duration::from_secs(env_parse(
                "GENESIS_PROBE_BUDGET_SECS",
                file.budgets.probe_secs,
            )),

            agent_memory_bytes: env_parse("GENESIS_AGENT_MEM_MB", file.limits.agent_memory_mb)
                << 20,
            tool_memory_bytes: env_parse("GENESIS_TOOL_MEM_MB", file.limits.tool_memory_mb) << 20,
            gateway_memory_bytes: env_parse(
                "GENESIS_GATEWAY_MEM_MB",
                file.limits.gateway_memory_mb,
            ) << 20,
            session_spend_limit_usd: env_parse(
                "GENESIS_SESSION_SPEND_LIMIT_USD",
                file.limits.session_spend_limit_usd,
            ),
            max_tool_output_bytes: env_parse(
                "GENESIS_MAX_TOOL_OUTPUT",
                file.limits.max_tool_output_bytes,
            ),
            max_attachment_bytes: env_parse(
                "GENESIS_MAX_ATTACHMENT_BYTES",
                file.limits.max_attachment_bytes,
            ),
            max_attachments: env_parse("GENESIS_MAX_ATTACHMENTS", file.limits.max_attachments),

            cache: CacheSettings {
                enabled: env_parse("GENESIS_CACHE", file.cache.enabled),
                ttl: env_string("GENESIS_CACHE_TTL").unwrap_or(file.cache.ttl),
                anchor_stride: file.cache.anchor_stride.max(1),
                explicit_vendors: file.cache.explicit_vendors,
            },

            build: BuildSettings {
                command: env_string("GENESIS_BUILD_COMMAND").unwrap_or(file.build.command),
                target: env_string("GENESIS_BUILD_TARGET").unwrap_or(file.build.target),
                profile: env_string("GENESIS_BUILD_PROFILE").unwrap_or(file.build.profile),
                target_dir: resolve(&root, &file.build.target_dir),
                locked: file.build.locked,
                extra_args: file.build.extra_args,
                timeout: Duration::from_secs(
                    env_parse("GENESIS_BUILD_TIMEOUT_SECS", file.build.timeout_secs).max(1),
                ),
                allowed_crates: file.build.allowed_crates,
            },

            wasi: WasiSettings {
                network: env_parse("GENESIS_WASI_NETWORK", file.wasi.network),
                dns: env_parse("GENESIS_WASI_DNS", file.wasi.dns),
                env: env_parse("GENESIS_WASI_ENV", file.wasi.env),
                stdio: env_parse("GENESIS_WASI_STDIO", file.wasi.stdio),
                dirs: file.wasi.dirs.iter().map(|d| resolve(&root, d)).collect(),
            },

            watchdog: WatchdogSettings {
                failure_window: Duration::from_secs(file.watchdog.failure_window_secs),
                failure_threshold: file.watchdog.failure_threshold.max(1),
                probe_interval: Duration::from_secs(file.watchdog.probe_interval_secs.max(1)),
                watch_suppression: Duration::from_secs(file.watchdog.watch_suppression_secs),
                debounce: Duration::from_millis(file.watchdog.debounce_ms),
            },

            devkit: DevkitSettings {
                enabled: env_parse("GENESIS_DEVKIT", file.devkit.enabled),
                protected_files: file.devkit.protected_files,
                protected_dirs: file.devkit.protected_dirs,
            },

            filesystem: FilesystemSettings {
                enabled: env_parse("GENESIS_FILESYSTEM", file.filesystem.enabled),
                roots: if file.filesystem.roots.is_empty() {
                    vec![root.clone()]
                } else {
                    file.filesystem
                        .roots
                        .iter()
                        .map(|r| resolve(&root, r))
                        .collect()
                },
                max_read_bytes: file.filesystem.max_read_bytes,
                protected: file.filesystem.protected,
                allow_delete: file.filesystem.allow_delete,
            },

            terminal: TerminalSettings {
                enabled: env_parse("GENESIS_TERMINAL", file.terminal.enabled),
                shell: if file.terminal.shell.is_empty() {
                    default_shell().to_string()
                } else {
                    file.terminal.shell
                },
                shell_args: if file.terminal.shell_args.is_empty() {
                    default_shell_args()
                } else {
                    file.terminal.shell_args
                },
                max_sessions: file.terminal.max_sessions.max(1),
                default_timeout: Duration::from_millis(file.terminal.default_timeout_ms),
                max_output_bytes: file.terminal.max_output_bytes,
                idle_timeout: Duration::from_secs(file.terminal.idle_timeout_secs),
            },

            control: ControlSettings {
                allow_restart: env_parse("GENESIS_ALLOW_RESTART", file.control.allow_restart),
                min_uptime: Duration::from_secs(file.control.min_uptime_secs),
                restart_delay: Duration::from_millis(file.control.restart_delay_ms),
            },

            sandbox_available: env_parse("GENESIS_SANDBOX", file.sandbox.enabled),
            tools: file.tools,
            config_path,
            root,
        };

        tracing::debug!(
            config = %config.config_path.display(),
            exists = config.config_path.is_file(),
            "configuration loaded"
        );
        Ok(config)
    }
}

/// The environment wins, so a key can be overridden for one run without editing
/// the file. Blank in either place counts as absent: an empty string would
/// otherwise become an `Authorization: Bearer ` header and fail confusingly at
/// request time rather than at startup.
fn resolve_api_key(from_env: Option<String>, from_file: &str) -> Option<Secret> {
    from_env
        .or_else(|| Some(from_file.to_string()))
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .map(Secret::new)
}

/// `GENESIS_MODELS` is a comma-separated list of `id=Label` pairs; a bare id
/// uses itself as the label.
fn parse_models_env(raw: &str) -> Vec<ModelSpec> {
    let parsed: Vec<ModelSpec> = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((id, label)) => ModelSpec {
                id: id.trim().to_string(),
                label: label.trim().to_string(),
            },
            None => ModelSpec {
                id: entry.to_string(),
                label: entry.to_string(),
            },
        })
        .collect();

    if parsed.is_empty() {
        builtin_models()
    } else {
        parsed
    }
}

/// A short starting list. Override it in `genesis.toml` — a provider's
/// catalogue changes far faster than this file does.
fn builtin_models() -> Vec<ModelSpec> {
    [
        ("anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5"),
        ("anthropic/claude-opus-4.1", "Claude Opus 4.1"),
        ("openai/gpt-4o", "GPT-4o"),
        ("google/gemini-2.5-pro", "Gemini 2.5 Pro"),
        ("mock/echo", "Mock (local test server)"),
    ]
    .into_iter()
    .map(|(id, label)| ModelSpec {
        id: id.to_string(),
        label: label.to_string(),
    })
    .collect()
}

/// PowerShell on Windows, a POSIX shell elsewhere.
fn default_shell() -> &'static str {
    if cfg!(windows) {
        "powershell"
    } else {
        "sh"
    }
}

fn default_shell_args() -> Vec<String> {
    if cfg!(windows) {
        // No profile and no banner: a predictable, quiet session.
        ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "-"]
            .map(String::from)
            .to_vec()
    } else {
        vec!["-s".to_string()]
    }
}

fn builtin_modes() -> Vec<ModeSpec> {
    vec![
        ModeSpec {
            id: "agent".into(),
            label: "Agent".into(),
            description: "Full access. Runs tools and can modify the running system.".into(),
            read_only: false,
        },
        ModeSpec {
            id: "plan".into(),
            label: "Plan".into(),
            description:
                "Reads and reasons, but makes no changes. Tools that would modify anything are withheld."
                    .into(),
            read_only: true,
        },
    ]
}

fn default_system_prompt() -> &'static str {
    "You are Genesis, an agent running inside a self-modifying WebAssembly harness.

You are unusual: your own agentic loop, your tools, and the chat interface you \
are speaking through are all WebAssembly components that you can rewrite while \
you run. Edits are compiled immediately and the compiler's verdict comes back \
in the same tool result, so iterate until it builds. Every component is \
versioned and can be rolled back, so a broken build is recoverable, never fatal.

Be direct and concise. Use tools when they help; explain what you changed when \
you modify yourself."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_toml(text: &str) -> Result<Config> {
        let file: spec::File = toml::from_str(text)?;
        Config::assemble(PathBuf::from("/proj"), PathBuf::from("/proj/genesis.toml"), file)
    }

    #[test]
    fn runs_with_no_configuration_at_all() {
        let cfg = from_toml("").unwrap();
        assert_eq!(cfg.bind_addr.port(), 7777);
        assert_eq!(cfg.build.target, "wasm32-wasip2");
        assert_eq!(cfg.modes.len(), 2);
        assert!(cfg.mode("plan").unwrap().read_only);
        assert!(!cfg.mode("agent").unwrap().read_only);
    }

    #[test]
    fn a_partial_file_only_overrides_what_it_names() {
        let cfg = from_toml(
            r#"
            [server]
            bind = "0.0.0.0:9000"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.bind_addr.port(), 9000);
        // Untouched sections keep their defaults.
        assert_eq!(cfg.max_iterations, 32);
        assert_eq!(cfg.watchdog.failure_threshold, 3);
    }

    #[test]
    fn a_mistyped_key_is_an_error_not_a_silent_no_op() {
        let err = from_toml(
            r#"
            [budgets]
            turn_seconds = 60
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("turn_seconds"), "{err:#}");
    }

    #[test]
    fn modes_are_configurable_including_which_are_read_only() {
        let cfg = from_toml(
            r#"
            [agent]
            default_mode = "review"

            [[modes]]
            id = "review"
            label = "Review"
            description = "Looks, never touches."
            read_only = true

            [[modes]]
            id = "build"
            label = "Build"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.modes.len(), 2);
        assert!(cfg.mode("review").unwrap().read_only);
        assert!(!cfg.mode("build").unwrap().read_only);
        // A mode without a label falls back to its id rather than showing blank.
        assert_eq!(cfg.mode("build").unwrap().label, "Build");
    }

    #[test]
    fn a_default_mode_that_does_not_exist_is_rejected() {
        let err = from_toml(
            r#"
            [agent]
            default_mode = "nonsense"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nonsense"), "{err:#}");
    }

    #[test]
    fn paths_are_resolved_against_the_root_but_absolute_ones_are_kept() {
        let cfg = from_toml(
            r#"
            [paths]
            data = "var/state"
            artifacts = "/srv/genesis/artifacts"
            agent = "components/brain"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.paths.data, PathBuf::from("/proj/var/state"));
        assert_eq!(cfg.paths.artifacts, PathBuf::from("/srv/genesis/artifacts"));
        assert_eq!(cfg.slot_source_dir(&Slot::Agent), PathBuf::from("/proj/components/brain"));
        // The crate name follows the directory, so renaming it needs no code change.
        assert_eq!(cfg.slot_crate_name(&Slot::Agent), "brain");
        assert_eq!(cfg.slot_wasm_filename(&Slot::Agent), "brain.wasm");
    }

    #[test]
    fn gateway_and_tool_naming_conventions_are_configurable() {
        let cfg = from_toml(
            r#"
            [paths]
            gateways = "surfaces"
            gateway_prefix = "ui-"
            tools = "plugins"
            tool_prefix = "plugin-"
            "#,
        )
        .unwrap();

        assert_eq!(
            cfg.slot_source_dir(&Slot::gateway("web")),
            PathBuf::from("/proj/surfaces/ui-web")
        );
        assert_eq!(cfg.slot_wasm_filename(&Slot::gateway("web")), "ui_web.wasm");
        assert_eq!(
            cfg.slot_source_dir(&Slot::tool("weather")),
            PathBuf::from("/proj/plugins/plugin-weather")
        );
    }

    #[test]
    fn build_settings_flow_through() {
        let cfg = from_toml(
            r#"
            [build]
            profile = "dev"
            target = "wasm32-wasip1"
            target_dir = "build-out"
            locked = false
            extra_args = ["--features", "wide"]
            "#,
        )
        .unwrap();

        assert_eq!(cfg.build.profile, "dev");
        assert_eq!(cfg.build.target, "wasm32-wasip1");
        assert_eq!(cfg.build.target_dir, PathBuf::from("/proj/build-out"));
        assert!(!cfg.build.locked);
        assert_eq!(cfg.build.extra_args, vec!["--features", "wide"]);
    }

    #[test]
    fn the_api_key_can_come_from_the_file() {
        let cfg = from_toml(
            r#"
            [llm]
            api_key = "sk-from-file"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.openrouter_api_key.as_ref().unwrap().expose(), "sk-from-file");
    }

    #[test]
    fn an_absent_or_blank_key_stays_none() {
        assert!(from_toml("").unwrap().openrouter_api_key.is_none());
        let cfg = from_toml(
            r#"
            [llm]
            api_key = "   "
            "#,
        )
        .unwrap();
        assert!(
            cfg.openrouter_api_key.is_none(),
            "whitespace is not a key, and would fail confusingly at request time"
        );
    }

    #[test]
    fn the_environment_wins_over_a_configured_key() {
        let from_both = resolve_api_key(Some("sk-env".into()), "sk-file").unwrap();
        assert_eq!(from_both.expose(), "sk-env");

        let file_only = resolve_api_key(None, "sk-file").unwrap();
        assert_eq!(file_only.expose(), "sk-file");

        // Blank on either side is the same as not set at all.
        assert!(resolve_api_key(None, "").is_none());
        assert!(resolve_api_key(None, "   ").is_none());
        assert!(resolve_api_key(Some("  ".into()), "").is_none());

        // Surrounding whitespace from a copy-paste is trimmed, not sent.
        assert_eq!(
            resolve_api_key(None, "  sk-padded  ").unwrap().expose(),
            "sk-padded"
        );
    }

    #[test]
    fn a_secret_does_not_print_itself() {
        let secret = Secret::new("sk-live-do-not-log-me");
        let shown = format!("{secret:?}");
        assert!(!shown.contains("sk-live"), "{shown}");
        assert_eq!(shown, "Secret(***)");

        // The same must hold when it is nested in the struct that gets logged.
        let cfg = from_toml(
            r#"
            [llm]
            api_key = "sk-live-do-not-log-me"
            "#,
        )
        .unwrap();
        assert!(!format!("{cfg:?}").contains("sk-live"));
    }

    #[test]
    fn models_take_their_id_as_a_label_when_none_is_given() {
        let cfg = from_toml(
            r#"
            [[models]]
            id = "local/tiny"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].label, "local/tiny");
    }

    #[test]
    fn local_overlay_sits_beside_the_config_file() {
        assert_eq!(
            local_overlay_path(Path::new("/srv/app/genesis.toml")),
            PathBuf::from("/srv/app/genesis.local.toml")
        );
        assert_eq!(
            local_overlay_path(Path::new("/srv/app/staging.toml")),
            PathBuf::from("/srv/app/staging.local.toml")
        );
    }

    #[test]
    fn overlay_sets_one_key_without_restating_its_section() {
        let mut base: toml::Value = toml::from_str(
            "[llm]
model = \"a/b\"
max_retries = 3

[tools.web-search]
timeout = 30
",
        )
        .unwrap();
        let overlay: toml::Value =
            toml::from_str("[tools.web-search]
api_key = \"secret\"
").unwrap();

        merge_toml(&mut base, overlay);

        // The overlay added its key and left the neighbours alone.
        let tools = base.get("tools").unwrap().get("web-search").unwrap();
        assert_eq!(tools.get("api_key").unwrap().as_str(), Some("secret"));
        assert_eq!(tools.get("timeout").unwrap().as_integer(), Some(30));
        assert_eq!(
            base.get("llm").unwrap().get("model").unwrap().as_str(),
            Some("a/b")
        );
    }

    #[test]
    fn overlay_replaces_scalars_and_arrays_rather_than_merging_them() {
        let mut base: toml::Value =
            toml::from_str("[build]
locked = true
allowed_crates = [\"a\", \"b\"]
").unwrap();
        let overlay: toml::Value =
            toml::from_str("[build]
locked = false
allowed_crates = [\"c\"]
").unwrap();

        merge_toml(&mut base, overlay);

        let build = base.get("build").unwrap();
        assert_eq!(build.get("locked").unwrap().as_bool(), Some(false));
        // Replaced, not appended: otherwise a list could never be shortened.
        let crates = build.get("allowed_crates").unwrap().as_array().unwrap();
        assert_eq!(crates.len(), 1);
        assert_eq!(crates[0].as_str(), Some("c"));
    }
}
