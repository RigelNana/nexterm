//! SSH connection establishment and lifecycle management via `russh`.

use anyhow::Result;
use async_trait::async_trait;
use russh::*;
use russh_keys::key::PublicKey;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::SshProfile;

/// An active SSH connection handle.
pub struct SshConnection {
    pub id: Uuid,
    pub profile: SshProfile,
    handle: Option<client::Handle<ClientHandler>>,
}

/// russh client handler (callbacks).
struct ClientHandler;

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO: implement known_hosts checking
        Ok(true)
    }
}

impl SshConnection {
    /// Connect to a remote host using the given profile.
    pub async fn connect(profile: SshProfile) -> Result<Self> {
        let config = client::Config {
            ..Default::default()
        };

        info!(host = %profile.host, port = %profile.port, user = %profile.username, "SSH connecting");

        let addr = format!("{}:{}", profile.host, profile.port);
        let mut handle = client::connect(Arc::new(config), &addr, ClientHandler).await?;

        // Authenticate
        match &profile.auth {
            crate::AuthMethod::Password(password) => {
                let authenticated = handle
                    .authenticate_password(&profile.username, password)
                    .await?;
                if !authenticated {
                    anyhow::bail!("password authentication failed");
                }
            }
            crate::AuthMethod::PublicKey { key_path, passphrase } => {
                let key = russh_keys::load_secret_key(key_path, passphrase.as_deref())?;
                let authenticated = handle
                    .authenticate_publickey(&profile.username, Arc::new(key))
                    .await?;
                if !authenticated {
                    anyhow::bail!("public key authentication failed");
                }
            }
            crate::AuthMethod::Agent => {
                // TODO: connect to ssh-agent
                warn!("SSH agent auth not yet implemented, falling back");
                anyhow::bail!("SSH agent not implemented yet");
            }
            crate::AuthMethod::KeyboardInteractive => {
                // TODO: implement keyboard-interactive
                anyhow::bail!("keyboard-interactive not implemented yet");
            }
        }

        info!(host = %profile.host, "SSH authenticated");

        Ok(Self {
            id: profile.id,
            profile,
            handle: Some(handle),
        })
    }

    /// Open an interactive PTY channel on the remote.
    pub async fn open_shell(&mut self, cols: u32, rows: u32) -> Result<russh::Channel<client::Msg>> {
        let handle = self.handle.as_ref().ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let channel = handle.channel_open_session().await?;
        channel.request_pty(false, "xterm-256color", cols, rows, 0, 0, &[]).await?;
        channel.request_shell(false).await?;
        Ok(channel)
    }

    /// Execute a command on a separate exec channel and return its stdout.
    /// Does **not** interfere with the interactive shell channel.
    pub async fn exec_command(&self, command: &str) -> Result<String> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let mut channel = handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut output = Vec::new();
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { ref data }) => output.extend_from_slice(data),
                Some(ChannelMsg::Eof) | None => break,
                _ => {}
            }
        }
        Ok(String::from_utf8_lossy(&output).to_string())
    }

    /// Open an SFTP subsystem channel for file operations.
    pub async fn open_sftp(&self) -> Result<russh_sftp::client::SftpSession> {
        let handle = self.handle.as_ref().ok_or_else(|| anyhow::anyhow!("not connected"))?;
        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await?;
        Ok(sftp)
    }

    /// Disconnect gracefully.
    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(handle) = self.handle.take() {
            handle.disconnect(Disconnect::ByApplication, "user requested", "en").await?;
        }
        Ok(())
    }
}
