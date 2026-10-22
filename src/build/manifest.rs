//! A Haps project keeps package metadata and optional build steps in one file.
use super::{Recipe, Steps};
use crate::model::PackageSpec;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub package: PackageSpec,
    pub build: Option<Steps>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        Self::for_target(text, crate::model::target())
    }

    pub fn for_target(text: &str, target: &str) -> Result<Self> {
        Self::select(text, target, None)
    }

    pub fn select(text: &str, target: &str, package: Option<&str>) -> Result<Self> {
        let mut document: toml::Table = toml::from_str(text)?;
        let packages = document.remove("packages");
        if let Some(name) = package {
            let default = document
                .get("package")
                .and_then(|p| p.get("name"))
                .or_else(|| document.get("name"))
                .and_then(|n| n.as_str());
            if default != Some(name) {
                document = packages
                    .as_ref()
                    .and_then(|p| p.get(name))
                    .and_then(|p| p.as_table())
                    .with_context(|| format!("source manifest has no package named {name}"))?
                    .clone();
            }
        }
        if let Some(targets) = document.remove("targets") {
            let targets = targets.as_table().context("targets must be a table")?;
            for (name, value) in targets {
                crate::model::safe_name(name)?;
                let section = value
                    .as_table()
                    .context("target configuration must be a table")?;
                ensure!(
                    section.keys().all(|key| key == "package" || key == "build"),
                    "target overrides support package and build sections only"
                );
            }
            if let Some(selected) = targets.get(target) {
                for (section, values) in selected
                    .as_table()
                    .context("invalid target configuration")?
                {
                    let values = values
                        .as_table()
                        .context("target section must be a table")?;
                    let base = if section == "package" && !document.contains_key("package") {
                        &mut document
                    } else {
                        document
                            .entry(section)
                            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                            .as_table_mut()
                            .context("manifest section must be a table")?
                    };
                    for (key, value) in values {
                        base.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        let mut build = document
            .remove("build")
            .map(Steps::deserialize)
            .transpose()?;
        let mut package = if let Some(package) = document.remove("package") {
            ensure!(document.is_empty(), "unknown project manifest fields");
            PackageSpec::deserialize(package)?
        } else {
            PackageSpec::deserialize(toml::Value::Table(document))?
        };
        if package.target == "host" {
            package.target = target.into();
        }
        let expand = |value: String| {
            value.replace("{target}", &package.target).replace(
                "{exe}",
                if package.target.contains("windows") {
                    ".exe"
                } else {
                    ""
                },
            )
        };
        package.commands = package
            .commands
            .into_iter()
            .map(|(name, path)| (name, expand(path)))
            .collect();
        if let Some(build) = &mut build {
            build.commands = std::mem::take(&mut build.commands)
                .into_iter()
                .map(|args| args.into_iter().map(&expand).collect())
                .collect();
            build.artifacts = std::mem::take(&mut build.artifacts)
                .into_iter()
                .map(|(destination, path)| (expand(destination), expand(path)))
                .collect();
        }
        Ok(Self { package, build })
    }

    pub fn recipe(self) -> Result<Recipe> {
        Ok(Recipe {
            package: self.package,
            build: self.build.context("no build instructions declared; use an existing project build recipe or ask a reviewer to prepare one from the project's instructions")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_manifest_selects_package_target_and_expands_paths() {
        let text = r#"
[package]
name = "gui"
version = "1.0.0"
target = "host"
description = "Fixture"
commands = { gui = "gui{exe}" }
[build]
commands = [["cargo", "build", "--target", "{target}"]]
artifacts = { "gui{exe}" = "target/{target}/gui{exe}" }
[targets.x86_64-unknown-linux-gnu.package]
commands = { gui = "usr/bin/gui" }
[targets.x86_64-unknown-linux-gnu.build]
artifacts = { "usr/bin/gui" = "target/{target}/gui" }
[packages.cli.package]
name = "cli"
version = "1.0.0"
target = "host"
description = "CLI fixture"
commands = { cli = "cli{exe}" }
[packages.cli.build]
commands = [["cargo", "build"]]
artifacts = { "cli{exe}" = "cli{exe}" }
"#;
        let linux = Manifest::for_target(text, "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(linux.package.commands["gui"], "usr/bin/gui");
        assert_eq!(
            linux.build.unwrap().commands[0][3],
            "x86_64-unknown-linux-gnu"
        );
        let windows = Manifest::select(text, "x86_64-pc-windows-msvc", Some("cli")).unwrap();
        assert_eq!(windows.package.commands["cli"], "cli.exe");
        assert_eq!(windows.build.unwrap().artifacts["cli.exe"], "cli.exe");
        assert!(Manifest::select(text, "x86_64-pc-windows-msvc", Some("missing")).is_err());
    }

    #[test]
    fn metadata_only_and_explicit_unavailability_are_distinct() {
        let text = "name='hello'\nversion='1.0.0'\ntarget='host'\ndescription='Fixture'\n";
        assert!(Manifest::parse(text).unwrap().build.is_none());
        let manifest = Manifest::parse(&format!(
            "{text}[build]\nunavailable='Requires the documented platform signing setup'\n"
        ))
        .unwrap();
        assert!(manifest.build.unwrap().unavailable.is_some());
        assert!(Manifest::parse(&format!("{text}unknown=true\n")).is_err());
    }
}
