use super::*;
use std::collections::BTreeMap;

pub(crate) fn directory(home: &Path, release: &Release) -> PathBuf {
    home.join("environments")
        .join(release.event.id.to_hex())
        .join("bin")
}

impl Installation {
    pub fn runtime_path(&self, name: &str) -> Result<Option<std::ffi::OsString>> {
        let release = Release::verify(self.receipt(name)?.current)?;
        if release.data.package.dependencies.is_empty() {
            return Ok(None);
        }
        let mut paths = vec![directory(&self.home, &release)];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        Ok(Some(std::env::join_paths(paths)?))
    }

    pub(super) fn prepare_environment(
        &self,
        release: &Release,
        available: &[Release],
    ) -> Result<()> {
        if release.data.package.dependencies.is_empty() {
            return Ok(());
        }
        let plan = crate::dependencies::resolve(release, available)?;
        let mut commands = BTreeMap::new();
        for dependency in plan.iter().filter(|r| r.event.id != release.event.id) {
            for (name, relative) in &dependency.data.package.commands {
                ensure!(
                    !release.data.package.commands.contains_key(name),
                    "dependency command conflicts with parent: {name}"
                );
                let destination = self.version_dir(dependency).join(relative);
                #[cfg(windows)]
                let name = {
                    let suffix = destination
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("exe");
                    format!("{name}.{suffix}")
                };
                #[cfg(not(windows))]
                let name = name.clone();
                ensure!(
                    commands.insert(name.clone(), destination).is_none(),
                    "ambiguous dependency command: {name}"
                );
            }
        }
        let bin = directory(&self.home, release);
        std::env::join_paths([&bin])?;
        if bin.symlink_metadata().is_ok() {
            ensure!(
                fs::symlink_metadata(&bin)?.is_dir(),
                "dependency environment is not a directory"
            );
            ensure!(
                fs::read_dir(&bin)?.count() == commands.len(),
                "dependency environment contains unexpected commands"
            );
            #[cfg(unix)]
            for (name, destination) in &commands {
                ensure!(
                    fs::read_link(bin.join(name)).ok().as_ref() == Some(destination),
                    "dependency command was modified: {name}"
                );
            }
            #[cfg(windows)]
            for (name, destination) in &commands {
                ensure!(
                    same_contents(&bin.join(name), destination)?,
                    "dependency command was modified: {name}"
                );
            }
            return Ok(());
        }
        let parent = bin.parent().context("invalid dependency environment")?;
        fs::create_dir_all(parent)?;
        let stage = tempfile::tempdir_in(parent)?;
        for (name, destination) in commands {
            #[cfg(unix)]
            std::os::unix::fs::symlink(destination, stage.path().join(name))?;
            #[cfg(windows)]
            fs::hard_link(destination, stage.path().join(name))?;
        }
        fs::rename(stage.path(), bin)?;
        Ok(())
    }
}

#[cfg(windows)]
fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    use std::io::{BufRead, BufReader};
    let mut left = BufReader::new(fs::File::open(left)?);
    let mut right = BufReader::new(fs::File::open(right)?);
    loop {
        let a = left.fill_buf()?;
        let b = right.fill_buf()?;
        if a.is_empty() || b.is_empty() {
            return Ok(a.is_empty() && b.is_empty());
        }
        let n = a.len().min(b.len());
        if a[..n] != b[..n] {
            return Ok(false);
        }
        left.consume(n);
        right.consume(n);
    }
}
