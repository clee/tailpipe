mod cast;
mod config;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::PrivateKey;
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{server, Channel, ChannelId};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::cast::PlaybackCommand;
use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tailpipe.toml".into());

    let app_config = Config::load(&PathBuf::from(&config_path))?;
    log::info!("loaded config from {config_path}");

    let host_key = if let Some(path) = &app_config.server.host_key {
        log::info!("loading host key from {path}");
        russh::keys::load_secret_key(path, None)?
    } else {
        log::info!("no host_key configured, generating ephemeral key");
        PrivateKey::random(&mut OsRng, russh::keys::Algorithm::Ed25519)?
    };

    let ssh_config = russh::server::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        auth_rejection_time: std::time::Duration::from_secs(0),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        keys: vec![host_key],
        ..Default::default()
    };

    let ssh_config = Arc::new(ssh_config);
    let app_config = Arc::new(app_config);

    let mut server = TailpipeServer {
        config: app_config,
    };

    let addr = format!("0.0.0.0:{}", server.config.server.port);
    log::info!("listening on {addr}");

    let socket = TcpListener::bind(&addr).await?;
    server.run_on_socket(ssh_config, &socket).await?;

    Ok(())
}

#[derive(Clone)]
struct TailpipeServer {
    config: Arc<Config>,
}

impl server::Server for TailpipeServer {
    type Handler = TailpipeHandler;

    fn new_client(&mut self, peer_addr: Option<std::net::SocketAddr>) -> TailpipeHandler {
        log::info!("new connection from {:?}", peer_addr);
        TailpipeHandler {
            config: self.config.clone(),
            user: None,
            cmd_tx: None,
            pty_size: None,
        }
    }

    fn handle_session_error(&mut self, error: <TailpipeHandler as server::Handler>::Error) {
        log::error!("session error: {error:#}");
    }
}

struct TailpipeHandler {
    config: Arc<Config>,
    user: Option<String>,
    cmd_tx: Option<mpsc::Sender<PlaybackCommand>>,
    pty_size: Option<(u16, u16)>,
}

impl server::Handler for TailpipeHandler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        self.user = Some(user.to_string());

        if self.config.user_config(user).is_some() {
            log::info!("accepted user: {user}");
            Ok(Auth::Accept)
        } else {
            log::warn!("rejected unknown user: {user}");
            Ok(Auth::reject())
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.user = Some(user.to_string());

        if self.config.user_config(user).is_some() {
            log::info!("accepted user (pubkey): {user}");
            Ok(Auth::Accept)
        } else {
            log::warn!("rejected unknown user (pubkey): {user}");
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.pty_size = Some((col_width as u16, row_height as u16));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.pty_size = Some((col_width as u16, row_height as u16));
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;

        let user = self.user.clone().unwrap_or_else(|| "unknown".into());
        let user_config = self.config.user_config(&user).cloned();
        let server_config_header = self.config.server.header.clone();
        let server_config_footer = self.config.server.footer.clone();
        let handle = session.handle();
        let pty_size = self.pty_size.unwrap_or((80, 24));

        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        self.cmd_tx = Some(cmd_tx);

        tokio::spawn(async move {
            if let Some(user_config) = user_config {
                // Play header cast (user override or server default)
                let header = user_config
                    .header
                    .as_deref()
                    .or(server_config_header.as_deref());
                if let Some(path) = header {
                    if path != "none" {
                        if let Err(e) =
                            cast::play(&PathBuf::from(path), channel, handle.clone()).await
                        {
                            log::error!("header playback error for {user}: {e:#}");
                        }
                    }
                }

                // Play the main cast file (interactive)
                let castfile = PathBuf::from(&user_config.castfile);
                if let Err(e) =
                    cast::play_interactive(&castfile, channel, handle.clone(), cmd_rx, pty_size)
                        .await
                {
                    log::error!("playback error for {user}: {e:#}");
                }

                // Play footer cast (user override or server default)
                let footer = user_config
                    .footer
                    .as_deref()
                    .or(server_config_footer.as_deref());
                if let Some(path) = footer {
                    if path != "none" {
                        if let Err(e) =
                            cast::play(&PathBuf::from(path), channel, handle.clone()).await
                        {
                            log::error!("footer playback error for {user}: {e:#}");
                        }
                    }
                }
            }

            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });

        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(tx) = &self.cmd_tx {
            let cmd = match data {
                [0x20] => Some(PlaybackCommand::TogglePause),
                [0x1b, 0x5b, 0x44] => Some(PlaybackCommand::SeekBack),
                [0x1b, 0x5b, 0x43] => Some(PlaybackCommand::SeekForward),
                [0x6f] => Some(PlaybackCommand::ToggleOverlay),
                [0x71] | [0x03] => Some(PlaybackCommand::Quit),
                _ => None,
            };
            if let Some(cmd) = cmd {
                let _ = tx.send(cmd).await;
            }
        } else if data == [3] {
            return Err(russh::Error::Disconnect.into());
        }
        Ok(())
    }
}
