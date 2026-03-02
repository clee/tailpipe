use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use russh::server::Handle;
use russh::{ChannelId, CryptoVec};
use serde::Deserialize;
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct HeaderV2 {
    #[allow(dead_code)]
    version: u8,
    #[allow(dead_code)]
    width: u16,
    #[allow(dead_code)]
    height: u16,
}

#[derive(Debug, Deserialize)]
struct HeaderV3 {
    #[allow(dead_code)]
    version: u8,
    #[allow(dead_code)]
    term: V3Term,
}

#[derive(Debug, Deserialize)]
struct V3Term {
    #[allow(dead_code)]
    cols: u16,
    #[allow(dead_code)]
    rows: u16,
}

#[derive(Debug, Deserialize)]
struct VersionProbe {
    version: u8,
}

pub enum PlaybackCommand {
    TogglePause,
    SeekBack,
    SeekForward,
    ToggleOverlay,
    Quit,
}

struct TerminalHandle {
    sender: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    sink: Vec<u8>,
}

impl TerminalHandle {
    fn start(handle: Handle, channel: ChannelId) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                if handle
                    .data(channel, CryptoVec::from_slice(&data))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            sender,
            sink: Vec::new(),
        }
    }
}

impl Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sender
            .send(self.sink.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        self.sink.clear();
        Ok(())
    }
}

fn parse_events(path: &Path) -> Result<Vec<(f64, String, String)>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open cast file: {}", path.display()))?;
    let reader = io::BufReader::new(file);
    let mut lines = reader.lines();

    let header_line = lines
        .next()
        .context("empty cast file")?
        .context("failed to read cast header")?;
    let probe: VersionProbe =
        serde_json::from_str(&header_line).context("failed to parse cast header")?;
    let is_v3 = match probe.version {
        2 => {
            let _: HeaderV2 =
                serde_json::from_str(&header_line).context("failed to parse v2 header")?;
            false
        }
        3 => {
            let _: HeaderV3 =
                serde_json::from_str(&header_line).context("failed to parse v3 header")?;
            true
        }
        v => anyhow::bail!("unsupported asciicast version: {v}"),
    };

    let mut events = Vec::new();
    let mut accumulated_time = 0.0;
    for line in lines {
        let line = line.context("failed to read cast line")?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let event: (f64, String, String) =
            serde_json::from_str(&line).context("failed to parse cast event")?;
        if is_v3 {
            accumulated_time += event.0;
            events.push((accumulated_time, event.1, event.2));
        } else {
            events.push(event);
        }
    }

    Ok(events)
}

pub async fn play(path: &Path, channel: ChannelId, handle: Handle) -> Result<()> {
    let events = parse_events(path)?;

    let start = tokio::time::Instant::now();
    for (time, code, data) in &events {
        if code != "o" {
            continue;
        }
        let target = Duration::from_secs_f64(*time);
        let elapsed = start.elapsed();
        if target > elapsed {
            tokio::time::sleep(target - elapsed).await;
        }
        if handle
            .data(channel, CryptoVec::from_slice(data.as_bytes()))
            .await
            .is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

pub async fn play_interactive(
    path: &Path,
    channel: ChannelId,
    handle: Handle,
    mut cmd_rx: mpsc::Receiver<PlaybackCommand>,
    pty_size: (u16, u16),
) -> Result<()> {
    let events = parse_events(path)?;

    let mut term_handle = TerminalHandle::start(handle.clone(), channel);

    let mut paused = false;
    let mut overlay_visible = false;
    let mut current_time = Duration::ZERO;
    let mut next_idx: usize = 0;
    let mut wall_start = tokio::time::Instant::now();
    let mut time_offset = Duration::ZERO;
    let cols = pty_size.0;
    let rows = pty_size.1;

    let total_duration = events
        .last()
        .map(|(t, _, _)| Duration::from_secs_f64(*t))
        .unwrap_or(Duration::ZERO);

    loop {
        if paused {
            match cmd_rx.recv().await {
                Some(PlaybackCommand::TogglePause) => {
                    paused = false;
                    if overlay_visible {
                        overlay_visible = false;
                        restore_screen(&handle, channel, &events, next_idx).await?;
                    }
                    wall_start = tokio::time::Instant::now();
                    time_offset = current_time;
                }
                Some(PlaybackCommand::SeekBack) => {
                    let target = current_time.saturating_sub(Duration::from_secs(5));
                    seek_to(
                        &handle,
                        channel,
                        &events,
                        target,
                        &mut next_idx,
                        &mut current_time,
                    )
                    .await?;
                    time_offset = current_time;
                    if overlay_visible {
                        render_overlay(
                            &mut term_handle,
                            cols,
                            rows,
                            current_time,
                            total_duration,
                        )?;
                    }
                }
                Some(PlaybackCommand::SeekForward) => {
                    let target = (current_time + Duration::from_secs(5)).min(total_duration);
                    seek_to(
                        &handle,
                        channel,
                        &events,
                        target,
                        &mut next_idx,
                        &mut current_time,
                    )
                    .await?;
                    time_offset = current_time;
                    if overlay_visible {
                        render_overlay(
                            &mut term_handle,
                            cols,
                            rows,
                            current_time,
                            total_duration,
                        )?;
                    }
                }
                Some(PlaybackCommand::ToggleOverlay) => {
                    overlay_visible = !overlay_visible;
                    if overlay_visible {
                        render_overlay(
                            &mut term_handle,
                            cols,
                            rows,
                            current_time,
                            total_duration,
                        )?;
                    } else {
                        restore_screen(&handle, channel, &events, next_idx).await?;
                    }
                }
                Some(PlaybackCommand::Quit) | None => break,
            }
        } else {
            // Skip non-output events
            while next_idx < events.len() && events[next_idx].1 != "o" {
                next_idx += 1;
            }

            if next_idx >= events.len() {
                break;
            }

            let event_time = Duration::from_secs_f64(events[next_idx].0);
            current_time = time_offset + wall_start.elapsed();

            if event_time > current_time {
                let remaining = event_time - current_time;
                tokio::select! {
                    _ = tokio::time::sleep(remaining) => {
                        let data = &events[next_idx].2;
                        if handle
                            .data(channel, CryptoVec::from_slice(data.as_bytes()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        current_time = event_time;
                        next_idx += 1;
                    }
                    cmd = cmd_rx.recv() => {
                        current_time = time_offset + wall_start.elapsed();
                        match cmd {
                            Some(PlaybackCommand::TogglePause) => {
                                paused = true;
                                overlay_visible = true;
                                render_overlay(
                                    &mut term_handle,
                                    cols,
                                    rows,
                                    current_time,
                                    total_duration,
                                )?;
                            }
                            Some(PlaybackCommand::SeekBack) => {
                                let target = current_time.saturating_sub(Duration::from_secs(5));
                                seek_to(&handle, channel, &events, target, &mut next_idx, &mut current_time).await?;
                                time_offset = current_time;
                                wall_start = tokio::time::Instant::now();
                            }
                            Some(PlaybackCommand::SeekForward) => {
                                let target = (current_time + Duration::from_secs(5)).min(total_duration);
                                seek_to(&handle, channel, &events, target, &mut next_idx, &mut current_time).await?;
                                time_offset = current_time;
                                wall_start = tokio::time::Instant::now();
                            }
                            Some(PlaybackCommand::ToggleOverlay) => {}
                            Some(PlaybackCommand::Quit) | None => break,
                        }
                    }
                }
            } else {
                let data = &events[next_idx].2;
                if handle
                    .data(channel, CryptoVec::from_slice(data.as_bytes()))
                    .await
                    .is_err()
                {
                    break;
                }
                current_time = event_time;
                next_idx += 1;
            }
        }
    }

    Ok(())
}

async fn seek_to(
    handle: &Handle,
    channel: ChannelId,
    events: &[(f64, String, String)],
    target: Duration,
    next_idx: &mut usize,
    current_time: &mut Duration,
) -> Result<()> {
    handle
        .data(channel, CryptoVec::from_slice(b"\x1b[2J\x1b[H"))
        .await
        .map_err(|_| anyhow::anyhow!("send error"))?;

    for (i, (time, code, data)) in events.iter().enumerate() {
        if code != "o" {
            continue;
        }
        if Duration::from_secs_f64(*time) > target {
            *next_idx = i;
            *current_time = target;
            return Ok(());
        }
        handle
            .data(channel, CryptoVec::from_slice(data.as_bytes()))
            .await
            .map_err(|_| anyhow::anyhow!("send error"))?;
    }

    *next_idx = events.len();
    *current_time = target;
    Ok(())
}

async fn restore_screen(
    handle: &Handle,
    channel: ChannelId,
    events: &[(f64, String, String)],
    up_to_idx: usize,
) -> Result<()> {
    handle
        .data(channel, CryptoVec::from_slice(b"\x1b[2J\x1b[H"))
        .await
        .map_err(|_| anyhow::anyhow!("send error"))?;

    for (_, code, data) in &events[..up_to_idx] {
        if code != "o" {
            continue;
        }
        handle
            .data(channel, CryptoVec::from_slice(data.as_bytes()))
            .await
            .map_err(|_| anyhow::anyhow!("send error"))?;
    }

    Ok(())
}

fn render_overlay(
    term_handle: &mut TerminalHandle,
    cols: u16,
    rows: u16,
    current_time: Duration,
    total_duration: Duration,
) -> Result<()> {
    let area = Rect::new(0, 0, cols, rows);
    let backend = CrosstermBackend::new(&mut *term_handle);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(area),
        },
    )?;

    terminal.draw(|frame| {
        let pos_secs = current_time.as_secs();
        let total_secs = total_duration.as_secs();
        let position = format!(
            "{:02}:{:02} / {:02}:{:02}",
            pos_secs / 60,
            pos_secs % 60,
            total_secs / 60,
            total_secs % 60,
        );

        let text = vec![
            Line::from(""),
            Line::from(position),
            Line::from(""),
            Line::from("space   pause/resume"),
            Line::from("\u{2190}/\u{2192}     seek \u{00b1}5s"),
            Line::from("o       toggle overlay"),
            Line::from("q       quit"),
        ];

        let popup_width = 30u16.min(cols.saturating_sub(4));
        let popup_height = (text.len() as u16 + 2).min(rows.saturating_sub(2));

        let [vert_area] = Layout::vertical([Constraint::Length(popup_height)])
            .flex(Flex::Center)
            .areas(frame.area());
        let [popup_area] = Layout::horizontal([Constraint::Length(popup_width)])
            .flex(Flex::Center)
            .areas(vert_area);

        frame.render_widget(Clear, popup_area);
        let block = Block::bordered().title(" PAUSED ");
        let paragraph = Paragraph::new(text)
            .block(block)
            .style(Style::default().fg(Color::White));
        frame.render_widget(paragraph, popup_area);
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cast(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, contents.as_bytes()).unwrap();
        std::io::Write::flush(&mut f).unwrap();
        f
    }

    #[test]
    fn parse_v2_absolute_timestamps() {
        let f = write_cast(
            r#"{"version":2,"width":80,"height":24}
[0.5, "o", "hello"]
[1.0, "o", " world"]
[2.5, "o", "\r\n"]
"#,
        );
        let events = parse_events(f.path()).unwrap();
        assert_eq!(events.len(), 3);
        assert!((events[0].0 - 0.5).abs() < 1e-9);
        assert!((events[1].0 - 1.0).abs() < 1e-9);
        assert!((events[2].0 - 2.5).abs() < 1e-9);
        assert_eq!(events[0].2, "hello");
        assert_eq!(events[1].2, " world");
    }

    #[test]
    fn parse_v3_relative_timestamps_accumulated() {
        let f = write_cast(
            r#"{"version":3,"term":{"cols":100,"rows":50}}
[0.5, "o", "hello"]
[1.0, "o", " world"]
[0.3, "o", "!"]
"#,
        );
        let events = parse_events(f.path()).unwrap();
        assert_eq!(events.len(), 3);
        assert!((events[0].0 - 0.5).abs() < 1e-9);
        assert!((events[1].0 - 1.5).abs() < 1e-9);
        assert!((events[2].0 - 1.8).abs() < 1e-9);
    }

    #[test]
    fn parse_v3_skips_comments_and_empty_lines() {
        let f = write_cast(
            r#"{"version":3,"term":{"cols":80,"rows":24}}
# this is a comment
[0.1, "o", "a"]

[0.2, "o", "b"]
# another comment
[0.3, "o", "c"]
"#,
        );
        let events = parse_events(f.path()).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].2, "a");
        assert_eq!(events[1].2, "b");
        assert_eq!(events[2].2, "c");
    }

    #[test]
    fn parse_v3_non_output_events_preserved() {
        let f = write_cast(
            r#"{"version":3,"term":{"cols":80,"rows":24}}
[0.1, "o", "hello"]
[0.5, "i", "\n"]
[0.2, "r", "90x30"]
[1.0, "o", "world"]
"#,
        );
        let events = parse_events(f.path()).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[1].1, "i");
        assert_eq!(events[2].1, "r");
        assert!((events[3].0 - 1.8).abs() < 1e-9);
    }

    #[test]
    fn parse_unsupported_version_errors() {
        let f = write_cast(r#"{"version":99,"width":80,"height":24}"#);
        let err = parse_events(f.path()).unwrap_err();
        assert!(err.to_string().contains("unsupported asciicast version: 99"));
    }

    #[test]
    fn parse_empty_file_errors() {
        let f = write_cast("");
        assert!(parse_events(f.path()).is_err());
    }

    #[test]
    fn parse_v2_skips_empty_lines() {
        let f = write_cast(
            r#"{"version":2,"width":80,"height":24}
[0.5, "o", "a"]

[1.0, "o", "b"]
"#,
        );
        let events = parse_events(f.path()).unwrap();
        assert_eq!(events.len(), 2);
    }
}
