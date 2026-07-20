#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::{self, Read as _};
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};

#[cfg(target_os = "linux")]
pub(crate) struct RawTtyInput {
    descriptor: tokio::io::unix::AsyncFd<File>,
}

#[cfg(target_os = "linux")]
impl RawTtyInput {
    pub(crate) fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK)
            .open("/dev/tty")?;
        if !file.metadata()?.file_type().is_char_device() {
            return Err(io::Error::other("/dev/tty is not a character device"));
        }
        Ok(Self {
            descriptor: tokio::io::unix::AsyncFd::new(file)?,
        })
    }

    pub(crate) async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.descriptor.readable().await?;
            match ready.try_io(|descriptor| {
                let mut file = descriptor.get_ref();
                file.read(buffer)
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}
