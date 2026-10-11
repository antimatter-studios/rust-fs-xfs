//! Attribute ABI guards must reject invalid pointers without dereferencing them.
use fs_xfs::capi::*;
use std::ptr;

#[test]
fn attribute_entry_points_reject_null_handles() {
    unsafe {
        assert_eq!(
            fs_xfs_setxattr(ptr::null_mut(), ptr::null(), ptr::null(), ptr::null(), 0, 0),
            -1
        );
        assert_eq!(
            fs_xfs_getxattr(ptr::null(), ptr::null(), ptr::null(), ptr::null_mut(), 0),
            -1
        );
        assert_eq!(
            fs_xfs_listxattr(ptr::null(), ptr::null(), ptr::null_mut(), 0),
            -1
        );
        assert_eq!(
            fs_xfs_removexattr(ptr::null_mut(), ptr::null(), ptr::null()),
            -1
        );
    }
    assert_ne!(fs_xfs_last_errno(), 0);
}
