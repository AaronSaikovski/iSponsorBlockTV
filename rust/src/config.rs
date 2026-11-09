//! Lossless persisted JSON, strict runtime options, and atomic draft persistence.
//!
//! Setup owns a `ConfigDocument`; conversion to runtime options never changes it.
//! Save deliberately does not perform runtime validation: an incomplete draft is
//! useful configuration data, but cannot be started as a service.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use tempfile::NamedTempFile;
use thiserror::Error;
use url::Url;

pub const DEFAULT_SPONSORBLOCK_API_URL: &str = "https://sponsor.ajay.app/api/";
const RESERVED_KEYS: [&str; 2] = ["config_file", "data_dir"];

/// Application logging severity. Dependency logging remains disabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Explicit directives prevent ambient RUST_LOG from enabling dependencies.
    pub fn filter(self) -> &'static str {
        match self {
            Self::Off => "off,isponsorblocktv=off",
            Self::Error => "off,isponsorblocktv=error",
            Self::Warn => "off,isponsorblocktv=warn",
            Self::Info => "off,isponsorblocktv=info",
            Self::Debug => "off,isponsorblocktv=debug",
            Self::Trace => "off,isponsorblocktv=trace",
        }
    }
}

/// The editable, untyped document. Unknown fields and JSON number precision are
/// retained. Only reserved top-level path keys are excluded from persistence.
#[derive(Clone, Default, PartialEq)]
pub struct ConfigDocument(pub Map<String, Value>);

impl fmt::Debug for ConfigDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigDocument([redacted])")
    }
}

impl Serialize for ConfigDocument {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(
            self.0
                .keys()
                .filter(|key| !RESERVED_KEYS.contains(&key.as_str()))
                .count(),
        ))?;
        for (key, value) in &self.0 {
            if !RESERVED_KEYS.contains(&key.as_str()) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ConfigDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut map = Map::<String, Value>::deserialize(deserializer)?;
        for key in RESERVED_KEYS {
            map.remove(key);
        }
        Ok(Self(map))
    }
}

/// A diagnostic contains only a field path and a fixed explanation, never the
/// rejected value, parser input, API key, or screen identifier.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration field {path}: {reason}")]
    Field { path: String, reason: &'static str },
    #[error("configuration file {}: {operation} ({kind:?})", path.display())]
    File {
        path: PathBuf,
        operation: &'static str,
        kind: io::ErrorKind,
    },
    #[error("configuration environment: {reason}")]
    Environment { reason: &'static str },
}

impl ConfigError {
    fn field(path: impl Into<String>, reason: &'static str) -> Self {
        Self::Field {
            path: path.into(),
            reason,
        }
    }

    fn file(path: &Path, operation: &'static str, error: &io::Error) -> Self {
        Self::File {
            path: path.to_owned(),
            operation,
            kind: error.kind(),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct RuntimeDevice {
    pub screen_id: String,
    pub name: Option<String>,
    /// Signed seconds, converted from the document's fractional milliseconds.
    pub offset_seconds: f64,
}

impl RuntimeDevice {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("Unnamed device")
    }
}

impl fmt::Debug for RuntimeDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RuntimeDevice([redacted])")
    }
}

#[derive(Clone, PartialEq)]
pub struct RuntimeChannel {
    pub id: String,
    pub name: Option<String>,
}

impl fmt::Debug for RuntimeChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RuntimeChannel([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    /// Index, not screen identity, is safe to include in operator diagnostics.
    LegacyScreenId { device_index: usize },
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyScreenId { device_index } => write!(
                f,
                "devices[{device_index}].screen_id uses the legacy 26-character format; pair the device again"
            ),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct RuntimeConfig {
    pub log_level: LogLevel,
    pub devices: Vec<RuntimeDevice>,
    pub apikey: String,
    pub skip_categories: Vec<String>,
    pub channel_whitelist: Vec<RuntimeChannel>,
    pub skip_count_tracking: bool,
    pub mute_ads: bool,
    pub skip_ads: bool,
    /// Seconds. Signed legacy values are supported.
    pub minimum_skip_length: f64,
    pub auto_play: bool,
    pub join_name: String,
    pub use_proxy: bool,
    /// Validated HTTP(S) base URL, normalized only in runtime state.
    pub sponsorblock_api_url: Url,
    pub warnings: Vec<ConfigWarning>,
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RuntimeConfig([redacted])")
    }
}

impl ConfigDocument {
    /// Validate logging independently of paired-device requirements, including
    /// for setup commands. Explicit null is not a missing preference.
    pub fn log_level(&self) -> Result<LogLevel, ConfigError> {
        match self.0.get("log_level") {
            None => Ok(LogLevel::default()),
            Some(Value::String(value)) => match value.as_str() {
                "off" => Ok(LogLevel::Off),
                "error" => Ok(LogLevel::Error),
                "warn" => Ok(LogLevel::Warn),
                "info" => Ok(LogLevel::Info),
                "debug" => Ok(LogLevel::Debug),
                "trace" => Ok(LogLevel::Trace),
                _ => Err(ConfigError::field(
                    "log_level",
                    "expected one of off, error, warn, info, debug, trace",
                )),
            },
            Some(_) => Err(ConfigError::field("log_level", "expected a string")),
        }
    }

    /// Validate for service startup, including migration and paired-device gates.
    /// Callers should display `RuntimeConfig::warnings` before starting devices.
    pub fn runtime_config(&self) -> Result<RuntimeConfig, ConfigError> {
        if self.0.contains_key("atvs") {
            return Err(ConfigError::field(
                "atvs",
                "legacy configuration requires explicit removal in setup",
            ));
        }
        let mut devices = Vec::new();
        let mut warnings = Vec::new();
        for (index, value) in array(&self.0, "devices")?.iter().enumerate() {
            let path = format!("devices[{index}]");
            let device = value
                .as_object()
                .ok_or_else(|| ConfigError::field(&path, "expected an object"))?;
            let screen_id = required_identifier(device, "screen_id", &format!("{path}.screen_id"))?;
            if screen_id.chars().count() == 26 {
                warnings.push(ConfigWarning::LegacyScreenId {
                    device_index: index,
                });
            }
            let offset_path = format!("{path}.offset");
            let milliseconds = number(device, "offset", &offset_path, 0.0)?;
            let offset_seconds = milliseconds / 1000.0;
            if milliseconds != 0.0 && offset_seconds == 0.0 {
                return Err(ConfigError::field(
                    offset_path,
                    "milliseconds cannot be represented in runtime seconds",
                ));
            }
            devices.push(RuntimeDevice {
                screen_id,
                name: optional_name(device, &format!("{path}.name"))?,
                offset_seconds,
            });
        }
        let apikey = string(&self.0, "apikey", "")?;
        let mut channel_whitelist = Vec::new();
        for (index, value) in array(&self.0, "channel_whitelist")?.iter().enumerate() {
            let path = format!("channel_whitelist[{index}]");
            let channel = value
                .as_object()
                .ok_or_else(|| ConfigError::field(&path, "expected an object"))?;
            channel_whitelist.push(RuntimeChannel {
                id: required_identifier(channel, "id", &format!("{path}.id"))?,
                name: optional_name(channel, &format!("{path}.name"))?,
            });
        }
        if !channel_whitelist.is_empty() && apikey.is_empty() {
            return Err(ConfigError::field(
                "apikey",
                "an API key is required for a nonempty channel whitelist",
            ));
        }
        let skip_categories = match self.0.get("skip_categories") {
            None | Some(Value::Null) => vec!["sponsor".to_owned()],
            Some(Value::Array(values)) => values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        ConfigError::field(format!("skip_categories[{index}]"), "expected a string")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(ConfigError::field(
                    "skip_categories",
                    "expected an array or null",
                ));
            }
        };
        let base = string(
            &self.0,
            "sponsorblock_api_url",
            DEFAULT_SPONSORBLOCK_API_URL,
        )?;
        let mut sponsorblock_api_url = Url::parse(&base).map_err(|_| {
            ConfigError::field(
                "sponsorblock_api_url",
                "expected an absolute HTTP(S) base URL",
            )
        })?;
        // An empty userinfo delimiter is also disallowed, even though Url's
        // username accessor represents it as an empty string.
        let has_userinfo = base.split_once(':').is_some_and(|(_, rest)| {
            rest.trim_start_matches(['/', '\\'])
                .split(['/', '\\', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        });
        if !matches!(sponsorblock_api_url.scheme(), "http" | "https")
            || sponsorblock_api_url.host_str().is_none()
            || !sponsorblock_api_url.username().is_empty()
            || sponsorblock_api_url.password().is_some()
            || has_userinfo
            || sponsorblock_api_url.query().is_some()
            || sponsorblock_api_url.fragment().is_some()
        {
            return Err(ConfigError::field(
                "sponsorblock_api_url",
                "base URL must be HTTP(S) without credentials, query, or fragment",
            ));
        }
        if !sponsorblock_api_url.path().ends_with('/') {
            let path = format!("{}/", sponsorblock_api_url.path());
            sponsorblock_api_url.set_path(&path);
        }
        let runtime = RuntimeConfig {
            log_level: self.log_level()?,
            devices,
            apikey,
            skip_categories,
            channel_whitelist,
            skip_count_tracking: boolean(&self.0, "skip_count_tracking", true)?,
            mute_ads: boolean(&self.0, "mute_ads", false)?,
            skip_ads: boolean(&self.0, "skip_ads", false)?,
            minimum_skip_length: number(
                &self.0,
                "minimum_skip_length",
                "minimum_skip_length",
                1.0,
            )?,
            auto_play: boolean(&self.0, "auto_play", true)?,
            join_name: string(&self.0, "join_name", "iSponsorBlockTV")?,
            use_proxy: boolean(&self.0, "use_proxy", false)?,
            sponsorblock_api_url,
            warnings,
        };
        if runtime.devices.is_empty() {
            return Err(ConfigError::field(
                "devices",
                "at least one device is required to start; use setup to pair a device",
            ));
        }
        Ok(runtime)
    }
}

fn array<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], ConfigError> {
    match map.get(key) {
        None => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        Some(_) => Err(ConfigError::field(key, "expected an array")),
    }
}

fn string(map: &Map<String, Value>, key: &str, default: &str) -> Result<String, ConfigError> {
    match map.get(key) {
        None => Ok(default.to_owned()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(ConfigError::field(key, "expected a string")),
    }
}

fn required_identifier(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<String, ConfigError> {
    match map.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(ConfigError::field(
            path,
            "expected a nonempty string identifier",
        )),
    }
}

fn optional_name(map: &Map<String, Value>, path: &str) -> Result<Option<String>, ConfigError> {
    match map.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ConfigError::field(path, "expected a string or null")),
    }
}

fn boolean(map: &Map<String, Value>, key: &str, default: bool) -> Result<bool, ConfigError> {
    match map.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ConfigError::field(key, "expected a boolean")),
    }
}

fn number(
    map: &Map<String, Value>,
    key: &str,
    path: &str,
    default: f64,
) -> Result<f64, ConfigError> {
    let Some(value) = map.get(key) else {
        return Ok(default);
    };
    let Value::Number(number) = value else {
        return Err(ConfigError::field(path, "expected a number"));
    };
    let parsed = number
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| ConfigError::field(path, "number is outside the finite runtime range"))?;
    // as_f64 can silently underflow an arbitrary-precision number to zero.
    if parsed == 0.0
        && number
            .as_str()
            .split(['e', 'E'])
            .next()
            .is_some_and(|mantissa| mantissa.bytes().any(|byte| matches!(byte, b'1'..=b'9')))
    {
        return Err(ConfigError::field(
            path,
            "number is outside the finite runtime range",
        ));
    }
    Ok(parsed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Unchanged,
}

/// One store serializes saves and changes its semantic baseline only after a
/// successful atomic replacement. No live document is retained or mutated.
pub struct ConfigStore {
    data_dir: PathBuf,
    config_file: PathBuf,
    baseline: Mutex<Option<Vec<u8>>>,
}

impl fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigStore")
            .field("config_file", &self.config_file)
            .finish_non_exhaustive()
    }
}

impl ConfigStore {
    pub fn load(data_dir: PathBuf, docker: bool) -> Result<(Self, ConfigDocument), ConfigError> {
        match fs::metadata(&data_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(ConfigError::File {
                    path: data_dir,
                    operation: "open data directory",
                    kind: io::ErrorKind::NotADirectory,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !docker => {
                fs::create_dir_all(&data_dir).map_err(|error| {
                    ConfigError::file(&data_dir, "create data directory", &error)
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ConfigError::file(
                    &data_dir,
                    "Docker requires an existing data directory mount",
                    &error,
                ));
            }
            Err(error) => return Err(ConfigError::file(&data_dir, "open data directory", &error)),
        }
        let config_file = data_dir.join("config.json");
        let (baseline, document) = match File::open(&config_file) {
            Ok(file) => {
                let mut map: Map<String, Value> = serde_json::from_reader(BufReader::new(file))
                    .map_err(|_| ConfigError::File {
                        path: config_file.clone(),
                        operation: "read a valid JSON object",
                        kind: io::ErrorKind::InvalidData,
                    })?;
                // Include reserved keys in the original baseline so the next
                // explicit save actually removes them from an existing file.
                let baseline = serialize(&map, &config_file)?;
                for key in RESERVED_KEYS {
                    map.remove(key);
                }
                (Some(baseline), ConfigDocument(map))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (None, ConfigDocument::default())
            }
            Err(error) => return Err(ConfigError::file(&config_file, "open", &error)),
        };
        Ok((
            Self {
                data_dir,
                config_file,
                baseline: Mutex::new(baseline),
            },
            document,
        ))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn save(&self, document: &ConfigDocument) -> Result<SaveOutcome, ConfigError> {
        // Serialization precedes creation of any replacement file.
        let bytes = serialize(document, &self.config_file)?;
        let mut baseline = self.baseline.lock().map_err(|_| ConfigError::File {
            path: self.config_file.clone(),
            operation: "acquire save lock",
            kind: io::ErrorKind::Other,
        })?;
        if baseline.as_ref() == Some(&bytes) {
            return Ok(SaveOutcome::Unchanged);
        }
        let permissions = match fs::metadata(&self.config_file) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ConfigError::file(
                    &self.config_file,
                    "read permissions",
                    &error,
                ));
            }
        };
        let mut temporary = NamedTempFile::new_in(&self.data_dir).map_err(|error| {
            ConfigError::file(
                &self.config_file,
                "create same-directory temporary file",
                &error,
            )
        })?;
        temporary.write_all(&bytes).map_err(|error| {
            ConfigError::file(&self.config_file, "write temporary file", &error)
        })?;
        temporary.flush().map_err(|error| {
            ConfigError::file(&self.config_file, "flush temporary file", &error)
        })?;
        if let Some(permissions) = permissions {
            temporary
                .as_file()
                .set_permissions(permissions)
                .map_err(|error| {
                    ConfigError::file(&self.config_file, "preserve file permissions", &error)
                })?;
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| ConfigError::file(&self.config_file, "sync temporary file", &error))?;
        temporary.persist(&self.config_file).map_err(|error| {
            ConfigError::file(&self.config_file, "atomically replace", &error.error)
        })?;
        *baseline = Some(bytes);
        Ok(SaveOutcome::Saved)
    }
}

fn serialize(value: &impl Serialize, path: &Path) -> Result<Vec<u8>, ConfigError> {
    let mut bytes = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
    value
        .serialize(&mut serializer)
        .map_err(|_| ConfigError::File {
            path: path.to_owned(),
            operation: "serialize JSON",
            kind: io::ErrorKind::InvalidData,
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
}

/// Captured path inputs make cross-platform resolution testable without mutating
/// process-global environment variables. Empty captured paths act as absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub platform: Platform,
    pub data_dir: Option<PathBuf>,
    pub docker: bool,
    pub home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    /// Windows LocalAppData from SHGetKnownFolderPath, never roaming AppData.
    pub local_app_data: Option<PathBuf>,
}

impl Environment {
    pub fn capture() -> Result<Self, ConfigError> {
        let platform = if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            return Err(ConfigError::Environment {
                reason: "unsupported operating system",
            });
        };
        #[cfg(windows)]
        let local_app_data = windows_local_app_data();
        #[cfg(not(windows))]
        let local_app_data = None;
        Ok(Self {
            platform,
            data_dir: captured_path("iSPBTV_data_dir"),
            docker: std::env::var_os("iSPBTV_docker").is_some_and(|value| !value.is_empty()),
            home: captured_path("HOME"),
            xdg_data_home: captured_path("XDG_DATA_HOME"),
            local_app_data,
        })
    }
}

fn captured_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn resolve_data_dir(cli: Option<&Path>, env: &Environment) -> Result<PathBuf, ConfigError> {
    if let Some(path) = cli {
        return Ok(path.to_owned());
    }
    if let Some(path) = nonempty_path(env.data_dir.as_deref()) {
        return Ok(path.to_owned());
    }
    match env.platform {
        Platform::Linux => {
            if let Some(path) = nonempty_path(env.xdg_data_home.as_deref()) {
                return Ok(path.join("iSponsorBlockTV"));
            }
            Ok(required_home(env)?.join(".local/share/iSponsorBlockTV"))
        }
        Platform::MacOs => {
            Ok(required_home(env)?.join("Library/Application Support/iSponsorBlockTV"))
        }
        Platform::Windows => nonempty_path(env.local_app_data.as_deref())
            .map(|path| path.join("dmunozv04/iSponsorBlockTV"))
            .ok_or(ConfigError::Environment {
                reason: "Windows local AppData known folder is unavailable; supply --data",
            }),
    }
}

fn nonempty_path(path: Option<&Path>) -> Option<&Path> {
    path.filter(|path| !path.as_os_str().is_empty())
}

fn required_home(env: &Environment) -> Result<&Path, ConfigError> {
    nonempty_path(env.home.as_deref()).ok_or(ConfigError::Environment {
        reason: "home directory is unavailable; supply --data",
    })
}

#[cfg(windows)]
fn windows_local_app_data() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

    let mut allocation = std::ptr::null_mut();
    // SAFETY: the known-folder GUID and output pointer are valid, the token is
    // null for the current user, and the returned allocation is freed exactly
    // once with the allocator required by SHGetKnownFolderPath.
    unsafe {
        let result = SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            0,
            std::ptr::null_mut(),
            &mut allocation,
        );
        let path = if result >= 0 && !allocation.is_null() {
            let mut length = 0;
            while *allocation.add(length) != 0 {
                length += 1;
            }
            let units = std::slice::from_raw_parts(allocation, length);
            Some(PathBuf::from(std::ffi::OsString::from_wide(units)))
        } else {
            None
        };
        if !allocation.is_null() {
            CoTaskMemFree(allocation.cast());
        }
        path.filter(|path| !path.as_os_str().is_empty())
    }
}
