use anyhow::{Context as _, Result};
use ohos_fileshare_binding::PolicyInfo;
use parking_lot::Mutex;
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
    policies: Option<BTreeMap<String, u32>>,
    pending: BTreeMap<String, u32>,
    restored: bool,
    dirty: bool,
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
        // A read failure preserves both the file and the bounded pending selection.
        if state.policies.is_none() {
            state.policies = Some(self.read_history()?);
        }
        self.restore_state(state, &mut failures);
        let pending = std::mem::take(&mut state.pending);
        let policies = state.policies.as_mut().expect("history loaded above");
        let mut requests = Vec::new();
        let mut count = policies.len();
        for (uri, operation_mode) in pending {
            if !policies.contains_key(&uri) {
                if count == MAX_GRANTS {
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
        let mut retained = apply_batches("persist", &requests, &mut failures, persist_batch);
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
            &mut failures,
            persist_batch,
        ));
        for policy in &retained {
            let mode = policies.get(&policy.uri).copied().unwrap_or(0) | policy.operation_mode;
            let previous = policies.insert(policy.uri.clone(), mode);
            state.dirty |= previous != Some(mode);
        }
        let activated = apply_batches("activate", &retained, &mut failures, activate_batch);
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
        let policies = state.policies.as_mut().expect("history loaded above");
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
                                state.dirty |= policies
                                    .insert(policy.uri.clone(), policy.operation_mode)
                                    != Some(policy.operation_mode);
                                active.push(policy.clone());
                            } else if policy.operation_mode == READ_WRITE {
                                fallback.push(PolicyInfo {
                                    uri: policy.uri.clone(),
                                    operation_mode: READ,
                                });
                            } else {
                                state.dirty |= policies.remove(&policy.uri).is_some();
                            }
                        }
                    }
                    Ok(results) => failures.push(format!(
                        "File permission query returned {} results for {} policies",
                        results.len(),
                        batch.len()
                    )),
                    Err(error) => failures.push(format!("Cannot check file grants: {error:#}")),
                }
            }
            to_check = fallback;
        }
        apply_batches("restore", &active, failures, activate_batch);
        state.restored = failures.len() == initial_failures;
    }

    fn read_history(&self) -> Result<BTreeMap<String, u32>> {
        let path = self
            .path
            .as_ref()
            .context("No private file access directory")?;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
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
        let policies: BTreeMap<String, u32> = serde_json::from_slice(&contents)
            .with_context(|| format!("Read file access history {}", path.display()))?;
        anyhow::ensure!(
            policies.len() <= MAX_GRANTS
                && policies
                    .iter()
                    .all(|(uri, mode)| valid_uri(uri) && matches!(*mode, READ | READ_WRITE)),
            "Invalid or excessive file grants in {}",
            path.display()
        );
        Ok(policies)
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
        let contents = serde_json::to_vec(policies)?;
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
    failures: &mut Vec<String>,
    call: impl Fn(&[PolicyInfo]) -> Result<Vec<PolicyFailure>>,
) -> Vec<PolicyInfo> {
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
            Err(error) => failures.push(format!("Cannot {operation} file grants: {error:#}")),
        }
    }
    successful
}
