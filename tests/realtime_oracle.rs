//! Files on a realtime section read back as the kernel reads them (#98).
//!
//! A volume made with `mkfs.xfs -r rtdev=...` keeps realtime files' data on
//! a second device, addressed in filesystem blocks from its start. The
//! inodes, the directories and the realtime files' maps stay on the data
//! device. Before this, the driver held one device: a realtime volume
//! mounted and listed, and every read of a realtime file failed with the
//! same refusal as a feature it did not have.
//!
//! The kernel builds a volume with `rtinherit`, so every file made in it is
//! realtime unless it is told otherwise, and writes:
//!
//! - `small`, a few bytes;
//! - `sparse`, pieces with holes between them;
//! - `large`, several MiB;
//! - `data`, a file made with the realtime flag cleared before it was
//!   written, so it lives on the data device beside the rest.
//!
//! The kernel reports each file's `xflags`, which say which device it is on,
//! and its SHA-256. Then:
//!
//! - `mount_with_realtime` with both devices must read every file to the
//!   kernel's digest;
//! - `mount` with the data device alone must still list the volume and read
//!   `data`, and must refuse each realtime file with
//!   `Error::RealtimeDeviceAbsent`, not with a generic refusal;
//! - a realtime device smaller than the section the superblock describes
//!   must be refused at mount.

mod common;

use common::{kernel_run, scratch, share};

/// Where this suite's scratch volumes live, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "realtime_oracle";

use fs_core::{BlockRead, FileDevice};
use fs_xfs::{Error, Filesystem};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The files, and whether each is realtime.
const FILES: [(&str, bool); 4] = [
    ("small", true),
    ("sparse", true),
    ("large", true),
    ("data", false),
];

fn open(path: &std::path::Path) -> Arc<dyn BlockRead> {
    Arc::new(FileDevice::open(path).expect("open a volume")) as Arc<dyn BlockRead>
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn files_on_a_realtime_section_read_back_as_the_kernel_reads_them() {
    // THE SHARED DIRECTORY IS ALWAYS THERE. `chore fixtures` makes it
    // before anything else runs, and this test writes its scratch volumes
    // beside the fixtures. An absent share is that build not having
    // happened, which has to be seen rather than skipped.
    assert!(
        share().is_dir(),
        "{} is not there: the fixtures are gitignored and generated, and this test \
         writes its scratch volumes beside them. `chore fixtures` builds the set and \
         makes the directory. Tests never skip on a missing fixture.",
        share().display()
    );
    let pid = std::process::id();
    let data = scratch::Volume::empty(SUITE, &format!("{pid}-data.img"), 400 * 1024 * 1024);
    let rt = scratch::Volume::empty(SUITE, &format!("{pid}-rt.img"), 64 * 1024 * 1024);
    let small_rt = scratch::Volume::empty(SUITE, &format!("{pid}-small.img"), 1024 * 1024);
    let (data_name, rt_name) = (data.guest(), rt.guest());

    let reports: String = FILES
        .iter()
        .map(|(f, _)| {
            format!(
                r#"
        echo "XFLAGS {f} $(xfs_io -r -c 'stat' "$m/{f}" | sed -n 's/^fsxattr.xflags = //p')"
        echo "SHA {f} $(sha256sum < "$m/{f}" | cut -d' ' -f1)"
        "#
            )
        })
        .collect();

    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -r rtdev={rt_name} -d rtinherit=1 {data_name} 2>&1 && echo MKFS_OK
        loop=$(losetup -f --show {rt_name})
        m=$(mktemp -d)
        mount -o loop,rtdev="$loop" {data_name} "$m" && echo MOUNT_OK
        printf 'a realtime file\n' > "$m/small"
        for i in 0 3 7 20; do
            xfs_io -f -c "pwrite -q -S 0x$((i + 17)) $((i * 65536)) 4096" "$m/sparse"
        done
        head -c 5000000 /dev/urandom > "$m/large"
        xfs_io -f -c 'chattr -r' "$m/data"
        head -c 300000 /dev/urandom > "$m/data"
        sync
        {reports}
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        losetup -d "$loop"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the realtime volume failed:\n{built}"
    );
    let report = |key: &str, f: &str| -> String {
        built
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key} {f} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key} for {f}:\n{built}"))
            .trim()
            .to_string()
    };

    // WHICH DEVICE EACH FILE IS ON, from the kernel.
    for (f, realtime) in FILES {
        let xflags = report("XFLAGS", f);
        assert_eq!(
            xflags.contains("[r"),
            realtime,
            "{f}: the kernel's xflags are {xflags}, so it is not on the device this case \
             meant"
        );
    }

    // BOTH DEVICES: every file, byte for byte.
    let fs = Filesystem::mount_with_realtime(open(data.path()), open(rt.path()))
        .expect("mount the volume with its realtime device");
    for (f, realtime) in FILES {
        let (inode, raw) = fs
            .lookup_path(&format!("/{f}"))
            .and_then(|i| fs.read_inode_raw(i.ino))
            .expect("the file");
        assert_eq!(
            inode.is_realtime(),
            realtime,
            "{f}: the driver's realtime flag"
        );
        let bytes = fs
            .read_file(&inode, &raw)
            .unwrap_or_else(|e| panic!("{f}: reading it with the realtime device: {e}"));
        assert_eq!(
            sha256_hex(&bytes),
            report("SHA", f),
            "{f}: {} bytes read, and they are not the bytes the kernel reads",
            bytes.len()
        );
    }
    drop(fs);

    // THE DATA DEVICE ALONE: everything but realtime file data, and that
    // refused by name.
    let fs = Filesystem::mount(open(data.path())).expect("mount the data device alone");
    let listed = fs
        .root()
        .expect("the root lists without the realtime device");
    assert!(!listed.is_empty(), "the root listed nothing");
    for (f, realtime) in FILES {
        let (inode, raw) = fs
            .lookup_path(&format!("/{f}"))
            .and_then(|i| fs.read_inode_raw(i.ino))
            .expect("the file");
        let read = fs.read_file(&inode, &raw);
        if realtime {
            assert_eq!(
                read.map(|b| b.len()),
                Err(Error::RealtimeDeviceAbsent { ino: inode.ino }),
                "{f}: a realtime file without the realtime device"
            );
        } else {
            let bytes = read.unwrap_or_else(|e| panic!("{f}: {e}"));
            assert_eq!(sha256_hex(&bytes), report("SHA", f), "{f}");
        }
    }
    drop(fs);

    // A REALTIME DEVICE TOO SMALL for the section is not the volume's.
    let refused = Filesystem::mount_with_realtime(open(data.path()), open(small_rt.path()));
    assert!(
        matches!(refused, Err(Error::BadSuperblock(_))),
        "a 1 MiB realtime device for a 64 MiB section was accepted: {:?}",
        refused.map(|_| ())
    );

    // THE C ABI (#291): the same reads, through both of its realtime
    // mounts, and the same refusals, as a C caller sees them.
    let digests: Vec<(&str, String)> = FILES.iter().map(|(f, _)| (*f, report("SHA", f))).collect();
    capi::reads_through_both_realtime_mounts(data.path(), rt.path(), &digests);
    capi::refuses_as_the_rust_api_does(data.path(), small_rt.path(), &digests);
}

/// The realtime volume read through the C ABI (#291).
mod capi {
    use super::{sha256_hex, FILES};
    use fs_xfs::capi::*;
    use std::ffi::{c_int, c_void, CStr, CString};
    use std::os::unix::fs::FileExt;
    use std::path::Path;

    /// Errno values the header documents, spelled out rather than taken
    /// from the source so the test asserts the contract.
    const EIO: i32 = 5;
    const ENXIO: i32 = 6;

    fn cstr(path: &Path) -> CString {
        CString::new(path.to_str().expect("a UTF-8 scratch path")).expect("no NUL")
    }

    fn last_error() -> String {
        unsafe { CStr::from_ptr(fs_xfs_last_error()) }
            .to_string_lossy()
            .into_owned()
    }

    /// A caller-supplied reader over a file, as a C caller would write it.
    struct Reader {
        file: std::fs::File,
    }

    unsafe extern "C" fn read_cb(
        ctx: *mut c_void,
        buf: *mut c_void,
        offset: u64,
        length: u64,
    ) -> c_int {
        let reader = unsafe { &*(ctx as *const Reader) };
        let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), length as usize) };
        match reader.file.read_exact_at(out, offset) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }

    /// A callback configuration over `path`, and the reader it points at,
    /// which the caller frees once the handle is released.
    fn callbacks(path: &Path) -> (fs_xfs_blockdev_cfg_t, *mut Reader) {
        let file = std::fs::File::open(path).expect("open a scratch volume");
        let size = file.metadata().expect("its size").len();
        let reader = Box::into_raw(Box::new(Reader { file }));
        let cfg = fs_xfs_blockdev_cfg_t {
            read: Some(read_cb),
            context: reader.cast::<c_void>(),
            size_bytes: size,
            block_size: 512,
        };
        (cfg, reader)
    }

    /// Every byte of `path` through `fs_xfs_read_file`, or the errno it
    /// failed with.
    fn read_all(fs: *mut fs_xfs_fs, path: &str) -> Result<Vec<u8>, i32> {
        let c = CString::new(path).expect("no NUL");
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = unsafe {
                fs_xfs_read_file(
                    fs,
                    c.as_ptr(),
                    buf.as_mut_ptr().cast::<c_void>(),
                    out.len() as u64,
                    buf.len() as u64,
                )
            };
            match n {
                -1 => return Err(fs_xfs_last_errno()),
                0 => return Ok(out),
                n => out.extend_from_slice(&buf[..n as usize]),
            }
        }
    }

    fn assert_reads_every_file(fs: *mut fs_xfs_fs, how: &str, digests: &[(&str, String)]) {
        assert!(!fs.is_null(), "{how}: the mount failed: {}", last_error());
        for (f, digest) in digests {
            let bytes = read_all(fs, &format!("/{f}"))
                .unwrap_or_else(|errno| panic!("{how}: /{f}: errno {errno}: {}", last_error()));
            assert_eq!(
                &sha256_hex(&bytes),
                digest,
                "{how}: /{f}: {} bytes read, and they are not the bytes the kernel reads",
                bytes.len()
            );
        }
        unsafe { fs_xfs_umount(fs) };
    }

    pub fn reads_through_both_realtime_mounts(data: &Path, rt: &Path, digests: &[(&str, String)]) {
        let (data_c, rt_c) = (cstr(data), cstr(rt));
        let fs = unsafe { fs_xfs_mount_with_realtime(data_c.as_ptr(), rt_c.as_ptr()) };
        assert_reads_every_file(fs, "fs_xfs_mount_with_realtime", digests);

        let (data_cfg, data_reader) = callbacks(data);
        let (rt_cfg, rt_reader) = callbacks(rt);
        let fs = unsafe { fs_xfs_mount_with_realtime_callbacks(&data_cfg, &rt_cfg) };
        assert_reads_every_file(fs, "fs_xfs_mount_with_realtime_callbacks", digests);
        drop(unsafe { Box::from_raw(data_reader) });
        drop(unsafe { Box::from_raw(rt_reader) });
    }

    pub fn refuses_as_the_rust_api_does(data: &Path, small_rt: &Path, digests: &[(&str, String)]) {
        let data_c = cstr(data);

        // Without the realtime device: realtime data is ENXIO, which a
        // client can tell apart from a feature the driver lacks, and the
        // file on the data device still reads.
        let fs = unsafe { fs_xfs_mount(data_c.as_ptr()) };
        assert!(!fs.is_null(), "fs_xfs_mount: {}", last_error());
        for ((f, realtime), (_, digest)) in FILES.iter().zip(digests) {
            match read_all(fs, &format!("/{f}")) {
                Err(errno) if *realtime => assert_eq!(errno, ENXIO, "/{f}: {}", last_error()),
                Ok(bytes) if !*realtime => assert_eq!(&sha256_hex(&bytes), digest, "/{f}"),
                other => panic!(
                    "/{f} (realtime: {realtime}) through fs_xfs_mount: {:?}",
                    other.map(|b| b.len())
                ),
            }
        }
        unsafe { fs_xfs_umount(fs) };

        // A realtime device smaller than the section is not the volume's,
        // through either entry point.
        let small_c = cstr(small_rt);
        let fs = unsafe { fs_xfs_mount_with_realtime(data_c.as_ptr(), small_c.as_ptr()) };
        assert!(
            fs.is_null(),
            "fs_xfs_mount_with_realtime took a 1 MiB realtime device"
        );
        assert_eq!(fs_xfs_last_errno(), EIO, "{}", last_error());

        let (data_cfg, data_reader) = callbacks(data);
        let (small_cfg, small_reader) = callbacks(small_rt);
        let fs = unsafe { fs_xfs_mount_with_realtime_callbacks(&data_cfg, &small_cfg) };
        assert!(
            fs.is_null(),
            "fs_xfs_mount_with_realtime_callbacks took a 1 MiB realtime device"
        );
        assert_eq!(fs_xfs_last_errno(), EIO, "{}", last_error());
        drop(unsafe { Box::from_raw(data_reader) });
        drop(unsafe { Box::from_raw(small_reader) });

        // No realtime device named at all is a caller's mistake, not a
        // request for fs_xfs_mount.
        let fs = unsafe { fs_xfs_mount_with_realtime(data_c.as_ptr(), std::ptr::null()) };
        assert!(fs.is_null(), "a NULL realtime path mounted");
        let (data_cfg, data_reader) = callbacks(data);
        let fs = unsafe { fs_xfs_mount_with_realtime_callbacks(&data_cfg, std::ptr::null()) };
        assert!(fs.is_null(), "a NULL realtime configuration mounted");
        drop(unsafe { Box::from_raw(data_reader) });
    }
}
