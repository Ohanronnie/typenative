use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    #[serde(rename = "aarch64-apple-darwin")]
    Aarch64AppleDarwin,
}

impl Target {
    /// Returns the required macOS ARM64 target matching the current host.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedHost`] outside the supported Darwin ARM64 host.
    pub const fn host() -> Result<Self, UnsupportedHost> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Ok(Self::Aarch64AppleDarwin);
        }
        Err(UnsupportedHost)
    }

    pub const fn triple(self) -> &'static str {
        match self {
            Self::Aarch64AppleDarwin => "aarch64-apple-darwin",
        }
    }

    pub const fn runtime_module(self) -> &'static str {
        match self {
            Self::Aarch64AppleDarwin => "darwin-arm64.tn",
        }
    }

    pub const fn is_macos(self) -> bool {
        matches!(self, Self::Aarch64AppleDarwin)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedHost;

impl std::fmt::Display for UnsupportedHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("host is not a supported TypeNative target")
    }
}

impl std::error::Error for UnsupportedHost {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    #[default]
    Debug,
    Optimized,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sanitizer {
    Address,
    Undefined,
    Thread,
}

impl Sanitizer {
    pub const fn codegen(self) -> tn_codegen_llvm::Sanitizer {
        match self {
            Self::Address => tn_codegen_llvm::Sanitizer::Address,
            Self::Undefined => tn_codegen_llvm::Sanitizer::Undefined,
            Self::Thread => tn_codegen_llvm::Sanitizer::Thread,
        }
    }

    pub const fn link_argument(self) -> &'static str {
        match self {
            Self::Address => "-fsanitize=address",
            Self::Undefined => "-fsanitize=undefined",
            Self::Thread => "-fsanitize=thread",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Emit {
    #[default]
    Executable,
    Object,
    LlvmIr,
    Bitcode,
    Assembly,
    SharedLibrary,
    NodeAddon,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct LinkConfig {
    pub libraries: Vec<String>,
    pub search_paths: Vec<PathBuf>,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProjectConfig {
    pub entry: PathBuf,
    #[serde(default = "default_out_dir")]
    pub out_dir: PathBuf,
    #[serde(default = "default_target")]
    pub target: Target,
    #[serde(default)]
    pub profile: Profile,
    #[serde(default)]
    pub emit: Emit,
    #[serde(default)]
    pub sanitizers: Vec<Sanitizer>,
    #[serde(default)]
    pub link: LinkConfig,
    #[serde(skip)]
    pub(crate) support_mode: SupportMode,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SupportMode {
    #[default]
    None,
    Runtime,
    Startup,
}

fn default_out_dir() -> PathBuf {
    PathBuf::from("build")
}

fn default_target() -> Target {
    Target::host().unwrap_or(Target::Aarch64AppleDarwin)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    pub root: PathBuf,
    pub entry: PathBuf,
    pub config: ProjectConfig,
    pub config_path: Option<PathBuf>,
}

/// Resolves a direct source or strict `typenative.json` project.
///
/// # Errors
///
/// Returns a structured driver error for missing inputs, invalid JSON, unknown configuration keys,
/// unsupported values, or an entry that is not a `.tn` file.
pub fn load_project(input: Option<&Path>) -> Result<Project, ProjectError> {
    let current = std::env::current_dir()?;
    let input = input.map_or_else(
        || find_config(&current),
        |path| resolve_input(path, &current),
    )?;
    if input.extension().is_some_and(|extension| extension == "tn") {
        let entry = absolute(input, &current);
        ensure_source(&entry)?;
        let root = entry.parent().unwrap_or(Path::new(".")).to_path_buf();
        return Ok(Project {
            root,
            entry: entry.clone(),
            config: ProjectConfig {
                entry,
                out_dir: default_out_dir(),
                target: default_target(),
                profile: Profile::default(),
                emit: Emit::default(),
                sanitizers: Vec::new(),
                link: LinkConfig::default(),
                support_mode: SupportMode::None,
            },
            config_path: None,
        });
    }

    let config_path = if input.is_dir() {
        input.join("typenative.json")
    } else {
        input
    };
    if config_path
        .file_name()
        .is_none_or(|name| name != "typenative.json")
    {
        return Err(ProjectError::InvalidInput(config_path));
    }
    let bytes = std::fs::read(&config_path)?;
    let config: ProjectConfig =
        serde_json::from_slice(&bytes).map_err(|source| ProjectError::InvalidConfiguration {
            path: config_path.clone(),
            source,
        })?;
    let root = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let entry = root.join(&config.entry);
    ensure_source(&entry)?;
    Ok(Project {
        root,
        entry,
        config,
        config_path: Some(config_path),
    })
}

fn resolve_input(path: &Path, current: &Path) -> Result<PathBuf, ProjectError> {
    let path = absolute(path.to_path_buf(), current);
    if path.exists() {
        Ok(path)
    } else {
        Err(ProjectError::NotFound(path))
    }
}

fn find_config(start: &Path) -> Result<PathBuf, ProjectError> {
    for directory in start.ancestors() {
        let candidate = directory.join("typenative.json");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(ProjectError::ConfigurationNotFound(start.to_path_buf()))
}

fn absolute(path: PathBuf, current: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        current.join(path)
    }
}

fn ensure_source(path: &Path) -> Result<(), ProjectError> {
    if !path.is_file() {
        return Err(ProjectError::NotFound(path.to_path_buf()));
    }
    if path.extension().is_none_or(|extension| extension != "tn") {
        return Err(ProjectError::InvalidSourceSuffix(path.to_path_buf()));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("project configuration was not found from {0}")]
    ConfigurationNotFound(PathBuf),
    #[error("project input was not found: {0}")]
    NotFound(PathBuf),
    #[error("project input must be a .tn file, a directory, or typenative.json: {0}")]
    InvalidInput(PathBuf),
    #[error("TypeNative source must use the .tn suffix: {0}")]
    InvalidSourceSuffix(PathBuf),
    #[error("invalid project configuration {path}: {source}")]
    InvalidConfiguration {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_config_rejects_unknown_keys_and_defaults_known_fields() {
        let config: ProjectConfig =
            serde_json::from_str(r#"{"entry":"src/main.tn"}"#).expect("minimal config");
        assert_eq!(config.out_dir, Path::new("build"));
        assert_eq!(config.profile, Profile::Debug);
        assert!(
            serde_json::from_str::<ProjectConfig>(r#"{"entry":"src/main.tn","dependencies":{}}"#)
                .is_err()
        );
    }

    #[test]
    fn target_triples_are_exact() {
        assert_eq!(Target::Aarch64AppleDarwin.triple(), "aarch64-apple-darwin");
        assert_eq!(
            Target::Aarch64AppleDarwin.runtime_module(),
            "darwin-arm64.tn"
        );
    }

    #[test]
    fn non_macos_target_is_rejected_by_strict_configuration() {
        let config = serde_json::from_str::<ProjectConfig>(
            r#"{"entry":"src/main.tn","target":"x86_64-unknown-linux-gnu"}"#,
        );
        assert!(config.is_err());
    }
}
