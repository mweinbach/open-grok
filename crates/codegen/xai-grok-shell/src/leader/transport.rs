//! Leader-specific thin wrappers over [`crate::local_ipc`], preserving the
//! historical `LeaderListener` / `LeaderStream` names and the exact
//! `grok-leader-<hash>` Windows pipe names (byte-identical to the
//! pre-extraction implementation — the prefix below must never change once
//! shipped).

#[cfg(unix)]
pub(super) use tokio::net::UnixListener as LeaderListener;
#[cfg(unix)]
pub(super) use tokio::net::UnixStream as LeaderStream;

#[cfg(windows)]
pub(super) use windows_impl::{LeaderListener, LeaderStream};

/// Windows named-pipe namespace for leader sockets.
const LEADER_PIPE_PREFIX: &str = "grok-leader-";

/// Has a leader bound a listener at `path`?
///
/// - Unix: stats the socket file.
/// - Windows: probes the named pipe (Named Pipes don't appear in the
///   filesystem, so `path.exists()` doesn't work).
pub fn listener_is_ready(path: &std::path::Path) -> bool {
    crate::local_ipc::listener_is_ready(path, LEADER_PIPE_PREFIX)
}

#[cfg(windows)]
mod windows_impl {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::LEADER_PIPE_PREFIX;

    /// Leader-flavored [`crate::local_ipc::LocalIpcListener`] with the
    /// leader pipe prefix baked in.
    pub(super) struct LeaderListener(crate::local_ipc::LocalIpcListener);

    impl LeaderListener {
        pub(super) fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
            Ok(Self(crate::local_ipc::bind(
                path.as_ref(),
                LEADER_PIPE_PREFIX,
            )?))
        }

        /// Mirrors `UnixListener::accept`'s tuple shape; the peer half is a
        /// unit (named pipes carry no peer address).
        pub(super) async fn accept(&self) -> io::Result<(LeaderStream, ())> {
            let (stream, peer) = self.0.accept().await?;
            Ok((LeaderStream(stream), peer))
        }
    }

    /// Leader-flavored [`crate::local_ipc::LocalIpcStream`] with the leader
    /// pipe prefix baked in.
    pub(super) struct LeaderStream(crate::local_ipc::LocalIpcStream);

    impl LeaderStream {
        pub(super) async fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
            Ok(Self(
                crate::local_ipc::connect(path.as_ref(), LEADER_PIPE_PREFIX).await?,
            ))
        }
    }

    impl AsyncRead for LeaderStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for LeaderStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
        }
    }
}
