//! Windows desktop declarations live in signed event tags so v1 readers can
//! still parse a mixed-platform catalog and install its ordinary commands.
use super::*;

const NAME: &str = "haps/windows-desktop-v1";
const MAX_BYTES: usize = 16 * 1024;

pub(super) fn hydrate(event: &Event, package: &mut PackageSpec) -> Result<()> {
    let extensions: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|name| name == NAME))
        .collect();
    ensure!(extensions.len() <= 1, "duplicate Windows desktop extension");
    if let Some(tag) = extensions.first() {
        let fields = tag.as_slice();
        ensure!(fields.len() == 2, "malformed Windows desktop extension");
        ensure!(
            fields[1].len() <= MAX_BYTES,
            "Windows desktop extension exceeds limit"
        );
        ensure!(
            package.target.ends_with("-windows-msvc"),
            "Windows desktop extension requires a Windows target"
        );
        ensure!(
            package.desktop.is_none(),
            "desktop metadata occurs in both release content and extension"
        );
        package.desktop =
            Some(serde_json::from_str(&fields[1]).context("invalid Windows desktop extension")?);
    } else {
        ensure!(
            !package.target.ends_with("-windows-msvc") || package.desktop.is_none(),
            "Windows desktop metadata must use the signed extension"
        );
    }
    Ok(())
}

pub(super) fn sign_release(keys: &Keys, data: &ReleaseData) -> Result<Event> {
    data.package.validate()?;
    let mut content = data.clone();
    let mut tags = vec![Tag::identifier(release_tag(&data.package))];
    if content.package.target.ends_with("-windows-msvc")
        && let Some(desktop) = content.package.desktop.take()
    {
        let value = serde_json::to_string(&desktop)?;
        ensure!(
            value.len() <= MAX_BYTES,
            "Windows desktop extension exceeds limit"
        );
        tags.push(Tag::parse([NAME, &value])?);
    }
    Ok(
        EventBuilder::new(APP_KIND, serde_json::to_string(&content)?)
            .tags(tags)
            .sign_with_keys(keys)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> ReleaseData {
        ReleaseData {
            schema: "haps.release.v1".into(),
            package: PackageSpec {
                name: "example".into(),
                version: "1.0.0".parse().unwrap(),
                target: "x86_64-pc-windows-msvc".into(),
                description: "Example".into(),
                commands: BTreeMap::from([("example".into(), "Example.exe".into())]),
                dependencies: vec![],
                source: None,
                app: None,
                desktop: Some(DesktopEntry {
                    name: "Example".into(),
                    command: "example".into(),
                    icon: "Example.ico".into(),
                    uri_schemes: vec!["example-app".into()],
                }),
            },
            manifest: "00".repeat(32),
        }
    }
    fn event(data: &ReleaseData, tags: Vec<Vec<String>>) -> Event {
        EventBuilder::new(APP_KIND, serde_json::to_string(data).unwrap())
            .tags(
                std::iter::once(Tag::identifier(release_tag(&data.package)))
                    .chain(tags.into_iter().map(|t| Tag::parse(t).unwrap())),
            )
            .sign_with_keys(&Keys::generate())
            .unwrap()
    }
    #[test]
    fn signed_windows_desktop_roundtrip_keeps_legacy_content() -> Result<()> {
        let data = data();
        let event = sign_release(&Keys::generate(), &data)?;
        let wire: ReleaseData = serde_json::from_str(&event.content)?;
        assert!(wire.package.desktop.is_none());
        assert!(!event.content.contains("uri_schemes"));
        let verified = Release::verify(event.clone())?;
        assert_eq!(verified.data.package, data.package);
        let mut tampered = event;
        tampered.tags = nostr::Tags::from_list(vec![Tag::identifier(release_tag(&data.package))]);
        assert!(Release::verify(tampered).is_err());
        Ok(())
    }
    #[test]
    fn invalid_windows_desktop_extensions_are_rejected() {
        let mut data = data();
        let json = serde_json::to_string(&data.package.desktop.take().unwrap()).unwrap();
        let valid = vec![NAME.into(), json.clone()];
        for tags in [
            vec![valid.clone(), valid.clone()],
            vec![vec![NAME.into()]],
            vec![vec![NAME.into(), json.clone(), "extra".into()]],
            vec![vec![NAME.into(), "{}".into()]],
            vec![vec![NAME.into(), "null".into()]],
            vec![vec![NAME.into(), " ".repeat(MAX_BYTES + 1)]],
            vec![vec![NAME.into(), json.replace("Example.ico", "../bad.ico")]],
            vec![vec![
                NAME.into(),
                json.replace("\"name\":", "\"name\":\"duplicate\",\"name\":"),
            ]],
            vec![vec![NAME.into(), json.replace("example-app", "https")]],
            vec![vec![
                NAME.into(),
                json.replace("\"name\":", "\"unknown\":0,\"name\":"),
            ]],
        ] {
            assert!(Release::verify(event(&data, tags)).is_err());
        }
        // Future extensions remain uninterpreted, preserving catalog compatibility.
        let future = Release::verify(event(
            &data,
            vec![vec!["haps/windows-desktop-v2".into(), json.clone()]],
        ))
        .unwrap();
        assert!(future.data.package.desktop.is_none());
        assert!(Release::verify(event(&data, vec![])).is_ok());
        data.package.target = "x86_64-unknown-linux-gnu".into();
        assert!(Release::verify(event(&data, vec![valid.clone()])).is_err());
        data.package.target = "x86_64-pc-windows-msvc".into();
        data.package.desktop = Some(serde_json::from_str(&json).unwrap());
        assert!(Release::verify(event(&data, vec![valid])).is_err());
        assert!(Release::verify(event(&data, vec![])).is_err());
    }
    #[test]
    fn linux_desktop_signing_preserves_existing_wire_format() -> Result<()> {
        let mut data = data();
        data.package.target = "x86_64-unknown-linux-gnu".into();
        let desktop = data.package.desktop.as_mut().unwrap();
        desktop.icon = "icon.png".into();
        desktop.uri_schemes.clear();
        let event = sign_release(&Keys::generate(), &data)?;
        assert_eq!(event.content, serde_json::to_string(&data)?);
        assert_eq!(event.tags.len(), 1);
        assert_eq!(Release::verify(event)?.data.package, data.package);
        Ok(())
    }
}
