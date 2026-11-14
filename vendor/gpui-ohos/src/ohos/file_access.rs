use anyhow::{Context as _, Result};
use ohos_fileshare_binding::{FileShareError, PolicyInfo};
use parking_lot::Mutex;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use super::presentation::Presentation;

const READ: u32 = 1;
const READ_WRITE: u32 = 3;
const MAX_GRANTS: usize = 256;
const BATCH_SIZE: usize = 64;
const MAX_URI_BYTES: usize = 4096;
const MAX_HISTORY_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) struct FileAccess {
    enabled: bool,
    path: Option<PathBuf>,
    presented: AtomicBool,
    state: Mutex<FileAccessState>,
}

#[derive(Default)]
struct FileAccessState {
    /// Persisted grants by URI, merged across sessions.
    policies: Option<BTreeMap<String, u32>>,
    /// The URIs of `policies` in least-recently-used order (oldest first).
    /// Mirrors the keys of `policies` exactly so eviction can pop the grant
    /// the user has touched least recently.
    order: Vec<String>,
    pending: BTreeMap<String, u32>,
    restored: bool,
    dirty: bool,
    /// Set when the file-share service refused a whole API call for a reason
    /// that cannot recover within this process; disables every further grant
    /// persistence, activation and revocation attempt.
    unavailable: Option<&'static str>,
}

impl FileAccessState {
    fn contains_grant(&self, uri: &str) -> bool {
        self.policies
            .as_ref()
            .is_some_and(|policies| policies.contains_key(uri))
    }

    /// Stores `mode` for `uri` verbatim (overwriting any previous mode)
    /// without refreshing recency; startup restoration keeps the persisted
    /// order intact.
    fn set_mode(&mut self, uri: &str, mode: u32) {
        if let Some(policies) = self.policies.as_mut() {
            if policies.get(uri).copied() != Some(mode) {
                policies.insert(uri.to_string(), mode);
                self.dirty = true;
            }
        }
        if !self.order.iter().any(|known| known == uri) {
            self.order.push(uri.to_string());
            self.dirty = true;
        }
    }

    /// Merges `mode` into the grant for `uri` and moves it to the back of the
    /// recency order (a fresh selection through Picker, a drop or open URI).
    /// Marks the history dirty when the mode, membership or recency changed.
    fn touch_grant(&mut self, uri: &str, mode: u32) -> u32 {
        let previous = self
            .policies
            .as_ref()
            .and_then(|policies| policies.get(uri).copied());
        let merged = previous.unwrap_or(0) | mode;
        if previous != Some(merged) {
            self.policies
                .get_or_insert_with(BTreeMap::new)
                .insert(uri.to_string(), merged);
            self.dirty = true;
        }
        match self.order.iter().position(|known| known == uri) {
            Some(position) => {
                if position + 1 != self.order.len() {
                    let known = self.order.remove(position);
                    self.order.push(known);
                    self.dirty = true;
                }
            }
            None => {
                self.order.push(uri.to_string());
                self.dirty = true;
            }
        }
        merged
    }

    /// Drops the grant and its recency entry, marking the history dirty.
    fn ungrant(&mut self, uri: &str) {
        if let Some(position) = self.order.iter().position(|known| known == uri) {
            self.order.remove(position);
            self.dirty = true;
        }
        if self
            .policies
            .as_mut()
            .and_then(|policies| policies.remove(uri))
            .is_some()
        {
            self.dirty = true;
        }
    }

    /// Removes the least recently used grant; returns it for revocation.
    fn pop_oldest(&mut self) -> Option<(String, u32)> {
        let uri = self.order.first()?.clone();
        let mode = self.policies.as_ref()?.get(&uri).copied()?;
        self.ungrant(&uri);
        Some((uri, mode))
    }
}

struct PolicyFailure {
    uri: String,
    message: String,
}

fn persist_batch(policies: &[PolicyInfo]) -> Result<Vec<PolicyFailure>> {
    Ok(ohos_fileshare_binding::persist_permission(policies)?
        .into_iter()
        .map(|error| PolicyFailure {
            message: format!("{error:?}"),
            uri: error.uri,
        })
        .collect())
}

fn check_batch(policies: &[PolicyInfo]) -> Result<Vec<bool>> {
    Ok(ohos_fileshare_binding::check_persistent_permission(
        policies,
    )?)
}

fn activate_batch(policies: &[PolicyInfo]) -> Result<Vec<PolicyFailure>> {
    Ok(ohos_fileshare_binding::activate_permission(policies)?
        .into_iter()
        .map(|error| PolicyFailure {
            message: format!("{error:?}"),
            uri: error.uri,
        })
        .collect())
}

fn revoke_batch(policies: &[PolicyInfo]) -> Result<Vec<PolicyFailure>> {
    Ok(ohos_fileshare_binding::revoke_permission(policies)?
        .into_iter()
        .map(|error| PolicyFailure {
            message: format!("{error:?}"),
            uri: error.uri,
        })
        .collect())
}

/// Whole-call failures that cannot recover within this process. A missing
/// `ohos.permission.FILE_ACCESS_PERSIST` ACL entry makes every fileshare call
/// return "permission verification failed"; devices without the capability
/// would fail the ArkTS `canIUse` check for FolderAuthorization.
fn unavailable_reason(error: &FileShareError) -> Option<&'static str> {
    match error {
        FileShareError::PermissionError => Some(
            "ohos.permission.FILE_ACCESS_PERSIST is not granted; re-sign the HAP with it in the profile ACL (scripts/sign-hap.sh) and reinstall",
        ),
        FileShareError::DeviceNotSupported => Some(
            "the device does not support FolderAuthorization (SystemCapability.FileManagement.AppFileService.FolderAuthorization)",
        ),
        _ => None,
    }
}

impl FileAccess {
    pub(crate) fn new(base_path: Option<String>, enabled: bool) -> Self {
        Self {
            enabled,
            path: base_path
                .filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join("gpui-ohos-file-access.json")),
            presented: AtomicBool::new(false),
            state: Mutex::new(FileAccessState::default()),
        }
    }

    /// Only the first successful presentation schedules work; never lock on the render thread.
    pub(crate) fn on_presentation(&self, presentation: Presentation) -> bool {
        self.enabled
            && presentation == Presentation::Presented
            && !self.presented.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn restore(&self) -> Result<()> {
        if !self.enabled || !self.presented.load(Ordering::Acquire) {
            return Ok(());
        }
        self.process(&mut self.state.lock(), Vec::new())
    }

    pub(crate) fn remember(&self, uris: &[String], writable: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut state = self.state.lock();
        let mut failures = Vec::new();
        for uri in uris {
            if !valid_uri(uri) {
                failures.push(format!("Invalid or oversized file grant URI: {uri}"));
                continue;
            }
            if !state.pending.contains_key(uri) && state.pending.len() == MAX_GRANTS {
                failures.push(format!(
                    "File grant queue is full ({MAX_GRANTS}); remaining selections are not retained"
                ));
                break;
            }
            let mode = if writable { READ_WRITE } else { READ };
            state
                .pending
                .entry(uri.clone())
                .and_modify(|previous| *previous |= mode)
                .or_insert(mode);
        }
        if self.presented.load(Ordering::Acquire) {
            self.process(&mut state, failures)
        } else {
            report(failures)
        }
    }

    fn process(&self, state: &mut FileAccessState, mut failures: Vec<String>) -> Result<()> {
        if state.unavailable.is_some() {
            return report(failures);
        }
        // A read failure preserves both the file and the bounded pending selection.
        if state.policies.is_none() {
            let (policies, order) = self.read_history()?;
            state.policies = Some(policies);
            state.order = order;
        }
        self.restore_state(state, &mut failures);
        let pending = std::mem::take(&mut state.pending);
        let mut requests = Vec::new();
        let mut evictions = Vec::new();
        let mut count = state.policies.as_ref().expect("history loaded above").len();
        for (uri, operation_mode) in pending {
            if !state.contains_grant(&uri) {
                // Evict the least recently used grant until there is room.
                // Evicted grants leave the history even when their revocation
                // fails, because a failed revocation usually means the file
                // or its grant is already gone.
                while count >= MAX_GRANTS {
                    match state.pop_oldest() {
                        Some((oldest_uri, oldest_mode)) => {
                            evictions.push(PolicyInfo {
                                uri: oldest_uri,
                                operation_mode: oldest_mode,
                            });
                            count -= 1;
                        }
                        None => break,
                    }
                }
                if count >= MAX_GRANTS {
                    failures.push(format!("File grant history is full ({MAX_GRANTS}): {uri}"));
                    continue;
                }
                count += 1;
            }
            requests.push(PolicyInfo {
                uri,
                operation_mode,
            });
        }
        if !evictions.is_empty() {
            let revoked = apply_batches("revoke", &evictions, state, &mut failures, revoke_batch);
            // The default log filter keeps gpui-prefixed targets at warn, so
            // use warn to make evictions observable in luban.log.
            log::warn!(
                "File grant history is full; evicted {} oldest grant(s), revoked {}",
                evictions.len(),
                revoked.len()
            );
        }
        let mut retained = apply_batches("persist", &requests, state, &mut failures, persist_batch);
        // Save dialogs may return providers that grant only reading.
        let read_fallbacks = requests
            .iter()
            .filter(|policy| {
                policy.operation_mode == READ_WRITE
                    && !retained.iter().any(|saved| saved.uri == policy.uri)
            })
            .map(|policy| PolicyInfo {
                uri: policy.uri.clone(),
                operation_mode: READ,
            })
            .collect::<Vec<_>>();
        retained.extend(apply_batches(
            "persist read-only",
            &read_fallbacks,
            state,
            &mut failures,
            persist_batch,
        ));
        for policy in &retained {
            state.touch_grant(&policy.uri, policy.operation_mode);
        }
        let activated = apply_batches("activate", &retained, state, &mut failures, activate_batch);
        state.restored &= activated.len() == retained.len();
        if let Err(error) = self.save_state(state) {
            failures.push(format!("Cannot save file access history: {error:#}"));
        }
        report(failures)
    }

    fn restore_state(&self, state: &mut FileAccessState, failures: &mut Vec<String>) {
        if state.restored {
            return;
        }
        let policies = state.policies.as_ref().expect("history loaded above");
        let mut to_check = policies
            .iter()
            .map(|(uri, mode)| PolicyInfo {
                uri: uri.clone(),
                operation_mode: *mode,
            })
            .collect::<Vec<_>>();
        let initial_failures = failures.len();
        let mut active = Vec::new();
        // The second pass checks READ only for revoked writable grants.
        for _ in 0..2 {
            let mut fallback = Vec::new();
            for batch in to_check.chunks(BATCH_SIZE) {
                match check_batch(batch) {
                    Ok(results) if results.len() == batch.len() => {
                        for (policy, granted) in batch.iter().zip(results) {
                            if granted {
                                // The check result is the mode the system
                                // actually holds; store it verbatim instead
                                // of merging it into the history mode.
                                state.set_mode(&policy.uri, policy.operation_mode);
                                active.push(policy.clone());
                            } else if policy.operation_mode == READ_WRITE {
                                fallback.push(PolicyInfo {
                                    uri: policy.uri.clone(),
                                    operation_mode: READ,
                                });
                            } else {
                                state.ungrant(&policy.uri);
                            }
                        }
                    }
                    Ok(results) => failures.push(format!(
                        "File permission query returned {} results for {} policies",
                        results.len(),
                        batch.len()
                    )),
                    Err(error) => {
                        if let Some(reason) = error
                            .downcast_ref::<FileShareError>()
                            .and_then(unavailable_reason)
                        {
                            state.unavailable = Some(reason);
                            log::error!(
                                "File grant persistence is disabled for this session: {reason}"
                            );
                            return;
                        }
                        failures.push(format!("Cannot check file grants: {error:#}"));
                    }
                }
            }
            to_check = fallback;
        }
        apply_batches("restore", &active, state, failures, activate_batch);
        state.restored = failures.len() == initial_failures;
    }

    fn read_history(&self) -> Result<(BTreeMap<String, u32>, Vec<String>)> {
        let path = self
            .path
            .as_ref()
            .context("No private file access directory")?;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((BTreeMap::new(), Vec::new()));
            }
            Err(error) => return Err(error).context("Read file access history"),
        };
        let mut contents = Vec::new();
        file.take(MAX_HISTORY_BYTES + 1)
            .read_to_end(&mut contents)?;
        anyhow::ensure!(
            contents.len() as u64 <= MAX_HISTORY_BYTES,
            "File access history exceeds {MAX_HISTORY_BYTES} bytes"
        );
        let stored: StoredGrants = serde_json::from_slice(&contents)
            .with_context(|| format!("Read file access history {}", path.display()))?;
        let mut policies = BTreeMap::new();
        let mut order = Vec::new();
        let mut load = |uri: String, mode: u32| -> Result<()> {
            anyhow::ensure!(
                valid_uri(&uri) && matches!(mode, READ | READ_WRITE),
                "Invalid file grant for {uri}"
            );
            let slot = policies.entry(uri.clone()).or_insert(0);
            *slot |= mode;
            if !order.iter().any(|known| known == &uri) {
                order.push(uri);
            }
            Ok(())
        };
        match stored {
            StoredGrants::Ordered(pairs) => {
                anyhow::ensure!(
                    pairs.len() <= MAX_GRANTS,
                    "File access history exceeds {MAX_GRANTS} grants"
                );
                for (uri, mode) in pairs {
                    load(uri, mode)?;
                }
            }
            StoredGrants::Mapped(map) => {
                anyhow::ensure!(
                    map.len() <= MAX_GRANTS,
                    "File access history exceeds {MAX_GRANTS} grants"
                );
                for (uri, mode) in map {
                    load(uri, mode)?;
                }
            }
        }
        Ok((policies, order))
    }

    fn save_state(&self, state: &mut FileAccessState) -> Result<()> {
        if !state.dirty {
            return Ok(());
        }
        let policies = state
            .policies
            .as_ref()
            .context("File history unavailable")?;
        anyhow::ensure!(policies.len() <= MAX_GRANTS, "File grant history is full");
        // v2: ordered [uri, mode] pairs, so restarts know which grant to
        // evict first. Older releases stored a URI-keyed map.
        let mut ordered: Vec<(&String, &u32)> = state
            .order
            .iter()
            .filter_map(|uri| policies.get_key_value(uri))
            .collect();
        if ordered.len() != policies.len() {
            // The recency order lost a key; keep the grant instead of
            // silently dropping it from the history.
            for (uri, mode) in policies {
                if !ordered.iter().any(|(known, _)| *known == uri) {
                    ordered.push((uri, mode));
                }
            }
        }
        let contents = serde_json::to_vec(&ordered)?;
        anyhow::ensure!(
            contents.len() as u64 <= MAX_HISTORY_BYTES,
            "File access history exceeds {MAX_HISTORY_BYTES} bytes"
        );
        let path = self
            .path
            .as_ref()
            .context("No private file access directory")?;
        std::fs::create_dir_all(path.parent().context("File history has no parent")?)?;
        let temporary_path = path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary_path)
            .context("Write file access history")?;
        file.write_all(&contents)?;
        file.sync_all()?;
        std::fs::rename(temporary_path, path).context("Replace file access history")?;
        state.dirty = false;
        Ok(())
    }
}

/// History format v2: ordered `[uri, mode]` pairs. Older releases stored a
/// URI-keyed map, which cannot express recency.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredGrants {
    Ordered(Vec<(String, u32)>),
    Mapped(BTreeMap<String, u32>),
}

fn valid_uri(uri: &str) -> bool {
    uri.starts_with("file://") && uri.len() <= MAX_URI_BYTES && !uri.contains('\0')
}

fn report(failures: Vec<String>) -> Result<()> {
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

fn apply_batches(
    operation: &str,
    policies: &[PolicyInfo],
    state: &mut FileAccessState,
    failures: &mut Vec<String>,
    call: impl Fn(&[PolicyInfo]) -> Result<Vec<PolicyFailure>>,
) -> Vec<PolicyInfo> {
    if state.unavailable.is_some() {
        return Vec::new();
    }
    let mut successful = Vec::new();
    for batch in policies.chunks(BATCH_SIZE) {
        match call(batch) {
            Ok(errors) => {
                if errors
                    .iter()
                    .any(|error| !batch.iter().any(|policy| policy.uri == error.uri))
                {
                    failures.push(format!(
                        "Invalid file permission {operation} failure response"
                    ));
                    continue;
                }
                successful.extend(
                    batch
                        .iter()
                        .filter(|policy| !errors.iter().any(|error| error.uri == policy.uri))
                        .cloned(),
                );
                for error in errors {
                    failures.push(format!(
                        "Cannot {operation} {}: {}",
                        error.uri, error.message
                    ));
                }
            }
            Err(error) => {
                if let Some(reason) = error
                    .downcast_ref::<FileShareError>()
                    .and_then(unavailable_reason)
                {
                    state.unavailable = Some(reason);
                    log::error!("File grant persistence is disabled for this session: {reason}");
                    return successful;
                }
                failures.push(format!("Cannot {operation} file grants: {error:#}"));
            }
        }
    }
    successful
}
