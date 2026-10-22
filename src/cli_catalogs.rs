//! Catalog commands keep their temporary async state outside main dispatch.
use crate::*;

pub(super) async fn run(
    home: &Path,
    config: &mut Config,
    trust: &mut Trust,
    action: CatalogAction,
) -> Result<()> {
    use haps::event_catalog::{IndexAnnouncement, directory};
    match action {
        CatalogAction::Discover => {
            refresh_discovery(home, config, trust, None, None, None).await?;
            println!("{}", serde_json::to_string_pretty(&config.indexes)?);
        }
        CatalogAction::List => {
            let path = home.join("catalogs");
            let mut own = Vec::new();
            if path.exists() {
                for entry in fs::read_dir(path)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        own.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            own.sort();
            println!(
                "{}",
                serde_json::json!({"own": own, "indexes": config.indexes})
            );
        }
        CatalogAction::Show { name } => {
            let path = directory(home, &name)?;
            ensure!(path.exists(), "catalog does not exist");
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &Discovery::open(&path)?
                        .events(vec![nostr::Filter::new()])
                        .await?
                )?
            );
        }
        CatalogAction::Remove { package, catalog } => {
            let package = resolve_package(config, &package)?;
            let path = directory(home, &catalog)?;
            ensure!(path.exists(), "catalog does not exist");
            let selected = Discovery::open(&path)?;
            let events = selected.events(vec![nostr::Filter::new()]).await?;
            let identity = |event: &Event| -> Option<String> {
                if let Ok(a) = Announcement::verify(event.clone()) {
                    Some(format!("{}/{}", a.author(), a.name))
                } else {
                    Release::verify(event.clone()).ok().map(|r| r.identity())
                }
            };
            let matches: BTreeSet<_> = events
                .iter()
                .filter_map(identity)
                .filter(|id| *id == package || id.rsplit('/').next() == Some(package.as_str()))
                .collect();
            ensure!(
                matches.len() == 1,
                "package is missing or ambiguous; use publisher/name"
            );
            selected
                .replace_events(
                    events
                        .into_iter()
                        .filter(|event| !identity(event).is_some_and(|id| matches.contains(&id))),
                )
                .await?;
            println!("Removed from catalog {catalog}");
        }
        CatalogAction::Add {
            name,
            location,
            author,
        } => {
            safe_name(&name)?;
            ensure!(!config.indexes.contains_key(&name), "index already exists");
            let author = resolve_key(config, &author)?;
            Discovery::open(home)?
                .import_index(&location, &author)
                .await?;
            config.indexes.insert(
                name,
                Source {
                    location,
                    author,
                    sequence: 0,
                    event_id: String::new(),
                },
            );
            save_config(home, config)?;
        }
        CatalogAction::Publish { name } => {
            let path = directory(home, &name)?;
            ensure!(path.exists(), "catalog does not exist; add a package first");
            let keys = publishing_keys(home, config)?;
            let selected = Discovery::open(&path)?;
            // Local package records are built once; public indexes retain
            // original signatures for packages curated from elsewhere.
            let selected_events = selected.events(vec![nostr::Filter::new()]).await?;
            let selected_ids: BTreeSet<_> = selected_events
                .iter()
                .filter_map(|e| Release::verify(e.clone()).ok().map(|r| r.identity()))
                .collect();
            let mut package_events = Vec::new();
            if path.join("packages/catalog.json").exists() {
                package_events =
                    haps::event_catalog::package_events(home, &path.join("packages"), &keys, None)
                        .await?;
                package_events.retain(|event| {
                    if let Ok(a) = Announcement::verify(event.clone()) {
                        selected_ids.contains(&format!("{}/{}", a.author(), a.name))
                    } else {
                        Release::verify(event.clone())
                            .is_ok_and(|r| selected_ids.contains(&r.identity()))
                    }
                });
                selected.ingest(package_events.clone()).await?;
            }
            let location = haps::event_catalog::upload_index(
                home,
                &selected.events(vec![nostr::Filter::new()]).await?,
            )?;
            drop(selected);
            let discovery = Discovery::open(home)?;
            let previous = discovery.events(vec![nostr::Filter::new()]).await?;
            let announcement = haps::event_catalog::advance(
                &keys,
                IndexAnnouncement::sign(&keys, &name, &location)?,
                &previous,
            )?;
            package_events.push(announcement);
            discovery.queue(&package_events)?;
            discovery.ingest(package_events.clone()).await?;
            flush_discovery(home, &discovery, &package_events).await?;
            println!("Published catalog {name}: {location}");
        }
    }

    Ok(())
}
