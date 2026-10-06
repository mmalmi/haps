use crate::{
    model::*,
    store::{VerifiedStore, download},
    transport::CatalogTransport,
};
use anyhow::{Context, Result, bail, ensure};
use hashtree_core::Cid;
use hashtree_index::{SearchIndex, SearchIndexOptions, SearchOptions};
use nostr::{Event, Keys};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRoot {
    pub schema: String,
    pub sequence: u64,
    pub root: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub releases: Vec<String>,
    pub search: Option<String>,
    #[serde(default)]
    pub packages: BTreeMap<String, String>,
}

pub struct Snapshot {
    pub event: Event,
    pub head: CatalogRoot,
    pub catalog: Catalog,
}

pub struct Repository {
    location: Location,
    pub store: Arc<VerifiedStore>,
}

enum Location {
    Local(PathBuf),
    Http(reqwest::Url),
    Hashtree(Arc<CatalogTransport>),
}

impl Repository {
    pub fn local(path: PathBuf) -> Result<Self> {
        let store = VerifiedStore::new(&path.join("blobs"), None)?;
        Ok(Self {
            location: Location::Local(path),
            store,
        })
    }
    pub fn open(location: &str, cache: &Path) -> Result<Self> {
        if location.starts_with("htree://") {
            return Self::hashtree(location, cache, hashtree_client::ClientConfig::from_env()?);
        }
        if location.starts_with("http://") || location.starts_with("https://") {
            let mut url = reqwest::Url::parse(location)?;
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "repository URL cannot contain credentials, query, or fragment"
            );
            if !url.path().ends_with('/') {
                url.set_path(&format!("{}/", url.path()));
            }
            let store = VerifiedStore::new(cache, Some(url.clone()))?;
            Ok(Self {
                location: Location::Http(url),
                store,
            })
        } else {
            let path = Path::new(location)
                .canonicalize()
                .context("repository does not exist")?;
            Self::local(path)
        }
    }
    pub fn hashtree(
        location: &str,
        cache: &Path,
        config: hashtree_client::ClientConfig,
    ) -> Result<Self> {
        let transport = Arc::new(CatalogTransport::with_config(location, cache, config)?);
        Ok(Self {
            store: VerifiedStore::hashtree(cache, transport.clone())?,
            location: Location::Hashtree(transport),
        })
    }
    pub fn path(&self) -> Option<&Path> {
        match &self.location {
            Location::Local(p) => Some(p),
            _ => None,
        }
    }
    pub async fn catalog(&self, author: &str) -> Result<Snapshot> {
        let event: Event = match &self.location {
            Location::Local(path) => read_json(&path.join("catalog.json"))?,
            Location::Hashtree(transport) => {
                serde_json::from_slice(&transport.read("catalog.json", MAX_METADATA).await?)?
            }
            Location::Http(url) => {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?;
                serde_json::from_slice(
                    &download(&client, url.join("catalog.json")?, MAX_METADATA).await?,
                )?
            }
        };
        verify_event(&event)?;
        ensure!(
            event.pubkey.to_hex() == author,
            "catalog publisher does not match pinned public key"
        );
        ensure!(
            tag_value(&event, "d")? == "haps/catalog/v1",
            "not a Haps catalog"
        );
        let head: CatalogRoot = serde_json::from_str(&event.content)?;
        ensure!(
            head.schema == "haps.catalog.v1",
            "unsupported catalog schema"
        );
        let catalog: Catalog = self.json(&head.root).await?;
        ensure!(catalog.releases.len() <= 10_000, "catalog is too large");
        Ok(Snapshot {
            event,
            head,
            catalog,
        })
    }
    pub async fn json<T: serde::de::DeserializeOwned>(&self, cid: &str) -> Result<T> {
        let bytes = self
            .store
            .tree()
            .get(&Cid::parse(cid)?, Some(MAX_METADATA as u64))
            .await?
            .context("metadata block unavailable")?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub async fn put_json(&self, value: &impl Serialize) -> Result<String> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(bytes.len() <= MAX_METADATA, "metadata is too large");
        Ok(self.store.tree().put(&bytes).await?.0.to_string())
    }
    pub async fn releases(&self, snapshot: &Snapshot) -> Result<Vec<Release>> {
        let mut results = Vec::new();
        let mut coordinates = BTreeSet::new();
        for cid in &snapshot.catalog.releases {
            let release = Release::verify(self.json(cid).await?)?;
            ensure!(
                coordinates.insert(release.coordinate()),
                "duplicate release coordinate in catalog"
            );
            results.push(release);
        }
        Ok(results)
    }
    pub async fn search(&self, snapshot: &Snapshot, query: &str) -> Result<Vec<Release>> {
        let index = SearchIndex::new(self.store.clone(), SearchIndexOptions::default());
        let root = snapshot
            .catalog
            .search
            .as_deref()
            .map(Cid::parse)
            .transpose()?;
        let results = index
            .search(
                root.as_ref(),
                "",
                query,
                SearchOptions {
                    limit: Some(100),
                    full_match: false,
                },
            )
            .await?;
        let allowed: BTreeSet<_> = snapshot.catalog.releases.iter().collect();
        let mut releases = Vec::new();
        for result in results {
            ensure!(
                allowed.contains(&result.value),
                "search index references a release outside its catalog"
            );
            let release = Release::verify(self.json(&result.value).await?)?;
            ensure!(
                result.id == release.coordinate(),
                "search identity mismatch"
            );
            releases.push(release);
        }
        Ok(releases)
    }
    /// Write a shareable repository. This does not upload or announce anything.
    pub async fn publish(&self, keys: &Keys, spec: PackageSpec, payload: &Path) -> Result<Release> {
        spec.validate()?;
        let path = self
            .path()
            .context("publish requires a local output directory")?;
        let _guard = lock(&path.join(".publish.lock"))?;
        let mut catalog = Catalog::default();
        let mut sequence = 1;
        if path.join("catalog.json").exists() {
            let previous = self.catalog(&keys.public_key().to_hex()).await?;
            for release in self.releases(&previous).await? {
                ensure!(
                    release.author() != keys.public_key().to_hex()
                        || release.data.package.name != spec.name
                        || release.data.package.version != spec.version
                        || release.data.package.target != spec.target,
                    "release already exists; publish a new version"
                );
            }
            sequence = previous
                .head
                .sequence
                .checked_add(1)
                .context("catalog sequence overflow")?;
            catalog = previous.catalog;
        }
        ensure!(catalog.releases.len() < 10_000, "catalog is full");
        let payload = payload
            .canonicalize()
            .context("payload directory is missing")?;
        ensure!(payload.is_dir(), "payload must be a directory");
        let repo_path = path.canonicalize()?;
        ensure!(
            !repo_path.starts_with(&payload),
            "output repository must be outside payload"
        );
        let mut files = Vec::new();
        let mut total = 0u64;
        let tree = self.store.tree();
        let mut overrides = ignore::overrides::OverrideBuilder::new(&payload);
        for pattern in [
            "!**/.git",
            "!**/.DS_Store",
            "!**/._*",
            "!**/Thumbs.db",
            "!**/desktop.ini",
            "!**/__MACOSX",
            "!/node_modules",
            "!/target",
            "!/.venv",
            "!/venv",
            "!/__pycache__",
            "!/.pytest_cache",
            "!/.mypy_cache",
            "!/.ruff_cache",
            "!/.cache",
            "!/.env",
            "!/.env.local",
            "!/.env.*.local",
        ] {
            overrides.add(pattern)?;
        }
        let entries = ignore::WalkBuilder::new(&payload)
            .hidden(false)
            .parents(false)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            .follow_links(false)
            .overrides(overrides.build()?)
            .sort_by_file_name(|a, b| a.cmp(b))
            .build();
        for entry in entries {
            let entry = entry?;
            if let Some(error) = entry.error() {
                bail!("cannot apply payload ignore rules: {error}");
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            // Never silently rewrite a signed application's filesystem layout.
            let source = if entry.file_type().is_some_and(|t| t.is_file()) {
                entry.path().to_path_buf()
            } else {
                bail!(
                    "symlinks and special files are not yet supported; payload must contain regular files"
                );
            };
            let relative = entry
                .path()
                .strip_prefix(&payload)?
                .to_str()
                .context("non-UTF8 package path")?;
            #[cfg(windows)]
            let relative = relative.replace('\\', "/");
            #[cfg(not(windows))]
            let relative = relative.to_string();
            safe_path(&relative)?;
            let metadata = fs::metadata(&source)?;
            total = total.checked_add(metadata.len()).context("size overflow")?;
            ensure!(
                total <= MAX_PACKAGE_BYTES && files.len() < MAX_FILES,
                "package exceeds size or file limit"
            );
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let executable = executable || spec.commands.values().any(|p| p == &relative);
            let (cid, size) = tree
                .put_stream(futures::io::AllowStdIo::new(fs::File::open(&source)?))
                .await?;
            ensure!(size == metadata.len(), "payload changed while being packed");
            files.push(PackageFile {
                path: relative,
                cid: cid.to_string(),
                size,
                executable,
            });
        }
        let manifest = Manifest {
            schema: "haps.files.v1".into(),
            files,
        };
        manifest.validate(&spec)?;
        let manifest = self.put_json(&manifest).await?;
        let data = ReleaseData {
            schema: "haps.release.v1".into(),
            package: spec.clone(),
            manifest,
        };
        let event = sign(keys, &release_tag(&spec), &data)?;
        let release = Release::verify(event.clone())?;
        let cid = self.put_json(&event).await?;
        let card = sign(
            keys,
            &format!("haps/package/{}", spec.name),
            &serde_json::json!({
                "schema": "haps.package.v1", "name": spec.name, "description": spec.description,
            }),
        )?;
        catalog
            .packages
            .insert(release.identity(), self.put_json(&card).await?);
        let index = SearchIndex::new(self.store.clone(), SearchIndexOptions::default());
        let terms = index.parse_keywords(&format!("{} {}", spec.name, spec.description));
        let terms = if terms.is_empty() {
            vec![spec.name.clone()]
        } else {
            terms
        };
        let previous = catalog.search.as_deref().map(Cid::parse).transpose()?;
        catalog.search = Some(
            index
                .index(previous.as_ref(), "", &terms, &release.coordinate(), &cid)
                .await?
                .to_string(),
        );
        catalog.releases.push(cid);
        let head = CatalogRoot {
            schema: "haps.catalog.v1".into(),
            sequence,
            root: self.put_json(&catalog).await?,
        };
        let event = sign(keys, "haps/catalog/v1", &head)?;
        atomic_write(
            &path.join("catalog.json"),
            &serde_json::to_vec_pretty(&event)?,
        )?;
        Ok(release)
    }
}
