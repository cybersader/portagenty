use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{
    BindingReceipt, LogicalSessionId, MuxTarget, LEGACY_RECEIPT_SCHEMA_VERSION,
    RECEIPT_SCHEMA_VERSION,
};

const RECEIPT_FILENAME: &str = "supervision.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ReceiptFile {
    #[serde(default = "receipt_schema_version")]
    pub schema_version: u32,
    #[serde(default, rename = "binding")]
    pub bindings: Vec<BindingReceipt>,
    #[serde(default, rename = "pending-launch")]
    pub pending_launches: Vec<PendingLaunch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PendingLaunch {
    pub logical_id: LogicalSessionId,
    pub unit_name: String,
    pub mux_target: MuxTarget,
    pub marker_path: PathBuf,
    pub created_at_unix_ms: u64,
    #[serde(default)]
    pub creator_pid: u32,
    #[serde(default)]
    pub creator_start_time_ticks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_boot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl PendingLaunch {
    pub fn validate_shape(&self) -> Result<()> {
        self.logical_id.workspace_uuid()?;
        if !self.unit_name.starts_with("portagenty-w")
            || !self.unit_name.ends_with(".service")
            || !self.marker_path.is_absolute()
        {
            return Err(anyhow!("invalid pending supervision launch"));
        }
        let filename = self
            .marker_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("pending marker path has no UTF-8 filename"))?;
        let nonce = filename
            .strip_suffix(".marker.toml")
            .ok_or_else(|| anyhow!("pending marker filename is invalid"))?;
        super::model::validate_workload_marker_shape(&self.marker_path, nonce)?;
        Ok(())
    }

    pub fn has_creator_proof(&self) -> bool {
        self.creator_pid != 0 && self.creator_start_time_ticks != 0
    }
}

impl Default for ReceiptFile {
    fn default() -> Self {
        Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            bindings: Vec::new(),
            pending_launches: Vec::new(),
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
            file.pending_launches
                .retain(|pending| pending.logical_id != receipt.logical_id);
            file.bindings.push(receipt);
            file.bindings
                .sort_by(|a, b| a.logical_id.cmp(&b.logical_id));
            Ok(())
        })
    }

    pub fn begin_pending(&self, pending: PendingLaunch) -> Result<()> {
        pending.validate_shape()?;
        if !pending.has_creator_proof() {
            return Err(anyhow!(
                "new pending launch is missing creator process proof"
            ));
        }
        self.mutate(|file| {
            if file
                .bindings
                .iter()
                .any(|receipt| receipt.logical_id == pending.logical_id)
                || file
                    .pending_launches
                    .iter()
                    .any(|existing| existing.logical_id == pending.logical_id)
            {
                return Err(anyhow!(
                    "a supervision binding or pending launch already exists for this session"
                ));
            }
            file.pending_launches.push(pending);
            file.pending_launches
                .sort_by(|a, b| a.logical_id.cmp(&b.logical_id));
            Ok(())
        })
    }

    pub fn finalize_pending(&self, receipt: BindingReceipt) -> Result<()> {
        receipt.validate_shape()?;
        self.mutate(|file| {
            let had_pending = file
                .pending_launches
                .iter()
                .any(|pending| pending.logical_id == receipt.logical_id);
            if !had_pending {
                return Err(anyhow!(
                    "pending launch disappeared before receipt persistence"
                ));
            }
            file.pending_launches
                .retain(|pending| pending.logical_id != receipt.logical_id);
            file.bindings
                .retain(|existing| existing.logical_id != receipt.logical_id);
            file.bindings.push(receipt);
            file.bindings
                .sort_by(|a, b| a.logical_id.cmp(&b.logical_id));
            Ok(())
        })
    }

    pub fn mark_pending_error(&self, logical_id: &LogicalSessionId, error: String) -> Result<()> {
        self.mutate(|file| {
            let pending = file
                .pending_launches
                .iter_mut()
                .find(|pending| &pending.logical_id == logical_id)
                .ok_or_else(|| anyhow!("pending launch is no longer present"))?;
            pending.last_error = Some(error);
            Ok(())
        })
    }

    pub fn clear_pending(&self, logical_id: &LogicalSessionId) -> Result<bool> {
        let mut removed = false;
        self.mutate(|file| {
            let before = file.pending_launches.len();
            file.pending_launches
                .retain(|pending| &pending.logical_id != logical_id);
            removed = before != file.pending_launches.len();
            Ok(())
        })?;
        Ok(removed)
    }

    pub fn find_pending(&self, logical_id: &LogicalSessionId) -> Result<Option<PendingLaunch>> {
        Ok(self
            .load()?
            .pending_launches
            .into_iter()
            .find(|pending| &pending.logical_id == logical_id))
    }

    pub fn remove(&self, logical_id: &LogicalSessionId) -> Result<bool> {
        self.update_locked(|file| {
            let Some(index) = file
                .bindings
                .iter()
                .position(|existing| &existing.logical_id == logical_id)
            else {
                return Ok(false);
            };
            if let Some(anchor) = file.bindings[index].workload_anchor.as_ref() {
                super::linux_systemd::remove_verified_workload_marker(anchor)?;
            }
            file.bindings.remove(index);
            Ok(true)
        })
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
    if !matches!(
        file.schema_version,
        LEGACY_RECEIPT_SCHEMA_VERSION | RECEIPT_SCHEMA_VERSION
    ) {
        return Err(anyhow!(
            "unsupported supervision store schema {}",
            file.schema_version
        ));
    }
    for receipt in &file.bindings {
        receipt.validate_shape()?;
    }
    for pending in &file.pending_launches {
        pending.validate_shape()?;
    }
    Ok(file)
}

fn save_receipts(path: &Path, receipts: &ReceiptFile) -> Result<()> {
    let mut receipts = receipts.clone();
    receipts.schema_version = RECEIPT_SCHEMA_VERSION;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("receipt path has no parent: {}", path.display()))?;
    ensure_private_dir(parent)?;
    let serialized =
        toml::to_string_pretty(&receipts).context("serializing supervision receipts")?;
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
    use crate::supervision::model::{BackendKind, MuxTarget, ResourceLimits};
    use std::os::unix::fs::PermissionsExt;

    fn logical(name: &str) -> LogicalSessionId {
        LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", name).unwrap()
    }

    fn receipt(name: &str, generation: &str) -> BindingReceipt {
        BindingReceipt {
            schema_version: LEGACY_RECEIPT_SCHEMA_VERSION,
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
            limits: ResourceLimits::default(),
            session_kind: None,
            requested_slice: None,
            workload_anchor: None,
            launch_boot_id: None,
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
    fn mixed_legacy_and_current_receipts_load_and_save_without_upgrading_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let legacy = receipt("legacy", "11112222333344445555666677778888");
        let mut current = receipt("current", "9999aaaabbbbccccddddeeeeffff0000");
        current.schema_version = RECEIPT_SCHEMA_VERSION;
        current.workload_anchor = Some(super::super::model::WorkloadAnchorProof {
            protocol_version: 1,
            nonce: "0123456789abcdef0123456789abcdef".into(),
            marker_path: temp
                .path()
                .join("portagenty/workloads/0123456789abcdef0123456789abcdef.marker.toml"),
            pid: 123,
            start_time_ticks: 456,
        });
        store.upsert(legacy.clone()).unwrap();
        store.upsert(current.clone()).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.schema_version, RECEIPT_SCHEMA_VERSION);
        assert_eq!(loaded.bindings.len(), 2);
        assert!(loaded.bindings.iter().any(BindingReceipt::is_legacy));
        assert!(loaded
            .bindings
            .iter()
            .any(|binding| binding.schema_version == RECEIPT_SCHEMA_VERSION));
        assert!(loaded
            .bindings
            .iter()
            .all(|binding| binding.launch_boot_id.is_none()));
        assert!(!fs::read_to_string(store.path())
            .unwrap()
            .contains("launch-boot-id"));
    }

    #[test]
    fn optional_boot_provenance_round_trips_without_shape_authority() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let mut stamped = receipt("stamped", "11112222333344445555666677778888");
        stamped.launch_boot_id = Some("550e8400-e29b-41d4-a716-446655440000".into());
        store.upsert(stamped.clone()).unwrap();

        let pending = PendingLaunch {
            logical_id: logical("pending-boot"),
            unit_name: "portagenty-wpending.service".into(),
            mux_target: MuxTarget::TmuxPrivate {
                socket: temp.path().join("pending.sock"),
                session: "main".into(),
            },
            marker_path: temp
                .path()
                .join("portagenty/workloads/fedcba9876543210fedcba9876543210.marker.toml"),
            created_at_unix_ms: 1,
            creator_pid: 123,
            creator_start_time_ticks: 456,
            creator_boot_id: Some("not-a-uuid".into()),
            last_error: None,
        };
        store.begin_pending(pending.clone()).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.bindings, vec![stamped]);
        assert_eq!(loaded.pending_launches, vec![pending]);
    }

    #[test]
    fn malformed_receipt_boot_hint_is_nonfatal() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let mut malformed = receipt("malformed", "11112222333344445555666677778888");
        malformed.schema_version = RECEIPT_SCHEMA_VERSION;
        malformed.workload_anchor = Some(super::super::model::WorkloadAnchorProof {
            protocol_version: 1,
            nonce: "0123456789abcdef0123456789abcdef".into(),
            marker_path: temp
                .path()
                .join("portagenty/workloads/0123456789abcdef0123456789abcdef.marker.toml"),
            pid: 123,
            start_time_ticks: 456,
        });
        malformed.launch_boot_id = Some("not-a-uuid".into());
        store.upsert(malformed.clone()).unwrap();
        assert_eq!(store.load().unwrap().bindings, vec![malformed]);
    }

    #[test]
    fn pending_launch_blocks_duplicate_and_finalizes_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let logical_id = logical("pending");
        let pending = PendingLaunch {
            logical_id: logical_id.clone(),
            unit_name: "portagenty-wpending.service".into(),
            mux_target: MuxTarget::TmuxPrivate {
                socket: temp.path().join("pending.sock"),
                session: "main".into(),
            },
            marker_path: temp
                .path()
                .join("portagenty/workloads/fedcba9876543210fedcba9876543210.marker.toml"),
            created_at_unix_ms: 1,
            creator_pid: 123,
            creator_start_time_ticks: 456,
            creator_boot_id: None,
            last_error: None,
        };
        store.begin_pending(pending.clone()).unwrap();
        assert!(store.begin_pending(pending).is_err());
        assert_eq!(
            store
                .find_pending(&logical_id)
                .unwrap()
                .unwrap()
                .creator_boot_id,
            None
        );

        let mut current = receipt("pending", "00112233445566778899aabbccddeeff");
        current.schema_version = RECEIPT_SCHEMA_VERSION;
        current.workload_anchor = Some(super::super::model::WorkloadAnchorProof {
            protocol_version: 1,
            nonce: "fedcba9876543210fedcba9876543210".into(),
            marker_path: temp
                .path()
                .join("portagenty/workloads/fedcba9876543210fedcba9876543210.marker.toml"),
            pid: 123,
            start_time_ticks: 456,
        });
        store.finalize_pending(current.clone()).unwrap();
        assert!(store.find_pending(&logical_id).unwrap().is_none());
        assert_eq!(store.find(&logical_id).unwrap(), Some(current));
    }

    #[test]
    fn malformed_store_is_not_silently_discarded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("supervision.toml");
        fs::write(&path, "this is not toml = [").unwrap();
        assert!(ReceiptStore::new(path).load().is_err());
    }
}
