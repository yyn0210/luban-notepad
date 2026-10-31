//! Plugin permissions and approval hashes.

use sha2::{Digest, Sha256};

/// Known permissions understood by this host version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    Filesystem,
    Network,
    Process,
}

impl Permission {
    pub const ALL: [Self; 3] = [Self::Filesystem, Self::Network, Self::Process];

    pub fn name(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::Network => "network",
            Self::Process => "process",
        }
    }

    pub fn implemented(self) -> bool {
        matches!(self, Self::Filesystem)
    }
}

/// The declared permission set of a plugin.
///
/// Unknown permission names are retained (and included in the approval hash)
/// so that adding a new permission to a plugin forces the user to re-approve
/// it, even if this host version does not implement the API behind it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionSet {
    declared: Vec<String>,
}

impl PermissionSet {
    pub fn parse(raw: &[String]) -> Self {
        let mut declared: Vec<String> = raw
            .iter()
            .map(|item| item.trim().to_ascii_lowercase())
            .filter(|item| !item.is_empty())
            .collect();
        declared.sort();
        declared.dedup();
        Self { declared }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn declared(&self) -> &[String] {
        &self.declared
    }

    pub fn known(&self) -> Vec<Permission> {
        Permission::ALL
            .into_iter()
            .filter(|permission| self.has(permission.name()))
            .collect()
    }

    pub fn unknown(&self) -> Vec<&str> {
        self.declared
            .iter()
            .filter(|item| {
                !Permission::ALL
                    .iter()
                    .any(|permission| permission.name() == *item)
            })
            .map(String::as_str)
            .collect()
    }

    pub fn has(&self, permission: &str) -> bool {
        self.declared.iter().any(|item| item == permission)
    }

    pub fn filesystem(&self) -> bool {
        self.has(Permission::Filesystem.name())
    }

    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }

    /// Stable hash of the declared permission set.
    pub fn approval_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for permission in &self.declared {
            hasher.update(permission.as_bytes());
            hasher.update(b"\n");
        }
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_hashes_permissions() {
        let set = PermissionSet::parse(&[
            " Filesystem ".into(),
            "network".into(),
            "filesystem".into(),
            "custom".into(),
            "".into(),
        ]);
        assert_eq!(set.declared(), ["custom", "filesystem", "network"]);
        assert!(set.filesystem());
        assert_eq!(set.unknown(), ["custom"]);
        assert_eq!(
            set.known(),
            vec![Permission::Filesystem, Permission::Network]
        );
        let same = PermissionSet::parse(&["network".into(), "custom".into(), "filesystem".into()]);
        assert_eq!(set.approval_hash(), same.approval_hash());
        let different = PermissionSet::parse(&["filesystem".into()]);
        assert_ne!(set.approval_hash(), different.approval_hash());
    }

    #[test]
    fn empty_permissions_have_a_stable_hash() {
        let empty = PermissionSet::empty();
        assert!(empty.is_empty());
        assert!(!empty.filesystem());
        assert_eq!(
            empty.approval_hash(),
            PermissionSet::parse(&[]).approval_hash()
        );
    }
}
