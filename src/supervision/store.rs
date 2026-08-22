use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{BindingReceipt, LogicalSessionId, RECEIPT_SCHEMA_VERSION};

const RECEIPT_FILENAME: &str = "supervision.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ReceiptFile {
    #[serde(default = "receipt_schema_version")]
    pub schema_version: u32,
    #[serde(default, rename = "binding")]
    pub bindings: Vec<BindingReceipt>,
}

impl Default for ReceiptFile {
    fn default() -> Self {
        Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            bindings: Vec::new(),
        }
    }
}

fn receipt_schema_version() -> u32 {
    RECEIPT_SCHEMA_VERSION
}

#[derive(Debug, Clone)]
pub struct ReceiptStore {
    path: PathBuf,
}

impl ReceiptStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn standard() -> Result<Self> {
        Ok(Self::new(crate::state::state_dir()?.join(RECEIPT_FILENAME)))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<ReceiptFile> {
        load_receipts(&self.path)
    }

    pub fn list(&self) -> Result<Vec<BindingReceipt>> {
        Ok(self.load()?.bindings)
    }

    pub fn find(&self, logical_id: &LogicalSessionId) -> Result<Option<BindingReceipt>> {
        Ok(self
            .load()?
            .bindings
            .into_iter()
            .find(|binding| &binding.logical_id == logical_id))
    }

    pub fn upsert(&self, receipt: BindingReceipt) -> Result<()> {
        receipt.validate_shape()?;
        self.mutate(|file| {
            file.bindings
                .retain(|existing| existing.logical_id != receipt.logical_id);
            file.bindings.push(receipt);
            file.bindings
                .sort_by(|a, b| a.logical_id.cmp(&b.logical_id));
            Ok(())
        })
    }

    pub fn remove(&self, logical_id: &LogicalSessionId) -> Result<bool> {
        let mut removed = false;
        self.mutate(|file| {
            let before = file.bindings.len();
            file.bindings
                .retain(|existing| &existing.logical_id != logical_id);
            removed = before != file.bindings.len();
            Ok(())
        })?;
        Ok(removed)
    }

    pub(crate) fn update_locked<T>(
        &self,
        op: impl FnOnce(&mut ReceiptFile) -> Result<T>,
    ) -> Result<T> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("receipt path has no parent: {}", self.path.display()))?;
        ensure_private_dir(parent)?;
        let lock_path = PathBuf::from(format!("{}.lock", self.path.display()));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("opening supervision lock {}", lock_path.display()))?;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "setting supervision lock permissions {}",
                lock_path.display()
            )
        })?;
        flock(&lock, FlockOperation::LockExclusive)
            .with_context(|| format!("locking {}", lock_path.display()))?;
        let _guard = FileLock(lock);

        let mut file = load_receipts(&self.path)?;
        let output = op(&mut file)?;
        save_receipts(&self.path, &file)?;
        Ok(output)
    }

    fn mutate(&self, op: impl FnOnce(&mut ReceiptFile) -> Result<()>) -> Result<()> {
        self.update_locked(op)
    }
}

struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

fn load_receipts(path: &Path) -> Result<ReceiptFile> {
    if !path.is_file() {
        return Ok(ReceiptFile::default());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading supervision receipts {}", path.display()))?;
    let file: ReceiptFile = toml::from_str(&raw)
        .with_context(|| format!("parsing supervision receipts {}", path.display()))?;
    if file.schema_version != RECEIPT_SCHEMA_VERSION {
        return Err(anyhow!(
            "unsupported supervision store schema {} (expected {})",
            file.schema_version,
            RECEIPT_SCHEMA_VERSION
        ));
    }
    for receipt in &file.bindings {
        receipt.validate_shape()?;
    }
    Ok(file)
}

fn save_receipts(path: &Path, receipts: &ReceiptFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("receipt path has no parent: {}", path.display()))?;
    ensure_private_dir(parent)?;
    let serialized =
        toml::to_string_pretty(receipts).context("serializing supervision receipts")?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("supervision.toml"),
        Uuid::new_v4().simple()
    ));
    let mut temp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)
        .with_context(|| format!("creating temporary receipt file {}", temp_path.display()))?;
    let result = (|| -> Result<()> {
        temp.write_all(serialized.as_bytes())
            .with_context(|| format!("writing {}", temp_path.display()))?;
        temp.sync_all()
            .with_context(|| format!("syncing {}", temp_path.display()))?;
        drop(temp);
        fs::rename(&temp_path, path)
            .with_context(|| format!("renaming receipts onto {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting receipt permissions {}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing receipt directory {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting private directory permissions {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::model::{BackendKind, MuxTarget, SoftLimits};
    use std::os::unix::fs::PermissionsExt;

    fn logical(name: &str) -> LogicalSessionId {
        LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", name).unwrap()
    }

    fn receipt(name: &str, generation: &str) -> BindingReceipt {
        BindingReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            logical_id: logical(name),
            backend: BackendKind::SystemdUserService,
            unit_name: format!(
                "portagenty-w550e8400e29b41d4a716446655440000-g{generation}.service"
            ),
            invocation_id: generation.to_string(),
            control_group: format!("/user.slice/example/{generation}.service"),
            mux_target: MuxTarget::TmuxPrivate {
                socket: PathBuf::from(format!("/run/user/1000/{generation}.sock")),
                session: "main".into(),
            },
            observed_at_unix_ms: 1,
            limits: SoftLimits::default(),
        }
    }

    #[test]
    fn upsert_replaces_only_the_same_logical_session() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        store.upsert(receipt("shell", "1111")).unwrap();
        store.upsert(receipt("editor", "2222")).unwrap();
        store.upsert(receipt("shell", "3333")).unwrap();
        let file = store.load().unwrap();
        assert_eq!(file.bindings.len(), 2);
        assert_eq!(
            store
                .find(&logical("shell"))
                .unwrap()
                .unwrap()
                .invocation_id,
            "3333"
        );
    }

    #[test]
    fn remove_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        store.upsert(receipt("shell", "1111")).unwrap();
        assert!(store.remove(&logical("shell")).unwrap());
        assert!(!store.remove(&logical("shell")).unwrap());
    }

    #[test]
    fn receipt_and_state_directory_are_private() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        store.upsert(receipt("shell", "1111")).unwrap();
        let dir_mode = fs::metadata(store.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn malformed_store_is_not_silently_discarded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("supervision.toml");
        fs::write(&path, "this is not toml = [").unwrap();
        assert!(ReceiptStore::new(path).load().is_err());
    }
}
