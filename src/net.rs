use std::fs::File;
use std::io::{self, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

const MAX_SENDFILE_CHUNK: usize = 1 << 30;
const FALLBACK_BUFFER_SIZE: usize = 64 * 1024;

pub fn wait_readable(stream: &TcpStream, deadline: Instant) -> io::Result<()> {
    wait_fd(stream.as_raw_fd(), libc::POLLIN, deadline)
}

pub fn wait_writable(stream: &TcpStream, deadline: Instant) -> io::Result<()> {
    wait_fd(stream.as_raw_fd(), libc::POLLOUT, deadline)
}

pub fn write_all(stream: &mut TcpStream, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    write_all_shared(stream, bytes, deadline)
}

fn write_all_shared(stream: &TcpStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let mut writer = stream;
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_writable(stream, deadline)?
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn send_file(
    stream: &TcpStream,
    file: &File,
    length: u64,
    deadline: Instant,
) -> io::Result<u64> {
    let mut offset: libc::off_t = 0;
    let mut sent = 0_u64;

    while sent < length {
        let remaining = (length - sent).min(MAX_SENDFILE_CHUNK as u64) as usize;
        let result =
            unsafe { libc::sendfile(stream.as_raw_fd(), file.as_raw_fd(), &mut offset, remaining) };

        if result > 0 {
            sent += result as u64;
            continue;
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file changed while being sent",
            ));
        }

        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => wait_writable(stream, deadline)?,
            _ if sendfile_can_fallback(&error) => {
                return send_file_fallback(stream, file, length, sent, deadline)
            }
            _ => return Err(error),
        }
    }

    Ok(sent)
}

fn send_file_fallback(
    stream: &TcpStream,
    file: &File,
    length: u64,
    mut sent: u64,
    deadline: Instant,
) -> io::Result<u64> {
    let mut buffer = [0_u8; FALLBACK_BUFFER_SIZE];
    while sent < length {
        let read_len = (length - sent).min(buffer.len() as u64) as usize;
        let read = file.read_at(&mut buffer[..read_len], sent)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file changed while being sent",
            ));
        }
        write_all_shared(stream, &buffer[..read], deadline)?;
        sent += read as u64;
    }
    Ok(sent)
}

fn sendfile_can_fallback(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOSYS)
    )
}

pub fn wait_fd(fd: libc::c_int, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let timeout = timeout_ms(deadline)?;
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result > 0 {
            if descriptor.revents & events != 0 {
                return Ok(());
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket closed"));
            }
            continue;
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "operation timed out",
            ));
        }

        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn timeout_ms(deadline: Instant) -> io::Result<libc::c_int> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "operation timed out",
        ));
    }
    Ok(duration_ms_ceil(remaining).min(libc::c_int::MAX as u128) as libc::c_int)
}

fn duration_ms_ceil(duration: Duration) -> u128 {
    duration.as_nanos().div_ceil(1_000_000)
}
