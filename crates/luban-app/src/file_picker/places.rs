//! Common locations shown in the picker sidebar.

use std::path::{Path, PathBuf};

use crate::i18n::tr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceIcon {
    Home,
    Workspace,
    Desktop,
    Documents,
    Downloads,
    Recent,
    Drive,
    Folder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
    pub icon: PlaceIcon,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaceSection {
    pub title: String,
    pub places: Vec<Place>,
}

impl PlaceSection {
    fn new(title: impl Into<String>, places: Vec<Place>) -> Option<Self> {
        (!places.is_empty()).then(|| Self {
            title: title.into(),
            places,
        })
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn push_unique(places: &mut Vec<Place>, place: Place) {
    if places
        .iter()
        .any(|existing| crate::util::same_path(&existing.path, &place.path))
    {
        return;
    }
    places.push(place);
}

/// Whether the process can actually enumerate this directory.
///
/// A directory can exist but still be unreadable: on OpenHarmony the app
/// sandbox rejects everything outside the application data directories, and
/// desktop systems have protected folders too. Offering such a place only
/// produces a permission error after one click.
fn is_readable_dir(path: &Path) -> bool {
    path.is_dir() && std::fs::read_dir(path).is_ok()
}

/// Build the sidebar. Empty sections are omitted; missing standard folders
/// (for example an unset Documents directory) are skipped.
pub fn build_sections(
    workspace_name: &str,
    roots: &[PathBuf],
    current_file_dir: Option<&Path>,
    recent: &[PathBuf],
) -> Vec<PlaceSection> {
    let mut sections = Vec::new();

    let mut quick = Vec::new();
    if let Some(home) = dirs::home_dir().filter(|dir| is_readable_dir(dir)) {
        push_unique(
            &mut quick,
            Place {
                label: tr("dialog.place_home"),
                path: home,
                icon: PlaceIcon::Home,
            },
        );
    }
    for (dir, label, icon) in [
        (
            dirs::desktop_dir(),
            "dialog.place_desktop",
            PlaceIcon::Desktop,
        ),
        (
            dirs::document_dir(),
            "dialog.place_documents",
            PlaceIcon::Documents,
        ),
        (
            dirs::download_dir(),
            "dialog.place_downloads",
            PlaceIcon::Downloads,
        ),
    ] {
        if let Some(dir) = dir.filter(|dir| is_readable_dir(dir)) {
            push_unique(
                &mut quick,
                Place {
                    label: tr(label),
                    path: dir,
                    icon,
                },
            );
        }
    }
    if let Some(section) = PlaceSection::new(tr("dialog.places_quick"), quick) {
        sections.push(section);
    }

    let mut workspace = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        if !is_readable_dir(root) {
            continue;
        }
        let label = if index == 0 && !workspace_name.trim().is_empty() {
            workspace_name.trim().to_string()
        } else {
            display_name(root)
        };
        push_unique(
            &mut workspace,
            Place {
                label,
                path: root.clone(),
                icon: PlaceIcon::Workspace,
            },
        );
    }
    if let Some(dir) = current_file_dir.filter(|dir| is_readable_dir(dir)) {
        let already_listed = roots
            .iter()
            .any(|root| crate::util::same_path(root, dir) || dir.starts_with(root));
        if !already_listed {
            push_unique(
                &mut workspace,
                Place {
                    label: tr("dialog.place_current_file"),
                    path: dir.to_path_buf(),
                    icon: PlaceIcon::Folder,
                },
            );
        }
    }
    if let Some(section) = PlaceSection::new(tr("dialog.places_workspace"), workspace) {
        sections.push(section);
    }

    let mut recent_places = Vec::new();
    for path in recent.iter().filter(|path| is_readable_dir(path)).take(5) {
        push_unique(
            &mut recent_places,
            Place {
                label: display_name(path),
                path: path.clone(),
                icon: PlaceIcon::Recent,
            },
        );
    }
    if let Some(section) = PlaceSection::new(tr("dialog.places_recent"), recent_places) {
        sections.push(section);
    }

    if let Some(section) = PlaceSection::new(tr("dialog.places_locations"), system_locations()) {
        sections.push(section);
    }

    sections
}

/// Filesystem roots and removable/mounted volumes for the current platform.
pub fn system_locations() -> Vec<Place> {
    let mut places = Vec::new();
    // OpenHarmony denies path access outside the application sandbox. User
    // documents are reached through the system picker (which grants access to
    // the selected URIs), so the fallback picker only offers sandbox folders
    // instead of `/` and mount points that can never be read.
    #[cfg(target_env = "ohos")]
    {
        if let Some(config) = std::env::var_os("LUBAN_NOTEPAD_CONFIG_DIR").map(PathBuf::from) {
            let documents = config.join("Documents");
            if is_readable_dir(&documents) {
                places.push(Place {
                    label: tr("dialog.place_sandbox_documents"),
                    path: documents,
                    icon: PlaceIcon::Documents,
                });
            }
            if is_readable_dir(&config) {
                places.push(Place {
                    label: tr("dialog.place_app_files"),
                    path: config,
                    icon: PlaceIcon::Folder,
                });
            }
        }
        if places.is_empty() {
            if let Ok(cwd) = std::env::current_dir() {
                if is_readable_dir(&cwd) {
                    places.push(Place {
                        label: display_name(&cwd),
                        path: cwd,
                        icon: PlaceIcon::Drive,
                    });
                }
            }
        }
    }
    #[cfg(all(target_os = "linux", not(target_env = "ohos")))]
    {
        places.push(Place {
            label: "/".to_string(),
            path: PathBuf::from("/"),
            icon: PlaceIcon::Drive,
        });
        for mount in read_linux_mounts() {
            push_unique(
                &mut places,
                Place {
                    label: mount.label,
                    path: mount.path,
                    icon: PlaceIcon::Drive,
                },
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        places.push(Place {
            label: "/".to_string(),
            path: PathBuf::from("/"),
            icon: PlaceIcon::Drive,
        });
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name.is_empty() {
                    continue;
                }
                push_unique(
                    &mut places,
                    Place {
                        label: name,
                        path: entry.path(),
                        icon: PlaceIcon::Drive,
                    },
                );
            }
        }
    }
    #[cfg(windows)]
    {
        for place in windows_drives() {
            push_unique(&mut places, place);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        places.push(Place {
            label: "/".to_string(),
            path: PathBuf::from("/"),
            icon: PlaceIcon::Drive,
        });
    }
    places
}

#[cfg_attr(target_env = "ohos", allow(dead_code))]
pub struct MountPoint {
    pub path: PathBuf,
    pub label: String,
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
fn read_linux_mounts() -> Vec<MountPoint> {
    std::fs::read_to_string("/proc/mounts")
        .map(|text| parse_linux_mounts(&text))
        .unwrap_or_default()
}

/// Parse `/proc/mounts` into user-visible volumes. Only real filesystems under
/// the usual removable-media roots are kept, so `/proc`, `/sys`, `/dev` and
/// boot partitions do not clutter the sidebar.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[cfg_attr(target_env = "ohos", allow(dead_code))]
pub fn parse_linux_mounts(text: &str) -> Vec<MountPoint> {
    const FILESYSTEMS: &[&str] = &[
        "ext2", "ext3", "ext4", "xfs", "btrfs", "vfat", "exfat", "ntfs", "ntfs3", "fuseblk",
        "cifs", "smbfs", "nfs", "nfs4", "sshfs", "zfs", "apfs", "hfs", "hfsplus", "iso9660", "udf",
    ];
    let mut mounts: Vec<MountPoint> = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(_device) = fields.next() else {
            continue;
        };
        let Some(raw_mount) = fields.next() else {
            continue;
        };
        let Some(fstype) = fields.next() else {
            continue;
        };
        let mount = unescape_mount(raw_mount);
        if mount == "/" {
            continue;
        }
        let visible_root = mount.starts_with("/media/")
            || mount.starts_with("/mnt/")
            || mount.starts_with("/run/media/");
        let real_filesystem = fstype.starts_with("fuse.") || FILESYSTEMS.contains(&fstype);
        if !visible_root || !real_filesystem {
            continue;
        }
        let path = PathBuf::from(&mount);
        if mounts.iter().any(|existing| existing.path == path) {
            continue;
        }
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| mount.clone());
        mounts.push(MountPoint { path, label });
    }
    mounts
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[cfg_attr(target_env = "ohos", allow(dead_code))]
fn unescape_mount(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let mut code = String::new();
        for _ in 0..3 {
            match chars.next() {
                Some(digit) if digit.is_ascii_digit() => code.push(digit),
                Some(other) => {
                    code.clear();
                    out.push(other);
                    break;
                }
                None => break,
            }
        }
        match u8::from_str_radix(&code, 8) {
            Ok(byte) => out.push(byte as char),
            Err(_) => out.push('\\'),
        }
    }
    out
}

#[cfg(windows)]
fn windows_drives() -> Vec<Place> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| {
            let letter = char::from(b'A' + index as u8);
            Place {
                label: format!("{letter}:"),
                path: PathBuf::from(format!("{letter}:\\")),
                icon: PlaceIcon::Drive,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_visible_linux_mounts() {
        let text = "\
proc /proc proc rw,nosuid 0 0
sysfs /sys sysfs rw 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
/dev/sdb1 /media/user/USB\\040STICK exfat rw 0 0
nas:/share /mnt/share cifs rw 0 0
";
        let mounts = parse_linux_mounts(text);
        let paths: Vec<_> = mounts.iter().map(|mount| mount.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![Path::new("/media/user/USB STICK"), Path::new("/mnt/share")]
        );
        assert_eq!(mounts[0].label, "USB STICK");
        assert_eq!(mounts[1].label, "share");
    }

    #[test]
    fn recent_places_only_include_existing_folders() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let sections = build_sections("", &[], None, &[dir.path().to_path_buf(), missing]);
        let recent = sections
            .iter()
            .find(|section| section.title == tr("dialog.places_recent"))
            .expect("recent section");
        assert_eq!(recent.places.len(), 1);
        assert_eq!(recent.places[0].path, dir.path());
    }

    #[test]
    fn unreadable_or_missing_roots_are_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_readable_dir(dir.path()));
        let missing = dir.path().join("missing");
        assert!(!is_readable_dir(&missing));

        // A root that cannot even be opened must not become a workspace place.
        let sections = build_sections("ws", &[missing], None, &[]);
        assert!(!sections.iter().any(|section| {
            section
                .places
                .iter()
                .any(|place| place.icon == PlaceIcon::Workspace)
        }));
    }
}
