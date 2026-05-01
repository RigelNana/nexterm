//! NexTerm — AI-native GPU-accelerated terminal with SSH & SFTP.
//!
//! Application entry point: initializes the window, PTY, VTE parser, GPU renderer,
//! and drives the event loop.

// Hide the console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod pane;
mod ssh_backend;
mod tabs;

use anyhow::Result;
use nexterm_config::schema::AppConfig;
use nexterm_agent::{AgentBridge, AgentBridgeCommand, AgentBridgeConfig, AgentBridgeEvent, ToolRequest, ToolResponse};
use nexterm_render::gui::{self, AgentChatMessage, AgentChatRole, GuiAction, GuiState};
use nexterm_render::renderer::Renderer;
use nexterm_history::HistoryDb;
use nexterm_session::store::SessionStore;
use nexterm_theme::ResolvedTheme;
use nexterm_vte::grid::Selection;
use pane::Rect;
use std::sync::{mpsc, Arc};
use std::time::Instant;
use tabs::{SplitDir, TabManager};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Cursor blink interval (~530ms like most terminals).
const CURSOR_BLINK_MS: u64 = 530;

/// Main application state.
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    tab_manager: TabManager,
    dirty: bool,

    // Input state
    modifiers: ModifiersState,

    // Cursor blink
    last_blink: Instant,
    cursor_blink_on: bool,

    // Mouse / selection
    mouse_pressed: bool,
    mouse_px: (f64, f64), // raw pixel position
    /// Pending selection anchor (abs_row, col) set on click, used on first drag.
    pending_select_anchor: Option<(usize, usize)>,

    // Clipboard
    clipboard: Option<arboard::Clipboard>,

    // Configuration + hot-reload
    config: AppConfig,
    config_path: std::path::PathBuf,
    config_reload_rx: Option<mpsc::Receiver<()>>,

    // Async runtime for SSH
    tokio_rt: Arc<tokio::runtime::Runtime>,

    // Session persistence
    session_store: SessionStore,
    stored_profiles: Vec<nexterm_ssh::SshProfile>,

    // Command history
    history_db: HistoryDb,
    /// Block IDs already recorded (avoid duplicate inserts).
    history_recorded_blocks: std::collections::HashSet<u64>,

    // Background font rebuild: receives pre-built GlyphCache from worker thread
    font_cache_rx: Option<mpsc::Receiver<nexterm_render::atlas::GlyphCache>>,
    // Generation counter to discard stale rebuilds
    font_gen: u64,

    // Render rate-limiting: avoid GPU-bound frames starving VTE parsing
    last_render: Instant,

    // Suppress spurious Tab after Alt+Tab window switch
    last_focus_time: Instant,

    // Find / search results: (abs_row, col, len)
    find_matches: Vec<(usize, usize, usize)>,

    // GUI overlay state
    gui_state: GuiState,

    // Actual terminal content rect from egui (updated each frame)
    terminal_rect: [f32; 4], // [x, y, w, h] in physical pixels

    // Background shell detection: result delivered after window is shown
    shell_detect_tx: mpsc::Sender<Vec<gui::ShellInfo>>,
    shell_detect_rx: mpsc::Receiver<Vec<gui::ShellInfo>>,

    /// True once the window has been made visible after the first paint.
    /// We start the window hidden to avoid the unpainted-transparent-window flash.
    window_visible: bool,

    // AI Agent bridge
    agent_bridge: Option<AgentBridge>,
}

impl App {
    fn new(config: AppConfig, config_path: std::path::PathBuf) -> Self {
        let tokio_rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("nexterm-async")
                .build()
                .expect("failed to create tokio runtime"),
        );
        let (shell_detect_tx, shell_detect_rx) = mpsc::channel::<Vec<gui::ShellInfo>>();
        let mut app = Self {
            window: None,
            renderer: None,
            tab_manager: TabManager::new(),
            dirty: true,
            modifiers: ModifiersState::empty(),
            last_blink: Instant::now(),
            cursor_blink_on: true,
            mouse_pressed: false,
            mouse_px: (0.0, 0.0),
            pending_select_anchor: None,
            clipboard: arboard::Clipboard::new().ok(),
            gui_state: {
                let mut gs = GuiState::new(
                    &config.appearance.font_family,
                    config.appearance.font_size,
                    &config.appearance.theme,
                    &config.general.default_shell,
                );
                gs.settings_opacity = config.appearance.opacity;
                gs.settings_padding = config.appearance.padding;
                gs.settings_background_image = config.appearance.background_image.clone();
                // Load AI agent config
                gs.agent_provider_type = config.ai.provider.clone();
                gs.agent_base_url = config.ai.base_url.clone();
                gs.agent_model_id = config.ai.model.clone();
                // Resolve API key: direct value takes priority, then env var
                gs.agent_api_key = if !config.ai.api_key.is_empty() {
                    config.ai.api_key.clone()
                } else {
                    std::env::var(&config.ai.api_key_env).unwrap_or_default()
                };
                gs
            },
            session_store: {
                let db_dir = dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("nexterm");
                let _ = std::fs::create_dir_all(&db_dir);
                let db_path = db_dir.join("sessions.db");
                SessionStore::open(db_path.to_str().unwrap_or("sessions.db"))
                    .expect("failed to open session database")
            },
            stored_profiles: Vec::new(),
            history_db: {
                let db_dir = dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("nexterm");
                let _ = std::fs::create_dir_all(&db_dir);
                let db_path = db_dir.join("history.db");
                HistoryDb::open(db_path.to_str().unwrap_or("history.db"))
                    .expect("failed to open history database")
            },
            history_recorded_blocks: std::collections::HashSet::new(),
            terminal_rect: [0.0, 0.0, 1280.0, 800.0],
            config,
            config_path,
            config_reload_rx: None,
            tokio_rt,
            font_cache_rx: None,
            font_gen: 0,
            last_render: Instant::now(),
            last_focus_time: Instant::now(),
            find_matches: Vec::new(),
            shell_detect_tx,
            shell_detect_rx,
            window_visible: false,
            agent_bridge: None,
        };
        // Load saved profiles from database
        app.load_profiles_from_store();
        app
    }

    /// Load stored profiles from SQLite and sync to GUI state.
    fn load_profiles_from_store(&mut self) {
        match self.session_store.load_profiles() {
            Ok(profiles) => {
                self.stored_profiles = profiles;
                self.sync_gui_profiles();
                info!(count = self.stored_profiles.len(), "loaded saved profiles");
            }
            Err(e) => {
                error!("failed to load profiles: {e}");
            }
        }
    }

    /// Rebuild `gui_state.ssh_profiles` from `stored_profiles`.
    fn sync_gui_profiles(&mut self) {
        self.gui_state.ssh_profiles = self
            .stored_profiles
            .iter()
            .map(|p| gui::SshProfileEntry {
                id: p.id.to_string(),
                name: p.name.clone(),
                group: p.group.clone().unwrap_or_else(|| "Default".to_string()),
                host: p.host.clone(),
                port: p.port,
                username: p.username.clone(),
                auth_display: match &p.auth {
                    nexterm_ssh::AuthMethod::Password(_) => "Password".to_string(),
                    nexterm_ssh::AuthMethod::PublicKey { .. } => "Key".to_string(),
                    nexterm_ssh::AuthMethod::Agent => "Agent".to_string(),
                    nexterm_ssh::AuthMethod::KeyboardInteractive => "Keyboard".to_string(),
                },
                color_tag: None,
            })
            .collect();
    }

    /// Load recent history from the database into the GUI state.
    fn refresh_history_list(&mut self) {
        match self.history_db.recent(500) {
            Ok(entries) => {
                self.gui_state.history_entries = entries
                    .into_iter()
                    .map(|e| gui::HistoryItem {
                        command: e.command,
                        exit_code: e.exit_code,
                        timestamp: e.timestamp,
                        host: e.host,
                        cwd: e.cwd,
                    })
                    .collect();
            }
            Err(e) => {
                warn!("failed to load history: {e}");
            }
        }
    }

    /// Ensure the Agent bridge is created and its worker spawned.
    fn ensure_agent_bridge(&mut self) {
        if self.agent_bridge.is_some() {
            return;
        }
        let config = AgentBridgeConfig {
            provider_type: self.gui_state.agent_provider_type.clone(),
            base_url: self.gui_state.agent_base_url.clone(),
            api_key: self.gui_state.agent_api_key.clone(),
            model_id: self.gui_state.agent_model_id.clone(),
            ..Default::default()
        };
        let (bridge, worker) = AgentBridge::new(config);
        self.tokio_rt.spawn(async move {
            if let Err(e) = worker.run().await {
                tracing::error!("agent bridge worker exited: {e}");
            }
        });
        self.agent_bridge = Some(bridge);
        info!("agent bridge started");
    }

    /// Drain agent bridge events and update GUI state.
    fn poll_agent_events(&mut self) {
        let bridge = match self.agent_bridge.as_mut() {
            Some(b) => b,
            None => return,
        };

        // Poll bridge events (non-blocking)
        while let Ok(event) = bridge.event_rx.try_recv() {
            match event {
                AgentBridgeEvent::TextDelta(delta) => {
                    self.gui_state.agent_streaming_text.push_str(&delta);
                    self.dirty = true;
                }
                AgentBridgeEvent::ToolCallStart { tool_name, args_summary } => {
                    // Flush any pending streaming text before the tool call
                    if !self.gui_state.agent_streaming_text.is_empty() {
                        let text = std::mem::take(&mut self.gui_state.agent_streaming_text);
                        self.gui_state.agent_messages.push(AgentChatMessage {
                            role: AgentChatRole::Assistant,
                            content: text,
                        });
                    }
                    self.gui_state.agent_messages.push(AgentChatMessage {
                        role: AgentChatRole::ToolCall,
                        content: format!("⚙ {tool_name}: {args_summary}"),
                    });
                    self.dirty = true;
                }
                AgentBridgeEvent::ToolCallEnd { tool_name, is_error } => {
                    let status = if is_error { "✗" } else { "✓" };
                    self.gui_state.agent_messages.push(AgentChatMessage {
                        role: AgentChatRole::ToolCall,
                        content: format!("{status} {tool_name} completed"),
                    });
                    self.dirty = true;
                }
                AgentBridgeEvent::Thinking(text) => {
                    // Could display thinking separately; for now append to streaming
                    self.gui_state.agent_streaming_text.push_str(&text);
                    self.dirty = true;
                }
                AgentBridgeEvent::Done => {
                    // Flush streaming text to a final assistant message
                    if !self.gui_state.agent_streaming_text.is_empty() {
                        let text = std::mem::take(&mut self.gui_state.agent_streaming_text);
                        self.gui_state.agent_messages.push(AgentChatMessage {
                            role: AgentChatRole::Assistant,
                            content: text,
                        });
                    }
                    self.gui_state.agent_is_running = false;
                    self.dirty = true;
                }
                AgentBridgeEvent::Error(err) => {
                    // Flush any partial streaming text
                    if !self.gui_state.agent_streaming_text.is_empty() {
                        let text = std::mem::take(&mut self.gui_state.agent_streaming_text);
                        self.gui_state.agent_messages.push(AgentChatMessage {
                            role: AgentChatRole::Assistant,
                            content: text,
                        });
                    }
                    self.gui_state.agent_messages.push(AgentChatMessage {
                        role: AgentChatRole::Error,
                        content: err,
                    });
                    self.gui_state.agent_is_running = false;
                    self.dirty = true;
                }
            }
        }

        // Poll tool requests (non-blocking) and respond
        while let Ok(req) = bridge.tool_request_rx.try_recv() {
            match req {
                ToolRequest::ExecuteCommand { command, pane_id, reply } => {
                    let cmd = command.trim().replace('\r', "").replace('\n', " ");
                    let pane = if let Some(id) = pane_id {
                        self.tab_manager.panes.values_mut().nth(id)
                    } else {
                        self.tab_manager.focused_pane_mut()
                    };
                    if let Some(pane) = pane {
                        // Shells expect CR ('\r') for Enter, NOT LF ('\n').
                        // Sending '\n' triggers multi-line edit mode in PSReadLine / zsh ZLE /
                        // bash readline with bracketed paste, so the command never executes.
                        pane.write_to_pty(cmd.as_bytes());
                        pane.write_to_pty(b"\r");
                        let _ = reply.send(ToolResponse::Output("ok".into()));
                    } else {
                        let _ = reply.send(ToolResponse::Error("No active terminal pane".into()));
                    }
                    self.dirty = true;
                }
                ToolRequest::ReadTerminalOutput { pane_id, last_n_blocks: _, reply } => {
                    let pane = if let Some(id) = pane_id {
                        self.tab_manager.panes.values().nth(id)
                    } else {
                        self.tab_manager.focused_pane()
                    };
                    if let Some(pane) = pane {
                        let grid = pane.terminal.grid();
                        let output = grid.extract_visible_text_last_n_lines(100);
                        let _ = reply.send(ToolResponse::Output(output));
                    } else {
                        let _ = reply.send(ToolResponse::Error("No active terminal pane".into()));
                    }
                }
                ToolRequest::ReadSystemStatus { reply } => {
                    // Try to find SSH pane with system status
                    let status = self.tab_manager.focused_pane()
                        .and_then(|p| p.server_status());
                    if let Some(ss) = status {
                        let _ = reply.send(ToolResponse::Output(format!(
                            "OS: {} | Kernel: {} | Host: {}\nUptime: {} | Load: {}\nCPU: {:.1}% | Memory: {}/{} MB\nDisk: {}",
                            ss.os, ss.kernel, ss.hostname,
                            ss.uptime, ss.load_avg,
                            ss.cpu_usage_pct, ss.mem_used_mb, ss.mem_total_mb,
                            ss.disk_usage,
                        )));
                    } else {
                        let _ = reply.send(ToolResponse::Error(
                            "System status not available (only for SSH sessions with status probe)".into()
                        ));
                    }
                }
            }
        }
    }

    /// Kick off a background thread to rebuild the glyph cache.
    /// Previous pending rebuild (if any) is discarded via generation counter.
    fn schedule_font_change(&mut self) {
        self.font_gen += 1;
        let fgen = self.font_gen;
        let size = self.config.appearance.font_size;
        let family = self.config.appearance.font_family.clone();
        let (tx, rx) = mpsc::channel();
        self.font_cache_rx = Some(rx);

        info!(font_size = size, %family, font_gen = fgen, "font rebuild dispatched to background");

        std::thread::Builder::new()
            .name(format!("font-rebuild-{fgen}"))
            .spawn(move || {
                let cache = nexterm_render::atlas::GlyphCache::new(size, &family);
                let _ = tx.send(cache);
            })
            .expect("failed to spawn font rebuild thread");
    }

    /// Check if a background font rebuild is ready and swap it in (called each frame).
    fn poll_font_change(&mut self) {
        if let Some(rx) = &self.font_cache_rx {
            if let Ok(cache) = rx.try_recv() {
                self.font_cache_rx = None;
                if let Some(renderer) = &mut self.renderer {
                    renderer.swap_glyph_cache(cache);
                }
                self.relayout();
                self.dirty = true;
            }
        }
    }

    /// Handle a GUI action from the egui overlay.
    fn handle_gui_action(&mut self, action: GuiAction, event_loop: &ActiveEventLoop) {
        match action {
            GuiAction::SwitchTab(idx) => {
                self.tab_manager.active_tab = idx;
                self.relayout();
            }
            GuiAction::CloseTab(idx) => {
                self.tab_manager.active_tab = idx;
                if !self.tab_manager.close_active_tab() {
                    event_loop.exit();
                }
                self.relayout();
            }
            GuiAction::NewTab => {
                let area = self.pane_area();
                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                let shell = self.configured_shell().map(|s| s.to_string());
                if let Err(e) = self.tab_manager.new_tab(area, cw, ch, shell.as_deref()) {
                    error!("failed to create tab: {e}");
                }
                self.relayout();
            }
            GuiAction::NewTabWithShell(shell_path) => {
                let area = self.pane_area();
                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                if let Err(e) = self.tab_manager.new_tab(area, cw, ch, Some(&shell_path)) {
                    error!("failed to create tab with {}: {e}", shell_path);
                }
                self.relayout();
            }
            GuiAction::NewWslTab => {
                let area = self.pane_area();
                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                if let Err(e) = self.tab_manager.new_tab(area, cw, ch, Some("wsl")) {
                    error!("failed to create WSL tab: {e}");
                } else {
                    // WSL runs bash/zsh inside wsl.exe — inject OSC 133 hooks via stdin
                    if let Some(pane) = self.tab_manager.focused_pane_mut() {
                        pane.inject_shell_integration();
                    }
                }
                self.relayout();
            }
            GuiAction::OpenSshDialog => {
                self.gui_state.show_ssh_dialog = true;
            }
            GuiAction::ConnectSsh { host, port, username, auth } => {
                let auth_method = match auth {
                    gui::SshAuthInput::Password(p) => nexterm_ssh::AuthMethod::Password(p),
                    gui::SshAuthInput::KeyFile(path) => nexterm_ssh::AuthMethod::PublicKey {
                        key_path: path,
                        passphrase: None,
                    },
                };
                let profile = nexterm_ssh::SshProfile {
                    host,
                    port,
                    username,
                    auth: auth_method,
                    ..Default::default()
                };
                self.open_ssh_tab(profile);
                self.gui_state.show_ssh_dialog = false;
            }
            GuiAction::ToggleSettings => {
                self.gui_state.show_settings = !self.gui_state.show_settings;
            }
            GuiAction::ApplySettings { font_family, font_size, theme, shell, opacity, padding, background_image, cursor_style, cursor_blink } => {
                self.config.appearance.font_family = font_family;
                self.config.appearance.font_size = font_size;
                self.config.appearance.theme = theme;
                self.config.general.default_shell = shell;
                self.config.appearance.padding = padding;
                self.config.appearance.background_image = background_image;

                // Apply cursor style to all panes
                let style_name = match cursor_style {
                    0 => "block",
                    2 => "underline",
                    _ => "beam",
                };
                self.config.appearance.cursor_style = style_name.into();
                self.config.appearance.cursor_blink = cursor_blink;
                let new_style = match cursor_style {
                    0 => nexterm_vte::grid::CursorStyle::Block,
                    2 => nexterm_vte::grid::CursorStyle::Underline,
                    _ => nexterm_vte::grid::CursorStyle::Bar,
                };
                for pane in self.tab_manager.panes.values_mut() {
                    pane.terminal.grid_mut().cursor_style = new_style;
                }
                self.dirty = true;

                // Apply OS-level window opacity
                self.config.appearance.opacity = opacity;
                if let Some(window) = &self.window {
                    apply_window_opacity(window, opacity);
                }

                let resolved = self.resolve_theme();
                if let Some(renderer) = &mut self.renderer {
                    renderer.theme = resolved;
                }
                self.schedule_font_change();
                self.relayout();

                // Persist settings to disk
                if let Err(e) = nexterm_config::save_config(&self.config_path, &self.config) {
                    error!("failed to save config: {e}");
                }
            }
            GuiAction::CopySelection => {
                self.copy_selection();
            }
            GuiAction::PasteClipboard => {
                self.paste_clipboard();
            }
            GuiAction::FontZoomIn => {
                self.config.appearance.font_size = (self.config.appearance.font_size + 1.0).min(72.0);
                self.schedule_font_change();
            }
            GuiAction::FontZoomOut => {
                self.config.appearance.font_size = (self.config.appearance.font_size - 1.0).max(6.0);
                self.schedule_font_change();
            }
            GuiAction::FontZoomReset => {
                self.config.appearance.font_size = 14.0;
                self.schedule_font_change();
            }
            GuiAction::ConnectSavedProfile(idx) => {
                if let Some(profile) = self.stored_profiles.get(idx).cloned() {
                    self.open_ssh_tab(profile);
                }
            }
            GuiAction::DeleteSavedProfile(idx) => {
                if let Some(profile) = self.stored_profiles.get(idx) {
                    let id = profile.id;
                    if let Err(e) = self.session_store.delete_profile(&id) {
                        error!("failed to delete profile: {e}");
                    }
                    self.stored_profiles.remove(idx);
                    self.sync_gui_profiles();
                }
            }
            GuiAction::SaveProfile { name, group, host, port, username, auth_mode, password, key_path } => {
                let auth = if auth_mode == 0 {
                    nexterm_ssh::AuthMethod::Password(password)
                } else {
                    nexterm_ssh::AuthMethod::PublicKey {
                        key_path,
                        passphrase: None,
                    }
                };
                let profile = nexterm_ssh::SshProfile {
                    name,
                    host,
                    port,
                    username,
                    auth,
                    group: Some(group),
                    ..Default::default()
                };
                if let Err(e) = self.session_store.save_profile(&profile) {
                    error!("failed to save profile: {e}");
                }
                self.stored_profiles.push(profile);
                self.sync_gui_profiles();
            }
            GuiAction::ToggleFullScreen => {
                if let Some(window) = &self.window {
                    let is_fullscreen = window.fullscreen().is_some();
                    if is_fullscreen {
                        window.set_fullscreen(None);
                    } else {
                        window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
                    }
                }
            }
            GuiAction::SplitHorizontal => {
                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                let shell = self.configured_shell().map(|s| s.to_string());
                let _ = self.tab_manager.split_focused(SplitDir::Horizontal, cw, ch, shell.as_deref(), &self.tokio_rt);
                self.relayout();
            }
            GuiAction::SplitVertical => {
                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                let shell = self.configured_shell().map(|s| s.to_string());
                let _ = self.tab_manager.split_focused(SplitDir::Vertical, cw, ch, shell.as_deref(), &self.tokio_rt);
                self.relayout();
            }
            GuiAction::Find => {
                self.gui_state.show_find_bar = !self.gui_state.show_find_bar;
                self.relayout();
            }
            GuiAction::EditSavedProfile(idx) => {
                if let Some(profile) = self.stored_profiles.get(idx) {
                    self.gui_state.ssh_editing_profile_id = Some(profile.id.to_string());
                    self.gui_state.ssh_name = profile.name.clone();
                    self.gui_state.ssh_host = profile.host.clone();
                    self.gui_state.ssh_port = profile.port.to_string();
                    self.gui_state.ssh_username = profile.username.clone();
                    self.gui_state.ssh_group = profile.group.clone().unwrap_or_else(|| "Default".to_string());
                    match &profile.auth {
                        nexterm_ssh::AuthMethod::Password(p) => {
                            self.gui_state.ssh_auth_mode = 0;
                            self.gui_state.ssh_password = p.clone();
                            self.gui_state.ssh_key_path.clear();
                        }
                        nexterm_ssh::AuthMethod::PublicKey { key_path, .. } => {
                            self.gui_state.ssh_auth_mode = 1;
                            self.gui_state.ssh_key_path = key_path.clone();
                            self.gui_state.ssh_password.clear();
                        }
                        _ => {
                            self.gui_state.ssh_auth_mode = 1;
                            self.gui_state.ssh_password.clear();
                            self.gui_state.ssh_key_path.clear();
                        }
                    }
                    self.gui_state.show_ssh_dialog = true;
                }
            }
            GuiAction::UpdateProfile { id, name, group, host, port, username, auth_mode, password, key_path } => {
                let auth = if auth_mode == 0 {
                    nexterm_ssh::AuthMethod::Password(password)
                } else {
                    nexterm_ssh::AuthMethod::PublicKey {
                        key_path,
                        passphrase: None,
                    }
                };
                // Find and update the existing profile
                if let Some(pos) = self.stored_profiles.iter().position(|p| p.id.to_string() == id) {
                    self.stored_profiles[pos].name = name;
                    self.stored_profiles[pos].host = host;
                    self.stored_profiles[pos].port = port;
                    self.stored_profiles[pos].username = username;
                    self.stored_profiles[pos].auth = auth;
                    self.stored_profiles[pos].group = Some(group);
                    if let Err(e) = self.session_store.save_profile(&self.stored_profiles[pos]) {
                        error!("failed to update profile: {e}");
                    }
                    self.sync_gui_profiles();
                }
            }
            GuiAction::ImportSshConfig => {
                let home = dirs::home_dir().unwrap_or_default();
                let config_path = home.join(".ssh").join("config");
                if let Ok(content) = std::fs::read_to_string(&config_path) {
                    let entries = nexterm_ssh::config_parser::parse_ssh_config(&content);
                    let mut imported = 0;
                    for entry in entries {
                        // Skip wildcard patterns
                        if entry.host_pattern.contains('*') || entry.host_pattern.contains('?') {
                            continue;
                        }
                        // Skip if already exists (same host pattern)
                        let hostname = entry.hostname.as_deref().unwrap_or(&entry.host_pattern);
                        if self.stored_profiles.iter().any(|p| p.host == hostname && p.name == entry.host_pattern) {
                            continue;
                        }
                        let auth = if let Some(key) = &entry.identity_file {
                            let expanded = if key.starts_with("~/") {
                                home.join(&key[2..]).to_string_lossy().to_string()
                            } else {
                                key.clone()
                            };
                            nexterm_ssh::AuthMethod::PublicKey { key_path: expanded, passphrase: None }
                        } else {
                            nexterm_ssh::AuthMethod::Agent
                        };
                        let profile = nexterm_ssh::SshProfile {
                            name: entry.host_pattern.clone(),
                            host: hostname.to_string(),
                            port: entry.port.unwrap_or(22),
                            username: entry.user.unwrap_or_else(|| "root".to_string()),
                            auth,
                            group: Some("SSH Config".to_string()),
                            ..Default::default()
                        };
                        if let Err(e) = self.session_store.save_profile(&profile) {
                            error!("failed to save imported profile: {e}");
                        }
                        self.stored_profiles.push(profile);
                        imported += 1;
                    }
                    info!(imported, "imported profiles from ~/.ssh/config");
                    self.sync_gui_profiles();
                } else {
                    warn!("could not read ~/.ssh/config");
                }
            }
            // ─── Agent ───
            GuiAction::ToggleAgentPanel => {
                self.gui_state.show_agent_panel = !self.gui_state.show_agent_panel;
                if self.gui_state.show_agent_panel {
                    self.ensure_agent_bridge();
                }
                self.relayout();
            }
            GuiAction::AgentSendMessage(msg) => {
                self.ensure_agent_bridge();
                if let Some(ref bridge) = self.agent_bridge {
                    // Gather terminal context from focused pane
                    let terminal_context = self.tab_manager.focused_pane()
                        .map(|p| p.terminal.grid().extract_visible_text_last_n_lines(30));
                    let _ = bridge.command_tx.try_send(AgentBridgeCommand::Query {
                        message: msg,
                        terminal_context,
                    });
                }
            }
            GuiAction::AgentCancel => {
                if let Some(ref bridge) = self.agent_bridge {
                    let _ = bridge.command_tx.try_send(AgentBridgeCommand::Cancel);
                }
                self.gui_state.agent_is_running = false;
            }
            GuiAction::AgentReset => {
                if let Some(ref bridge) = self.agent_bridge {
                    let _ = bridge.command_tx.try_send(AgentBridgeCommand::Reset);
                }
                self.gui_state.agent_messages.clear();
                self.gui_state.agent_streaming_text.clear();
                self.gui_state.agent_is_running = false;
            }
            GuiAction::AgentConfigure { provider_type, base_url, api_key, model_id } => {
                // Persist to config file
                self.config.ai.provider = provider_type.clone();
                self.config.ai.base_url = base_url.clone();
                self.config.ai.api_key = api_key.clone();
                self.config.ai.model = model_id.clone();
                if let Err(e) = nexterm_config::save_config(&self.config_path, &self.config) {
                    error!("failed to save AI config: {e}");
                }
                // Forward to running bridge
                if let Some(ref bridge) = self.agent_bridge {
                    let _ = bridge.command_tx.try_send(AgentBridgeCommand::Configure {
                        provider_type,
                        base_url,
                        api_key,
                        model_id,
                    });
                }
            }
            // Stubs for features to be implemented
            GuiAction::SelectAll
            | GuiAction::DuplicateTab
            | GuiAction::ShowAbout
            | GuiAction::RenameTab(_, _)
            | GuiAction::DisconnectTab(_)
            | GuiAction::ReconnectTab(_)
            | GuiAction::ToggleSessionPanel
            | GuiAction::ToggleTerminalMode => {
                self.relayout();
            }
            GuiAction::SftpNavigate(path) => {
                if let Some(pane) = self.tab_manager.focused_pane() {
                    pane.sftp_navigate(&path);
                }
            }
            GuiAction::SftpGoUp => {
                if let Some(pane) = self.tab_manager.focused_pane() {
                    pane.sftp_go_up();
                }
            }
            GuiAction::SftpGoHome => {
                if let Some(pane) = self.tab_manager.focused_pane() {
                    pane.sftp_go_home();
                }
            }
            GuiAction::SftpMkdir(path) => {
                if let Some(pane) = self.tab_manager.focused_pane() {
                    pane.sftp_mkdir(&path);
                }
            }
            GuiAction::SftpTouch(path) => {
                if let Some(pane) = self.tab_manager.focused_pane() {
                    pane.sftp_touch(&path);
                }
            }
            GuiAction::SftpDownload(remote_path) => {
                // Extract filename for the save dialog default
                let filename = remote_path.rsplit('/').next().unwrap_or("download").to_string();
                let rp = remote_path.clone();
                // Open a native save-file dialog
                if let Some(local_path) = rfd::FileDialog::new()
                    .set_file_name(&filename)
                    .save_file()
                {
                    if let Some(pane) = self.tab_manager.focused_pane_mut() {
                        pane.sftp_download(&rp, &local_path.to_string_lossy());
                    }
                }
            }
            GuiAction::SftpUpload => {
                // Open native file picker for the file to upload
                if let Some(local_path) = rfd::FileDialog::new().pick_file() {
                    let local_str = local_path.to_string_lossy().to_string();
                    let filename = local_path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "upload".to_string());
                    if let Some(pane) = self.tab_manager.focused_pane_mut() {
                        let cur_dir = pane.sftp_state.current_path.trim_end_matches('/').to_string();
                        let remote_path = if cur_dir.is_empty() {
                            format!("/{filename}")
                        } else {
                            format!("{cur_dir}/{filename}")
                        };
                        pane.sftp_upload(&local_str, &remote_path);
                    }
                }
            }
            GuiAction::SftpClearTransfers => {
                if let Some(pane) = self.tab_manager.focused_pane_mut() {
                    pane.sftp_clear_done_transfers();
                }
            }
            GuiAction::ToggleHistoryPanel => {
                self.gui_state.show_history_panel = !self.gui_state.show_history_panel;
                if self.gui_state.show_history_panel {
                    self.refresh_history_list();
                }
            }
            GuiAction::RefreshHistory => {
                self.refresh_history_list();
            }
            GuiAction::ExecHistoryCommand(cmd) => {
                if let Some(pane) = self.tab_manager.focused_pane_mut() {
                    pane.write_to_pty(cmd.as_bytes());
                    // Use CR, not LF — see ExecuteCommand comment above.
                    pane.write_to_pty(b"\r");
                }
                self.gui_state.show_history_panel = false;
            }
            GuiAction::ToggleFold(pane_id, block_id) => {
                let set = self.gui_state.folded_blocks.entry(pane_id).or_default();
                if !set.remove(&block_id) {
                    set.insert(block_id);
                }
                // Reset scroll to bottom so the cursor/prompt stays visible
                // after folding (especially the last output block).
                if let Some(pane) = self.tab_manager.panes.get_mut(&pane_id) {
                    pane.terminal.grid_mut().scroll_reset();
                }
            }
            GuiAction::ScrollTo(offset) => {
                if let Some(pane) = self.tab_manager.focused_pane_mut() {
                    let max = pane.terminal.grid().scrollback.len();
                    pane.terminal.grid_mut().scroll_offset = offset.min(max);
                }
            }
            GuiAction::FindAll { query, case_sensitive, whole_word, use_regex } => {
                if let Some(pane) = self.tab_manager.focused_pane() {
                    let grid = pane.terminal.grid();
                    self.find_matches = grid.search_text(&query, case_sensitive, whole_word, use_regex);
                    self.gui_state.find_match_count = self.find_matches.len();
                    if self.find_matches.is_empty() {
                        self.gui_state.find_current_index = 0;
                    } else {
                        // Jump to first match from viewport
                        let vp_start = grid.viewport_start();
                        let idx = self.find_matches.iter()
                            .position(|(r, _, _)| *r >= vp_start)
                            .unwrap_or(0);
                        self.gui_state.find_current_index = idx;
                        self.scroll_to_find_match(idx);
                    }
                }
            }
            GuiAction::FindNext { query, case_sensitive, whole_word, use_regex } => {
                if self.find_matches.is_empty() {
                    if let Some(pane) = self.tab_manager.focused_pane() {
                        self.find_matches = pane.terminal.grid().search_text(&query, case_sensitive, whole_word, use_regex);
                        self.gui_state.find_match_count = self.find_matches.len();
                    }
                }
                if !self.find_matches.is_empty() {
                    let next = (self.gui_state.find_current_index + 1) % self.find_matches.len();
                    self.gui_state.find_current_index = next;
                    self.scroll_to_find_match(next);
                }
            }
            GuiAction::FindPrev { query, case_sensitive, whole_word, use_regex } => {
                if self.find_matches.is_empty() {
                    if let Some(pane) = self.tab_manager.focused_pane() {
                        self.find_matches = pane.terminal.grid().search_text(&query, case_sensitive, whole_word, use_regex);
                        self.gui_state.find_match_count = self.find_matches.len();
                    }
                }
                if !self.find_matches.is_empty() {
                    let prev = if self.gui_state.find_current_index == 0 {
                        self.find_matches.len() - 1
                    } else {
                        self.gui_state.find_current_index - 1
                    };
                    self.gui_state.find_current_index = prev;
                    self.scroll_to_find_match(prev);
                }
            }
            GuiAction::ClearFind => {
                self.find_matches.clear();
                self.gui_state.find_match_count = 0;
                self.gui_state.find_current_index = 0;
            }
        }
        self.dirty = true;
    }

    /// Scroll the focused pane's viewport so that the match at `idx` is visible.
    fn scroll_to_find_match(&mut self, idx: usize) {
        if let Some((abs_row, _, _)) = self.find_matches.get(idx) {
            if let Some(pane) = self.tab_manager.focused_pane_mut() {
                let grid = pane.terminal.grid_mut();
                let sb_len = grid.scrollback.len();
                let rows = grid.rows;
                // Calculate scroll_offset so that abs_row is visible
                // viewport shows rows [sb_len - offset .. sb_len - offset + rows)
                if *abs_row < sb_len {
                    // Match is in scrollback — scroll so it's ~1/3 from top
                    let target_start = abs_row.saturating_sub(rows / 3);
                    grid.scroll_offset = sb_len.saturating_sub(target_start);
                } else {
                    // Match is in active screen — just reset to live
                    grid.scroll_offset = 0;
                }
            }
        }
    }

    /// Get the configured shell (None = auto-detect).
    fn configured_shell(&self) -> Option<&str> {
        let s = self.config.general.default_shell.as_str();
        if s.is_empty() || s == "auto" { None } else { Some(s) }
    }

    /// Open a new SSH tab. Reads connection info from the given profile.
    #[allow(clippy::needless_pass_by_value)]
    fn open_ssh_tab(&mut self, profile: nexterm_ssh::SshProfile) {
        let area = self.pane_area();
        let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
        self.tab_manager.new_ssh_tab(area, cw, ch, profile, &self.tokio_rt);
        self.relayout();
    }

    /// Resolve the current theme from config.
    fn resolve_theme(&self) -> ResolvedTheme {
        let theme_name = &self.config.appearance.theme;
        let theme = nexterm_theme::builtin_theme(theme_name)
            .unwrap_or_else(|| {
                // Try loading from file: <config_dir>/nexterm/themes/<name>.toml
                let theme_path = self.config_path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join("themes")
                    .join(format!("{}.toml", theme_name));
                if theme_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&theme_path) {
                        if let Ok(t) = nexterm_theme::load_theme_toml(&content) {
                            info!(theme = %theme_name, path = %theme_path.display(), "custom theme loaded");
                            return t;
                        }
                    }
                }
                warn!(theme = %theme_name, "theme not found, using default");
                nexterm_theme::Theme::default()
            });
        ResolvedTheme::from_theme(&theme)
    }

    /// Reload config from disk and apply changes.
    fn reload_config(&mut self) {
        match nexterm_config::load_config(&self.config_path) {
            Ok(new_config) => {
                info!("config reloaded");
                let theme_changed = new_config.appearance.theme != self.config.appearance.theme;
                self.config = new_config;
                if theme_changed {
                    let resolved = self.resolve_theme();
                    info!(theme = %resolved.name, "theme applied");
                    if let Some(renderer) = &mut self.renderer {
                        renderer.theme = resolved;
                    }
                    self.dirty = true;
                }
            }
            Err(e) => warn!("config reload failed: {e}"),
        }
    }

    /// Compute the available rect for pane content using the actual egui central rect,
    /// inset by the configured padding.
    fn pane_area(&self) -> Rect {
        let [x, y, w, h] = self.terminal_rect;
        let pad = self.config.appearance.padding;
        Rect {
            x: x + pad,
            y: y + pad,
            w: (w - pad * 2.0).max(0.0),
            h: (h - pad * 2.0).max(0.0),
        }
    }

    /// Convert pixel position to (col, row) relative to the focused pane.
    fn pixel_to_pane_cell(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        let pane = self.tab_manager.panes.get(&self.tab_manager.focused_pane_id()?)?;
        let vp = &pane.viewport;
        let renderer = self.renderer.as_ref()?;
        let (cw, ch) = renderer.cell_size();

        let local_x = px as f32 - vp.x;
        let local_y = py as f32 - vp.y;
        if local_x < 0.0 || local_y < 0.0 {
            return None;
        }
        let col = (local_x / cw).floor() as usize;
        let row = (local_y / ch).floor() as usize;
        Some((col, row))
    }

    /// Extract selected text from the focused pane.
    /// Selection uses absolute row indices (scrollback + screen).
    fn get_selection_text(&self) -> Option<String> {
        let pane = self.tab_manager.panes.get(&self.tab_manager.focused_pane_id()?)?;
        let grid = pane.terminal.grid();
        let sel = grid.selection.as_ref()?;
        let (r0, c0, r1, c1) = sel.ordered();

        let mut text = String::new();
        for abs_row in r0..=r1 {
            let row_data = match grid.absolute_row(abs_row) {
                Some(r) => r,
                None => continue,
            };
            let col_start = if abs_row == r0 { c0 } else { 0 };
            let col_end = if abs_row == r1 { c1 + 1 } else { grid.cols };
            let col_end = col_end.min(row_data.len());
            let col_start = col_start.min(col_end);

            let line: String = row_data[col_start..col_end]
                .iter()
                .filter(|c| !c.attrs.contains(nexterm_vte::grid::CellAttrs::WIDE_TAIL))
                .map(|c| c.ch)
                .collect();
            text.push_str(line.trim_end());
            if abs_row < r1 {
                text.push('\n');
            }
        }
        if text.is_empty() { None } else { Some(text) }
    }

    /// Paste text from clipboard into the focused pane's PTY.
    fn paste_clipboard(&mut self) {
        let text = self
            .clipboard
            .as_mut()
            .and_then(|cb| cb.get_text().ok());
        if let Some(text) = text {
            if let Some(pane) = self.tab_manager.focused_pane_mut() {
                // Convert \r\n and \n to \r (terminal expects CR for newlines).
                let converted = text.replace("\r\n", "\r").replace('\n', "\r");
                let bp = pane.terminal.grid().bracketed_paste;
                if bp { pane.write_to_pty(b"\x1b[200~"); }
                pane.write_to_pty(converted.as_bytes());
                if bp { pane.write_to_pty(b"\x1b[201~"); }
            }
        }
    }

    /// Copy current selection to clipboard.
    fn copy_selection(&mut self) {
        if let Some(text) = self.get_selection_text() {
            if let Some(cb) = &mut self.clipboard {
                let _ = cb.set_text(&text);
            }
        }
    }

    /// Map a key event to bytes for the PTY.
    fn key_to_bytes(&self, key: &Key) -> Option<Vec<u8>> {
        let ctrl = self.modifiers.control_key();
        let shift = self.modifiers.shift_key();

        match key {
            Key::Named(n) => match n {
                NamedKey::Space => Some(b" ".to_vec()),
                NamedKey::Enter => Some(b"\r".to_vec()),
                NamedKey::Backspace => {
                    if ctrl { Some(b"\x08".to_vec()) } else { Some(b"\x7f".to_vec()) }
                }
                NamedKey::Tab => {
                    // Suppress spurious Tab from Alt+Tab window switch
                    if self.last_focus_time.elapsed().as_millis() < 150 {
                        None
                    } else if shift {
                        Some(b"\x1b[Z".to_vec())
                    } else {
                        Some(b"\t".to_vec())
                    }
                }
                NamedKey::Escape => Some(b"\x1b".to_vec()),
                NamedKey::ArrowUp => Some(b"\x1b[A".to_vec()),
                NamedKey::ArrowDown => Some(b"\x1b[B".to_vec()),
                NamedKey::ArrowRight => Some(b"\x1b[C".to_vec()),
                NamedKey::ArrowLeft => Some(b"\x1b[D".to_vec()),
                NamedKey::Home => Some(b"\x1b[H".to_vec()),
                NamedKey::End => Some(b"\x1b[F".to_vec()),
                NamedKey::Insert => Some(b"\x1b[2~".to_vec()),
                NamedKey::Delete => Some(b"\x1b[3~".to_vec()),
                NamedKey::PageUp => Some(b"\x1b[5~".to_vec()),
                NamedKey::PageDown => Some(b"\x1b[6~".to_vec()),
                NamedKey::F1 => Some(b"\x1bOP".to_vec()),
                NamedKey::F2 => Some(b"\x1bOQ".to_vec()),
                NamedKey::F3 => Some(b"\x1bOR".to_vec()),
                NamedKey::F4 => Some(b"\x1bOS".to_vec()),
                NamedKey::F5 => Some(b"\x1b[15~".to_vec()),
                NamedKey::F6 => Some(b"\x1b[17~".to_vec()),
                NamedKey::F7 => Some(b"\x1b[18~".to_vec()),
                NamedKey::F8 => Some(b"\x1b[19~".to_vec()),
                NamedKey::F9 => Some(b"\x1b[20~".to_vec()),
                NamedKey::F10 => Some(b"\x1b[21~".to_vec()),
                NamedKey::F11 => Some(b"\x1b[23~".to_vec()),
                NamedKey::F12 => Some(b"\x1b[24~".to_vec()),
                _ => None,
            },
            Key::Character(c) => {
                let s = c.as_str();
                if ctrl && s.len() == 1 {
                    let ch = s.chars().next().unwrap();
                    match ch {
                        'a'..='z' => Some(vec![ch as u8 - b'a' + 1]),
                        '@' => Some(vec![0x00]),
                        '[' => Some(vec![0x1b]),
                        '\\' => Some(vec![0x1c]),
                        ']' => Some(vec![0x1d]),
                        '^' => Some(vec![0x1e]),
                        '_' => Some(vec![0x1f]),
                        _ => Some(s.as_bytes().to_vec()),
                    }
                } else {
                    Some(s.as_bytes().to_vec())
                }
            }
            _ => None,
        }
    }

    /// Re-layout all panes after window resize or split change.
    fn relayout(&mut self) {
        if self.renderer.is_some() {
            let area = self.pane_area();
            let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
            let gutter = if self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log {
                nexterm_render::renderer::LOG_GUTTER_COLS
            } else {
                0
            };
            self.tab_manager.layout_active_tab(area, cw, ch, gutter);
            self.dirty = true;
        }
    }
}

/// Detect available shells on the system.
fn detect_available_shells() -> Vec<gui::ShellInfo> {
    let mut shells = Vec::new();

    #[cfg(windows)]
    {
        // Windows: check well-known paths and PATH
        let candidates: Vec<(&str, Vec<&str>)> = vec![
            ("PowerShell 7", vec![
                r"C:\Program Files\PowerShell\7\pwsh.exe",
                "pwsh.exe",
            ]),
            ("Windows PowerShell", vec![
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                "powershell.exe",
            ]),
            ("CMD", vec![
                r"C:\Windows\System32\cmd.exe",
                "cmd.exe",
            ]),
            ("Git Bash", vec![
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Program Files (x86)\Git\bin\bash.exe",
            ]),
            ("WSL", vec![
                r"C:\Windows\System32\wsl.exe",
                "wsl.exe",
            ]),
            ("Nushell", vec![
                "nu.exe",
                r"C:\Program Files\nu\bin\nu.exe",
            ]),
        ];

        for (name, paths) in &candidates {
            for path in paths {
                let p = std::path::Path::new(path);
                if p.exists() {
                    shells.push(gui::ShellInfo {
                        name: name.to_string(),
                        path: path.to_string(),
                    });
                    break;
                }
                // Try finding on PATH via `where`
                if !path.contains('\\') {
                    use std::os::windows::process::CommandExt;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    if let Ok(output) = std::process::Command::new("where")
                        .arg(path)
                        .creation_flags(CREATE_NO_WINDOW)
                        .output()
                    {
                        if output.status.success() {
                            let found = String::from_utf8_lossy(&output.stdout);
                            if let Some(first) = found.lines().next() {
                                shells.push(gui::ShellInfo {
                                    name: name.to_string(),
                                    path: first.trim().to_string(),
                                });
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        let candidates = vec![
            ("Zsh", "/bin/zsh"),
            ("Bash", "/bin/bash"),
            ("Fish", "/usr/bin/fish"),
            ("Fish", "/opt/homebrew/bin/fish"),       // macOS Homebrew (Apple Silicon)
            ("Fish", "/usr/local/bin/fish"),           // macOS Homebrew (Intel)
            ("Nushell", "/usr/bin/nu"),
            ("Nushell", "/opt/homebrew/bin/nu"),
            ("Bash", "/opt/homebrew/bin/bash"),        // newer Bash via Homebrew
            ("Dash", "/bin/dash"),
            ("Sh", "/bin/sh"),
        ];
        for (name, path) in &candidates {
            if shells.iter().any(|s: &gui::ShellInfo| s.name == *name) { continue; }
            if std::path::Path::new(path).exists() {
                shells.push(gui::ShellInfo {
                    name: name.to_string(),
                    path: path.to_string(),
                });
            }
        }
        // Also check PATH for nu, fish in non-standard locations
        for (name, cmd) in &[("Fish", "fish"), ("Nushell", "nu")] {
            if !shells.iter().any(|s| s.name == *name) {
                if let Ok(output) = std::process::Command::new("which")
                    .arg(cmd)
                    .output()
                {
                    if output.status.success() {
                        let found = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !found.is_empty() {
                            shells.push(gui::ShellInfo {
                                name: name.to_string(),
                                path: found,
                            });
                        }
                    }
                }
            }
        }
    }

    shells
}

impl ApplicationHandler for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::wait_duration(
            std::time::Duration::from_millis(8),
        ));

        // Pick up async shell-detection result if it has arrived
        if let Ok(shells) = self.shell_detect_rx.try_recv() {
            self.gui_state.available_shells = shells;
            self.dirty = true;
        }

        // Poll Agent bridge events
        self.poll_agent_events();

        // Continuous auto-scroll while drag-selecting and mouse is held outside
        if self.mouse_pressed && !self.gui_state.scrollbar_dragging {
            if let Some(pane_id) = self.tab_manager.focused_pane_id() {
                if let Some(pane) = self.tab_manager.panes.get(&pane_id) {
                    let vp = &pane.viewport;
                    if let Some(renderer) = &self.renderer {
                        let (cw, ch) = renderer.cell_size();
                        let (mx, my) = self.mouse_px;
                        let local_x = mx as f32 - vp.x;
                        let local_y = my as f32 - vp.y;

                        let auto_scroll: i32 = if local_y < 0.0 {
                            -1
                        } else if (local_y / ch).floor() as usize >= pane.rows {
                            1
                        } else {
                            0
                        };

                        if auto_scroll != 0 {
                            let viewport_row = if auto_scroll < 0 { 0 } else { pane.rows.saturating_sub(1) };
                            let col = (local_x / cw).floor().max(0.0) as usize;
                            let col = col.min(pane.cols.saturating_sub(1));

                            let log_mode = self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log;
                            let empty_folds = std::collections::HashSet::new();
                            let folds = self.gui_state.folded_blocks.get(&pane_id).cloned().unwrap_or(empty_folds);
                            if let Some(pane) = self.tab_manager.panes.get_mut(&pane_id) {
                                let grid = pane.terminal.grid_mut();
                                if auto_scroll < 0 {
                                    grid.scroll_viewport_up(1);
                                } else {
                                    grid.scroll_viewport_down(1);
                                }
                                let abs_row = if log_mode {
                                    grid.visual_to_absolute(viewport_row, &folds)
                                } else {
                                    grid.viewport_to_absolute(viewport_row)
                                };
                                if grid.selection.is_none() {
                                    if let Some((anchor_row, anchor_col)) = self.pending_select_anchor {
                                        grid.selection = Some(Selection {
                                            start_row: anchor_row,
                                            start_col: anchor_col,
                                            end_row: abs_row,
                                            end_col: col,
                                        });
                                    }
                                } else if let Some(sel) = &mut grid.selection {
                                    sel.end_row = abs_row;
                                    sel.end_col = col;
                                }
                                self.dirty = true;
                            }
                        }
                    }
                }
            }
        }

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // Start hidden to avoid showing an unpainted transparent window during
        // GPU/font initialization (~1s). We make it visible after the first redraw.
        let attrs = Window::default_attributes()
            .with_title("NexTerm")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0))
            .with_visible(false);

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        // Set window icon from embedded PNG
        {
            static ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");
            if let Ok(img) = image::load_from_memory(ICON_PNG) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                // Windows & Linux: set taskbar/window icon
                if let Ok(icon) = winit::window::Icon::from_rgba(rgba.clone().into_raw(), w, h) {
                    window.set_window_icon(Some(icon));
                }
                // macOS: set Dock icon via NSApplication setApplicationIconImage
                #[cfg(target_os = "macos")]
                {
                    unsafe {
                        use std::ffi::c_void;
                        unsafe extern "C" {
                            fn objc_getClass(name: *const u8) -> *mut c_void;
                            fn objc_msgSend(obj: *mut c_void, sel: *mut c_void, ...) -> *mut c_void;
                            fn sel_registerName(name: *const u8) -> *mut c_void;
                        }

                        // Use the original PNG bytes directly for NSImage
                        let ns_data_class = objc_getClass(b"NSData\0".as_ptr());
                        let sel_data = sel_registerName(b"dataWithBytes:length:\0".as_ptr());
                        let ns_data = objc_msgSend(
                            ns_data_class,
                            sel_data,
                            ICON_PNG.as_ptr() as *const c_void,
                            ICON_PNG.len(),
                        );

                        // [[NSImage alloc] initWithData:]
                        let ns_image_class = objc_getClass(b"NSImage\0".as_ptr());
                        let sel_alloc = sel_registerName(b"alloc\0".as_ptr());
                        let sel_init_data = sel_registerName(b"initWithData:\0".as_ptr());
                        let ns_image = objc_msgSend(ns_image_class, sel_alloc);
                        let ns_image = objc_msgSend(ns_image, sel_init_data, ns_data);

                        if !ns_image.is_null() {
                            // [NSApplication sharedApplication]
                            let ns_app_class = objc_getClass(b"NSApplication\0".as_ptr());
                            let sel_shared = sel_registerName(b"sharedApplication\0".as_ptr());
                            let ns_app = objc_msgSend(ns_app_class, sel_shared);
                            // [app setApplicationIconImage:image]
                            let sel_set_icon = sel_registerName(b"setApplicationIconImage:\0".as_ptr());
                            objc_msgSend(ns_app, sel_set_icon, ns_image);
                        }
                    }
                }
            }
        }

        // Enable IME (Input Method Editor) for CJK input
        window.set_ime_allowed(true);

        // Apply OS-level window opacity from config
        apply_window_opacity(&window, self.config.appearance.opacity);

        info!("window created: {:?}", window.id());

        let font_size = self.config.appearance.font_size;
        let font_family = &self.config.appearance.font_family;
        let mut renderer = match pollster::block_on(Renderer::new(
            window.clone(),
            font_size,
            font_family,
        )) {
            Ok(r) => r,
            Err(e) => {
                error!("failed to initialize GPU renderer: {e}");
                event_loop.exit();
                return;
            }
        };

        info!(
            "GPU renderer initialized ({}x{})",
            renderer.width, renderer.height
        );

        // Apply theme from config
        let resolved_theme = self.resolve_theme();
        info!(theme = %resolved_theme.name, "theme applied");
        renderer.theme = resolved_theme;

        self.renderer = Some(renderer);

        // Start config file watcher
        match nexterm_config::watcher::watch_config(&self.config_path) {
            Ok(rx) => self.config_reload_rx = Some(rx),
            Err(e) => warn!("config watcher failed: {e}"),
        }

        // Detect available shells in the background so it doesn't block startup.
        // The new-tab dropdown will populate as soon as detection completes.
        let shells_tx = self.shell_detect_tx.clone();
        std::thread::spawn(move || {
            let shells = detect_available_shells();
            info!("detected {} shells", shells.len());
            let _ = shells_tx.send(shells);
        });

        // Create initial tab
        let area = self.pane_area();
        let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
        let shell = self.configured_shell().map(|s| s.to_string());
        if let Err(e) = self.tab_manager.new_tab(area, cw, ch, shell.as_deref()) {
            error!("failed to create initial tab: {e}");
            event_loop.exit();
            return;
        }

        info!("initial tab created");

        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        // Forward to egui first
        let egui_resp_consumed = if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
            renderer.handle_egui_event(window, &event)
        } else {
            false
        };

        // For mouse events, egui always reports consumed=true due to panels.
        // Instead, check if the mouse is inside the terminal content area.
        let is_mouse_event = matches!(
            event,
            WindowEvent::MouseInput { .. }
            | WindowEvent::CursorMoved { .. }
            | WindowEvent::MouseWheel { .. }
        );
        let egui_using_pointer = self.renderer.as_ref()
            .map(|r| r.egui_wants_pointer())
            .unwrap_or(false);
        let egui_consumed = if is_mouse_event {
            if egui_using_pointer {
                // egui is actively dragging (e.g. panel resize) — block terminal
                true
            } else {
                let [tx, ty, tw, th] = self.terminal_rect;
                let (mx, my) = self.mouse_px;
                let mx = mx as f32;
                let my = my as f32;
                // If mouse is outside the terminal rect, egui (panels) consumed it
                !(mx >= tx && mx <= tx + tw && my >= ty && my <= ty + th)
            }
        } else {
            egui_resp_consumed
        };

        match event {
            WindowEvent::CloseRequested => {
                info!("close requested, shutting down");
                event_loop.exit();
            }

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }

            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                self.relayout();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // Make sure the renderer surface matches the current window size.
                // Guards against startup races where window.inner_size() during
                // Renderer::new differs from the size at first present.
                if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
                    let size = window.inner_size();
                    if size.width >= 1
                        && size.height >= 1
                        && (renderer.width != size.width || renderer.height != size.height)
                    {
                        renderer.resize(size.width, size.height);
                        self.relayout();
                        self.dirty = true;
                    }
                }

                // Check for config hot-reload
                if let Some(rx) = &self.config_reload_rx {
                    if rx.try_recv().is_ok() {
                        self.reload_config();
                    }
                }

                // Check if background font rebuild is ready
                self.poll_font_change();

                // Poll all panes for PTY output
                let has_pty_data = self.tab_manager.poll_active_tab();
                if has_pty_data {
                    self.dirty = true;
                    self.cursor_blink_on = true;
                    self.last_blink = Instant::now();
                }

                // Capture completed command blocks into history
                if has_pty_data {
                    for pane in self.tab_manager.panes.values() {
                        let grid = pane.terminal.grid();
                        for (idx, block) in grid.block_list.blocks().iter().enumerate() {
                            if block.state != nexterm_vte::grid::BlockState::Completed {
                                continue;
                            }
                            if self.history_recorded_blocks.contains(&block.id) {
                                continue;
                            }
                            if let Some(cmd_text) = grid.block_command_text(idx) {
                                let entry = nexterm_history::HistoryEntry {
                                    id: uuid::Uuid::new_v4(),
                                    command: cmd_text,
                                    output_summary: String::new(),
                                    exit_code: block.exit_code.unwrap_or(-1),
                                    session_id: None,
                                    host: None,
                                    cwd: None,
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() as i64)
                                        .unwrap_or(0),
                                };
                                if let Err(e) = self.history_db.insert(&entry) {
                                    warn!("failed to insert history: {e}");
                                }
                                self.history_recorded_blocks.insert(block.id);
                            }
                        }
                    }
                }

                // Poll SFTP results for SSH panes
                for pane in self.tab_manager.panes.values_mut() {
                    if pane.poll_sftp() {
                        self.dirty = true;
                    }
                }

                // Detect exited panes and close them.
                {
                    let exited_ids: Vec<usize> = self
                        .tab_manager
                        .panes
                        .iter()
                        .filter(|(_, p)| p.exited)
                        .map(|(&id, _)| id)
                        .collect();
                    for pane_id in exited_ids {
                        info!(pane_id, "pane exited, closing");
                        // Find which tab owns this pane
                        let tab_idx = self.tab_manager.tabs.iter().position(|t| {
                            t.layout.pane_ids().contains(&pane_id)
                        });
                        if let Some(ti) = tab_idx {
                            let tab = &mut self.tab_manager.tabs[ti];
                            let pane_ids = tab.layout.pane_ids();
                            if pane_ids.len() <= 1 {
                                // Last pane in tab → close the entire tab
                                let tab = self.tab_manager.tabs.remove(ti);
                                for pid in tab.layout.pane_ids() {
                                    self.tab_manager.panes.remove(&pid);
                                }
                                if self.tab_manager.active_tab >= self.tab_manager.tabs.len()
                                    && !self.tab_manager.tabs.is_empty()
                                {
                                    self.tab_manager.active_tab = self.tab_manager.tabs.len() - 1;
                                }
                                if self.tab_manager.tabs.is_empty() {
                                    event_loop.exit();
                                    return;
                                }
                            } else {
                                // Remove pane from layout tree
                                if let Some(replacement) = tab.layout.remove_pane(pane_id) {
                                    tab.layout = replacement;
                                }
                                self.tab_manager.panes.remove(&pane_id);
                                // Re-focus if the closed pane was focused
                                let tab = &mut self.tab_manager.tabs[ti];
                                if tab.focused_pane == pane_id {
                                    let remaining = tab.layout.pane_ids();
                                    if let Some(&first) = remaining.first() {
                                        tab.focused_pane = first;
                                    }
                                }
                            }
                        } else {
                            // Orphan pane (not in any tab) — just remove
                            self.tab_manager.panes.remove(&pane_id);
                        }
                        self.dirty = true;
                    }
                }

                // Cursor blink timer
                let elapsed = self.last_blink.elapsed().as_millis() as u64;
                if elapsed >= CURSOR_BLINK_MS {
                    self.cursor_blink_on = !self.cursor_blink_on;
                    self.last_blink = Instant::now();
                    if let Some(renderer) = &mut self.renderer {
                        renderer.cursor_blink_visible = self.cursor_blink_on;
                    }
                    self.dirty = true;
                }

                // Rate-limit GPU rendering to ~30fps during sustained PTY output.
                // This frees CPU for VTE parsing during high-throughput streams
                // while still providing smooth visual feedback.
                const MIN_RENDER_INTERVAL: std::time::Duration =
                    std::time::Duration::from_millis(33);
                if has_pty_data && self.last_render.elapsed() < MIN_RENDER_INTERVAL {
                    return;
                }

                if self.dirty {
                    if let Some(renderer) = &mut self.renderer {
                        // Build multi-pane instances
                        let focused_id = self.tab_manager.focused_pane_id();
                        let pane_data: Vec<_> = self
                            .tab_manager
                            .visible_panes()
                            .iter()
                            .map(|(pane, id)| {
                                (
                                    pane.terminal.grid(),
                                    pane.viewport.x,
                                    pane.viewport.y,
                                    Some(*id) == focused_id,
                                    *id,
                                )
                            })
                            .collect();

                        // Build find-highlight cell sets
                        let mut find_all = std::collections::HashSet::new();
                        let mut find_cur = std::collections::HashSet::new();
                        for (i, (abs_row, col, len)) in self.find_matches.iter().enumerate() {
                            for c in *col..(*col + *len) {
                                find_all.insert((*abs_row, c));
                                if i == self.gui_state.find_current_index {
                                    find_cur.insert((*abs_row, c));
                                }
                            }
                        }

                        let log_mode = self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log;
                        renderer.update_multi_pane(&pane_data, &find_all, &find_cur, log_mode, &self.gui_state.folded_blocks);
                    }
                    self.dirty = false;
                }

                // --- egui GUI overlay ---
                let (gui_actions, gui_rect) = {
                    let window = self.window.clone();
                    if let (Some(renderer), Some(window)) = (&mut self.renderer, &window) {
                        renderer.begin_egui_frame(window);

                        // Build tab list for GUI
                        let tabs: Vec<gui::TabInfo> = self
                            .tab_manager
                            .tabs
                            .iter()
                            .enumerate()
                            .map(|(i, t)| {
                                // Find the focused pane in this tab to get ssh/size info
                                let pane_ids: Vec<usize> = t.layout.pane_ids();
                                let focused = pane_ids.get(t.focused_pane).copied()
                                    .or_else(|| pane_ids.first().copied());
                                let pane = focused.and_then(|id| self.tab_manager.panes.get(&id));
                                let server_status = pane.and_then(|p| {
                                    p.server_status().map(|ss| {
                                        let dur = ss.connection_duration();
                                        gui::ServerStatusSnapshot {
                                            os: ss.os.clone(),
                                            kernel: ss.kernel.clone(),
                                            hostname: ss.hostname.clone(),
                                            uptime: ss.uptime.clone(),
                                            load_avg: ss.load_avg.clone(),
                                            mem_total_mb: ss.mem_total_mb,
                                            mem_used_mb: ss.mem_used_mb,
                                            cpu_pct: ss.cpu_usage_pct,
                                            disks: ss.disks.iter().map(|d| gui::DiskSnapshot {
                                                mount: d.mount.clone(),
                                                fstype: d.fstype.clone(),
                                                total: d.total.clone(),
                                                used: d.used.clone(),
                                                avail: d.avail.clone(),
                                                use_pct: d.use_pct.clone(),
                                            }).collect(),
                                            net_history: ss.net_history.iter().map(|n| gui::NetRateSnapshot {
                                                rx_bps: n.rx_bps,
                                                tx_bps: n.tx_bps,
                                            }).collect(),
                                            net_interfaces: ss.net_if_history.iter().map(|(name, hist)| gui::NetIfInfo {
                                                name: name.clone(),
                                                history: hist.iter().map(|n| gui::NetRateSnapshot {
                                                    rx_bps: n.rx_bps,
                                                    tx_bps: n.tx_bps,
                                                }).collect(),
                                            }).collect(),
                                            disk_usage: ss.disk_usage.clone(),
                                            latency_ms: ss.latency_ms,
                                            connection_duration: dur,
                                            top_procs: ss.top_procs.iter().map(|p| gui::ProcessSnapshot {
                                                name: p.name.clone(),
                                                cpu_pct: p.cpu_pct,
                                                mem_str: p.mem_str.clone(),
                                                mem_kb: p.mem_kb,
                                            }).collect(),
                                        }
                                    })
                                });
                                gui::TabInfo {
                                    title: t.title.clone(),
                                    is_active: i == self.tab_manager.active_tab,
                                    is_ssh: pane.map(|p| p.is_ssh()).unwrap_or(false),
                                    is_connected: pane.is_some(),
                                    cols: pane.map(|p| p.cols as u16).unwrap_or(80),
                                    rows: pane.map(|p| p.rows as u16).unwrap_or(24),
                                    server_status,
                                }
                            })
                            .collect();

                        // Build SFTP snapshot from focused SSH pane
                        let sftp_snap = self.tab_manager.focused_pane().and_then(|p| {
                            if !p.is_ssh() { return None; }
                            let st = &p.sftp_state;
                            Some(gui::SftpBrowserSnapshot {
                                current_path: st.current_path.clone(),
                                entries: st.entries.iter().map(|e| gui::SftpEntry {
                                    name: e.name.clone(),
                                    path: e.path.clone(),
                                    is_dir: e.is_dir,
                                    size: e.size,
                                    file_type: e.file_type.clone(),
                                    permissions: gui::fmt_perms(e.permissions),
                                    modified: gui::fmt_mtime(e.modified),
                                    owner: e.owner.clone(),
                                    group: format!("{}/{}", e.owner, e.group),
                                }).collect(),
                                initialized: st.initialized,
                                error: st.error.clone(),
                                transfers: st.transfers.iter().map(|t| gui::SftpTransferInfo {
                                    direction: match t.direction {
                                        crate::ssh_backend::TransferDir::Upload => "Upload",
                                        crate::ssh_backend::TransferDir::Download => "Download",
                                    },
                                    remote_path: t.remote_path.clone(),
                                    local_path: t.local_path.clone(),
                                    bytes: t.bytes,
                                    total: t.total,
                                    done: t.done,
                                    error: t.error.clone(),
                                }).collect(),
                            })
                        });

                        let (mut actions, central_rect) = gui::draw_gui(
                            renderer.egui_ctx(),
                            &mut self.gui_state,
                            &tabs,
                            self.tab_manager.active_tab,
                            sftp_snap.as_ref(),
                        );

                        // Draw scrollbar overlay
                        if let Some(pane_id) = self.tab_manager.focused_pane_id() {
                        if let Some(pane) = self.tab_manager.panes.get(&pane_id) {
                            let grid = pane.terminal.grid();
                            // Use virtual (fold-compressed) row count for scrollbar
                            let is_log = self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log;
                            let (virt_scrollback, virt_offset) = if is_log && !grid.is_alt_screen {
                                let empty = std::collections::HashSet::new();
                                let folds = self.gui_state.folded_blocks.get(&pane_id).unwrap_or(&empty);
                                let cur = grid.scrollback.len() + grid.cursor_row;
                                let vtotal = grid.block_list.virtual_total(folds, grid.total_rows(), cur);
                                let vsb = vtotal.saturating_sub(grid.rows);
                                (vsb, grid.scroll_offset.min(vsb))
                            } else {
                                (grid.scrollback.len(), grid.scroll_offset)
                            };
                            let scroll_info = gui::ScrollInfo {
                                scrollback_len: virt_scrollback,
                                visible_rows: grid.rows,
                                scroll_offset: virt_offset,
                            };
                            if let Some(scroll_action) = gui::draw_scrollbar(
                                renderer.egui_ctx(),
                                &mut self.gui_state,
                                central_rect,
                                &scroll_info,
                            ) {
                                actions.push(scroll_action);
                            }
                        }
                        }

                        // Convert egui logical rect to physical pixels and snap to
                        // integer pixel boundaries. Fractional viewport offsets
                        // make every glyph's atlas UV land between texels, which
                        // LINEAR sampling smears → blurry text + 1px clipping.
                        let ppp = renderer.egui_ctx().pixels_per_point();
                        let x = (central_rect.min.x * ppp).round();
                        let y = (central_rect.min.y * ppp).round();
                        let max_x = (central_rect.max.x * ppp).round();
                        let max_y = (central_rect.max.y * ppp).round();
                        let new_rect = [x, y, (max_x - x).max(0.0), (max_y - y).max(0.0)];
                        (actions, new_rect)
                    } else {
                        (vec![], self.terminal_rect)
                    }
                };

                // Update terminal rect and relayout if changed
                if (gui_rect[0] - self.terminal_rect[0]).abs() > 1.0
                    || (gui_rect[1] - self.terminal_rect[1]).abs() > 1.0
                    || (gui_rect[2] - self.terminal_rect[2]).abs() > 1.0
                    || (gui_rect[3] - self.terminal_rect[3]).abs() > 1.0
                {
                    self.terminal_rect = gui_rect;
                    self.relayout();
                }

                // Process GUI actions (outside renderer borrow)
                for action in gui_actions {
                    self.handle_gui_action(action, event_loop);
                }

                // Render frame (terminal cells + egui overlay)
                if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
                    if let Err(e) = renderer.render_frame(window) {
                        error!("render error: {e}");
                        event_loop.exit();
                        return;
                    }
                    // First successful paint — reveal the window now so users
                    // never see the unpainted transparent flash during init.
                    if !self.window_visible {
                        window.set_visible(true);
                        self.window_visible = true;
                    }
                }
                self.last_render = Instant::now();
            }

            // ---- Focus tracking (suppress spurious Tab after Alt+Tab) ----
            WindowEvent::Focused(gained) => {
                if gained {
                    self.last_focus_time = Instant::now();
                }
            }

            // ---- Keyboard ----
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
                if key_event.state != ElementState::Pressed {
                    return;
                }
                let ctrl = self.modifiers.control_key();
                let shift = self.modifiers.shift_key();
                let alt = self.modifiers.alt_key();

                // ---- App-level shortcuts (Ctrl+Shift+...) ----
                if ctrl && shift {
                    if let Key::Character(c) = &key_event.logical_key {
                        match c.as_str() {
                            "C" | "c" => { self.copy_selection(); return; }
                            "V" | "v" => { self.paste_clipboard(); return; }
                            "T" | "t" => {
                                // New tab
                                let area = self.pane_area();
                                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                                let shell = self.configured_shell().map(|s| s.to_string());
                                if let Err(e) = self.tab_manager.new_tab(area, cw, ch, shell.as_deref()) {
                                    error!("failed to create tab: {e}");
                                }
                                self.relayout();
                                return;
                            }
                            "W" | "w" => {
                                // Close tab
                                if !self.tab_manager.close_active_tab() {
                                    event_loop.exit();
                                }
                                self.relayout();
                                return;
                            }
                            "D" | "d" => {
                                // Split horizontal
                                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                                let shell = self.configured_shell().map(|s| s.to_string());
                                let _ = self.tab_manager.split_focused(SplitDir::Horizontal, cw, ch, shell.as_deref(), &self.tokio_rt);
                                self.relayout();
                                return;
                            }
                            "E" | "e" => {
                                // Split vertical
                                let (cw, ch) = self.renderer.as_ref().unwrap().cell_size();
                                let shell = self.configured_shell().map(|s| s.to_string());
                                let _ = self.tab_manager.split_focused(SplitDir::Vertical, cw, ch, shell.as_deref(), &self.tokio_rt);
                                self.relayout();
                                return;
                            }
                            "S" | "s" => {
                                // Open SSH tab from config profile or env vars
                                let profile = if let Some(cfg) = self.config.ssh_profiles.first() {
                                    let auth = match cfg.auth.as_str() {
                                        "password" => {
                                            let pass = cfg.password.as_deref().unwrap_or("").to_string();
                                            nexterm_ssh::AuthMethod::Password(pass)
                                        }
                                        "key" => nexterm_ssh::AuthMethod::PublicKey {
                                            key_path: cfg.key_path.clone().unwrap_or_default(),
                                            passphrase: cfg.key_passphrase.clone(),
                                        },
                                        _ => nexterm_ssh::AuthMethod::Agent,
                                    };
                                    Some(nexterm_ssh::SshProfile {
                                        host: cfg.host.clone(),
                                        port: cfg.port,
                                        username: cfg.username.clone(),
                                        auth,
                                        ..Default::default()
                                    })
                                } else if let Ok(host) = std::env::var("NEXTERM_SSH_HOST") {
                                    let user = std::env::var("NEXTERM_SSH_USER").unwrap_or_else(|_| "root".into());
                                    let port: u16 = std::env::var("NEXTERM_SSH_PORT")
                                        .ok()
                                        .and_then(|p| p.parse().ok())
                                        .unwrap_or(22);
                                    let auth = if let Ok(pass) = std::env::var("NEXTERM_SSH_PASS") {
                                        nexterm_ssh::AuthMethod::Password(pass)
                                    } else if let Ok(key) = std::env::var("NEXTERM_SSH_KEY") {
                                        nexterm_ssh::AuthMethod::PublicKey {
                                            key_path: key,
                                            passphrase: std::env::var("NEXTERM_SSH_KEY_PASS").ok(),
                                        }
                                    } else {
                                        nexterm_ssh::AuthMethod::Agent
                                    };
                                    Some(nexterm_ssh::SshProfile {
                                        host,
                                        port,
                                        username: user,
                                        auth,
                                        ..Default::default()
                                    })
                                } else {
                                    None
                                };

                                if let Some(p) = profile {
                                    info!(host = %p.host, user = %p.username, "opening SSH tab");
                                    self.open_ssh_tab(p);
                                } else {
                                    warn!("Ctrl+Shift+S: no SSH profile in config and NEXTERM_SSH_HOST not set");
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                    // Ctrl+Shift+Tab = prev tab
                    if let Key::Named(NamedKey::Tab) = &key_event.logical_key {
                        self.tab_manager.prev_tab();
                        self.relayout();
                        return;
                    }
                }

                // Ctrl-only shortcuts
                if ctrl && !shift {
                    // Ctrl+Tab = next tab
                    if let Key::Named(NamedKey::Tab) = &key_event.logical_key {
                        self.tab_manager.next_tab();
                        self.relayout();
                        return;
                    }
                    // Ctrl+= / Ctrl++ → zoom in
                    if let Key::Character(c) = &key_event.logical_key {
                        match c.as_str() {
                            "=" | "+" => {
                                self.config.appearance.font_size = (self.config.appearance.font_size + 1.0).min(72.0);
                                self.schedule_font_change();
                                return;
                            }
                            "-" => {
                                self.config.appearance.font_size = (self.config.appearance.font_size - 1.0).max(6.0);
                                self.schedule_font_change();
                                return;
                            }
                            "0" => {
                                self.config.appearance.font_size = 14.0;
                                self.schedule_font_change();
                                return;
                            }
                            "f" | "F" => {
                                self.gui_state.show_find_bar = !self.gui_state.show_find_bar;
                                self.relayout();
                                return;
                            }
                            "r" | "R" => {
                                self.gui_state.show_history_panel = !self.gui_state.show_history_panel;
                                if self.gui_state.show_history_panel {
                                    self.refresh_history_list();
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                // Alt+Arrow = focus next pane; Alt+Tab = ignore (OS window switch)
                if alt {
                    match &key_event.logical_key {
                        Key::Named(
                            NamedKey::ArrowLeft
                            | NamedKey::ArrowRight
                            | NamedKey::ArrowUp
                            | NamedKey::ArrowDown,
                        ) => {
                            self.tab_manager.focus_next_pane();
                            self.dirty = true;
                            return;
                        }
                        Key::Named(NamedKey::Tab) => {
                            // Swallow Alt+Tab so the OS window-switch doesn't inject a tab
                            return;
                        }
                        _ => {}
                    }
                }

                // If egui has a text input focused, don't forward to terminal
                let egui_wants_kb = self.renderer.as_ref()
                    .map(|r| r.egui_ctx().wants_keyboard_input())
                    .unwrap_or(false);
                if egui_wants_kb {
                    return;
                }

                // Reset cursor blink on keypress
                self.cursor_blink_on = true;
                self.last_blink = Instant::now();
                if let Some(renderer) = &mut self.renderer {
                    renderer.cursor_blink_visible = true;
                }

                // Clear selection on non-modifier keypress
                let is_modifier_only = matches!(
                    &key_event.logical_key,
                    Key::Named(
                        NamedKey::Control | NamedKey::Shift | NamedKey::Alt
                        | NamedKey::Super | NamedKey::Meta
                    )
                );
                if !is_modifier_only {
                    if let Some(pane) = self.tab_manager.focused_pane_mut() {
                        if pane.terminal.grid().selection.is_some() {
                            pane.terminal.grid_mut().selection = None;
                            self.dirty = true;
                        }
                    }
                }

                // Forward key to focused pane's PTY
                let bytes = self.key_to_bytes(&key_event.logical_key);
                if let Some(data) = bytes {
                    if let Some(pane) = self.tab_manager.focused_pane_mut() {
                        // Snap scroll to live on keypress
                        pane.terminal.grid_mut().scroll_reset();

                        // Behavioural block detection — only when shell
                        // integration is NOT active (OSC 133 handles this).
                        // Skipped in alt screen so vim/less doesn't pollute.
                        if !pane.terminal.grid().is_alt_screen
                            && !pane.terminal.grid().block_list.has_osc133()
                        {
                            if data == b"\r" || data == b"\n" {
                                // Enter: mark the current block as Executing.
                                tracing::info!("key=Enter → mark_block_executing (no osc133)");
                                pane.terminal.grid_mut().mark_block_executing();
                            } else {
                                // Non-Enter key: if the block is Executing and
                                // the cursor moved past its start, we are at
                                // a new prompt — start a fresh block.
                                use nexterm_vte::grid::{BlockState, BlockTrigger};
                                let needs_new_block = {
                                    let grid = pane.terminal.grid();
                                    if let Some(cur) = grid.block_list.current() {
                                        let abs = grid.scrollback.len()
                                            + grid.cursor_row;
                                        cur.state == BlockState::Executing
                                            && abs > cur.start_row
                                    } else {
                                        false
                                    }
                                };
                                if needs_new_block {
                                    tracing::info!("keypress InputDetected → start new block");
                                    pane.terminal.grid_mut()
                                        .start_block(BlockTrigger::InputDetected);
                                }
                            }
                        } else if !pane.terminal.grid().is_alt_screen {
                            tracing::info!(
                                has_osc133 = pane.terminal.grid().block_list.has_osc133(),
                                "keypress skipped: block detection disabled (osc133 active)"
                            );
                        }

                        pane.write_to_pty(&data);
                    }
                }
            }

            // ---- Mouse ----
            WindowEvent::MouseInput { state, button, .. } => {
                // If egui consumed the event (dialog, menu, panel resize), skip terminal
                if egui_consumed {
                    // Still track release to avoid stuck mouse_pressed state
                    if button == MouseButton::Left && state == ElementState::Released {
                        self.mouse_pressed = false;
                    }
                    return;
                }
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            // Don't start selection if clicking on the scrollbar region.
                            // The scrollbar is 10 logical px wide in egui; convert to physical.
                            let ppp = self.renderer.as_ref()
                                .map(|r| r.egui_ctx().pixels_per_point())
                                .unwrap_or(1.0);
                            let scrollbar_phys_w = 14.0 * ppp; // small extra padding for easy hit
                            let in_scrollbar = {
                                let [x, y, w, h] = self.terminal_rect;
                                let (mx, my) = self.mouse_px;
                                let mx = mx as f32;
                                let my = my as f32;
                                mx >= x + w - scrollbar_phys_w && mx <= x + w
                                    && my >= y && my <= y + h
                            };
                            if self.gui_state.scrollbar_dragging || in_scrollbar {
                                return;
                            }

                            // Log mode: gutter click → toggle fold (skip in alt screen)
                            if self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log {
                                if let Some(pane_id) = self.tab_manager.focused_pane_id() {
                                    if let Some(pane) = self.tab_manager.panes.get(&pane_id) {
                                        let in_alt = pane.terminal.grid().is_alt_screen;
                                        let vp = &pane.viewport;
                                        if !in_alt {
                                            if let Some(renderer) = &self.renderer {
                                                let (cw, ch) = renderer.cell_size();
                                                let gutter_w = 20.0 * cw;
                                                let (mx, my) = self.mouse_px;
                                                let local_x = mx as f32 - vp.x;
                                                if local_x >= 0.0 && local_x < gutter_w {
                                                    let local_y = (my as f32 - vp.y).max(0.0);
                                                    let row = (local_y / ch).floor() as usize;
                                                    let grid = pane.terminal.grid();
                                                    let empty_folds = std::collections::HashSet::new();
                                                    let folds = self.gui_state.folded_blocks.get(&pane_id).unwrap_or(&empty_folds);
                                                    let abs_row = grid.visual_to_absolute(row.min(grid.rows.saturating_sub(1)), folds);
                                                    // Toggle fold by block ID, per-pane
                                                    if let Some(block_id) = grid.block_list.block_id_at_row(abs_row) {
                                                        if grid.is_block_start(abs_row) {
                                                            let set = self.gui_state.folded_blocks.entry(pane_id).or_default();
                                                            if !set.remove(&block_id) {
                                                                set.insert(block_id);
                                                            }
                                                            self.dirty = true;
                                                            return;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            self.mouse_pressed = true;
                            // Record click position as pending anchor; selection is only
                            // created on first drag movement (so a single click doesn't
                            // highlight a cell).
                            if let Some(pane_id) = self.tab_manager.focused_pane_id() {
                                if let Some(pane) = self.tab_manager.panes.get(&pane_id) {
                                    let vp = &pane.viewport;
                                    if let Some(renderer) = &self.renderer {
                                        let (cw, ch) = renderer.cell_size();
                                        let (mx, my) = self.mouse_px;
                                        let in_alt = pane.terminal.grid().is_alt_screen;
                                        let gutter_px = if self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log && !in_alt {
                                            20.0 * cw
                                        } else { 0.0 };
                                        let local_x = (mx as f32 - vp.x - gutter_px).max(0.0);
                                        let local_y = (my as f32 - vp.y).max(0.0);
                                        let col = (local_x / cw).floor() as usize;
                                        let col = col.min(pane.cols.saturating_sub(1));
                                        let row = (local_y / ch).floor() as usize;
                                        let row = row.min(pane.rows.saturating_sub(1));

                                        let log_mode = self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log;
                                        let empty_folds = std::collections::HashSet::new();
                                        let folds = self.gui_state.folded_blocks.get(&pane_id).cloned().unwrap_or(empty_folds);
                                        if let Some(pane) = self.tab_manager.panes.get_mut(&pane_id) {
                                            let grid = pane.terminal.grid_mut();
                                            let abs_row = if log_mode {
                                                grid.visual_to_absolute(row, &folds)
                                            } else {
                                                grid.viewport_to_absolute(row)
                                            };
                                            self.pending_select_anchor = Some((abs_row, col));
                                            // Clear any existing selection so the click hides it
                                            if grid.selection.is_some() {
                                                grid.selection = None;
                                                self.dirty = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ElementState::Released => {
                            self.mouse_pressed = false;
                            self.pending_select_anchor = None;
                        }
                    }
                }
                if button == MouseButton::Middle && state == ElementState::Pressed {
                    self.paste_clipboard();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if egui_consumed {
                    return;
                }
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as isize,
                    MouseScrollDelta::PixelDelta(pos) => {
                        let ch = self.renderer.as_ref().map(|r| r.cell_size().1).unwrap_or(20.0);
                        (pos.y / ch as f64).round() as isize
                    }
                };
                if lines != 0 {
                    let pane_id = self.tab_manager.focused_pane_id();
                    let is_log = self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log;

                    if let Some(pane) = self.tab_manager.focused_pane_mut() {
                        let grid = pane.terminal.grid_mut();
                        if lines > 0 {
                            grid.scroll_viewport_up(lines as usize);
                        } else {
                            grid.scroll_viewport_down((-lines) as usize);
                        }
                        // In log mode, clamp scroll_offset to virtual scrollback range
                        if is_log && !grid.is_alt_screen {
                            if let Some(pid) = pane_id {
                                let empty = std::collections::HashSet::new();
                                let folds = self.gui_state.folded_blocks.get(&pid).unwrap_or(&empty);
                                let cur = grid.scrollback.len() + grid.cursor_row;
                                let vtotal = grid.block_list.virtual_total(folds, grid.total_rows(), cur);
                                let max_virt = vtotal.saturating_sub(grid.rows);
                                if grid.scroll_offset > max_virt {
                                    grid.scroll_offset = max_virt;
                                }
                            }
                        }
                        self.dirty = true;
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_px = (position.x, position.y);

                // If a terminal drag-select is in progress, don't let egui_consumed
                // block us — we need out-of-bounds positions for auto-scroll.
                if egui_consumed && !self.mouse_pressed {
                    return;
                }

                if self.mouse_pressed && !self.gui_state.scrollbar_dragging {
                    if let Some(pane_id) = self.tab_manager.focused_pane_id() {
                        if let Some(pane) = self.tab_manager.panes.get(&pane_id) {
                            let vp = &pane.viewport;
                            if let Some(renderer) = &self.renderer {
                                let (cw, ch) = renderer.cell_size();
                                let in_alt = pane.terminal.grid().is_alt_screen;
                                let gutter_px = if self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log && !in_alt {
                                    20.0 * cw
                                } else { 0.0 };
                                let local_x = position.x as f32 - vp.x - gutter_px;
                                let local_y = position.y as f32 - vp.y;

                                let col = (local_x / cw).floor().max(0.0) as usize;
                                let col = col.min(pane.cols.saturating_sub(1));

                                // Determine viewport row and auto-scroll direction
                                let (viewport_row, auto_scroll) = if local_y < 0.0 {
                                    // Above terminal — auto-scroll up 1 line
                                    (0usize, -1i32)
                                } else {
                                    let r = (local_y / ch).floor() as usize;
                                    if r >= pane.rows {
                                        // Below terminal — auto-scroll down 1 line
                                        (pane.rows.saturating_sub(1), 1i32)
                                    } else {
                                        (r, 0i32)
                                    }
                                };

                                // Apply auto-scroll (1 line at a time)
                                if auto_scroll != 0 {
                                    if let Some(pane) = self.tab_manager.panes.get_mut(&pane_id) {
                                        let grid = pane.terminal.grid_mut();
                                        if auto_scroll < 0 {
                                            grid.scroll_viewport_up(1);
                                        } else {
                                            grid.scroll_viewport_down(1);
                                        }
                                    }
                                }

                                // Update selection: create on first drag using pending anchor,
                                // otherwise just update end
                                let log_mode = self.gui_state.terminal_mode == nexterm_render::gui::TerminalDisplayMode::Log;
                                let empty_folds = std::collections::HashSet::new();
                                let folds = self.gui_state.folded_blocks.get(&pane_id).cloned().unwrap_or(empty_folds);
                                if let Some(pane) = self.tab_manager.panes.get_mut(&pane_id) {
                                    let grid = pane.terminal.grid_mut();
                                    let abs_row = if log_mode {
                                        grid.visual_to_absolute(viewport_row, &folds)
                                    } else {
                                        grid.viewport_to_absolute(viewport_row)
                                    };
                                    if grid.selection.is_none() {
                                        if let Some((anchor_row, anchor_col)) = self.pending_select_anchor {
                                            // Only create selection if drag has moved off anchor cell
                                            if anchor_row != abs_row || anchor_col != col {
                                                grid.selection = Some(Selection {
                                                    start_row: anchor_row,
                                                    start_col: anchor_col,
                                                    end_row: abs_row,
                                                    end_col: col,
                                                });
                                                self.dirty = true;
                                            }
                                        }
                                    } else if let Some(sel) = &mut grid.selection {
                                        if sel.end_row != abs_row || sel.end_col != col {
                                            sel.end_row = abs_row;
                                            sel.end_col = col;
                                            self.dirty = true;
                                        }
                                    }
                                }

                                // Request redraw to keep auto-scrolling while dragging
                                if auto_scroll != 0 {
                                    if let Some(window) = &self.window {
                                        window.request_redraw();
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ---- IME (Input Method Editor) ----
            WindowEvent::Ime(ime_event) => {
                if egui_consumed {
                    return;
                }
                match ime_event {
                    winit::event::Ime::Commit(text) => {
                        // IME committed text (e.g. Chinese characters)
                        if let Some(pane) = self.tab_manager.focused_pane_mut() {
                            pane.terminal.grid_mut().scroll_reset();
                            pane.write_to_pty(text.as_bytes());
                            self.dirty = true;
                        }
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }
}

/// Apply OS-level window opacity. Supports Windows (SetLayeredWindowAttributes)
/// and macOS (NSWindow setAlphaValue). On other platforms this is a no-op.
fn apply_window_opacity(window: &Window, opacity: f32) {
    let _ = opacity; // suppress unused-variable warning on unsupported platforms
    let _ = window;

    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::HasWindowHandle;
        if let Ok(handle) = window.window_handle() {
            if let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_ref() {
                unsafe {
                    use windows_sys::Win32::UI::WindowsAndMessaging::*;
                    use windows_sys::Win32::Foundation::HWND;
                    let hwnd = win32.hwnd.get() as HWND;
                    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
                    SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_LAYERED as i32);
                    SetLayeredWindowAttributes(hwnd, 0, (opacity * 255.0) as u8, LWA_ALPHA);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        use raw_window_handle::HasWindowHandle;
        if let Ok(handle) = window.window_handle() {
            if let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_ref() {
                // Use raw ObjC runtime calls to set NSWindow alphaValue.
                // This avoids pulling in extra macOS crate dependencies.
                unsafe extern "C" {
                    fn objc_msgSend(obj: *mut std::ffi::c_void, sel: *mut std::ffi::c_void, ...) -> *mut std::ffi::c_void;
                    fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
                }
                unsafe {
                    let ns_view = appkit.ns_view.as_ptr() as *mut std::ffi::c_void;
                    let sel_window = sel_registerName(b"window\0".as_ptr() as *const _);
                    let ns_window = objc_msgSend(ns_view, sel_window);
                    if !ns_window.is_null() {
                        let sel_alpha = sel_registerName(b"setAlphaValue:\0".as_ptr() as *const _);
                        let _: *mut std::ffi::c_void = {
                            // objc_msgSend with f64 argument
                            let f: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, f64) -> *mut std::ffi::c_void
                                = std::mem::transmute(objc_msgSend as *const ());
                            f(ns_window, sel_alpha, opacity as f64)
                        };
                    }
                }
            }
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,wgpu_core=warn,wgpu_hal=error")),
        )
        .init();

    info!("NexTerm v{} starting", env!("CARGO_PKG_VERSION"));

    let config_path = nexterm_config::default_config_path();
    let config = nexterm_config::load_config(&config_path)?;
    info!("configuration loaded from {}", config_path.display());

    let event_loop = EventLoop::new()?;
    let mut app = App::new(config, config_path);
    event_loop.run_app(&mut app)?;

    Ok(())
}
