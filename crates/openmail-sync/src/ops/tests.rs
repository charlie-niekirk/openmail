//! The queue's local bookkeeping, without a Gmail server.

use serde_json::json;

use super::*;

#[test]
fn ops_persist_as_tagged_json() {
    let op = Op::archive(Target::thread("t1"));
    assert_eq!(
        serde_json::to_value(&op).unwrap(),
        json!({"type": "modify_labels", "target": {"threads": ["t1"]}, "remove": ["INBOX"]})
    );
    let op = Op::RenameLabel {
        label: LabelId::new("Label_1"),
        name: "Work/Old".to_owned(),
    };
    let text = serde_json::to_string(&op).unwrap();
    assert_eq!(serde_json::from_str::<Op>(&text).unwrap(), op);
    assert_eq!(op.kind(), "rename_label");
}

#[test]
fn backoff_doubles_up_to_the_cap_and_honours_retry_after() {
    let config = OpsConfig::default();
    let secs = |attempts, after: Option<u64>| {
        config
            .backoff(attempts, after.map(Duration::from_secs))
            .as_secs()
    };
    assert_eq!(secs(1, None), 2);
    assert_eq!(secs(2, None), 4);
    assert_eq!(secs(5, None), 32);
    assert_eq!(secs(30, None), 300);
    assert_eq!(secs(1, Some(30)), 30);
    assert_eq!(secs(8, Some(30)), 256);
}
