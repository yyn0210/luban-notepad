//! Single-instance IPC for CLI automation.
//!
//! The primary instance owns a configuration-directory lock and a local socket.
//! Later launches forward a JSON request and exit (or wait for a response when
//! `--wait` is used). `interprocess` maps the local socket to a Unix domain
//! socket on Unix and to a named pipe on Windows; both are user-local endpoints.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions, Name, Stream};
use serde::{Deserialize, Serialize};

const LOCK_FILE: &str = "instance.lock";
const SOCKET_FILE: &str = "instance.sock";
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GotoRequest {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub id: String,
    pub files: Vec<PathBuf>,
    pub goto: Option<GotoRequest>,
    pub diff: Option<(PathBuf, PathBuf)>,
    pub stdin: Option<String>,
    pub command: Option<String>,
    pub new_window: bool,
    pub wait: bool,
}

impl LaunchRequest {
    pub fn new(
        files: Vec<PathBuf>,
        goto: Option<GotoRequest>,
        diff: Option<(PathBuf, PathBuf)>,
        stdin: Option<String>,
        command: Option<String>,
        new_window: bool,
        wait: bool,
    ) -> Self {
        let id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        );
        Self {
            id,
            files,
            goto,
            diff,
            stdin,
            command,
            new_window,
            wait,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: String,
    pub ok: bool,
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn ok(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            error: None,
        }
    }

    pub fn error(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            error: Some(error.into()),
        }
    }
}

pub struct IpcCommand {
    pub request: LaunchRequest,
    pub reply: Sender<IpcResponse>,
}

pub type IpcQueue = Arc<Mutex<VecDeque<IpcCommand>>>;

static SHARED_QUEUE: OnceLock<IpcQueue> = OnceLock::new();

pub fn set_shared_queue(queue: IpcQueue) {
    let _ = SHARED_QUEUE.set(queue);
}

pub fn shared_queue() -> Option<IpcQueue> {
    SHARED_QUEUE.get().cloned()
}

pub struct IpcBridge {
    pub queue: IpcQueue,
    pub wake_rx: Receiver<()>,
}

pub struct IpcServer {
    lock_path: PathBuf,
    socket_path: PathBuf,
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.lock_path);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
}

#[derive(Debug)]
pub enum ForwardError {
    /// No live primary instance was found; the caller should start one.
    NoPrimary,
    Failed(String),
}

pub fn lock_path(config_dir: &Path) -> PathBuf {
    config_dir.join(LOCK_FILE)
}

pub fn socket_path(config_dir: &Path) -> PathBuf {
    config_dir.join(SOCKET_FILE)
}

/// Local endpoint name for this configuration directory.
///
/// Unix uses a filesystem Unix-domain socket; Windows uses a named pipe whose
/// name is derived from the same configuration directory.
fn endpoint_name(config_dir: &Path) -> Result<Name<'static>, String> {
    #[cfg(unix)]
    {
        let text: &'static str = Box::leak(
            socket_path(config_dir)
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        text.to_fs_name::<GenericFilePath>()
            .map_err(|error| format!("invalid IPC socket path: {error}"))
    }
    #[cfg(not(unix))]
    {
        let canonical = config_dir
            .canonicalize()
            .unwrap_or_else(|_| config_dir.to_path_buf());
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in canonical.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        let text: &'static str =
            Box::leak(format!(r"\\.\pipe\luban-notepad-{hash:016x}").into_boxed_str());
        text.to_fs_name::<GenericFilePath>()
            .map_err(|error| format!("invalid IPC pipe name: {error}"))
    }
}

/// Move a request to the running primary instance.
pub fn forward(config_dir: &Path, request: &LaunchRequest) -> Result<IpcResponse, ForwardError> {
    let lock = match fs::read_to_string(lock_path(config_dir)) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ForwardError::NoPrimary)
        }
        Err(error) => return Err(ForwardError::Failed(error.to_string())),
    };
    let info: LockInfo = serde_json::from_str(&lock)
        .map_err(|error| ForwardError::Failed(format!("invalid instance lock: {error}")))?;
    if !process_alive(info.pid) {
        cleanup(config_dir);
        return Err(ForwardError::NoPrimary);
    }
    if request
        .stdin
        .as_ref()
        .is_some_and(|text| text.len() > 16 * 1024 * 1024)
    {
        return Err(ForwardError::Failed(
            "standard input is larger than 16 MiB; start a new process with --new-window"
                .to_string(),
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match send_request(config_dir, request) {
            Ok(response) => return Ok(response),
            Err(error) => {
                if !process_alive(info.pid) {
                    cleanup(config_dir);
                    return Err(ForwardError::NoPrimary);
                }
                // The main instance may still be binding its endpoint; retry
                // connection failures briefly. Do not retry a request that may
                // already have been delivered.
                if !error.contains("cannot connect") || Instant::now() >= deadline {
                    return Err(ForwardError::Failed(error));
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Start the primary endpoint. Fails when another live instance owns the lock.
pub fn start(config_dir: &Path) -> Result<(IpcServer, IpcBridge), String> {
    fs::create_dir_all(config_dir)
        .map_err(|error| format!("cannot create {}: {error}", config_dir.display()))?;
    let lock_path = lock_path(config_dir);
    let socket_path = socket_path(config_dir);
    let lock = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let alive = fs::read_to_string(&lock_path)
                .ok()
                .and_then(|text| serde_json::from_str::<LockInfo>(&text).ok())
                .is_some_and(|info| process_alive(info.pid));
            if alive {
                return Err("another Luban Notepad instance is already running".to_string());
            }
            cleanup(config_dir);
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .map_err(|error| format!("cannot acquire instance lock: {error}"))?
        }
        Err(error) => return Err(format!("cannot acquire instance lock: {error}")),
    };
    drop(lock);
    // Publish the PID before binding so a concurrent start waits for the
    // endpoint instead of stealing the lock while it is still empty.
    if let Err(error) = write_lock(&lock_path) {
        let _ = fs::remove_file(&lock_path);
        return Err(error);
    }
    let name = match endpoint_name(config_dir) {
        Ok(name) => name,
        Err(error) => {
            cleanup(config_dir);
            return Err(error);
        }
    };
    #[cfg(unix)]
    {
        let _ = fs::remove_file(&socket_path);
    }
    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(listener) => listener,
        Err(error) => {
            cleanup(config_dir);
            return Err(format!("cannot bind IPC endpoint: {error}"));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
    }
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let (wake_tx, wake_rx) = mpsc::channel();
    let queue_for_thread = queue.clone();
    thread::Builder::new()
        .name("luban-ipc".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let queue = queue_for_thread.clone();
                let wake = wake_tx.clone();
                thread::spawn(move || serve_connection(stream, queue, wake));
            }
        })
        .map_err(|error| format!("cannot start IPC thread: {error}"))?;
    Ok((
        IpcServer {
            lock_path,
            socket_path,
        },
        IpcBridge { queue, wake_rx },
    ))
}

fn serve_connection(mut stream: Stream, queue: Arc<Mutex<VecDeque<IpcCommand>>>, wake: Sender<()>) {
    let line = match read_request_line(&mut stream) {
        Ok(line) if !line.is_empty() && line.len() <= MAX_REQUEST_BYTES => line,
        _ => {
            let response = IpcResponse::error("", "invalid IPC request");
            let _ = write_response(&mut stream, &response);
            return;
        }
    };
    let request: LaunchRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(error) => {
            let response = IpcResponse::error("", format!("invalid IPC request: {error}"));
            let _ = write_response(&mut stream, &response);
            return;
        }
    };
    let (reply, reply_rx) = mpsc::channel();
    queue
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push_back(IpcCommand {
            request: request.clone(),
            reply,
        });
    let _ = wake.send(());
    let response = reply_rx
        .recv()
        .unwrap_or_else(|_| IpcResponse::error(request.id.clone(), "application closed"));
    let _ = write_response(&mut stream, &response);
}

fn read_request_line(stream: &mut impl Read) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            break;
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() > MAX_REQUEST_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

fn write_lock(path: &Path) -> Result<(), String> {
    let json = serde_json::to_vec(&LockInfo {
        pid: std::process::id(),
    })
    .map_err(|error| error.to_string())?;
    fs::write(path, json).map_err(|error| format!("cannot write instance lock: {error}"))
}

fn write_response(stream: &mut impl Write, response: &IpcResponse) -> std::io::Result<()> {
    let mut json = serde_json::to_vec(response).unwrap_or_default();
    json.push(b'\n');
    stream.write_all(&json)?;
    stream.flush()
}

fn send_request(config_dir: &Path, request: &LaunchRequest) -> Result<IpcResponse, String> {
    let name = endpoint_name(config_dir)?;
    let mut stream = Stream::connect(name)
        .map_err(|error| format!("cannot connect to the running instance: {error}"))?;
    exchange(&mut stream, request)
}

fn exchange(
    stream: &mut (impl Read + Write),
    request: &LaunchRequest,
) -> Result<IpcResponse, String> {
    let mut json = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    json.push(b'\n');
    stream
        .write_all(&json)
        .map_err(|error| format!("cannot send IPC request: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("cannot flush IPC request: {error}"))?;
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream
            .read(&mut byte)
            .map_err(|error| format!("cannot read IPC response: {error}"))?;
        if read == 0 {
            break;
        }
        response.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if response.len() > MAX_REQUEST_BYTES {
            return Err("IPC response is too large".to_string());
        }
    }
    if response.is_empty() {
        return Err("the running instance closed the connection".to_string());
    }
    serde_json::from_slice(&response).map_err(|error| format!("invalid IPC response: {error}"))
}

pub fn path_is_readable(path: &Path) -> bool {
    path.is_file() && fs::File::open(path).is_ok()
}

pub fn cleanup(config_dir: &Path) {
    let _ = fs::remove_file(lock_path(config_dir));
    let _ = fs::remove_file(socket_path(config_dir));
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
fn process_alive(pid: u32) -> bool {
    pid != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_and_has_unique_ids() {
        let first = LaunchRequest::new(
            vec![PathBuf::from("a.txt")],
            None,
            None,
            Some("hello".into()),
            None,
            false,
            true,
        );
        let second = LaunchRequest::new(Vec::new(), None, None, None, None, false, false);
        assert_ne!(first.id, second.id);
        let json = serde_json::to_string(&first).unwrap();
        assert!(!json.contains('\n'));
        let decoded: LaunchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stdin.as_deref(), Some("hello"));
        assert!(decoded.wait);
    }

    #[test]
    fn responses_carry_identity_and_errors() {
        let ok = IpcResponse::ok("7");
        assert!(ok.ok);
        let error = IpcResponse::error("7", "boom");
        assert!(!error.ok);
        assert_eq!(error.id, "7");
    }
}
