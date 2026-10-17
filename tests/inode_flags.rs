//! Public inode flag masks agree with the independently measured log core.

use fs_xfs::format::log_items::log_dinode::flags::DI_FLAG_EXTSIZE;
use fs_xfs::inode::flags;

#[test]
fn the_public_extent_size_flag_matches_the_measured_inode_flag() {
    assert_eq!(
        flags::EXTSIZE,
        DI_FLAG_EXTSIZE,
        "the public mask must select the extent-size hint recorded by the kernel"
    );
}
