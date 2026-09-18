//! The wire format against the specification's own examples.

use super::*;

/// The announcement from the specification, verbatim.
const ANNOUNCE: &str = r#"{
  "alias": "Nice Orange",
  "version": "2.0",
  "deviceModel": "Samsung",
  "deviceType": "mobile",
  "fingerprint": "random string",
  "port": 53317,
  "protocol": "https",
  "download": true,
  "announce": true
}"#;

/// The `register` answer from the specification: no port, no protocol.
const REGISTER_ANSWER: &str = r#"{
  "alias": "Nice Orange",
  "version": "2.0",
  "deviceModel": "Samsung",
  "deviceType": "mobile",
  "fingerprint": "random string",
  "download": true
}"#;

/// The `prepare-upload` from the specification.
const PREPARE: &str = r#"{
  "info": {
    "alias": "Nice Orange",
    "version": "2.0",
    "deviceModel": "Samsung",
    "deviceType": "mobile",
    "fingerprint": "random string",
    "port": 53317,
    "protocol": "https",
    "download": true
  },
  "files": {
    "some file id": {
      "id": "some file id",
      "fileName": "my image.png",
      "size": 324242,
      "fileType": "image/jpeg",
      "sha256": "*sha256 hash*",
      "preview": "*preview data*",
      "metadata": {
        "modified": "2021-01-01T12:34:56Z",
        "accessed": "2021-01-01T12:34:56Z"
      }
    }
  }
}"#;

/// The answer to it.
const PREPARED: &str = r#"{
  "sessionId": "mySessionId",
  "files": {
    "someFileId": "someFileToken"
  }
}"#;

#[test]
fn the_specifications_announcement_reads_and_writes_back_the_same() {
    let announcement: Announcement = serde_json::from_str(ANNOUNCE).expect("parse");
    assert_eq!(announcement.device.alias, "Nice Orange");
    assert_eq!(announcement.device.device_type, Some(DeviceType::Mobile));
    assert_eq!(announcement.device.port, Some(53317));
    assert_eq!(announcement.device.protocol, Some(Protocol::Https));
    assert!(announcement.device.download);
    assert!(announcement.announce);
    // Back out and in again is the same value, and the JSON has every field.
    let text = serde_json::to_string(&announcement).expect("write");
    let again: Announcement = serde_json::from_str(&text).expect("re-parse");
    assert_eq!(again, announcement);
    for key in [
        "\"alias\"",
        "\"deviceModel\"",
        "\"deviceType\"",
        "\"port\"",
        "\"protocol\"",
        "\"announce\"",
    ] {
        assert!(text.contains(key), "{key} in {text}");
    }
}

#[test]
fn an_answer_without_port_or_protocol_still_parses_and_is_written_without_them() {
    let device: DeviceInfo = serde_json::from_str(REGISTER_ANSWER).expect("parse");
    assert_eq!(device.port, None);
    assert_eq!(device.protocol, None);
    let text = serde_json::to_string(&device).expect("write");
    assert!(!text.contains("port"), "{text}");
    assert!(!text.contains("protocol"), "{text}");
}

#[test]
fn a_device_type_this_version_does_not_know_is_kept_not_refused() {
    let text = ANNOUNCE.replace("\"mobile\"", "\"toaster\"");
    let announcement: Announcement = serde_json::from_str(&text).expect("parse");
    assert_eq!(announcement.device.device_type, Some(DeviceType::Unknown));
}

#[test]
fn a_prepare_upload_round_trips_with_every_file_field() {
    let request: PrepareUploadRequest = serde_json::from_str(PREPARE).expect("parse");
    let file = request.files.get("some file id").expect("the file");
    assert_eq!(file.file_name, "my image.png");
    assert_eq!(file.size, 324_242);
    assert_eq!(file.file_type, "image/jpeg");
    assert_eq!(file.sha256.as_deref(), Some("*sha256 hash*"));
    assert_eq!(
        file.metadata.as_ref().and_then(|m| m.modified.as_deref()),
        Some("2021-01-01T12:34:56Z")
    );
    let text = serde_json::to_string(&request).expect("write");
    let again: PrepareUploadRequest = serde_json::from_str(&text).expect("re-parse");
    assert_eq!(again, request);

    let answer: PrepareUploadResponse = serde_json::from_str(PREPARED).expect("parse");
    assert_eq!(answer.session_id, "mySessionId");
    assert_eq!(
        answer.files.get("someFileId").map(String::as_str),
        Some("someFileToken")
    );
}

#[test]
fn a_file_without_the_optional_fields_is_written_without_them() {
    let file = FileMeta {
        id: "x".into(),
        file_name: "a.txt".into(),
        size: 3,
        file_type: "text/plain".into(),
        sha256: None,
        preview: None,
        metadata: None,
    };
    let text = serde_json::to_string(&file).expect("write");
    assert_eq!(
        text,
        r#"{"id":"x","fileName":"a.txt","size":3,"fileType":"text/plain"}"#
    );
}

#[test]
fn our_own_device_has_the_fields_the_other_side_needs_and_a_fresh_fingerprint() {
    let a = DeviceInfo::ours("hcmd on box", 53317, Protocol::Http);
    let b = DeviceInfo::ours("hcmd on box", 53317, Protocol::Http);
    assert_eq!(a.version, PROTOCOL_VERSION);
    assert_eq!(a.version_parts(), Some((2, 0)));
    assert_eq!(a.device_type, Some(DeviceType::Headless));
    assert_eq!(a.port, Some(53317));
    assert_eq!(a.protocol, Some(Protocol::Http));
    assert_eq!(a.fingerprint.len(), 32);
    assert_ne!(a.fingerprint, b.fingerprint, "two devices, two identities");
    assert!(
        a.device_model
            .is_some_and(|m| m.starts_with(|c: char| c.is_ascii_uppercase()))
    );
}

#[test]
fn a_file_is_described_from_disk_with_its_size_type_and_modified_time() {
    let dir = std::env::temp_dir().join(format!("hcmd-ls-meta-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("photo.jpg");
    std::fs::write(&path, b"not really a jpeg").expect("file");
    let meta = FileMeta::of(&path, "album/photo.jpg").expect("meta");
    assert_eq!(meta.file_name, "album/photo.jpg");
    assert_eq!(meta.size, 17);
    assert_eq!(meta.file_type, "image/jpeg");
    assert_eq!(meta.id.len(), 32);
    let modified = meta
        .metadata
        .and_then(|m| m.modified)
        .expect("a modified time");
    assert!(
        modified.ends_with('Z') && modified.len() == 20,
        "{modified}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
