use hashtree_core::{Cid, DirEntry, HashTree, LinkType, MemoryStore};
use std::{fs, future::Future, pin::Pin};

pub fn add_directory<'a>(
    tree: &'a HashTree<MemoryStore>,
    path: &'a std::path::Path,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Cid>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = vec![];
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let (cid, size, kind) = if entry.file_type()?.is_dir() {
                (add_directory(tree, &entry.path()).await?, 0, LinkType::Dir)
            } else {
                let (cid, size) = tree.put(&fs::read(entry.path())?).await?;
                (cid, size, LinkType::File)
            };
            entries.push(DirEntry {
                name: entry.file_name().to_str().unwrap().into(),
                hash: cid.hash,
                key: cid.key,
                size,
                link_type: kind,
                meta: None,
            });
        }
        Ok(tree.put_directory(entries).await?)
    })
}
