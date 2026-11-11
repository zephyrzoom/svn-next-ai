//! 契约冻结测试。
//!
//! proto 是协议唯一真源，但真源本身不会因为「有人改了编号」而报错——
//! 这里把已经对外承诺过的数值与结构钉死，任何破坏性改动都会在 `cargo test` 阶段暴露。

use vcs_proto::vcs::repo::v1 as pb;

#[test]
fn error_code_numbers_are_stable() {
    // 客户端会按数值做分支，编号只能新增不能复用。
    let expected: &[(pb::ErrorCode, i32)] = &[
        (pb::ErrorCode::Ok, 1),
        (pb::ErrorCode::OutOfDate, 2),
        (pb::ErrorCode::PathConflict, 3),
        (pb::ErrorCode::TreeConflict, 4),
        (pb::ErrorCode::PermissionDenied, 5),
        (pb::ErrorCode::QuotaExceeded, 6),
        (pb::ErrorCode::ContentPurged, 7),
        (pb::ErrorCode::Locked, 8),
        (pb::ErrorCode::ObjectMissing, 9),
        (pb::ErrorCode::InvalidSession, 10),
        (pb::ErrorCode::Unauthenticated, 11),
        (pb::ErrorCode::InvalidCredentials, 12),
        (pb::ErrorCode::AccountPending, 13),
        (pb::ErrorCode::AccountDisabled, 14),
        (pb::ErrorCode::RegistrationExists, 15),
        (pb::ErrorCode::ApprovalNotFound, 16),
        (pb::ErrorCode::SetupTokenInvalid, 17),
        (pb::ErrorCode::LdapUnavailable, 18),
        (pb::ErrorCode::TokenExpired, 19),
        (pb::ErrorCode::PatRevoked, 20),
        (pb::ErrorCode::RepoNotFound, 21),
        (pb::ErrorCode::RepoDisabled, 22),
        (pb::ErrorCode::RepoArchived, 23),
        (pb::ErrorCode::RepoNameInvalid, 24),
        (pb::ErrorCode::RepoCreationExists, 25),
        (pb::ErrorCode::RepoApprovalNotFound, 26),
        (pb::ErrorCode::PasswordPolicyViolation, 27),
        (pb::ErrorCode::RepoNotMember, 28),
        (pb::ErrorCode::RepoMemberExists, 29),
        (pb::ErrorCode::RepoMemberNotFound, 30),
        (pb::ErrorCode::RepoOwnerProtected, 31),
        (pb::ErrorCode::RepoMemberForbidden, 32),
        (pb::ErrorCode::UserNotFound, 33),
        (pb::ErrorCode::QuotaConfigInvalid, 34),
        (pb::ErrorCode::IoError, 35),
        (pb::ErrorCode::InvalidArgument, 36),
    ];
    for (code, number) in expected {
        assert_eq!(*code as i32, *number, "错误码编号被改动: {code:?}");
    }
}

#[test]
fn three_services_are_generated() {
    // 只要服务定义还在，下面这些类型就必须存在；顺带证明客户端桩可编译。
    fn _repo(_c: pb::repo_service_client::RepoServiceClient<tonic::transport::Channel>) {}
    fn _auth(_c: pb::auth_service_client::AuthServiceClient<tonic::transport::Channel>) {}
    fn _admin(
        _c: pb::repo_admin_service_client::RepoAdminServiceClient<tonic::transport::Channel>,
    ) {
    }
}

#[test]
fn revision_is_the_only_way_to_locate_a_version() {
    // 语义约束：无分支与标签，rev 是唯一版本定位方式。
    let rev = pb::RevisionInfo {
        rev: 7,
        root_tree: "a".repeat(64),
        parent_rev: 6,
        author: "alice".into(),
        message: "m".into(),
        committed_at: 0,
        change_count: 0,
    };
    assert_eq!(rev.rev, 7);
    let rendered = format!("{rev:?}").to_lowercase();
    assert!(!rendered.contains("branch"), "契约中出现了 branch 字段");
    assert!(!rendered.contains("tag"), "契约中出现了 tag 字段");
}

#[test]
fn quota_source_enum_matches_core() {
    assert_eq!(pb::QuotaSource::RepoOverride as i32, 1);
    assert_eq!(pb::QuotaSource::GlobalDefault as i32, 2);
    assert_eq!(pb::QuotaSource::Fallback as i32, 3);
}
