use anyhow::{Context, Result, bail, ensure};
use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub const APP_KIND: Kind = Kind::Custom(30078);
pub const MAX_METADATA: usize = 8 * 1024 * 1024;
pub const MAX_FILES: usize = 100_000;
pub const MAX_PACKAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

pub fn target() -> &'static str {
    env!("HAPS_TARGET")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PackageSpec {
    pub name: String,
    pub version: Version,
    pub target: String,
    pub description: String,
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,
    /// Relative macOS application bundle path, launched with Launch Services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Linux launcher metadata; commands are declared separately, never shell snippets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<DesktopEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DesktopEntry {
    pub name: String,
    pub command: String,
    pub icon: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceInfo {
    pub git: String,
    pub rev: String,
}

impl SourceInfo {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.git.starts_with("https://")
                || self.git.starts_with("htree://")
                || self.git.starts_with("file://"),
            "source must use an https://, htree://, or file:// Git URL"
        );
        let url = reqwest::Url::parse(&self.git)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "source URL cannot contain credentials, query, or fragment"
        );
        ensure!(
            [40, 64].contains(&self.rev.len()) && self.rev.bytes().all(|b| b.is_ascii_hexdigit()),
            "source revision must be a full Git commit hash"
        );
        Ok(())
    }
}

impl PackageSpec {
    pub fn validate(&self) -> Result<()> {
        safe_name(&self.name)?;
        safe_name(&self.target)?;
        ensure!(self.version.to_string().len() <= 100, "version is too long");
        ensure!(self.description.len() <= 4096, "description is too long");
        ensure!(self.commands.len() <= 100, "too many commands");
        if let Some(source) = &self.source {
            source.validate()?;
        }
        if let Some(app) = &self.app {
            safe_path(app)?;
            ensure!(
                app.ends_with(".app") && self.target.ends_with("apple-darwin"),
                "app must identify a macOS .app bundle"
            );
        }
        if let Some(desktop) = &self.desktop {
            ensure!(
                self.target.contains("-linux-"),
                "desktop entries require a Linux target"
            );
            ensure!(
                !desktop.name.trim().is_empty()
                    && desktop.name.len() <= 200
                    && !desktop.name.chars().any(char::is_control),
                "invalid desktop name"
            );
            ensure!(
                self.commands.contains_key(&desktop.command),
                "desktop command is not declared"
            );
            safe_path(&desktop.icon)?;
            ensure!(
                desktop.icon.ends_with(".png") || desktop.icon.ends_with(".svg"),
                "desktop icon must be PNG or SVG"
            );
        }
        for (name, path) in &self.commands {
            safe_name(name)?;
            safe_path(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseData {
    pub schema: String,
    pub package: PackageSpec,
    pub manifest: String,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub event: Event,
    pub data: ReleaseData,
}

impl Release {
    pub fn verify(event: Event) -> Result<Self> {
        verify_event(&event)?;
        let data: ReleaseData = serde_json::from_str(&event.content)?;
        ensure!(
            data.schema == "haps.release.v1",
            "unsupported release schema"
        );
        data.package.validate()?;
        ensure!(
            tag_value(&event, "d")? == release_tag(&data.package),
            "release tag does not match package"
        );
        hashtree_core::Cid::parse(&data.manifest)?;
        Ok(Self { event, data })
    }
    pub fn author(&self) -> String {
        self.event.pubkey.to_hex()
    }
    pub fn identity(&self) -> String {
        format!("{}/{}", self.author(), self.data.package.name)
    }
    pub fn coordinate(&self) -> String {
        format!(
            "{}@{}#{}",
            self.identity(),
            self.data.package.version,
            self.data.package.target
        )
    }
}

pub fn release_tag(package: &PackageSpec) -> String {
    format!(
        "haps/release/{}/{}/{}",
        package.name, package.version, package.target
    )
}

pub fn sign(keys: &Keys, identifier: &str, content: &impl Serialize) -> Result<Event> {
    Ok(EventBuilder::new(APP_KIND, serde_json::to_string(content)?)
        .tags([Tag::identifier(identifier)])
        .sign_with_keys(keys)?)
}

pub fn verify_event(event: &Event) -> Result<()> {
    ensure!(
        event.content.len() <= MAX_METADATA,
        "event exceeds metadata limit"
    );
    event
        .verify()
        .context("invalid Nostr event signature or ID")?;
    ensure!(event.kind == APP_KIND, "unexpected Nostr event kind");
    Ok(())
}

pub fn tag_value<'a>(event: &'a Event, name: &str) -> Result<&'a str> {
    let tags: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().is_some_and(|v| v == name))
        .collect();
    ensure!(
        tags.len() == 1 && tags[0].as_slice().len() == 2,
        "expected exactly one {name} tag"
    );
    Ok(&tags[0].as_slice()[1])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub files: Vec<PackageFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFile {
    pub path: String,
    pub cid: String,
    pub size: u64,
    pub executable: bool,
}

impl Manifest {
    pub fn validate(&self, spec: &PackageSpec) -> Result<()> {
        ensure!(self.schema == "haps.files.v1", "unsupported file manifest");
        ensure!(
            !self.files.is_empty() && self.files.len() <= MAX_FILES,
            "invalid file count"
        );
        let mut paths = BTreeSet::new();
        let mut total = 0u64;
        for file in &self.files {
            safe_path(&file.path)?;
            hashtree_core::Cid::parse(&file.cid)?;
            ensure!(
                paths.insert(file.path.to_lowercase()),
                "duplicate or case-colliding path: {}",
                file.path
            );
            total = total
                .checked_add(file.size)
                .context("package size overflow")?;
        }
        ensure!(total <= MAX_PACKAGE_BYTES, "package exceeds 8 GiB limit");
        for file in &self.files {
            let mut parent = Path::new(&file.path).parent();
            while let Some(p) = parent {
                ensure!(
                    !paths.contains(&p.to_string_lossy().to_lowercase()),
                    "file used as directory"
                );
                parent = p.parent();
            }
        }
        for path in spec.commands.values() {
            let file = self
                .files
                .iter()
                .find(|f| &f.path == path)
                .context("command is missing from package")?;
            ensure!(file.executable, "command is not executable: {path}");
        }
        if let Some(desktop) = &spec.desktop {
            ensure!(
                self.files.iter().any(|f| f.path == desktop.icon),
                "desktop icon is missing from package"
            );
        }
        if let Some(app) = &spec.app {
            ensure!(
                self.files
                    .iter()
                    .any(|f| f.path == format!("{app}/Contents/Info.plist")),
                "application bundle Info.plist is missing"
            );
        }
        Ok(())
    }
}

pub fn safe_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty() && name.len() <= 100, "invalid name length");
    ensure!(
        name.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'),
        "names must use lowercase letters, digits, hyphens or underscores"
    );
    safe_path(name)?;
    Ok(())
}

/// Validate on every OS, including Windows drive paths and reserved devices.
pub fn safe_path(path: &str) -> Result<PathBuf> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "invalid path length"
    );
    for part in path.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "unsafe path: {path}"
        );
        ensure!(!part.ends_with(['.', ' ']), "non-portable path: {path}");
        ensure!(
            !part
                .chars()
                .any(|c| c.is_control() || "\\:<>\"|?*".contains(c)),
            "unsafe path: {path}"
        );
        let base = part.split('.').next().unwrap().to_ascii_uppercase();
        if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&base.as_str())
            || (base.len() == 4
                && (base.starts_with("COM") || base.starts_with("LPT"))
                && base.as_bytes()[3].is_ascii_digit())
        {
            bail!("reserved path: {path}");
        }
    }
    Ok(PathBuf::from(path))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_METADATA as u64 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_METADATA, "metadata exceeds limit");
    Ok(serde_json::from_slice(&data)?)
}

pub fn lock(path: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(path.parent().context("lock has no parent")?)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}
