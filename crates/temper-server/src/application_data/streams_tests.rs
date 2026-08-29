use super::{FileStream, FileStreamRegistry};
use temper_wasm_sdk::data::ModuleDataBudgets;

#[test]
fn typed_stream_admission_has_no_application_field_aliases() {
    let source = include_str!("streams.rs");
    for forbidden in [
        "version_belongs_to_file",
        "declared_stream_length",
        "\"Size\"",
        "\"size\"",
        "\"ContentLength\"",
        "\"content_length\"",
        "\"SizeBytes\"",
        "\"size_bytes\"",
        "\"FileId\"",
        "\"file_id\"",
    ] {
        assert!(
            !source.contains(forbidden),
            "typed admission must not infer authority from '{forbidden}'"
        );
    }
}

#[test]
fn final_file_chunk_is_followed_by_eof_then_consumption() {
    let mut registry = FileStreamRegistry::new(&ModuleDataBudgets::default());
    let handle = registry
        .insert(FileStream::Read { offset: 0 }, b"abc".to_vec())
        .unwrap();
    assert_eq!(registry.read(handle, 3).unwrap(), b"abc");
    assert_eq!(registry.read(handle, 3).unwrap(), b"");
    assert_eq!(registry.read(handle, 3), Err(-3));
}

#[test]
fn write_chunks_append_under_stream_budget() {
    let mut registry = FileStreamRegistry::new(&ModuleDataBudgets::default());
    let handle = registry
        .insert(
            FileStream::Write {
                entity_type: "Temper.FileSystem.File".into(),
                file_id: "file-1".into(),
                expected_length: None,
                expected_hash: None,
                expected_sequence: None,
                committing: false,
            },
            Vec::new(),
        )
        .unwrap();
    registry.max_bytes = 3;
    assert_eq!(registry.write(handle, b"ab"), Ok(2));
    assert_eq!(registry.write(handle, b"c"), Ok(1));
    assert_eq!(registry.write(handle, b"d"), Err(-4));
    assert_eq!(registry.take(handle).unwrap().1, b"abc");
}

#[test]
fn commit_validation_and_failed_dispatch_leave_write_retryable() {
    let mut registry = FileStreamRegistry::new(&ModuleDataBudgets::default());
    let handle = registry
        .insert(
            FileStream::Write {
                entity_type: "Temper.FileSystem.File".into(),
                file_id: "file-1".into(),
                expected_length: Some(3),
                expected_hash: None,
                expected_sequence: Some(7),
                committing: false,
            },
            Vec::new(),
        )
        .unwrap();
    registry.write(handle, b"ab").unwrap();
    assert_eq!(
        registry.begin_commit(handle).unwrap_err().code,
        "FileLengthMismatch"
    );
    registry.write(handle, b"c").unwrap();
    assert_eq!(registry.begin_commit(handle).unwrap().bytes, b"abc");
    assert_eq!(registry.write(handle, b"d"), Err(-3));
    registry.finish_commit(handle, false).unwrap();
    assert_eq!(registry.begin_commit(handle).unwrap().bytes, b"abc");
    registry.finish_commit(handle, true).unwrap();
    assert!(registry.begin_commit(handle).is_err());
}

#[test]
fn commit_rejects_read_direction_without_consuming_it() {
    let mut registry = FileStreamRegistry::new(&ModuleDataBudgets::default());
    let handle = registry
        .insert(FileStream::Read { offset: 0 }, b"abc".to_vec())
        .unwrap();
    assert!(registry.begin_commit(handle).is_err());
    assert_eq!(registry.read(handle, 3).unwrap(), b"abc");
}
