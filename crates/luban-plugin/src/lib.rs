//! Plugin platform for Luban Notepad.
//!
//! This crate is GUI-free: it parses and validates plugin manifests, resolves
//! dependencies, manages enabled/safe-mode state, owns the sandboxed Lua
//! runtime and implements install/update/uninstall from local directories, zip
//! archives and Git repositories. The application supplies the document and UI
//! callbacks through [`runtime::Host`].

pub mod install;
pub mod manifest;
pub mod permissions;
pub mod registry;
pub mod runtime;
pub mod settings;
