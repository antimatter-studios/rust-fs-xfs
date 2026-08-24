/*
 * fs_xfs.h — C ABI for the pure-Rust XFS driver.
 *
 * Link against libfs_xfs.a. Every function is thread-safe with respect
 * to distinct handles; a single handle must not be used concurrently
 * from two threads.
 *
 * Error convention: functions returning int return 0 on success and -1
 * on failure. Functions returning a pointer return NULL on failure. In
 * either case fs_xfs_last_error() gives a human-readable message for the
 * calling thread and fs_xfs_last_errno() a POSIX errno suitable for
 * returning to a filesystem client.
 *
 * The driver is read-only. It refuses rather than guesses: a volume
 * whose log needs replaying, an inode on the real-time device, or a
 * B+tree-format fork all produce an error rather than partial data,
 * because silently wrong file contents cannot be detected by a caller.
 */

#ifndef FS_XFS_H
#define FS_XFS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque mounted-filesystem handle. */
typedef struct fs_xfs_fs fs_xfs_fs_t;

/* Opaque directory iterator. */
typedef struct fs_xfs_dir_iter fs_xfs_dir_iter_t;

/* File types, matching the values used by the sibling drivers. */
typedef enum {
    FS_XFS_FT_UNKNOWN = 0,
    FS_XFS_FT_REGULAR = 1,
    FS_XFS_FT_DIRECTORY = 2,
    FS_XFS_FT_CHARDEV = 3,
    FS_XFS_FT_BLOCKDEV = 4,
    FS_XFS_FT_FIFO = 5,
    FS_XFS_FT_SOCKET = 6,
    FS_XFS_FT_SYMLINK = 7
} fs_xfs_file_type_t;

/* Attributes of one filesystem object. */
typedef struct {
    uint64_t inode;
    uint16_t mode;        /* permission bits and type, as on disk */
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    int64_t  atime;       /* unix epoch seconds; may be negative */
    int64_t  mtime;
    int64_t  ctime;
    int64_t  crtime;      /* creation time; 0 on v4 filesystems */
    uint32_t link_count;
    uint32_t file_type;   /* fs_xfs_file_type_t */
} fs_xfs_attr_t;

/* One directory entry. */
typedef struct {
    uint64_t inode;
    uint8_t  file_type;   /* fs_xfs_file_type_t */
    uint8_t  name_len;
    char     name[256];   /* NUL-terminated */
} fs_xfs_dirent_t;

/* Volume-wide information. */
typedef struct {
    uint32_t block_size;
    uint32_t sector_size;
    uint32_t inode_size;
    uint64_t total_blocks;
    uint64_t free_blocks;
    uint64_t inode_count;
    uint64_t free_inodes;
    uint32_t ag_count;
    uint16_t version;          /* 4 or 5 */
    char     volume_name[13];  /* NUL-terminated, <= 12 bytes on disk */
    uint8_t  uuid[16];
    uint32_t feature_compat;
    uint32_t feature_ro_compat;
    uint32_t feature_incompat;
} fs_xfs_volume_info_t;

/*
 * Read callback for mounting over a caller-supplied device.
 *
 * Must fill exactly `length` bytes at `offset` and return 0, or return
 * non-zero on failure. A short read is a failure, not a partial success.
 */
typedef int (*fs_xfs_read_fn)(void *context, void *buf,
                              uint64_t offset, uint64_t length);

typedef struct {
    fs_xfs_read_fn read;
    void          *context;
    uint64_t       size_bytes;  /* total device or partition size */
    uint32_t       block_size;  /* physical block size; informational */
} fs_xfs_blockdev_cfg_t;

/* ---- diagnostics ---- */

/*
 * Message describing the most recent failure on the calling thread.
 * Valid until the next failing call on that thread. Never NULL.
 */
const char *fs_xfs_last_error(void);

/* POSIX errno for the most recent failure on the calling thread. */
int fs_xfs_last_errno(void);

/* ---- mounting ---- */

/* Mount the image or device at `path`. NULL on failure. */
fs_xfs_fs_t *fs_xfs_mount(const char *device_path);

/* Mount over a caller-supplied reader. NULL on failure. */
fs_xfs_fs_t *fs_xfs_mount_with_callbacks(const fs_xfs_blockdev_cfg_t *cfg);

/* Release a handle. Safe to call with NULL. */
void fs_xfs_umount(fs_xfs_fs_t *fs);

/* Fill `out` with volume information. 0 on success, -1 on failure. */
int fs_xfs_get_volume_info(fs_xfs_fs_t *fs, fs_xfs_volume_info_t *out);

/* ---- lookup and metadata ---- */

/*
 * Attributes of `path`. Symbolic links are NOT followed; the attributes
 * describe the link itself.
 */
int fs_xfs_stat(fs_xfs_fs_t *fs, const char *path, fs_xfs_attr_t *out);

/* Attributes of an inode by number. */
int fs_xfs_stat_ino(fs_xfs_fs_t *fs, uint64_t inode, fs_xfs_attr_t *out);

/* ---- directories ---- */

/* Open a directory for iteration. NULL on failure. */
fs_xfs_dir_iter_t *fs_xfs_dir_open(fs_xfs_fs_t *fs, const char *path);

/*
 * Next entry. Returns 1 when `out` was filled, 0 at end of directory,
 * and -1 on failure.
 */
int fs_xfs_dir_next(fs_xfs_dir_iter_t *iter, fs_xfs_dirent_t *out);

/* Release an iterator. Safe to call with NULL. */
void fs_xfs_dir_close(fs_xfs_dir_iter_t *iter);

/* ---- file contents ---- */

/*
 * Read up to `length` bytes of `path` starting at `offset`. Returns the
 * number of bytes read, 0 at end of file, or -1 on failure.
 *
 * Holes and unwritten extents read as zeros. An unwritten extent has
 * blocks allocated that were never written; returning their contents
 * would disclose whatever previously occupied them.
 */
int64_t fs_xfs_read_file(fs_xfs_fs_t *fs, const char *path,
                         uint64_t offset, void *buf, uint64_t length);

/*
 * Target of a symbolic link, NUL-terminated, truncated to `bufsize`.
 * Returns the length written excluding the terminator, or -1.
 */
int fs_xfs_readlink(fs_xfs_fs_t *fs, const char *path,
                    char *buf, size_t bufsize);

#ifdef __cplusplus
}
#endif

#endif /* FS_XFS_H */
