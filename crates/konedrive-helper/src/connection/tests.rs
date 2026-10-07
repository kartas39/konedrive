use super::*;

/// Only a `Hello` names a version, and only another version than the
/// helper's closes the connection.
#[test]
fn only_a_hello_with_another_version_is_another_version() {
    assert_eq!(another_version(&ToHelper::Hello { version: PROTOCOL_VERSION }), None);
    assert_eq!(
        another_version(&ToHelper::Hello { version: PROTOCOL_VERSION + 1 }),
        Some(PROTOCOL_VERSION + 1)
    );
    assert_eq!(another_version(&ToHelper::Hello { version: 0 }), Some(0));
    assert_eq!(another_version(&ToHelper::MarkDir), None);
    assert_eq!(another_version(&ToHelper::HydrateDone { req_id: 0, errno: 0 }), None);
}
