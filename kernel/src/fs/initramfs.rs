//! The initramfs: a ustar archive loaded by Limine as a boot module (see
//! the Makefile), unpacked into the root filesystem at boot. Only regular
//! files and directories are used; anything else is skipped.

use super::FsError;
use alloc::string::String;

const BLOCK: usize = 512;

/// One tar header field: bytes up to the first NUL.
fn field(header: &[u8], start: usize, len: usize) -> &[u8] {
    let f = &header[start..start + len];
    &f[..f.iter().position(|&b| b == 0).unwrap_or(len)]
}

fn octal(bytes: &[u8]) -> Option<usize> {
    let digits = core::str::from_utf8(bytes).ok()?.trim_matches(|c: char| c == ' ' || c == '\0');
    usize::from_str_radix(digits, 8).ok()
}

/// Unpack `archive` into `/`. Returns the number of files created.
pub fn unpack(archive: &[u8]) -> Result<usize, &'static str> {
    let mut files = 0;
    let mut at = 0;
    while at + BLOCK <= archive.len() {
        let header = &archive[at..at + BLOCK];
        if header.iter().all(|&b| b == 0) {
            break; // end-of-archive marker
        }
        if &header[257..262] != b"ustar" {
            return Err("not a ustar archive");
        }
        let size = octal(field(header, 124, 12)).ok_or("bad size field")?;
        let mut path = String::new();
        for part in [field(header, 345, 155), field(header, 0, 100)] {
            if !part.is_empty() {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(core::str::from_utf8(part).map_err(|_| "file name is not UTF-8")?);
            }
        }
        let data_start = at + BLOCK;
        let data_end = data_start.checked_add(size).filter(|&e| e <= archive.len()).ok_or("truncated archive")?;
        let path = path.trim_start_matches("./").trim_end_matches('/');
        if !path.is_empty() {
            let path = alloc::format!("/{}", path);
            match header[156] {
                b'0' | 0 => {
                    super::write_file(&path, &archive[data_start..data_end]).map_err(|_| "cannot create file")?;
                    files += 1;
                }
                b'5' => match super::mkdir("/", &path) {
                    Ok(()) | Err(FsError::Exists) => {}
                    Err(_) => return Err("cannot create directory"),
                },
                _ => {}
            }
        }
        at = data_start + size.div_ceil(BLOCK) * BLOCK;
    }
    Ok(files)
}
