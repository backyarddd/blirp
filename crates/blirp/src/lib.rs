//! blirp binary crate: daemon (HTTP/WS API), PTY supervisor, agents, CLI.
//! Exposed as a library so integration tests can run the daemon in-process.

pub mod agents;
pub mod api;
pub mod cli;
pub mod clone;
pub mod daemon;
pub mod files;
pub mod hooks;
pub mod ingest;
pub mod keep_awake;
pub mod log_limit;
pub mod mcp;
pub mod memory;
pub mod portal;
pub mod pty;
pub mod sessions;
pub mod state;
pub mod static_files;
pub mod sync;
pub mod update;
pub mod uploads;

/// Install ring as the process-wide rustls provider. iroh turns on
/// reqwest's rustls backend without a provider, so every reqwest client
/// needs this first. Idempotent.
pub fn install_crypto_provider() {
    // Err only means a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A non-blocking TCP listener on `addr` that no other process can take
/// over. Windows lets another socket bind a more specific address on the
/// same port (`127.0.0.1` or a LAN address under our `0.0.0.0` portal) and
/// receive the connections meant for us, unless the first socket set
/// `SO_EXCLUSIVEADDRUSE`. Unix refuses such binds anyway; there the socket
/// gets `SO_REUSEADDR` like `std` sets it (restart during TIME_WAIT).
pub fn bind_exclusive(addr: std::net::SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let socket = bound_socket(addr)?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn bound_socket(addr: std::net::SocketAddr) -> std::io::Result<socket2::Socket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    #[cfg(windows)]
    exclusive_address_use(&socket)?;
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    Ok(socket)
}

#[cfg(windows)]
fn exclusive_address_use(socket: &socket2::Socket) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET_ERROR, SOL_SOCKET, setsockopt,
    };
    let on: i32 = 1;
    #[allow(unsafe_code)]
    // SAFETY: the handle belongs to `socket`, which outlives the call, and
    // the option value points at a live i32 of the length passed.
    let r = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const on).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if r == SOCKET_ERROR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    // Nobody can bind a more specific address on a port we hold on all
    // interfaces (the LAN portal), nor our loopback address itself.
    #[test]
    fn a_second_bind_of_the_port_fails() {
        // Windows only: Unix refuses it once the wildcard socket listens,
        // and it is not put into listening state here, so the test never
        // triggers a firewall prompt.
        if cfg!(windows) {
            let portal = super::bound_socket(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).unwrap();
            let port = portal.local_addr().unwrap().as_socket().unwrap().port();
            assert!(
                std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err(),
                "127.0.0.1:{port} was bound under 0.0.0.0:{port}"
            );
        }
        let local = super::bind_exclusive(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = local.local_addr().unwrap();
        assert!(
            std::net::TcpListener::bind(addr).is_err(),
            "{addr} bound twice"
        );
    }
}
