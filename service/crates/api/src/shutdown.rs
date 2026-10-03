//! Graceful-shutdown trigger: SIGTERM (what `docker stop` / orchestrators send) or Ctrl-C.

use std::future::Future;
use std::io;

/// Install the signal handlers now and return a future that resolves on the first
/// SIGTERM or SIGINT. Handlers are registered before this returns, so no signal is missed.
#[cfg(unix)]
pub fn shutdown_listener() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received, shutting down"),
            _ = int.recv() => tracing::info!("SIGINT received, shutting down"),
        }
    })
}

#[cfg(not(unix))]
pub fn shutdown_listener() -> io::Result<impl Future<Output = ()>> {
    Ok(async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c; shutting down");
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn shutdown_listener_resolves_on_sigterm() {
        let listener = shutdown_listener().expect("install handlers");
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success());
        tokio::time::timeout(WAIT, listener)
            .await
            .expect("SIGTERM must trigger shutdown");
    }
}
