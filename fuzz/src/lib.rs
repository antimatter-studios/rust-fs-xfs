//! Shared setup for the fuzz targets.
//!
//! Most of these decoders need a superblock before they can do
//! anything: the block size, the inode size and the feature bits all
//! come from it, and a decoder handed a superblock it cannot believe
//! rejects its input on the first line without exercising anything.
//!
//! So the superblock is fixed, taken from the committed corpus, and the
//! fuzzer's bytes go to the structure under test. `Superblock::parse`
//! has its own target where the superblock is what gets mutated.

use std::sync::OnceLock;

use fs_xfs::{ag, Superblock};

const SUPERBLOCK: &[u8] = include_bytes!("../corpus/superblock/mkfs-crc-rmapbt-reflink.bin");
const AGF: &[u8] = include_bytes!("../corpus/agf/mkfs-ag0.bin");

/// The superblock of the image the corpus was cut from.
pub fn superblock() -> &'static Superblock {
    static SB: OnceLock<Superblock> = OnceLock::new();
    SB.get_or_init(|| Superblock::parse(SUPERBLOCK).expect("the committed superblock seed parses"))
}

/// The AG 0 free-space header of the same image, for the one decoder
/// that needs a parsed AGF as well as a superblock.
pub fn agf() -> &'static ag::Agf {
    static AGF_ONCE: OnceLock<ag::Agf> = OnceLock::new();
    AGF_ONCE.get_or_init(|| ag::Agf::parse(AGF, superblock(), 0).expect("the committed AGF parses"))
}

/// Present the fuzzer's bytes as a block of exactly `len`.
///
/// A device hands back a whole sector or a whole filesystem block, so
/// that is what these decoders are called with in the real path. Left
/// to itself libFuzzer would spend most of its budget on lengths no
/// image can produce, and report panics that no image can cause.
///
/// Short input is filled by repeating itself rather than padded with
/// zeros: zeros are a structure the decoders already reject early, and
/// the point is to reach further in than that.
pub fn block(data: &[u8], len: usize) -> Vec<u8> {
    if data.is_empty() {
        return vec![0; len];
    }
    data.iter().copied().cycle().take(len).collect()
}

/// A 512-byte sector: superblock, AG headers, one inode.
pub const SECTOR: usize = 512;

/// A 4096-byte filesystem block: directory blocks, btree blocks.
pub const BLOCK: usize = 4096;
