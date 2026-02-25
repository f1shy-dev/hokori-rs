use crate::{FileType, RawDirEntry, SysError};
use std::mem::size_of;

pub const BULK_BUF_SIZE: usize = 128 * 1024;

const ATTR_CMN_ERROR: u32 = 0x20000000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy)]
struct AttrReference {
    attr_dataoffset: i32,
    attr_length: u32,
}

fn obj_type_to_file_type(obj_type: u32) -> FileType {
    match obj_type {
        VREG => FileType::File,
        VDIR => FileType::Directory,
        VLNK => FileType::Symlink,
        _ => FileType::Other,
    }
}

pub fn read_dir_raw(
    fd: i32,
    buf: &mut [u8],
    callback: &mut dyn FnMut(RawDirEntry),
) -> Result<(), SysError> {
    let mut attrlist = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT as u16,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };

    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                fd,
                &mut attrlist as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };

        if n == 0 {
            break;
        }
        if n < 0 {
            return Err(SysError::Io(std::io::Error::last_os_error()));
        }

        parse_bulk_buffer(buf, n as usize, callback);
    }

    Ok(())
}

fn parse_bulk_buffer(buf: &[u8], count: usize, callback: &mut dyn FnMut(RawDirEntry)) {
    let mut pos = 0usize;
    for _ in 0..count {
        if pos + 4 > buf.len() {
            break;
        }

        let entry_len = u32::from_ne_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if entry_len == 0 || pos + entry_len > buf.len() {
            break;
        }

        let entry = &buf[pos..pos + entry_len];
        if entry.len() < 24 {
            pos += entry_len;
            continue;
        }

        let commonattr = u32::from_ne_bytes(entry[4..8].try_into().unwrap());
        let fileattr = u32::from_ne_bytes(entry[16..20].try_into().unwrap());

        let mut field_pos = 24usize;
        let mut name_ref: Option<AttrReference> = None;
        let mut error_code: Option<u32> = None;
        let mut obj_type: Option<u32> = None;
        let mut file_id: Option<u64> = None;
        let mut alloc_size: Option<u64> = None;

        if (commonattr & libc::ATTR_CMN_NAME) != 0 {
            if field_pos + size_of::<AttrReference>() <= entry.len() {
                let ptr = unsafe { entry.as_ptr().add(field_pos) as *const AttrReference };
                name_ref = Some(unsafe { ptr.read_unaligned() });
            }
            field_pos += size_of::<AttrReference>();
        }

        if (commonattr & ATTR_CMN_ERROR) != 0 {
            if field_pos + 4 <= entry.len() {
                error_code = Some(u32::from_ne_bytes(
                    entry[field_pos..field_pos + 4].try_into().unwrap(),
                ));
            }
            field_pos += 4;
        }

        if (commonattr & libc::ATTR_CMN_OBJTYPE) != 0 {
            if field_pos + 4 <= entry.len() {
                obj_type = Some(u32::from_ne_bytes(
                    entry[field_pos..field_pos + 4].try_into().unwrap(),
                ));
            }
            field_pos += 4;
        }

        if (commonattr & libc::ATTR_CMN_FILEID) != 0 {
            if field_pos + 8 <= entry.len() {
                file_id = Some(u64::from_ne_bytes(
                    entry[field_pos..field_pos + 8].try_into().unwrap(),
                ));
            }
            field_pos += 8;
        }

        if (fileattr & libc::ATTR_FILE_ALLOCSIZE) != 0 {
            if field_pos + 8 <= entry.len() {
                alloc_size = Some(u64::from_ne_bytes(
                    entry[field_pos..field_pos + 8].try_into().unwrap(),
                ));
            }
        }

        if let Some(code) = error_code {
            if code != 0 {
                pos += entry_len;
                continue;
            }
        }

        let name_ref = match name_ref {
            Some(n) => n,
            None => {
                pos += entry_len;
                continue;
            }
        };

        let ref_offset = 24usize;
        let name_start_signed = ref_offset as isize + name_ref.attr_dataoffset as isize;
        if name_start_signed < 0 {
            pos += entry_len;
            continue;
        }
        let name_start = name_start_signed as usize;
        let name_len = name_ref.attr_length as usize;
        if name_start >= entry.len() || name_start + name_len > entry.len() {
            pos += entry_len;
            continue;
        }

        let raw_name = &entry[name_start..name_start + name_len];
        let nul = raw_name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(raw_name.len());
        let name = &raw_name[..nul];
        if name == b"." || name == b".." {
            pos += entry_len;
            continue;
        }

        let kind = obj_type.map_or(FileType::Unknown, obj_type_to_file_type);
        let ino = file_id.unwrap_or(0);
        let size = if kind == FileType::File {
            alloc_size
        } else {
            None
        };

        callback(RawDirEntry {
            // PERF: RawDirEntry keeps owning `Vec<u8>` names to avoid threading short-lived
            // borrowed lifetimes through public callback APIs across crates.
            name: name.to_vec(),
            file_type: kind,
            ino,
            size,
        });

        pos += entry_len;
    }
}
