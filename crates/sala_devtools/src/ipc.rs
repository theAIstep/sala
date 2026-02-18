pub mod devcontainer {
    tonic::include_proto!("devcontainer");
}

pub use devcontainer::dev_container_service_client::DevContainerServiceClient;

use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
pub const TALA_SOCKET: &str = "/tmp/tala.sock";
#[cfg(windows)]
pub const TALA_SOCKET: &str = "\\\\.\\pipe\\tala";

pub const TALA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// IPC transport
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod transport {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::UnixStream;

    pub struct IpcStream(UnixStream);

    impl AsyncRead for IpcStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for IpcStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl hyper::client::connect::Connection for IpcStream {
        fn connected(&self) -> hyper::client::connect::Connected {
            hyper::client::connect::Connected::new()
        }
    }

    pub async fn create_channel(path: String) -> anyhow::Result<tonic::transport::Channel> {
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: hyper::Uri| {
                let path = path.clone();
                async move {
                    let stream = UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(IpcStream(stream))
                }
            }))
            .await?;
        Ok(channel)
    }
}

#[cfg(windows)]
mod transport {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::NamedPipeClient;

    pub struct IpcStream(NamedPipeClient);

    impl AsyncRead for IpcStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for IpcStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl hyper::client::connect::Connection for IpcStream {
        fn connected(&self) -> hyper::client::connect::Connected {
            hyper::client::connect::Connected::new()
        }
    }

    pub async fn create_channel(path: String) -> anyhow::Result<tonic::transport::Channel> {
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: hyper::Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::windows::named_pipe::ClientOptions::new()
                        .open(&path)?;
                    Ok::<_, std::io::Error>(IpcStream(stream))
                }
            }))
            .await?;
        Ok(channel)
    }
}

pub use transport::create_channel;

// ---------------------------------------------------------------------------
// Convenience helpers
// ---------------------------------------------------------------------------

/// Connect to the Tala daemon at the default socket path.
pub async fn connect_tala(
) -> anyhow::Result<DevContainerServiceClient<tonic::transport::Channel>> {
    let channel = create_channel(TALA_SOCKET.to_string())
        .await
        .context("failed to connect to tala daemon -- is it running?")?;
    Ok(DevContainerServiceClient::new(channel))
}

/// Connect to the Tala daemon with a timeout.
pub async fn connect_tala_with_timeout(
    timeout: Duration,
) -> anyhow::Result<DevContainerServiceClient<tonic::transport::Channel>> {
    let channel = tokio::time::timeout(timeout, create_channel(TALA_SOCKET.to_string()))
        .await
        .map_err(|_| anyhow::anyhow!("connection timed out after {:?}", timeout))??;
    Ok(DevContainerServiceClient::new(channel))
}

/// Detect the devcontainer config file in a workspace.
/// Checks `.devcontainer/devcontainer.json` first, then `.devcontainer.json`.
pub fn detect_devcontainer_config(workspace: &Path) -> Option<PathBuf> {
    let primary = workspace.join(".devcontainer/devcontainer.json");
    if primary.exists() {
        return Some(primary);
    }
    let fallback = workspace.join(".devcontainer.json");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}
