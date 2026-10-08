//! Kernel-made quota fixtures shared by the record and checker oracles.

use crate::common::{guest_quote, scratch};

pub const CASES: &[(&str, &str, &str)] = &[
    ("v5-all", "-m crc=1,bigtime=1", "usrquota,grpquota,prjquota"),
    (
        "v5-1k-legacy",
        "-m crc=1,bigtime=0 -b size=1024",
        "usrquota,grpquota,prjquota",
    ),
    ("v4-user-group", "-m crc=0", "usrquota,grpquota"),
    ("v4-user-project", "-m crc=0", "usrquota,prjquota"),
];

/// Build on the guest's local disk, then publish the unmounted image into
/// this suite's scratch directory. Quota inodes have size zero; the high
/// project ID ensures their extent maps are sparse.
pub fn build(
    suite: &str,
    label: &str,
    geometry: &str,
    options: &str,
    run: impl FnOnce(&str) -> String,
) -> scratch::Volume {
    let image = scratch::Volume::empty(
        suite,
        &format!("{}-{label}.img", std::process::id()),
        400 << 20,
    );
    let output = run(&format!(
        r#"
set -euo pipefail
d=$(mktemp -d)
mounted=0
cleanup() {{
    if [ "$mounted" = 1 ]; then umount "$d/m" || {{ echo 'quota fixture unmount failed' >&2; exit 1; }}; fi
    rm -rf "$d"
}}
trap cleanup EXIT
mkdir "$d/m"
truncate -s 400M "$d/image"
mkfs.xfs -f -q {geometry} "$d/image"
mount -o loop,{options} "$d/image" "$d/m"
mounted=1
mkdir "$d/m/project"
xfs_io -c 'chproj 65553' -c 'chattr +P' "$d/m/project"
xfs_io -f -c 'pwrite -S 0x51 0 16384' "$d/m/project/file" >/dev/null
chown 17:18 "$d/m/project/file"
for kind in u g p; do
    case "$kind" in u) id=17; enabled=usrquota;; g) id=18; enabled=grpquota;; p) id=65553; enabled=prjquota;; esac
    case ',{options},' in *,$enabled,*)
        xfs_quota -x -c "limit -$kind bsoft=1m bhard=2m isoft=10 ihard=20 $id" "$d/m"
        xfs_quota -x -c "report -$kind -n -b -i" "$d/m"
    ;; esac
done
sync
umount "$d/m" || {{ echo 'quota fixture unmount failed' >&2; exit 1; }}
mounted=0
cp --sparse=always "$d/image" {destination}
echo QUOTA_FIXTURE_BUILT
"#,
        destination = guest_quote(&image.guest())
    ));
    assert!(
        output.contains("QUOTA_FIXTURE_BUILT"),
        "quota fixture {label} did not finish:\n{output}"
    );
    image
}
