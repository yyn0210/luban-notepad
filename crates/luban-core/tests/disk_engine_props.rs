//! Property tests for the disk engine.
//!
//! Random edit sequences (inserts, deletes and replaces) are applied to both a
//! `String` model and a `DiskEngine`, and the engine's view is compared with
//! the model after every operation, through undo/redo, and across a
//! save/reopen round trip. Failures print the proptest seed so the case can be
//! shrunk and replayed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};

use luban_core::disk_engine::DiskEngine;
use luban_core::encoding::TextEncoding;
use luban_core::eol;
use proptest::prelude::*;
use tempfile::TempDir;

const CASES: u32 = 64;

const TOKENS: &[&str] = &["a", "b", " ", "\n", "é", "中", "🦀", "line\n"];

#[derive(Debug, Clone)]
enum Op {
    Insert { at: u32, text: String },
    Delete { at: u32, len: u32 },
    Replace { at: u32, len: u32, text: String },
}

fn token() -> impl Strategy<Value = &'static str> {
    prop::sample::select(TOKENS)
}

fn inserted_text() -> impl Strategy<Value = String> {
    prop::collection::vec(token(), 0..6).prop_map(|parts| parts.concat())
}

/// Replacement text is never empty, so a `Replace` always removes and inserts
/// in the same edit (a pure delete is already covered by `Delete`).
fn replacement_text() -> impl Strategy<Value = String> {
    prop::collection::vec(token(), 1..6).prop_map(|parts| parts.concat())
}

fn operation() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u32..1000, inserted_text()).prop_map(|(at, text)| Op::Insert { at, text }),
        (0u32..1000, 0u32..12).prop_map(|(at, len)| Op::Delete { at, len }),
        (0u32..1000, 1u32..12, replacement_text()).prop_map(|(at, len, text)| Op::Replace {
            at,
            len,
            text
        }),
    ]
}

fn operations() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(operation(), 0..48)
}

fn initial_text() -> impl Strategy<Value = String> {
    prop::collection::vec(token(), 1..24).prop_map(|parts| parts.concat())
}

fn open(path: &Path) -> DiskEngine {
    DiskEngine::open(path, &AtomicBool::new(false), &AtomicU64::new(0)).unwrap()
}

fn write_source(dir: &TempDir, name: &str, text: &str, crlf: bool, bom: bool) -> PathBuf {
    let path = dir.path().join(name);
    let body = if crlf {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    };
    let mut bytes = Vec::new();
    if bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(body.as_bytes());
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Floor `offset` to the nearest UTF-8 character boundary.
fn boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn insert_at(text: &str, permille: u32) -> usize {
    boundary(text, text.len() * permille as usize / 1000)
}

fn delete_range(text: &str, permille: u32, len: u32) -> (usize, usize) {
    let start = insert_at(text, permille);
    let end = boundary(text, start.saturating_add(len as usize));
    (start, end)
}

fn line_count(text: &str) -> u64 {
    text.split('\n').count() as u64
}

fn assert_matches(engine: &DiskEngine, model: &str, step: &str) {
    assert_eq!(
        engine.total_len(),
        model.len() as u64,
        "length after {step}"
    );
    assert_eq!(
        engine.total_lines(),
        line_count(model),
        "line count after {step}"
    );
    let actual = engine
        .read_text(0, engine.total_len())
        .unwrap_or_else(|error| panic!("read after {step}: {error}"));
    assert_eq!(actual, model, "text after {step}");
}

/// Apply one generated operation to the engine and the model. Returns whether
/// the operation changed the text; empty edits produce no history entry.
fn apply(engine: &mut DiskEngine, model: &mut String, op: &Op) -> bool {
    match op {
        Op::Insert { at, text } => {
            let start = insert_at(model, *at);
            engine
                .apply_edit(start as u64, 0, text)
                .unwrap_or_else(|error| panic!("insert {start:?}: {error}"));
            model.insert_str(start, text);
            !text.is_empty()
        }
        Op::Delete { at, len } => {
            let (start, end) = delete_range(model, *at, *len);
            engine
                .apply_edit(start as u64, (end - start) as u64, "")
                .unwrap_or_else(|error| panic!("delete {start}..{end}: {error}"));
            model.replace_range(start..end, "");
            end > start
        }
        Op::Replace { at, len, text } => {
            let (start, end) = delete_range(model, *at, *len);
            engine
                .apply_edit(start as u64, (end - start) as u64, text)
                .unwrap_or_else(|error| panic!("replace {start}..{end}: {error}"));
            model.replace_range(start..end, text);
            end > start || !text.is_empty()
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        max_shrink_iters: 512,
        // The seed is printed in the failure message; do not drop a
        // regression file into the source tree.
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn random_edits_match_the_reference_model(
        initial in initial_text(),
        ops in operations(),
        crlf in any::<bool>(),
    ) {
        let dir = TempDir::new().unwrap();
        let path = write_source(&dir, "edits.txt", &initial, crlf, false);
        let mut engine = open(&path);
        let mut model = initial.replace("\r\n", "\n");
        assert_matches(&engine, &model, "open");

        for (index, op) in ops.iter().enumerate() {
            apply(&mut engine, &mut model, op);
            assert_matches(&engine, &model, &format!("operation {index}: {op:?}"));
        }
    }

    #[test]
    fn undo_and_redo_restore_each_model_state(
        initial in initial_text(),
        ops in operations(),
        crlf in any::<bool>(),
    ) {
        let dir = TempDir::new().unwrap();
        let path = write_source(&dir, "history.txt", &initial, crlf, false);
        let mut engine = open(&path);
        let mut model = initial.replace("\r\n", "\n");
        let mut states = vec![model.clone()];

        for op in &ops {
            if apply(&mut engine, &mut model, op) {
                states.push(model.clone());
            }
        }
        if states.len() == 1 {
            prop_assert!(!engine.can_undo());
            prop_assert!(!engine.can_redo());
            return Ok(());
        }
        prop_assert!(engine.can_undo());

        // Undo every operation and walk the model states backwards. The final
        // undo returns to the unmodified source text.
        for expected in states.iter().rev().skip(1) {
            prop_assert!(engine.undo().is_some());
            assert_matches(&engine, expected, "undo");
        }
        prop_assert!(!engine.can_undo());
        prop_assert!(!engine.is_modified());
        prop_assert_eq!(
            engine.read_text(0, engine.total_len()).unwrap(),
            states[0].clone()
        );

        // Redo the whole history. The final redo returns to the edited text.
        for expected in states.iter().skip(1) {
            prop_assert!(engine.redo().is_some());
            assert_matches(&engine, expected, "redo");
        }
        prop_assert!(!engine.can_redo());
        prop_assert!(engine.is_modified());
        assert_matches(&engine, states.last().unwrap(), "redo end");
    }

    #[test]
    fn save_round_trips_the_model_text(
        initial in initial_text(),
        ops in operations(),
        crlf in any::<bool>(),
        bom in any::<bool>(),
    ) {
        let dir = TempDir::new().unwrap();
        let path = write_source(&dir, "source.txt", &initial, crlf, bom);
        let original = initial.clone();
        let expected_eol = eol::detect(&if crlf {
            original.replace('\n', "\r\n")
        } else {
            original.clone()
        });
        let mut engine = open(&path);
        let mut model = initial.replace("\r\n", "\n");
        for op in &ops {
            apply(&mut engine, &mut model, op);
        }
        // An empty file has no line-ending evidence on reopen, so it falls back
        // to the platform convention instead of the source convention.
        prop_assume!(!model.is_empty());
        assert_matches(&engine, &model, "before save");

        let target = dir.path().join("saved.txt");
        let report = engine
            .snapshot()
            .write(&target, false, &AtomicU64::new(0), &AtomicBool::new(false))
            .expect("save");
        prop_assert_eq!(report.file_size(), std::fs::metadata(&target).unwrap().len());
        engine.adopt_saved(report, target.clone()).unwrap();
        prop_assert!(!engine.is_modified());
        assert_matches(&engine, &model, "after save");

        let reopened = open(&target);
        assert_matches(&reopened, &model, "reopen");
        // Line endings are only observable when the saved text contains a line
        // break; an empty file or a single line falls back to the native
        // convention on reopen.
        if model.contains('\n') {
            prop_assert_eq!(reopened.eol(), expected_eol);
        }
        prop_assert_eq!(
            reopened.encoding(),
            if bom { TextEncoding::Utf8Bom } else { TextEncoding::Utf8 }
        );

        let mut expected_bytes = Vec::new();
        if bom {
            expected_bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        expected_bytes.extend_from_slice(eol::normalize(&model, expected_eol).as_bytes());
        prop_assert_eq!(std::fs::read(&target).unwrap(), expected_bytes);
    }
}
