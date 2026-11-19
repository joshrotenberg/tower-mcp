use std::ffi::OsString;
use std::io;
use std::mem::offset_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use cap_std::fs::{Dir, Metadata, MetadataExt};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FULL_DIR_INFO, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    GetFileInformationByHandleEx,
};

use super::{EntryKind, SkillError, io_error};

const SKIPPED_ATTRIBUTES: u32 = FILE_ATTRIBUTE_DEVICE | FILE_ATTRIBUTE_REPARSE_POINT;

pub(super) fn check_attributes(metadata: &Metadata) -> io::Result<()> {
    if metadata.file_attributes() & SKIPPED_ATTRIBUTES != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected an ordinary file or directory",
        ));
    }
    Ok(())
}

pub(super) fn directory_entries(
    dir: &Dir,
    display: &Path,
) -> Result<Vec<(OsString, EntryKind)>, SkillError> {
    // cap-std's Windows Dir::entries reopens a pathname. Query the original
    // directory handle so enumeration and subsequent relative opens share it.
    #[repr(align(8))]
    struct Buffer([u8; 16 * 1024]);
    let mut buffer = Buffer([0; 16 * 1024]);
    let mut entries = Vec::new();
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: The directory handle is live. The buffer is writable,
        // appropriately aligned for FILE_FULL_DIR_INFO, and sized in bytes.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.as_raw_handle(),
                class,
                buffer.0.as_mut_ptr().cast(),
                buffer.0.len() as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            let code = error.raw_os_error().map(|code| code as u32);
            // An empty initial query may be reported as no matching file.
            if code == Some(ERROR_NO_MORE_FILES)
                || (class == FileFullDirectoryRestartInfo && code == Some(ERROR_FILE_NOT_FOUND))
            {
                return Ok(entries);
            }
            return Err(io_error(display, error));
        }
        append_entries(&buffer.0, &mut entries).map_err(|source| io_error(display, source))?;
        class = FileFullDirectoryInfo;
    }
}

fn append_entries(mut data: &[u8], entries: &mut Vec<(OsString, EntryKind)>) -> io::Result<()> {
    const NAME_OFFSET: usize = offset_of!(FILE_FULL_DIR_INFO, FileName);
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid directory record");
    loop {
        if data.len() < NAME_OFFSET {
            return Err(invalid());
        }
        let field = |offset| u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap());
        let next = field(offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset)) as usize;
        let attributes = field(offset_of!(FILE_FULL_DIR_INFO, FileAttributes));
        let name_len = field(offset_of!(FILE_FULL_DIR_INFO, FileNameLength)) as usize;
        let record_len = if next == 0 { data.len() } else { next };
        if name_len == 0 || !name_len.is_multiple_of(2) || record_len > data.len() {
            return Err(invalid());
        }
        let name = data[..record_len]
            .get(NAME_OFFSET..)
            .and_then(|bytes| bytes.get(..name_len))
            .ok_or_else(invalid)?;
        if attributes & SKIPPED_ATTRIBUTES == 0 && !name.starts_with(&[b'.', 0]) {
            let wide: Vec<_> = name
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                .collect();
            if wide.contains(&u16::from(b':')) {
                return Err(invalid());
            }
            let kind = if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            entries.push((OsString::from_wide(&wide), kind));
        }
        if next == 0 {
            return Ok(());
        }
        data = &data[next..];
    }
}
