use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

/// How the server should listen: on a Unix socket file or on a TCP address.
pub(crate) enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

/// A listening socket, either a Unix socket or a TCP listener.
pub(crate) enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    pub(crate) fn accept(&self) -> io::Result<Connection> {
        match self {
            Listener::Unix(listener) => listener
                .accept()
                .map(|(stream, _)| Connection::Unix(stream)),
            Listener::Tcp(listener) => listener.accept().map(|(stream, _)| Connection::Tcp(stream)),
        }
    }
}

/// A connected client, wrapping either a Unix stream or a TCP stream.
pub(crate) enum Connection {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(stream) => stream.read(buf),
            Connection::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(stream) => stream.write(buf),
            Connection::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Connection::Unix(stream) => stream.flush(),
            Connection::Tcp(stream) => stream.flush(),
        }
    }
}

impl Connection {
    pub(crate) fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Connection::Unix(stream) => stream.shutdown(how),
            Connection::Tcp(stream) => stream.shutdown(how),
        }
    }
}

/// Removes the socket file from disk when the guard is dropped. Unix sockets
/// leave their file behind after the process exits, so this prevents stale
/// files from accumulating (the server also removes any pre-existing file at
/// startup).
pub(crate) struct SocketFileGuard(pub(crate) Option<PathBuf>);

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Classify the first command-line argument as a TCP address or a Unix socket
/// path. The argument is a TCP address when its last `:` is followed only by
/// digits (a port), e.g. `0.0.0.0:4096` or `[::1]:4096`; anything else is
/// treated as a Unix socket file path.
pub(crate) fn parse_endpoint(arg: &str) -> Endpoint {
    if let Some(idx) = arg.rfind(':') {
        if idx > 0 && idx < arg.len() - 1 && arg[idx + 1..].bytes().all(|b| b.is_ascii_digit()) {
            return Endpoint::Tcp(arg.to_string());
        }
    }
    Endpoint::Unix(PathBuf::from(arg))
}

/// Write a framed response: `u32 payload_length | u32 status | data`.
pub(crate) fn write_response(
    connection: &mut Connection,
    status: u32,
    data: &[u8],
) -> io::Result<()> {
    let payload_length = 4 + data.len() as u32;
    connection.write_all(&payload_length.to_be_bytes())?;
    connection.write_all(&status.to_be_bytes())?;
    connection.write_all(data)?;
    connection.flush()
}

/// Read exactly `buf.len()` bytes into `buf`. Returns `true` if the stream hit
/// EOF before the buffer was filled, `false` on success.
pub(crate) fn read_exact(connection: &mut Connection, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = connection.read(&mut buf[filled..])?;
        if n == 0 {
            let _ = connection.shutdown(Shutdown::Both);
            return Ok(true);
        }
        filled += n;
    }
    Ok(false)
}
