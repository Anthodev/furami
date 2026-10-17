//! Validated local Y4M input; the owned descriptor remains alive in the media owner.
use crate::domain::capture::{FrameRate, FrameSize};
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
};

#[derive(Debug, thiserror::Error)]
pub enum QualificationVideoError {
    #[error("qualification_path: expected an absolute readable regular file")]
    Path,
    #[error("qualification_format: invalid YUV4MPEG2 header")]
    Format,
}

#[derive(Debug)]
pub struct QualificationVideo {
    // Never reopen the caller's pathname after validation.
    _file: File,
    load_path: CString,
    size: FrameSize,
    rate: FrameRate,
}
impl QualificationVideo {
    pub fn open(path: &Path) -> Result<Self, QualificationVideoError> {
        use QualificationVideoError::{Format, Path as InvalidPath};
        if !path.is_absolute() {
            return Err(InvalidPath);
        }
        // Linux O_PATH pins the inode without opening a device or waiting on a
        // FIFO. Reopen only a proven regular inode through its descriptor, not
        // through the caller's mutable pathname. No libc dependency is needed.
        const O_PATH: i32 = 0o10000000;
        const O_CLOEXEC: i32 = 0o2000000;
        let inode = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_CLOEXEC)
            .open(path)
            .map_err(|_| InvalidPath)?;
        if !inode.metadata().map_err(|_| InvalidPath)?.is_file() {
            return Err(InvalidPath);
        }
        let mut file =
            File::open(format!("/proc/self/fd/{}", inode.as_raw_fd())).map_err(|_| InvalidPath)?;
        let mut header = [0u8; 4096];
        let mut length = 0;
        while length < header.len() {
            if file
                .read(&mut header[length..length + 1])
                .map_err(|_| Format)?
                != 1
            {
                return Err(Format);
            }
            length += 1;
            if header[length - 1] == b'\n' {
                break;
            }
        }
        if length == 0 || header[length - 1] != b'\n' {
            return Err(Format);
        }
        let text = std::str::from_utf8(&header[..length]).map_err(|_| Format)?;
        let mut tokens = text.trim_end_matches('\n').split(' ');
        if tokens.next() != Some("YUV4MPEG2") {
            return Err(Format);
        }
        let (mut width, mut height, mut rate, mut chroma, mut interlace, mut aspect) =
            (None, None, None, false, false, false);
        let number = |value: &str| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Format);
            }
            value.parse::<u32>().map_err(|_| Format)
        };
        for token in tokens {
            let (key, value) = token.split_at_checked(1).ok_or(Format)?;
            match key {
                "W" if width.is_none() => width = Some(number(value)?),
                "H" if height.is_none() => height = Some(number(value)?),
                "F" if rate.is_none() => {
                    let (n, d) = value.split_once(':').ok_or(Format)?;
                    rate = Some(FrameRate::new(number(n)?, number(d)?).map_err(|_| Format)?);
                }
                "C" if !chroma => {
                    let valid = matches!(
                        value,
                        "420jpeg"
                            | "420"
                            | "420mpeg2"
                            | "420paldv"
                            | "411"
                            | "422"
                            | "444"
                            | "444alpha"
                            | "mono"
                    ) || ["420p", "422p", "444p", "mono"].iter().any(|prefix| {
                        value
                            .strip_prefix(prefix)
                            .is_some_and(|depth| matches!(depth, "9" | "10" | "12" | "14" | "16"))
                    });
                    if !valid {
                        return Err(Format);
                    }
                    chroma = true;
                }
                "I" if !interlace && matches!(value, "p" | "t" | "b" | "m" | "?") => {
                    interlace = true
                }
                "A" if !aspect => {
                    let (n, d) = value.split_once(':').ok_or(Format)?;
                    let (n, d) = (number(n)?, number(d)?);
                    if (n == 0) != (d == 0) {
                        return Err(Format);
                    }
                    aspect = true;
                }
                "X" if !value.is_empty() && value.is_ascii() => {}
                _ => return Err(Format),
            }
        }
        let size =
            FrameSize::new(width.ok_or(Format)?, height.ok_or(Format)?).map_err(|_| Format)?;
        let rate = rate.ok_or(Format)?;
        file.seek(SeekFrom::Start(0)).map_err(|_| InvalidPath)?;
        let load_path =
            CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|_| InvalidPath)?;
        Ok(Self {
            _file: file,
            load_path,
            size,
            rate,
        })
    }
    pub fn load_path(&self) -> &CStr {
        &self.load_path
    }
    pub fn size(&self) -> FrameSize {
        self.size
    }
    pub fn rate(&self) -> FrameRate {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_relative_path_without_opening() {
        assert!(matches!(
            QualificationVideo::open(Path::new("fixture.y4m")),
            Err(QualificationVideoError::Path)
        ));
    }
    #[test]
    fn descriptor_survives_path_replacement_and_non_utf8() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let dir = std::env::temp_dir().join(format!("furami-y4m-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(std::ffi::OsString::from_vec(b"fixture-\xff.y4m".to_vec()));
        std::fs::write(
            &path,
            b"YUV4MPEG2 W320 H180 F30:1 Ip A1:1 C420jpeg\nFRAME\n",
        )
        .unwrap();
        let video = QualificationVideo::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(video.size(), FrameSize::new(320, 180).unwrap());
        assert_eq!(video.rate(), FrameRate::new(30, 1).unwrap());
        assert!(
            std::fs::read(std::path::Path::new(std::ffi::OsStr::from_bytes(
                video.load_path().to_bytes()
            )))
            .unwrap()
            .starts_with(b"YUV4MPEG2")
        );
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn invalid_y4m_headers_are_refused_without_paths_in_error() {
        let path = std::env::temp_dir().join(format!("furami-invalid-y4m-{}", std::process::id()));
        for bytes in [
            b"not a video\n".as_slice(),
            b"YUV4MPEG2 W320 H180 F0:1 Ip C420jpeg\n".as_slice(),
            b"YUV4MPEG2 W320 H180 F30:0 Ip C420jpeg\n".as_slice(),
            b"YUV4MPEG2 W320 W640 H180 F30:1 Ip C420jpeg\n".as_slice(),
            b"YUV4MPEG2 W320 H180 F30:1 Ip Ainvalid C420jpeg\n".as_slice(),
            b"YUV4MPEG2 W320 H180 F30:1 Ip C420jpeg".as_slice(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            let error = QualificationVideo::open(&path).unwrap_err();
            assert!(matches!(error, QualificationVideoError::Format));
            assert!(!error.to_string().contains(path.to_str().unwrap()));
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn valid_nonfixture_y4m_headers_are_not_restricted_to_even_420() {
        let path = std::env::temp_dir().join(format!("furami-valid-y4m-{}", std::process::id()));
        for header in [
            b"YUV4MPEG2 W321 H181 F30:1 Ip\n".as_slice(),
            b"YUV4MPEG2 W320 H180 F30:1 It A0:0 C444\n".as_slice(),
            b"YUV4MPEG2 W320 H180 F30000:1001 Ip C422p10\n".as_slice(),
        ] {
            std::fs::write(&path, header).unwrap();
            assert!(QualificationVideo::open(&path).is_ok());
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn fifo_without_a_writer_is_rejected_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
        }
        let path = std::env::temp_dir().join(format!("furami-fifo-y4m-{}", std::process::id()));
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: Valid NUL-terminated temporary pathname and Linux mode_t.
        assert_eq!(unsafe { mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            QualificationVideo::open(&path),
            Err(QualificationVideoError::Path)
        ));
        std::fs::remove_file(path).unwrap();
    }
}
