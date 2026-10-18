use zerocopy::{little_endian as le, FromBytes, Immutable, IntoBytes, KnownLayout};

/// ZRC directory record representing an embedded asset file in a Zoom `.zrc` archive.
///
/// Each record is 528 bytes (0x210):
/// - 512 bytes: null-terminated ASCII file name
/// - 4 bytes: little-endian offset
/// - 4 bytes: little-endian size
/// - 8 bytes: flags and reserved metadata
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
#[repr(C)]
pub struct ZrcRecord {
    pub name: [u8; 512],
    pub offset: le::U32,
    pub size: le::U32,
    pub flags: le::U16,
    pub unk: le::U16,
    pub reserved: le::U32,
}

impl ZrcRecord {
    /// Returns the null-terminated file name as a string slice.
    #[must_use]
    pub fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
    }

    /// Returns the offset into the archive payload.
    #[must_use]
    pub const fn file_offset(&self) -> usize {
        self.offset.get() as usize
    }

    /// Returns the file size in bytes.
    #[must_use]
    pub const fn file_size(&self) -> usize {
        self.size.get() as usize
    }
}
