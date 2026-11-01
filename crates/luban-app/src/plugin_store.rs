//! Plugin store client.
//!
//! Catalog fetches and artifact downloads run on background threads. The UI
//! polls the job channel from the existing idle timer, so no network or disk
//! work blocks rendering. Only validated bytes are handed to the installer.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::component::input::InputState;
use gpui_kit::*;
use luban_plugin::store::{
    self, Artifact, Catalog, CatalogCacheMeta, CACHE_SCHEMA, MAX_ARTIFACT_BYTES, MAX_CATALOG_BYTES,
};
use ureq::ResponseExt;

use crate::app::LubanApp;
use crate::i18n::{tr, tr_args};

/// Directory under the plugin root used for catalog caches and downloads.
pub const STORE_DIR: &str = ".store";
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct CatalogPayload {
    pub catalog: Catalog,
    pub warnings: Vec<String>,
    pub bytes: Vec<u8>,
    pub fetched_at: u64,
}

struct CachedCatalog {
    catalog: Catalog,
    warnings: Vec<String>,
    fetched_at: u64,
}

/// Work handed from a background thread back to the UI thread.
enum StoreOutcome {
    Catalog {
        source: String,
        payload: Result<CatalogPayload, String>,
    },
    Downloaded {
        plugin_id: String,
        version: String,
        path: Result<PathBuf, String>,
    },
}

#[derive(Clone, Debug)]
pub struct PendingInstall {
    pub plugin_id: String,
    pub version: String,
    pub replace: bool,
    pub catalog: String,
}

/// A finished background job the UI still has to act on.
pub enum StoreEvent {
    CatalogLoaded {
        offline: bool,
    },
    CatalogFailed(String),
    DownloadReady {
        install: PendingInstall,
        path: PathBuf,
    },
    DownloadFailed {
        plugin_id: String,
        message: String,
    },
}

struct StoreJob {
    outcome: Receiver<StoreOutcome>,
}

/// Store tab state, owned by the plugin manager.
pub struct PluginStoreState {
    pub catalog: Option<Catalog>,
    pub source: Option<String>,
    pub fetched_at: Option<u64>,
    pub offline: bool,
    pub warnings: Vec<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub message: Option<String>,
    pub search: Entity<InputState>,
    pub url_input: Entity<InputState>,
    pub selected: Option<String>,
    pub installing: Option<String>,
    pub install_error: Option<String>,
    pub url_error: Option<String>,
    job: Option<StoreJob>,
    pending_install: Option<PendingInstall>,
}

impl PluginStoreState {
    pub fn new(window: &mut Window, cx: &mut Context<LubanApp>, url: &str) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr("plugins.store.search_placeholder"))
        });
        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(url)
                .placeholder(tr("plugins.store.url_placeholder"))
        });
        Self {
            catalog: None,
            source: None,
            fetched_at: None,
            offline: false,
            warnings: Vec::new(),
            loading: false,
            error: None,
            message: None,
            search,
            url_input,
            selected: None,
            installing: None,
            install_error: None,
            url_error: None,
            job: None,
            pending_install: None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.job.is_some() || self.installing.is_some()
    }

    pub fn source_matches(&self, source: &str) -> bool {
        self.source.as_deref() == Some(source)
    }

    pub fn clear_catalog(&mut self) {
        self.catalog = None;
        self.source = None;
        self.fetched_at = None;
        self.offline = false;
        self.warnings.clear();
        self.selected = None;
    }

    /// Fetch a catalog in the background. Refuses to start while another job
    /// is running so two writers never race for the same cache file.
    pub fn begin_fetch(&mut self, source: String) -> Result<(), String> {
        if self.is_busy() {
            return Err(tr("plugins.store.busy"));
        }
        let (sender, receiver) = mpsc::channel();
        let thread_source = source.clone();
        std::thread::spawn(move || {
            let payload = fetch_catalog(&thread_source);
            let _ = sender.send(StoreOutcome::Catalog {
                source: thread_source,
                payload,
            });
        });
        self.job = Some(StoreJob { outcome: receiver });
        self.loading = true;
        self.error = None;
        self.message = None;
        self.install_error = None;
        Ok(())
    }

    /// Download and verify one artifact in the background.
    pub fn begin_download(
        &mut self,
        root: &Path,
        plugin_id: &str,
        version: &str,
        artifact: &Artifact,
        replace: bool,
        catalog: &str,
    ) -> Result<(), String> {
        if self.is_busy() {
            return Err(tr("plugins.store.busy"));
        }
        let (sender, receiver) = mpsc::channel();
        let root = root.to_path_buf();
        let plugin_id = plugin_id.to_string();
        let version = version.to_string();
        let artifact = artifact.clone();
        let thread_id = plugin_id.clone();
        let thread_version = version.clone();
        std::thread::spawn(move || {
            let path = download_artifact(&root, &thread_id, &thread_version, &artifact);
            let _ = sender.send(StoreOutcome::Downloaded {
                plugin_id: thread_id,
                version: thread_version,
                path,
            });
        });
        self.pending_install = Some(PendingInstall {
            plugin_id: plugin_id.clone(),
            version,
            replace,
            catalog: catalog.to_string(),
        });
        self.installing = Some(plugin_id);
        self.install_error = None;
        self.message = None;
        self.job = Some(StoreJob { outcome: receiver });
        Ok(())
    }

    /// Check for a finished job and fold catalog results into the view state.
    /// Download results are returned for the caller to install.
    pub fn poll(&mut self, root: &Path) -> Option<StoreEvent> {
        let outcome = match self.job.as_ref()?.outcome.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                self.loading = false;
                let error = tr("plugins.store.worker_failed");
                if let Some(plugin_id) = self.installing.take() {
                    self.pending_install = None;
                    return Some(StoreEvent::DownloadFailed {
                        plugin_id,
                        message: error,
                    });
                }
                return Some(StoreEvent::CatalogFailed(error));
            }
        };
        self.job = None;
        match outcome {
            StoreOutcome::Catalog { source, payload } => match payload {
                Ok(payload) => {
                    if let Err(error) = write_cache(root, &source, &payload) {
                        log::warn!("cannot cache plugin store catalog: {error}");
                    }
                    self.loading = false;
                    self.offline = false;
                    self.error = None;
                    self.fetched_at = Some(payload.fetched_at);
                    self.warnings = payload.warnings;
                    self.catalog = Some(payload.catalog);
                    self.source = Some(source);
                    self.ensure_selection();
                    Some(StoreEvent::CatalogLoaded { offline: false })
                }
                Err(error) => {
                    self.apply_catalog_failure(root, &source, error.clone());
                    Some(StoreEvent::CatalogFailed(error))
                }
            },
            StoreOutcome::Downloaded {
                plugin_id,
                version,
                path,
            } => {
                let Some(install) = self.pending_install.take() else {
                    self.installing = None;
                    return None;
                };
                match path {
                    Ok(path) => {
                        if install.plugin_id != plugin_id || install.version != version {
                            self.installing = None;
                            return Some(StoreEvent::DownloadFailed {
                                plugin_id,
                                message: tr("plugins.store.install_mismatch"),
                            });
                        }
                        Some(StoreEvent::DownloadReady { install, path })
                    }
                    Err(message) => {
                        self.installing = None;
                        Some(StoreEvent::DownloadFailed { plugin_id, message })
                    }
                }
            }
        }
    }

    /// Record the outcome of the UI-thread install step.
    pub fn finish_install(&mut self, result: Result<String, String>) {
        self.installing = None;
        self.pending_install = None;
        match result {
            Ok(message) => {
                self.message = Some(message);
                self.install_error = None;
            }
            Err(error) => {
                self.install_error = Some(error);
                self.message = None;
            }
        }
    }

    fn apply_catalog_failure(&mut self, root: &Path, source: &str, error: String) {
        self.loading = false;
        let display = localize_error(StoreErrorContext::Catalog, &error);
        self.error = Some(display.clone());
        if let Some(cached) = load_cache(root, source) {
            self.catalog = Some(cached.catalog);
            self.warnings = cached.warnings;
            self.fetched_at = Some(cached.fetched_at);
            self.source = Some(source.to_string());
            self.offline = true;
        } else if self.source.as_deref() == Some(source) && self.catalog.is_some() {
            self.offline = true;
        } else {
            self.clear_catalog();
            self.source = Some(source.to_string());
            self.error = Some(display);
        }
        self.ensure_selection();
    }

    fn ensure_selection(&mut self) {
        let still_listed = self
            .selected
            .as_deref()
            .and_then(|id| self.catalog.as_ref()?.find(id))
            .is_some();
        if !still_listed {
            self.selected = self
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.plugins.first())
                .map(|plugin| plugin.id.clone());
        }
    }
}

/// Where a store error came from, so the localized lead-in matches the action
/// the user was performing.
#[derive(Clone, Copy)]
pub enum StoreErrorContext {
    Catalog,
    Download,
    Install,
}

/// Localized error for a catalog URL that failed validation.
pub fn localize_url_error(source: &str) -> String {
    if source.trim().is_empty() {
        tr("plugins.store.url_empty")
    } else {
        tr("plugins.store.url_invalid")
    }
}

/// Localize a store error for display. Known network and catalog-version
/// errors get a complete translation; anything else keeps its technical detail
/// after a localized lead-in so diagnostics are not lost.
pub fn localize_error(context: StoreErrorContext, error: &str) -> String {
    let text = error.trim();
    if text.is_empty() {
        return tr("plugins.store.unknown_error");
    }
    if let Some(rest) = text.strip_prefix("unsupported catalog_version ") {
        if let Some((version, _)) = rest.split_once(';') {
            return tr_args(
                "plugins.store.unsupported_catalog_version",
                &[("version", version.trim().to_string())],
            );
        }
    }
    if let Some(rest) = text.strip_prefix("cannot fetch ") {
        if let Some((url, detail)) = rest.split_once(": ") {
            return tr_args(
                "plugins.store.error_fetch",
                &[
                    ("url", url.to_string()),
                    ("detail", localize_network_detail(detail)),
                ],
            );
        }
    }
    if let Some(rest) = text.strip_prefix("cannot download ") {
        if let Some((url, detail)) = rest.split_once(": ") {
            if url != "package" {
                return tr_args(
                    "plugins.store.error_download_url",
                    &[
                        ("url", url.to_string()),
                        ("detail", localize_network_detail(detail)),
                    ],
                );
            }
        }
    }
    if let Some((url, status)) = text.rsplit_once(" returned HTTP ") {
        return tr_args(
            "plugins.store.error_http",
            &[("url", url.to_string()), ("status", status.to_string())],
        );
    }
    if let Some(url) = text.strip_prefix("redirected to a non-HTTPS URL: ") {
        return tr_args("plugins.store.error_redirect", &[("url", url.to_string())]);
    }
    let key = match context {
        StoreErrorContext::Catalog => "plugins.store.error_catalog",
        StoreErrorContext::Download => "plugins.store.error_download",
        StoreErrorContext::Install => "plugins.store.error_install",
    };
    tr_args(key, &[("detail", text.to_string())])
}

fn localize_network_detail(detail: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("connection refused") {
        tr("plugins.store.network_refused")
    } else if lower.contains("timed out") || lower.contains("timeout") {
        tr("plugins.store.network_timeout")
    } else if lower.contains("dns") || lower.contains("name or service not known") {
        tr("plugins.store.network_dns")
    } else if lower.contains("certificate") || lower.contains("tls") || lower.contains("handshake")
    {
        tr("plugins.store.network_tls")
    } else if lower.contains("connection reset") {
        tr("plugins.store.network_reset")
    } else {
        detail.to_string()
    }
}

fn download_error(detail: String) -> String {
    localize_error(StoreErrorContext::Download, &detail)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn store_dir(root: &Path) -> PathBuf {
    root.join(STORE_DIR)
}

pub fn catalog_cache_path(root: &Path) -> PathBuf {
    store_dir(root).join("catalog.json")
}

pub fn catalog_meta_path(root: &Path) -> PathBuf {
    store_dir(root).join("catalog.meta.json")
}

pub fn downloads_dir(root: &Path) -> PathBuf {
    store_dir(root).join("downloads")
}

fn write_cache(root: &Path, source: &str, payload: &CatalogPayload) -> Result<(), String> {
    let dir = store_dir(root);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let meta = CatalogCacheMeta {
        schema: CACHE_SCHEMA,
        source: source.to_string(),
        fetched_at: payload.fetched_at,
    };
    let meta_json = serde_json::to_vec_pretty(&meta)
        .map_err(|error| format!("cannot serialize cache metadata: {error}"))?;
    luban_core::atomic_write::write_atomic(&catalog_cache_path(root), &payload.bytes)
        .map_err(|error| format!("cannot write catalog cache: {error}"))?;
    luban_core::atomic_write::write_atomic(&catalog_meta_path(root), &meta_json)
        .map_err(|error| format!("cannot write catalog cache metadata: {error}"))
}

fn load_cache(root: &Path, source: &str) -> Option<CachedCatalog> {
    let meta_text = fs::read_to_string(catalog_meta_path(root)).ok()?;
    let meta: CatalogCacheMeta = serde_json::from_str(&meta_text).ok()?;
    if meta.schema != CACHE_SCHEMA || meta.source != source {
        return None;
    }
    let bytes = match fs::read(catalog_cache_path(root)) {
        Ok(bytes) => bytes,
        Err(_) => {
            remove_cache(root);
            return None;
        }
    };
    match store::parse_catalog(&bytes) {
        Ok(parsed) => Some(CachedCatalog {
            catalog: parsed.catalog,
            warnings: parsed.warnings,
            fetched_at: meta.fetched_at,
        }),
        Err(error) => {
            log::warn!("discarding invalid plugin store cache: {error}");
            remove_cache(root);
            None
        }
    }
}

fn remove_cache(root: &Path) {
    let _ = fs::remove_file(catalog_cache_path(root));
    let _ = fs::remove_file(catalog_meta_path(root));
}

fn fetch_catalog(source: &str) -> Result<CatalogPayload, String> {
    let bytes = read_source(source, MAX_CATALOG_BYTES as u64, CATALOG_TIMEOUT)?;
    let parsed = store::parse_catalog(&bytes)?;
    Ok(CatalogPayload {
        catalog: parsed.catalog,
        warnings: parsed.warnings,
        bytes,
        fetched_at: now_secs(),
    })
}

/// Read a catalog or artifact source with a hard byte limit.
fn read_source(source: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>, String> {
    if source.starts_with("https://") {
        return http_get(source, limit, timeout);
    }
    let path = local_source_path(source)
        .ok_or_else(|| tr_args("plugins.store.unsupported_url", &[("url", source.into())]))?;
    read_limited_file(&path, limit)
}

fn read_limited_file(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file =
        fs::File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if metadata.len() > limit {
        return Err(format!(
            "{} is larger than the {limit} byte limit",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "{} is larger than the {limit} byte limit",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Proxy from the usual environment variables, with two practical fixes over
/// `Proxy::try_from_env`: `HTTPS_PROXY` wins over `ALL_PROXY` (matching curl and
/// git), and plain `socks5://` proxies use remote DNS. Local DNS inside a SOCKS
/// tunnel fails with many desktop proxies, while `socks5h` is the working form.
fn env_proxy() -> Option<ureq::Proxy> {
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    let entries: Vec<&str> = no_proxy
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        let value = normalize_proxy_url(value.trim());
        if value.is_empty() {
            continue;
        }
        if let Some(proxy) = build_proxy(&value, &entries) {
            return Some(proxy);
        }
    }
    None
}

fn normalize_proxy_url(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    match value.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("socks5") => {
            format!("socks5h://{rest}")
        }
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("socks") => {
            format!("socks5h://{rest}")
        }
        Some(_) => value.to_string(),
        None => format!("http://{value}"),
    }
}

fn build_proxy(value: &str, no_proxy: &[&str]) -> Option<ureq::Proxy> {
    let uri: ureq::http::Uri = value.parse().ok()?;
    let protocol = match uri.scheme_str()? {
        "http" => ureq::ProxyProtocol::Http,
        "https" => ureq::ProxyProtocol::Https,
        "socks4" => ureq::ProxyProtocol::Socks4,
        "socks4a" => ureq::ProxyProtocol::Socks4A,
        "socks" | "socks5" => ureq::ProxyProtocol::Socks5,
        "socks5h" => ureq::ProxyProtocol::Socks5h,
        _ => return None,
    };
    let authority = uri.authority()?;
    let mut builder = ureq::Proxy::builder(protocol).host(authority.host());
    if let Some(port) = authority.port_u16() {
        builder = builder.port(port);
    }
    if let Some((userinfo, _host)) = authority.as_str().rsplit_once('@') {
        let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        if let Some(username) = percent_decode(username) {
            builder = builder.username(&username);
        }
        if !password.is_empty() {
            if let Some(password) = percent_decode(password) {
                builder = builder.password(&password);
            }
        }
    }
    for entry in no_proxy {
        builder = builder.no_proxy(entry);
    }
    builder.build().ok()
}

fn http_get(url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .max_redirects(5);
    if let Some(proxy) = env_proxy() {
        builder = builder.proxy(Some(proxy));
    }
    let agent: ureq::Agent = builder.build().into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| format!("cannot fetch {url}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{url} returned HTTP {status}"));
    }
    if response.get_uri().scheme_str() != Some("https") {
        return Err(format!(
            "redirected to a non-HTTPS URL: {}",
            response.get_uri()
        ));
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read response from {url}: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("response from {url} is larger than {limit} bytes"));
    }
    Ok(bytes)
}

/// Download, length-check and hash-check one artifact.
fn download_artifact(
    root: &Path,
    plugin_id: &str,
    version: &str,
    artifact: &Artifact,
) -> Result<PathBuf, String> {
    if artifact.size == 0 || artifact.size > MAX_ARTIFACT_BYTES {
        return Err(tr("plugins.store.artifact_too_large"));
    }
    let dir = downloads_dir(root);
    fs::create_dir_all(&dir)
        .map_err(|error| download_error(format!("cannot create {}: {error}", dir.display())))?;
    let stem = format!("{plugin_id}-{version}");
    let part = dir.join(format!("{stem}.zip.part"));
    let final_path = dir.join(format!("{stem}.zip"));
    let _ = fs::remove_file(&part);
    let result = write_source_to_file(&artifact.url, &part, artifact.size);
    if let Err(error) = result {
        let _ = fs::remove_file(&part);
        return Err(error);
    }
    let metadata = fs::metadata(&part)
        .map_err(|error| download_error(format!("cannot read {}: {error}", part.display())))?;
    if metadata.len() != artifact.size {
        let _ = fs::remove_file(&part);
        return Err(tr_args(
            "plugins.store.size_mismatch",
            &[
                ("expected", artifact.size.to_string()),
                ("actual", metadata.len().to_string()),
            ],
        ));
    }
    let hash = match store::sha256_file(&part) {
        Ok(hash) => hash,
        Err(error) => {
            let _ = fs::remove_file(&part);
            return Err(download_error(error));
        }
    };
    if hash != artifact.sha256 {
        let _ = fs::remove_file(&part);
        return Err(tr("plugins.store.hash_mismatch"));
    }
    if final_path.exists() {
        let _ = fs::remove_file(&final_path);
    }
    if let Err(error) = fs::rename(&part, &final_path) {
        let _ = fs::remove_file(&part);
        return Err(download_error(format!(
            "cannot move downloaded package: {error}"
        )));
    }
    Ok(final_path)
}

fn write_source_to_file(source: &str, target: &Path, limit: u64) -> Result<(), String> {
    if source.starts_with("https://") {
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .max_redirects(5);
        if let Some(proxy) = env_proxy() {
            builder = builder.proxy(Some(proxy));
        }
        let agent: ureq::Agent = builder.build().into();
        let mut response = agent
            .get(source)
            .call()
            .map_err(|error| download_error(format!("cannot download {source}: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(download_error(format!("{source} returned HTTP {status}")));
        }
        if response.get_uri().scheme_str() != Some("https") {
            return Err(download_error(format!(
                "redirected to a non-HTTPS URL: {}",
                response.get_uri()
            )));
        }
        copy_limited(response.body_mut().as_reader(), target, limit)
    } else {
        let path = local_source_path(source)
            .ok_or_else(|| tr_args("plugins.store.unsupported_url", &[("url", source.into())]))?;
        let file = fs::File::open(&path)
            .map_err(|error| download_error(format!("cannot read {}: {error}", path.display())))?;
        copy_limited(file, target, limit)
    }
}

fn copy_limited(reader: impl Read, target: &Path, limit: u64) -> Result<(), String> {
    let mut output = fs::File::create(target)
        .map_err(|error| download_error(format!("cannot create {}: {error}", target.display())))?;
    let copied = io::copy(&mut reader.take(limit.saturating_add(1)), &mut output)
        .map_err(|error| download_error(format!("cannot download package: {error}")))?;
    output
        .flush()
        .map_err(|error| download_error(format!("cannot write {}: {error}", target.display())))?;
    if copied > limit {
        return Err(tr("plugins.store.artifact_too_large"));
    }
    Ok(())
}

/// Local filesystem path for an absolute path or `file://` URL.
pub fn local_source_path(source: &str) -> Option<PathBuf> {
    if let Some(rest) = source.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') {
            return None;
        }
        #[cfg(windows)]
        let rest = match rest.strip_prefix('/') {
            Some(stripped) if stripped.as_bytes().get(1) == Some(&b':') => stripped,
            _ => rest,
        };
        let decoded = percent_decode(rest)?;
        return Some(PathBuf::from(decoded));
    }
    let path = Path::new(source);
    path.is_absolute().then(|| path.to_path_buf())
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            output.push(value);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(test)]
mod tests {
    // Import the pieces under test explicitly. A glob import would also pull
    // in GPUI's `test` macro and shadow Rust's built-in #[test].
    use super::normalize_proxy_url;
    use super::{
        catalog_cache_path, catalog_meta_path, download_artifact, downloads_dir, fetch_catalog,
        load_cache, local_source_path, localize_error, localize_url_error, store, store_dir,
        write_cache, Artifact, CatalogCacheMeta, StoreErrorContext, CACHE_SCHEMA,
    };
    use std::fs;
    use std::path::PathBuf;

    fn catalog_bytes(
        plugin_id: &str,
        target: &str,
        artifact_url: &str,
        sha: &str,
        size: u64,
    ) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "catalog_version": 1,
            "repository": {
                "id": "test",
                "name": "Test store",
                "homepage": "https://example.com"
            },
            "plugins": [{
                "id": plugin_id,
                "name": "Demo",
                "publisher": "example",
                "latest_version": "1.0.0",
                "versions": [{
                    "version": "1.0.0",
                    "api_version": 1,
                    "artifacts": [{
                        "target": target,
                        "url": artifact_url,
                        "sha256": sha,
                        "size": size
                    }]
                }]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn file_source_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let bytes = catalog_bytes(
            "com.example.demo",
            "universal",
            "https://example.com/demo.zip",
            &"0".repeat(64),
            10,
        );
        fs::write(&path, &bytes).unwrap();
        let source = format!("file://{}", path.display());
        let payload = fetch_catalog(&source).unwrap();
        assert_eq!(payload.catalog.plugins.len(), 1);
        assert_eq!(payload.bytes, bytes);
        assert!(load_cache(dir.path(), &source).is_none());
        write_cache(dir.path(), &source, &payload).unwrap();
        let cached = load_cache(dir.path(), &source).unwrap();
        assert_eq!(cached.catalog.plugins.len(), 1);
        assert_eq!(cached.warnings.len(), 0);
        assert!(load_cache(dir.path(), "https://example.com/other.json").is_none());
    }

    #[test]
    fn cache_survives_a_failed_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        let bytes = catalog_bytes(
            "com.example.demo",
            "universal",
            "https://example.com/demo.zip",
            &"0".repeat(64),
            10,
        );
        fs::write(&path, &bytes).unwrap();
        let source = format!("file://{}", path.display());
        let payload = fetch_catalog(&source).unwrap();
        write_cache(dir.path(), &source, &payload).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(fetch_catalog(&source).is_err());
        let cached = load_cache(dir.path(), &source).unwrap();
        assert_eq!(cached.catalog.plugins.len(), 1);
        assert_eq!(cached.fetched_at, payload.fetched_at);
    }

    #[test]
    fn invalid_cache_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let source = "https://example.com/catalog.json";
        fs::create_dir_all(store_dir(dir.path())).unwrap();
        fs::write(
            catalog_meta_path(dir.path()),
            serde_json::to_vec(&CatalogCacheMeta {
                schema: CACHE_SCHEMA,
                source: source.to_string(),
                fetched_at: 1,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(catalog_cache_path(dir.path()), b"not json").unwrap();
        assert!(load_cache(dir.path(), source).is_none());
        assert!(!catalog_cache_path(dir.path()).exists());
    }

    #[test]
    fn downloads_and_verifies_a_file_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("demo.zip");
        fs::write(&package, b"package bytes").unwrap();
        let hash = store::sha256_file(&package).unwrap();
        let artifact = Artifact {
            target: store::Target::Universal,
            url: format!("file://{}", package.display()),
            sha256: hash,
            size: 13,
        };
        let root = dir.path().join("root");
        let downloaded = download_artifact(&root, "com.example.demo", "1.0.0", &artifact).unwrap();
        assert!(downloaded.is_file());
        assert!(!downloaded.with_extension("zip.part").exists());

        let bad = Artifact {
            sha256: "0".repeat(64),
            ..artifact.clone()
        };
        assert!(download_artifact(&root, "com.example.demo", "1.1.0", &bad).is_err());
        assert!(!downloads_dir(&root)
            .join("com.example.demo-1.1.0.zip")
            .exists());
        assert!(!downloads_dir(&root)
            .join("com.example.demo-1.1.0.zip.part")
            .exists());
    }

    #[test]
    fn download_rejects_the_wrong_size_and_oversized_streams() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("demo.zip");
        fs::write(&package, b"0123456789").unwrap();
        let hash = store::sha256_file(&package).unwrap();
        let root = dir.path().join("root");
        let artifact = Artifact {
            target: store::Target::Universal,
            url: format!("file://{}", package.display()),
            sha256: hash.clone(),
            size: 9,
        };
        let error = download_artifact(&root, "com.example.demo", "1.0.0", &artifact).unwrap_err();
        assert!(!error.is_empty());
        assert!(!downloads_dir(&root)
            .join("com.example.demo-1.0.0.zip.part")
            .exists());
        let artifact = Artifact {
            size: 11,
            ..artifact
        };
        let error = download_artifact(&root, "com.example.demo", "1.0.0", &artifact).unwrap_err();
        assert!(error.contains("11") && error.contains("10"), "{error}");
    }

    #[test]
    fn end_to_end_install_from_a_local_artifact() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        let package = dir.path().join("demo.zip");
        let manifest = r#"{
            "id": "com.example.e2e",
            "name": "End to end",
            "version": "1.0.0",
            "api_version": 1,
            "main": "main.lua"
        }"#;
        let file = fs::File::create(&package).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("plugin.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("main.lua", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"return {}\n").unwrap();
        zip.finish().unwrap();

        let size = fs::metadata(&package).unwrap().len();
        let hash = store::sha256_file(&package).unwrap();
        // Catalog validation only accepts HTTPS artifact URLs. The download
        // layer also accepts file URLs so this test can run without a server;
        // build the plugin model directly to exercise selection, verify and
        // install together.
        let plugin = luban_plugin::store::StorePlugin {
            id: "com.example.e2e".to_string(),
            name: "End to end".to_string(),
            description: None,
            publisher: "test".to_string(),
            verified: false,
            icon: None,
            tags: Vec::new(),
            permissions: Vec::new(),
            source: None,
            homepage: None,
            license: None,
            versions: vec![luban_plugin::store::StoreVersion {
                version: "1.0.0".to_string(),
                api_version: 1,
                released_at: None,
                release_notes: None,
                artifacts: vec![Artifact {
                    target: store::Target::Universal,
                    url: format!("file://{}", package.display()),
                    sha256: hash,
                    size,
                }],
            }],
            localizations: Default::default(),
        };
        let target = store::Target::current().unwrap_or(store::Target::Universal);
        let candidate = plugin.install_candidate(target, 1).unwrap();
        let downloaded = download_artifact(
            &root,
            &plugin.id,
            &candidate.version.version,
            candidate.artifact,
        )
        .unwrap();
        let catalog = "https://example.com/catalog.json".to_string();
        let record = luban_plugin::install::install_zip_checked_with_source(
            &root,
            &downloaded,
            false,
            &luban_plugin::install::ExpectedPlugin {
                id: plugin.id.clone(),
                version: candidate.version.version.clone(),
            },
            luban_plugin::install::InstallSource::Store {
                catalog: catalog.clone(),
            },
        )
        .unwrap();
        assert_eq!(record.id, "com.example.e2e");
        assert_eq!(record.version, "1.0.0");
        match &record.source {
            luban_plugin::install::InstallSource::Store { catalog: recorded } => {
                assert_eq!(recorded, &catalog);
            }
            other => panic!("unexpected install source: {other:?}"),
        }
        assert!(root.join("com.example.e2e/main.lua").is_file());
        assert!(luban_plugin::install::load_records(&root)
            .unwrap()
            .plugins
            .contains_key("com.example.e2e"));
    }

    #[test]
    fn localizes_store_errors() {
        let _guard = crate::i18n::test_lock();
        rust_i18n::set_locale("zh-CN");
        assert_eq!(
            localize_error(
                StoreErrorContext::Catalog,
                "unsupported catalog_version 2; this build supports 1"
            ),
            "此版本不支持插件目录版本 2。"
        );
        let network = localize_error(
            StoreErrorContext::Catalog,
            "cannot fetch https://example.com/catalog.json: io: Connection refused (os error 111)",
        );
        assert!(network.contains("连接被拒绝"), "{network}");
        assert!(localize_url_error("").contains("请输入"));
        rust_i18n::set_locale("en");
    }

    #[test]
    fn normalizes_proxy_urls() {
        assert_eq!(
            normalize_proxy_url("socks5://127.0.0.1:7897"),
            "socks5h://127.0.0.1:7897"
        );
        assert_eq!(
            normalize_proxy_url("http://127.0.0.1:7897"),
            "http://127.0.0.1:7897"
        );
        assert_eq!(
            normalize_proxy_url("127.0.0.1:7897"),
            "http://127.0.0.1:7897"
        );
        assert_eq!(normalize_proxy_url(""), "");
    }

    #[test]
    fn rejects_http_and_relative_sources() {
        assert!(fetch_catalog("http://example.com/catalog.json").is_err());
        assert!(fetch_catalog("relative/catalog.json").is_err());
        assert!(fetch_catalog("file://relative/path").is_err());
    }

    #[test]
    fn decodes_file_urls() {
        assert_eq!(
            local_source_path("file:///tmp/a%20b.json"),
            Some(PathBuf::from("/tmp/a b.json"))
        );
        assert_eq!(
            local_source_path("file://localhost/tmp/c.json"),
            Some(PathBuf::from("/tmp/c.json"))
        );
        assert_eq!(
            local_source_path("/tmp/c.json"),
            Some(PathBuf::from("/tmp/c.json"))
        );
        assert_eq!(local_source_path("https://example.com"), None);
    }
}
