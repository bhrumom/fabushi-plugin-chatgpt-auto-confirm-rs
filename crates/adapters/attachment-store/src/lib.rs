use anyhow::{Context, Result, bail};
use fabushi_chatgpt_domain::AttachmentId;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    pub attachment_id: AttachmentId,
    pub file_name: String,
    pub sha256: String,
    pub byte_len: u64,
    pub storage_ref: PathBuf,
}

pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).with_context(|| format!("create attachment root {root:?}"))?;
        Ok(Self { root })
    }

    pub fn persist_bytes(
        &self,
        attachment_id: AttachmentId,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<StoredAttachment> {
        let safe_name = sanitize_file_name(file_name)?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let directory = self.root.join(attachment_id.as_str());
        fs::create_dir_all(&directory)
            .with_context(|| format!("create attachment directory {directory:?}"))?;

        let final_path = directory.join(&safe_name);
        let temporary_path = directory.join(format!(".{safe_name}.tmp-{}", std::process::id()));

        {
            let mut file = fs::File::create(&temporary_path)
                .with_context(|| format!("create attachment temporary file {temporary_path:?}"))?;
            file.write_all(bytes)
                .with_context(|| format!("write attachment temporary file {temporary_path:?}"))?;
            file.sync_all()
                .with_context(|| format!("sync attachment temporary file {temporary_path:?}"))?;
        }

        fs::rename(&temporary_path, &final_path).with_context(|| {
            format!("atomically install attachment {temporary_path:?} -> {final_path:?}")
        })?;

        Ok(StoredAttachment {
            attachment_id,
            file_name: safe_name,
            sha256: digest,
            byte_len: bytes.len() as u64,
            storage_ref: final_path,
        })
    }

    pub fn verify(&self, attachment: &StoredAttachment) -> Result<bool> {
        let canonical_root = self
            .root
            .canonicalize()
            .context("canonicalize attachment root")?;
        let canonical_path = attachment.storage_ref.canonicalize().with_context(|| {
            format!("canonicalize attachment path {:?}", attachment.storage_ref)
        })?;

        if !canonical_path.starts_with(&canonical_root) {
            bail!("attachment storage reference escaped attachment root");
        }

        let bytes = fs::read(&canonical_path)
            .with_context(|| format!("read attachment path {canonical_path:?}"))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        Ok(digest == attachment.sha256 && bytes.len() as u64 == attachment.byte_len)
    }

    pub fn bytes(&self, attachment: &StoredAttachment) -> Result<Vec<u8>> {
        if !self.verify(attachment)? {
            bail!("attachment integrity verification failed");
        }
        fs::read(&attachment.storage_ref).context("read verified attachment")
    }

    pub fn remove_storage_ref(&self, storage_ref: impl AsRef<Path>) -> Result<()> {
        let storage_ref = storage_ref.as_ref();
        if !storage_ref.exists() {
            return Ok(());
        }
        let canonical_root = self
            .root
            .canonicalize()
            .context("canonicalize attachment root")?;
        let canonical_path = storage_ref
            .canonicalize()
            .with_context(|| format!("canonicalize attachment path {storage_ref:?}"))?;
        if !canonical_path.starts_with(&canonical_root) {
            bail!("attachment deletion escaped attachment root");
        }
        fs::remove_file(&canonical_path)
            .with_context(|| format!("remove task attachment {canonical_path:?}"))?;
        if let Some(parent) = canonical_path.parent()
            && parent != canonical_root
            && parent.starts_with(&canonical_root)
            && fs::read_dir(parent)?.next().is_none()
        {
            fs::remove_dir(parent)
                .with_context(|| format!("remove empty attachment directory {parent:?}"))?;
        }
        Ok(())
    }
}

fn sanitize_file_name(file_name: &str) -> Result<String> {
    let candidate = file_name.trim();
    if candidate.is_empty()
        || candidate == "."
        || candidate == ".."
        || candidate.contains('/')
        || candidate.contains('\\')
        || candidate.contains('\0')
    {
        bail!("unsafe attachment file name");
    }
    Ok(candidate.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "fabushi-attachment-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn persists_and_verifies_attachment_bytes() {
        let root = test_root();
        let store = AttachmentStore::open(&root).unwrap();
        let attachment = store
            .persist_bytes(AttachmentId::new("a-1"), "notes.txt", b"hello")
            .unwrap();

        assert!(store.verify(&attachment).unwrap());
        assert_eq!(store.bytes(&attachment).unwrap(), b"hello");
        assert_eq!(attachment.file_name, "notes.txt");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_mutated_attachment_before_use() {
        let root = test_root();
        let store = AttachmentStore::open(&root).unwrap();
        let attachment = store
            .persist_bytes(AttachmentId::new("a-1"), "notes.txt", b"hello")
            .unwrap();

        fs::write(&attachment.storage_ref, b"changed").unwrap();
        assert!(!store.verify(&attachment).unwrap());
        assert!(store.bytes(&attachment).is_err());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_stored_attachment_idempotently_and_rejects_escape() {
        let root = test_root();
        let store = AttachmentStore::open(&root).unwrap();
        let attachment = store
            .persist_bytes(AttachmentId::new("a-delete"), "notes.txt", b"hello")
            .unwrap();
        store.remove_storage_ref(&attachment.storage_ref).unwrap();
        assert!(!attachment.storage_ref.exists());
        store.remove_storage_ref(&attachment.storage_ref).unwrap();

        let outside = root.parent().unwrap().join(format!(
            "outside-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&outside, b"do not delete").unwrap();
        assert!(store.remove_storage_ref(&outside).is_err());
        assert!(outside.exists());
        fs::remove_file(outside).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_path_traversal_names() {
        let root = test_root();
        let store = AttachmentStore::open(&root).unwrap();
        assert!(
            store
                .persist_bytes(AttachmentId::new("a-1"), "../secret", b"x")
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
