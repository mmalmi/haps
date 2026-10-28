use super::*;
use futures::{TryStreamExt, stream};
use hashtree_core::Cid;
use std::io::Write;

impl Installation {
    pub(super) async fn stage_payload(&self, repo: &Repository, release: &Release) -> Result<()> {
        self.progress.stage("Reading package manifest");
        let manifest: Manifest = repo.json(&release.data.manifest).await?;
        manifest.validate(&release.data.package)?;
        let final_dir = self.version_dir(release);
        let parent = final_dir.parent().context("invalid install directory")?;
        fs::create_dir_all(parent)?;
        let stage = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(parent)?;
        let tree = repo.store.tree();
        self.progress.download(
            manifest.files.iter().map(|f| f.size).sum(),
            manifest.files.len(),
        );
        let tree = &tree;
        let stage_path = stage.path();
        // Manifest validation guarantees distinct, non-overlapping paths. Keep
        // downloads bounded and borrowed: an error drops every pending writer
        // before the staging directory is removed.
        stream::iter(manifest.files.iter().map(Ok))
            .try_for_each_concurrent(Some(4), |file| async move {
                let output = stage_path.join(safe_path(&file.path)?);
                fs::create_dir_all(output.parent().unwrap())?;
                let mut destination = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&output)?;
                let cid = Cid::parse(&file.cid)?;
                let mut stream = tree.get_stream(&cid);
                let mut written = 0u64;
                while let Some(chunk) = stream.try_next().await? {
                    written = written
                        .checked_add(chunk.len() as u64)
                        .context("file size overflow")?;
                    ensure!(written <= file.size, "download exceeds signed file size");
                    destination.write_all(&chunk)?;
                    self.progress.advance(chunk.len() as u64, 0);
                }
                ensure!(written == file.size, "incomplete file: {}", file.path);
                // Check existence even for empty files: a missing root must not masquerade as empty content.
                use hashtree_core::Store;
                ensure!(repo.store.has(&cid.hash).await?, "file root is unavailable");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    destination.set_permissions(fs::Permissions::from_mode(
                        if file.executable { 0o755 } else { 0o644 },
                    ))?;
                }
                destination.sync_all()?;
                self.progress.advance(0, 1);
                Ok(())
            })
            .await?;
        self.progress.finish_download();
        self.progress.stage("Registering package");
        // A previous interrupted install may have left this immutable slot. Never
        // trust those bytes: replace it only after the fresh verified stage exists.
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)?;
        }
        fs::rename(stage.path(), &final_dir)?;
        Ok(())
    }
}
