//! XShell-style fully GUI terminal overlay.
//!
//! Layout (top to bottom):
//!   ┌─ Menu Bar ──────────────────────────────────────────────┐
//!   │ File  Edit  View  Session  Tools  Help                  │
//!   ├─ Toolbar ───────────────────────────────────────────────┤
//!   │ [+Local] [+SSH] [Disconnect] │ [Split H] [Split V] │⚙│ │
//!   ├─ Tab Bar ───────────────────────────────────────────────┤
//!   │ ● Terminal 1  ✕ │ ● user@host  ✕ │  +                  │
//!   ├──────────┬──────────────────────────────────────────────┤
//!   │ Sessions │  Terminal content area (wgpu rendered)       │
//!   │  ├ Local │                                              │
//!   │  └ SSH   │                                              │
//!   │   ├ prod │                                              │
//!   │   └ dev  │                                              │
//!   ├──────────┴──────────────────────────────────────────────┤
//!   │ Status: Connected │ UTF-8 │ 120×35 │ bash │ 0.2s       │
//!   └────────────────────────────────────────────────────────-┘

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Terminal display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDisplayMode {
    /// Standard raw terminal.
    Normal,
    /// Log viewer: line numbers, timestamps, alternating row bg, block style.
    Log,
}

#[derive(Debug, Clone)]
pub enum GuiAction {
    SwitchTab(usize),
    CloseTab(usize),
    NewTab,
    NewTabWithShell(String),
    NewWslTab,
    ConnectSsh {
        host: String,
        port: u16,
        username: String,
        auth: SshAuthInput,
    },
    OpenSshDialog,
    ToggleSettings,
    ApplySettings {
        font_family: String,
        font_size: f32,
        theme: String,
        shell: String,
        opacity: f32,
        padding: f32,
        background_image: String,
        cursor_style: usize,
        cursor_blink: bool,
    },
    // XShell-style extras
    SplitHorizontal,
    SplitVertical,
    DisconnectTab(usize),
    CopySelection,
    PasteClipboard,
    SelectAll,
    Find,
    FindNext {
        query: String,
        case_sensitive: bool,
        whole_word: bool,
        use_regex: bool,
    },
    FindPrev {
        query: String,
        case_sensitive: bool,
        whole_word: bool,
        use_regex: bool,
    },
    FindAll {
        query: String,
        case_sensitive: bool,
        whole_word: bool,
        use_regex: bool,
    },
    ClearFind,
    ToggleTerminalMode,
    ToggleFold(usize, u64),
    ToggleFullScreen,
    ToggleSessionPanel,
    FontZoomIn,
    FontZoomOut,
    FontZoomReset,
    ShowAbout,
    ConnectSavedProfile(usize),
    DeleteSavedProfile(usize),
    EditSavedProfile(usize),
    DuplicateTab,
    RenameTab(usize, String),
    ReconnectTab(usize),
    /// Scroll the focused terminal to a specific offset into scrollback.
    ScrollTo(usize),
    /// Save a new SSH profile (name, group, host, port, username, auth_mode, password/key_path).
    /// The app will persist it and refresh the GUI profile list.
    SaveProfile {
        name: String,
        group: String,
        host: String,
        port: u16,
        username: String,
        auth_mode: usize,
        password: String,
        key_path: String,
    },
    /// Navigate the SFTP browser to a given path.
    SftpNavigate(String),
    /// Navigate SFTP browser up one level.
    SftpGoUp,
    /// Navigate SFTP browser to home.
    SftpGoHome,
    /// Create a new directory at the given path.
    SftpMkdir(String),
    /// Create a new empty file at the given path.
    SftpTouch(String),
    /// Download a remote file to local.
    SftpDownload(String),
    /// Upload a local file to the current remote directory.
    SftpUpload,
    /// Clear completed transfers from the visible list.
    SftpClearTransfers,
    /// Execute a command from history (write it to the active PTY + Enter).
    ExecHistoryCommand(String),
    /// Toggle command history panel.
    ToggleHistoryPanel,
    /// Refresh the cached history list from the database.
    RefreshHistory,
    /// Import profiles from ~/.ssh/config.
    ImportSshConfig,
    /// Update an existing saved profile by its UUID string.
    UpdateProfile {
        id: String,
        name: String,
        group: String,
        host: String,
        port: u16,
        username: String,
        auth_mode: usize,
        password: String,
        key_path: String,
    },
    // ─── Agent ───
    /// Toggle the Agent chat panel.
    ToggleAgentPanel,
    /// Send a message to the Agent.
    AgentSendMessage(String),
    /// Cancel the current agent run.
    AgentCancel,
    /// Reset agent conversation.
    AgentReset,
    /// Configure agent (provider_type, base_url, api_key, model_id).
    AgentConfigure {
        provider_type: String,
        base_url: String,
        api_key: String,
        model_id: String,
    },
}

#[derive(Debug, Clone)]
pub enum SshAuthInput {
    Password(String),
    KeyFile(String),
}

// ---------------------------------------------------------------------------
// Command history item (GUI-side)
// ---------------------------------------------------------------------------

/// A command history entry for GUI display.
#[derive(Debug, Clone)]
pub struct HistoryItem {
    pub command: String,
    pub exit_code: i32,
    pub timestamp: i64,
    pub host: Option<String>,
    pub cwd: Option<String>,
}

// ---------------------------------------------------------------------------
// Session profile (GUI-side)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SshProfileEntry {
    pub id: String,
    pub name: String,
    pub group: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_display: String,
    pub color_tag: Option<[u8; 3]>,
}

// ---------------------------------------------------------------------------
// SFTP browser snapshot (passed in from app)
// ---------------------------------------------------------------------------

/// One remote file/dir entry for GUI display.
#[derive(Debug, Clone)]
pub struct SftpEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub file_type: String,
    pub permissions: String,
    pub modified: String,
    pub owner: String,
    pub group: String,
}

/// In-flight or recently-completed file transfer info for UI display.
#[derive(Debug, Clone)]
pub struct SftpTransferInfo {
    /// "Upload" or "Download".
    pub direction: &'static str,
    pub remote_path: String,
    pub local_path: String,
    pub bytes: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
}

/// Snapshot of the SFTP browser state for the active SSH pane.
#[derive(Debug, Clone, Default)]
pub struct SftpBrowserSnapshot {
    pub current_path: String,
    pub entries: Vec<SftpEntry>,
    pub initialized: bool,
    pub error: Option<String>,
    pub transfers: Vec<SftpTransferInfo>,
}

// ---------------------------------------------------------------------------
// Tab info (passed in from app)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TabInfo {
    pub title: String,
    pub is_active: bool,
    pub is_ssh: bool,
    pub is_connected: bool,
    pub cols: u16,
    pub rows: u16,
    /// Server status snapshot (SSH only).
    pub server_status: Option<ServerStatusSnapshot>,
}

/// Per-disk usage snapshot for GUI display.
#[derive(Debug, Clone, Default)]
pub struct DiskSnapshot {
    pub mount: String,
    pub fstype: String,
    pub total: String,
    pub used: String,
    pub avail: String,
    pub use_pct: String,
}

/// One network rate sample (bytes/sec).
#[derive(Debug, Clone, Default)]
pub struct NetRateSnapshot {
    pub rx_bps: f64,
    pub tx_bps: f64,
}

/// Per-interface network info with rate history.
#[derive(Debug, Clone, Default)]
pub struct NetIfInfo {
    pub name: String,
    pub history: Vec<NetRateSnapshot>,
}

/// Detected shell info for the "New Tab" shell selector.
#[derive(Debug, Clone)]
pub struct ShellInfo {
    pub name: String,
    pub path: String,
}

/// Lightweight snapshot of SSH server status for GUI display.
#[derive(Debug, Clone, Default)]
pub struct ServerStatusSnapshot {
    pub os: String,
    pub kernel: String,
    pub hostname: String,
    pub uptime: String,
    pub load_avg: String,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub cpu_pct: f32,
    pub disks: Vec<DiskSnapshot>,
    /// Aggregate network history (all interfaces combined).
    pub net_history: Vec<NetRateSnapshot>,
    /// Per-interface info with individual history.
    pub net_interfaces: Vec<NetIfInfo>,
    pub disk_usage: String,
    pub latency_ms: u32,
    pub connection_duration: String,
    /// Top processes by CPU usage.
    pub top_procs: Vec<ProcessSnapshot>,
}

/// A single process entry for display.
#[derive(Debug, Clone, Default)]
pub struct ProcessSnapshot {
    pub name: String,
    pub cpu_pct: f32,
    pub mem_str: String,
    /// Raw RSS in KB for sorting.
    pub mem_kb: u64,
}

// ---------------------------------------------------------------------------
// Agent chat messages (GUI-side)
// ---------------------------------------------------------------------------

/// Role in the agent conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentChatRole {
    User,
    Assistant,
    ToolCall,
    Error,
}

/// A single message in the Agent chat panel.
#[derive(Debug, Clone)]
pub struct AgentChatMessage {
    pub role: AgentChatRole,
    pub content: String,
}

// ---------------------------------------------------------------------------
// GUI state
// ---------------------------------------------------------------------------

pub struct GuiState {
    // Panel visibility
    pub show_settings: bool,
    pub show_ssh_dialog: bool,
    pub show_session_panel: bool,
    pub show_about: bool,
    pub show_find_bar: bool,
    pub show_system_panel: bool,
    pub show_sftp_panel: bool,
    /// Ratio of SFTP left tree panel width (0.0..1.0).
    pub sftp_tree_ratio: f32,
    /// Editable SFTP path input.
    pub sftp_path_input: String,
    /// SFTP new item name input (for mkdir/touch dialogs).
    pub sftp_new_name: String,
    /// Selected network interface index for the system panel (None = aggregate all).
    pub selected_net_if: Option<usize>,
    /// Process sort column: 0 = CPU (default), 1 = Memory.
    pub proc_sort_col: u8,
    /// Process sort ascending (false = descending, default).
    pub proc_sort_asc: bool,
    /// UUID of the profile currently being edited (None = creating new).
    pub ssh_editing_profile_id: Option<String>,
    /// Detected available shells on the system.
    pub available_shells: Vec<ShellInfo>,

    // Terminal display mode
    pub terminal_mode: TerminalDisplayMode,
    /// Block IDs that are currently folded (collapsed), keyed by pane ID.
    pub folded_blocks: std::collections::HashMap<usize, std::collections::HashSet<u64>>,

    // Settings fields
    pub settings_font_family: String,
    pub settings_font_size: f32,
    pub settings_theme: String,
    pub settings_shell: String,
    pub settings_opacity: f32,
    pub settings_padding: f32,
    pub settings_background_image: String,
    pub settings_cursor_style: usize,
    pub settings_cursor_blink: bool,
    pub settings_scrollback: u32,

    // SSH dialog fields
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_username: String,
    pub ssh_password: String,
    pub ssh_key_path: String,
    pub ssh_auth_mode: usize,
    pub ssh_name: String,
    pub ssh_group: String,

    // Session manager
    pub ssh_profiles: Vec<SshProfileEntry>,
    pub session_filter: String,
    pub selected_profile: Option<usize>,

    // Find bar
    pub find_query: String,
    pub find_case_sensitive: bool,
    pub find_whole_word: bool,
    pub find_use_regex: bool,
    pub find_match_count: usize,
    pub find_current_index: usize,
    /// Track last query state to detect changes and auto-search.
    find_last_query: String,

    // Context menu
    pub context_menu_pos: Option<egui::Pos2>,

    // Settings tab
    pub settings_tab: usize,

    // Scrollbar drag state
    pub scrollbar_dragging: bool,
    /// Offset from thumb top to mouse Y at time of click.
    pub scrollbar_drag_offset: f32,

    // Animated style applied flag (to avoid re-applying every frame after first)
    style_applied: bool,

    // Background image
    /// The loaded background image texture handle.
    pub bg_texture: Option<egui::TextureHandle>,
    /// Path that was last successfully loaded (to detect changes).
    bg_loaded_path: String,

    // Command history panel
    pub show_history_panel: bool,
    pub history_search: String,
    pub history_entries: Vec<HistoryItem>,

    // Agent panel
    pub show_agent_panel: bool,
    pub agent_input: String,
    pub agent_messages: Vec<AgentChatMessage>,
    pub agent_is_running: bool,
    /// Accumulated streaming text for the current assistant response.
    pub agent_streaming_text: String,
    /// Agent configuration fields (shown in settings).
    pub agent_provider_type: String,
    pub agent_base_url: String,
    pub agent_api_key: String,
    pub agent_model_id: String,
}

impl GuiState {
    pub fn new(font_family: &str, font_size: f32, theme: &str, shell: &str) -> Self {
        Self {
            show_settings: false,
            show_ssh_dialog: false,
            show_session_panel: true,
            show_about: false,
            show_find_bar: false,
            show_system_panel: false,
            show_sftp_panel: false,
            sftp_tree_ratio: 0.20,
            sftp_path_input: String::new(),
            sftp_new_name: String::new(),
            selected_net_if: None,
            proc_sort_col: 0,
            proc_sort_asc: false,
            ssh_editing_profile_id: None,
            available_shells: Vec::new(),

            terminal_mode: TerminalDisplayMode::Normal,
            folded_blocks: std::collections::HashMap::new(),

            settings_font_family: font_family.to_string(),
            settings_font_size: font_size,
            settings_theme: theme.to_string(),
            settings_shell: shell.to_string(),
            settings_opacity: 0.95,
            settings_padding: 4.0,
            settings_background_image: String::new(),
            settings_cursor_style: 1,
            settings_cursor_blink: true,
            settings_scrollback: 100_000,
            scrollbar_dragging: false,
            scrollbar_drag_offset: 0.0,

            ssh_host: String::new(),
            ssh_port: "22".to_string(),
            ssh_username: String::new(),
            ssh_password: String::new(),
            ssh_key_path: String::new(),
            ssh_auth_mode: 0,
            ssh_name: String::new(),
            ssh_group: "Default".to_string(),

            ssh_profiles: Vec::new(),
            session_filter: String::new(),
            selected_profile: None,

            find_query: String::new(),
            find_case_sensitive: false,
            find_whole_word: false,
            find_use_regex: false,
            find_match_count: 0,
            find_current_index: 0,
            find_last_query: String::new(),

            context_menu_pos: None,

            settings_tab: 0,
            style_applied: false,

            bg_texture: None,
            bg_loaded_path: String::new(),

            show_history_panel: false,
            history_search: String::new(),
            history_entries: Vec::new(),

            show_agent_panel: false,
            agent_input: String::new(),
            agent_messages: Vec::new(),
            agent_is_running: false,
            agent_streaming_text: String::new(),
            agent_provider_type: "openai".to_string(),
            agent_base_url: "https://api.deepseek.com/v1".to_string(),
            agent_api_key: String::new(),
            agent_model_id: "deepseek-chat".to_string(),
        }
    }

    /// Load or reload the background image if the path changed.
    pub fn sync_background_image(&mut self, ctx: &egui::Context) {
        let path = self.settings_background_image.trim().to_string();
        if path == self.bg_loaded_path {
            return; // no change
        }
        // Clear previous texture
        self.bg_texture = None;
        self.bg_loaded_path.clear();

        if path.is_empty() {
            return;
        }

        match image::open(&path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let pixels = rgba.into_raw();
                let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                let handle = ctx.load_texture(
                    "bg_image",
                    color_image,
                    egui::TextureOptions::LINEAR,
                );
                self.bg_texture = Some(handle);
                self.bg_loaded_path = path;
                tracing::info!("loaded background image: {}", self.bg_loaded_path);
            }
            Err(e) => {
                tracing::warn!("failed to load background image '{}': {}", path, e);
                self.bg_loaded_path = path; // mark as attempted to avoid retry every frame
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Height constants (pixels) — used by app to offset terminal content
// ---------------------------------------------------------------------------

/// Total height of the top chrome (menu + toolbar + tab bar).
pub const TOP_CHROME_HEIGHT: f32 = 90.0;
/// Height of the bottom status bar.
pub const STATUS_BAR_HEIGHT: f32 = 26.0;
/// Default session panel width.
pub const SESSION_PANEL_WIDTH: f32 = 220.0;

// ---------------------------------------------------------------------------
// Main draw entry point
// ---------------------------------------------------------------------------

/// Draw the full GUI overlay.
/// Returns (actions, central_rect) where central_rect is the remaining area
/// for terminal content after all egui panels have claimed their space.
pub fn draw_gui(
    ctx: &egui::Context,
    state: &mut GuiState,
    tabs: &[TabInfo],
    active_tab: usize,
    sftp_snapshot: Option<&SftpBrowserSnapshot>,
) -> (Vec<GuiAction>, egui::Rect) {
    let mut actions = Vec::new();

    if !state.style_applied {
        apply_theme(ctx);
        state.style_applied = true;
    }

    draw_menu_bar(ctx, state, &mut actions);
    draw_toolbar(ctx, state, &mut actions);
    draw_tab_bar(ctx, state, tabs, active_tab, &mut actions);
    draw_status_bar(ctx, state, tabs, active_tab);

    if state.show_session_panel {
        draw_session_panel(ctx, state, &mut actions, tabs);
    }

    if state.show_settings {
        draw_settings_window(ctx, state, &mut actions);
    }
    if state.show_ssh_dialog {
        draw_ssh_dialog(ctx, state, &mut actions);
    }
    if state.show_about {
        draw_about_window(ctx, state);
    }
    if state.show_find_bar {
        draw_find_bar(ctx, state, &mut actions);
    }

    // History panel (right sidebar) — draw before system panel
    if state.show_history_panel {
        draw_history_panel(ctx, state, &mut actions);
    }

    // Agent panel (right sidebar)
    if state.show_agent_panel {
        draw_agent_panel(ctx, state, &mut actions);
    }

    // System status panel (right side, SSH only) — draw BEFORE SFTP so it claims
    // its width first and the SFTP bottom panel doesn't overlap it.
    if state.show_system_panel {
        let active_ss = tabs.get(active_tab).and_then(|t| t.server_status.as_ref()).cloned();
        if let Some(ss) = active_ss {
            draw_system_panel(ctx, state, &ss);
        }
    }

    // SFTP file browser panel (bottom, SSH only)
    if state.show_sftp_panel {
        if let Some(snap) = sftp_snapshot {
            draw_sftp_panel(ctx, state, snap, &mut actions);
        }
    }

    // Sync background image texture if path changed
    state.sync_background_image(ctx);

    // After all panels are drawn, the remaining area is for the terminal
    let central_rect = ctx.available_rect();

    // Paint background image behind terminal content (if loaded)
    if let Some(tex) = &state.bg_texture {
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Background,
            egui::Id::new("bg_image_layer"),
        ));
        // Cover the full window (absolute positioning), preserving aspect ratio
        let full_rect = ctx.screen_rect();
        let img_aspect = tex.size()[0] as f32 / tex.size()[1] as f32;
        let rect_aspect = full_rect.width() / full_rect.height();
        let (uv_min, uv_max) = if img_aspect > rect_aspect {
            // Image wider than rect → crop left/right
            let visible = rect_aspect / img_aspect;
            let offset = (1.0 - visible) / 2.0;
            (egui::pos2(offset, 0.0), egui::pos2(1.0 - offset, 1.0))
        } else {
            // Image taller than rect → crop top/bottom
            let visible = img_aspect / rect_aspect;
            let offset = (1.0 - visible) / 2.0;
            (egui::pos2(0.0, offset), egui::pos2(1.0, 1.0 - offset))
        };
        // Blend image with opacity so the terminal content shows through.
        let alpha = ((1.0 - state.settings_opacity).clamp(0.0, 1.0) * 255.0) as u8;
        let tint = egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha);
        let mut mesh = egui::Mesh::with_texture(tex.id());
        mesh.add_rect_with_uv(
            full_rect,
            egui::Rect::from_min_max(uv_min, uv_max),
            tint,
        );
        painter.add(egui::Shape::mesh(mesh));
    }

    (actions, central_rect)
}

/// Information needed to draw the terminal scrollbar.
pub struct ScrollInfo {
    /// Number of scrollback lines available.
    pub scrollback_len: usize,
    /// Number of visible rows.
    pub visible_rows: usize,
    /// Current scroll offset (0 = bottom/live).
    pub scroll_offset: usize,
}

/// Draw a scrollbar overlay on the right side of the terminal area.
/// Returns an optional ScrollTo action if the user clicked/dragged the scrollbar.
/// Sets `state.scrollbar_dragging` so the main loop can block terminal selection.
pub fn draw_scrollbar(
    ctx: &egui::Context,
    state: &mut GuiState,
    terminal_rect: egui::Rect,
    info: &ScrollInfo,
) -> Option<GuiAction> {
    let total_lines = info.scrollback_len + info.visible_rows;
    if total_lines == 0 || info.scrollback_len == 0 {
        state.scrollbar_dragging = false;
        return None;
    }

    let bar_width = 10.0f32;
    // Track spans the full height of the terminal content area
    let track_rect = egui::Rect::from_min_max(
        egui::pos2(terminal_rect.max.x - bar_width, terminal_rect.min.y),
        terminal_rect.max,
    );
    let track_h = track_rect.height();
    if track_h <= 0.0 {
        return None;
    }

    // Thumb size proportional to visible fraction of total content
    let visible_frac = (info.visible_rows as f32) / (total_lines as f32);
    let thumb_h = (visible_frac * track_h).clamp(20.0, track_h);
    let scrollable_track = track_h - thumb_h; // pixels the thumb can travel

    // Thumb position: offset 0 = bottom (live), scrollback_len = top
    let scroll_frac = if info.scrollback_len > 0 {
        1.0 - (info.scroll_offset as f32 / info.scrollback_len as f32)
    } else {
        1.0
    };
    let thumb_top = track_rect.min.y + scroll_frac * scrollable_track;
    let thumb_rect = egui::Rect::from_min_size(
        egui::pos2(track_rect.min.x, thumb_top),
        egui::vec2(bar_width, thumb_h),
    );

    let mut action = None;

    let pointer_pos = ctx.input(|i| i.pointer.hover_pos());
    let hover_rect = track_rect.expand2(egui::vec2(4.0, 0.0));
    let hovered = pointer_pos.map_or(false, |p| hover_rect.contains(p));
    let pressing = ctx.input(|i| i.pointer.primary_down());
    let just_pressed = ctx.input(|i| i.pointer.primary_pressed());

    // Start dragging only on a fresh click within the scrollbar area
    if just_pressed && hovered {
        if let Some(p) = pointer_pos {
            // If clicked on thumb, remember offset for smooth dragging.
            // If clicked on empty track, center thumb on click position.
            if thumb_rect.contains(p) {
                state.scrollbar_drag_offset = p.y - thumb_rect.min.y;
            } else {
                state.scrollbar_drag_offset = thumb_h / 2.0;
            }
            state.scrollbar_dragging = true;
        }
    }
    if !pressing {
        state.scrollbar_dragging = false;
    }

    let rounding = egui::CornerRadius::same(4);
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("terminal_scrollbar"),
    ));

    // Track background
    let track_alpha = if hovered || state.scrollbar_dragging { 60u8 } else { 30 };
    painter.rect_filled(
        track_rect,
        rounding,
        egui::Color32::from_rgba_unmultiplied(80, 80, 80, track_alpha),
    );

    // Thumb
    let thumb_alpha = if hovered || state.scrollbar_dragging || info.scroll_offset > 0 { 200u8 } else { 80 };
    painter.rect_filled(
        thumb_rect,
        rounding,
        egui::Color32::from_rgba_unmultiplied(180, 180, 180, thumb_alpha),
    );

    // Handle dragging: thumb_top follows pointer.y - drag_offset
    if state.scrollbar_dragging && scrollable_track > 0.0 {
        if let Some(p) = pointer_pos {
            let new_thumb_top = p.y - state.scrollbar_drag_offset;
            // Position within scrollable track [0, scrollable_track]
            let pos_in_track = (new_thumb_top - track_rect.min.y).clamp(0.0, scrollable_track);
            let scroll_frac = pos_in_track / scrollable_track;
            // scroll_frac 0 = top (max offset = scrollback_len), 1 = bottom (offset = 0)
            let new_offset = ((1.0 - scroll_frac) * info.scrollback_len as f32).round() as usize;
            action = Some(GuiAction::ScrollTo(new_offset.min(info.scrollback_len)));
        }
    }

    action
}

// ---------------------------------------------------------------------------
// Menu bar — File / Edit / View / Session / Tools / Help
// ---------------------------------------------------------------------------

fn draw_menu_bar(ctx: &egui::Context, state: &mut GuiState, actions: &mut Vec<GuiAction>) {
    egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
        egui::menu::bar(ui, |ui| {
            // ---- 文件 ----
            ui.menu_button("文件", |ui| {
                if ui.button("  新建本地终端      Ctrl+Shift+T").clicked() {
                    actions.push(GuiAction::NewTab);
                    ui.close_menu();
                }
                if ui.button("  新建 WSL 终端").clicked() {
                    actions.push(GuiAction::NewWslTab);
                    ui.close_menu();
                }
                if ui.button("  新建 SSH 连接...").clicked() {
                    state.show_ssh_dialog = true;
                    ui.close_menu();
                }
                if ui.button("  复制标签页").clicked() {
                    actions.push(GuiAction::DuplicateTab);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("  断开连接").clicked() {
                    actions.push(GuiAction::DisconnectTab(0));
                    ui.close_menu();
                }
                if ui.button("  重新连接").clicked() {
                    actions.push(GuiAction::ReconnectTab(0));
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("  退出                  Alt+F4").clicked() {
                    std::process::exit(0);
                }
            });

            // ---- 编辑 ----
            ui.menu_button("编辑", |ui| {
                if ui.button("  复制              Ctrl+Shift+C").clicked() {
                    actions.push(GuiAction::CopySelection);
                    ui.close_menu();
                }
                if ui.button("  粘贴              Ctrl+Shift+V").clicked() {
                    actions.push(GuiAction::PasteClipboard);
                    ui.close_menu();
                }
                if ui.button("  全选              Ctrl+Shift+A").clicked() {
                    actions.push(GuiAction::SelectAll);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("  查找...           Ctrl+F").clicked() {
                    state.show_find_bar = !state.show_find_bar;
                    ui.close_menu();
                }
            });

            // ---- 视图 ----
            ui.menu_button("视图", |ui| {
                if ui.button("  放大              Ctrl+=").clicked() {
                    actions.push(GuiAction::FontZoomIn);
                    ui.close_menu();
                }
                if ui.button("  缩小              Ctrl+-").clicked() {
                    actions.push(GuiAction::FontZoomOut);
                    ui.close_menu();
                }
                if ui.button("  重置缩放          Ctrl+0").clicked() {
                    actions.push(GuiAction::FontZoomReset);
                    ui.close_menu();
                }
                ui.separator();
                let panel_label = if state.show_session_panel {
                    "  隐藏会话面板"
                } else {
                    "  显示会话面板"
                };
                if ui.button(panel_label).clicked() {
                    state.show_session_panel = !state.show_session_panel;
                    actions.push(GuiAction::ToggleSessionPanel);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("  全屏              F11").clicked() {
                    actions.push(GuiAction::ToggleFullScreen);
                    ui.close_menu();
                }
            });

            // ---- 会话 ----
            ui.menu_button("会话", |ui| {
                if ui.button("  会话管理器...").clicked() {
                    state.show_session_panel = true;
                    actions.push(GuiAction::ToggleSessionPanel);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("  水平分屏          Ctrl+Shift+D").clicked() {
                    actions.push(GuiAction::SplitHorizontal);
                    ui.close_menu();
                }
                if ui.button("  垂直分屏          Ctrl+Shift+E").clicked() {
                    actions.push(GuiAction::SplitVertical);
                    ui.close_menu();
                }
            });

            // ---- 工具 ----
            ui.menu_button("工具", |ui| {
                if ui.button("  设置...           Ctrl+,").clicked() {
                    state.show_settings = true;
                    ui.close_menu();
                }
            });

            // ---- 帮助 ----
            ui.menu_button("帮助", |ui| {
                if ui.button("  关于 NexTerm").clicked() {
                    state.show_about = true;
                    ui.close_menu();
                }
            });
        });
    });
}

// ---------------------------------------------------------------------------
// Toolbar — icon buttons row
// ---------------------------------------------------------------------------

fn toolbar_icon_btn(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> bool {
    let text = egui::RichText::new(icon).size(14.0);
    let btn = egui::Button::new(text)
        .corner_radius(egui::CornerRadius::same(3))
        .min_size(egui::vec2(24.0, 22.0));
    ui.add(btn).on_hover_text(tooltip).clicked()
}

fn draw_toolbar(ctx: &egui::Context, state: &mut GuiState, actions: &mut Vec<GuiAction>) {
    egui::TopBottomPanel::top("toolbar")
        .exact_height(34.0)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(5.0, 2.0);
                ui.spacing_mut().item_spacing.x = 2.0;

                // ---- New tab (with dropdown for detected shells) ----
                ui.menu_button(
                    egui::RichText::new("+ 新建").size(12.0),
                    |ui| {
                        if state.available_shells.is_empty() {
                            if ui.button("  本地终端   Ctrl+Shift+T").clicked() {
                                actions.push(GuiAction::NewTab);
                                ui.close_menu();
                            }
                        } else {
                            for sh in &state.available_shells {
                                let label = format!("  {}  ({})", sh.name, sh.path);
                                if ui.button(&label).clicked() {
                                    actions.push(GuiAction::NewTabWithShell(sh.path.clone()));
                                    ui.close_menu();
                                }
                            }
                        }
                        ui.separator();
                        if ui.button("  SSH 连接...").clicked() {
                            state.show_ssh_dialog = true;
                            ui.close_menu();
                        }
                    },
                );

                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                // ---- Split panes ----
                if toolbar_icon_btn(ui, "|", "水平分屏  Ctrl+Shift+D") {
                    actions.push(GuiAction::SplitHorizontal);
                }
                if toolbar_icon_btn(ui, "-", "垂直分屏  Ctrl+Shift+E") {
                    actions.push(GuiAction::SplitVertical);
                }

                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                // ---- Terminal mode toggle ----
                let mode_label = match state.terminal_mode {
                    TerminalDisplayMode::Normal => "TTY",
                    TerminalDisplayMode::Log => "LOG",
                };
                let mode_tip = match state.terminal_mode {
                    TerminalDisplayMode::Normal => "切换到日志视图（时间戳、行号）",
                    TerminalDisplayMode::Log => "切换到普通终端",
                };
                if toolbar_icon_btn(ui, mode_label, mode_tip) {
                    state.terminal_mode = match state.terminal_mode {
                        TerminalDisplayMode::Normal => TerminalDisplayMode::Log,
                        TerminalDisplayMode::Log => TerminalDisplayMode::Normal,
                    };
                    actions.push(GuiAction::ToggleTerminalMode);
                }

                // ---- Right side ----
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if toolbar_icon_btn(ui, "*", "设置  Ctrl+,") {
                        state.show_settings = !state.show_settings;
                    }
                    let agent_label = if state.show_agent_panel { "AI*" } else { "AI" };
                    if toolbar_icon_btn(ui, agent_label, "AI Agent 面板") {
                        actions.push(GuiAction::ToggleAgentPanel);
                    }
                    let hist_label = if state.show_history_panel { "HIST*" } else { "HIST" };
                    if toolbar_icon_btn(ui, hist_label, "命令历史  Ctrl+R") {
                        actions.push(GuiAction::ToggleHistoryPanel);
                    }
                    let sftp_label = if state.show_sftp_panel { "SFTP*" } else { "SFTP" };
                    if toolbar_icon_btn(ui, sftp_label, "切换 SFTP 文件浏览器") {
                        state.show_sftp_panel = !state.show_sftp_panel;
                    }
                    let sys_label = if state.show_system_panel { "SYS*" } else { "SYS" };
                    if toolbar_icon_btn(ui, sys_label, "切换系统状态面板") {
                        state.show_system_panel = !state.show_system_panel;
                    }
                    let panel_label = if state.show_session_panel { "<<" } else { ">>" };
                    if toolbar_icon_btn(ui, panel_label, "切换会话面板  Ctrl+Shift+B") {
                        state.show_session_panel = !state.show_session_panel;
                        actions.push(GuiAction::ToggleSessionPanel);
                    }
                });
            });
        });
}

// ---------------------------------------------------------------------------
// Tab bar — Chrome-style tabs with colored dots
// ---------------------------------------------------------------------------

fn draw_tab_bar(
    ctx: &egui::Context,
    _state: &mut GuiState,
    tabs: &[TabInfo],
    active_tab: usize,
    actions: &mut Vec<GuiAction>,
) {
    egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;

            for (i, tab) in tabs.iter().enumerate() {
                let is_active = i == active_tab;

                // Tab frame
                let bg = if is_active {
                    egui::Color32::from_rgb(255, 255, 255)
                } else {
                    egui::Color32::from_rgb(235, 235, 235)
                };

                let frame = egui::Frame::new()
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(10, 4))
                    .corner_radius(egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 });

                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // Status dot
                        let dot_color = if tab.is_ssh {
                            if tab.is_connected {
                                egui::Color32::from_rgb(22, 160, 70)   // green
                            } else {
                                egui::Color32::from_rgb(210, 50, 45)   // red
                            }
                        } else {
                            egui::Color32::from_rgb(0, 120, 212) // blue (local)
                        };
                        let (dot_rect, _) = ui.allocate_exact_size(
                            egui::vec2(8.0, 8.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().circle_filled(dot_rect.center(), 3.5, dot_color);

                        ui.add_space(4.0);

                        // Title
                        let tab_fg = egui::Color32::from_rgb(30, 30, 30);
                        let text = if is_active {
                            egui::RichText::new(&tab.title).strong().size(12.0).color(tab_fg)
                        } else {
                            egui::RichText::new(&tab.title).size(12.0).color(tab_fg)
                        };

                        let tab_resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                        if tab_resp.clicked() && !is_active {
                            actions.push(GuiAction::SwitchTab(i));
                        }

                        ui.add_space(6.0);

                        // Close button
                        let close_text = egui::RichText::new("x").size(14.0);
                        if ui.add(egui::Button::new(close_text).frame(false)).clicked() {
                            actions.push(GuiAction::CloseTab(i));
                        }
                    });
                });

                // Small gap between tabs
                ui.add_space(1.0);
            }

            ui.add_space(4.0);

            // New tab button
            let plus = egui::RichText::new("+").size(16.0).strong();
            if ui.add(egui::Button::new(plus).frame(false)).on_hover_text("新建标签页").clicked() {
                actions.push(GuiAction::NewTab);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Status bar — connection info, encoding, terminal size
// ---------------------------------------------------------------------------

fn draw_status_bar(
    ctx: &egui::Context,
    _state: &mut GuiState,
    tabs: &[TabInfo],
    active_tab: usize,
) {
    egui::TopBottomPanel::bottom("status_bar")
        .exact_height(STATUS_BAR_HEIGHT)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;

                if let Some(tab) = tabs.get(active_tab) {
                    // Connection status dot + text
                    let (status_text, status_color) = if tab.is_ssh {
                        if tab.is_connected {
                            ("SSH Connected", egui::Color32::from_rgb(152, 151, 26))
                        } else {
                            ("Disconnected", egui::Color32::from_rgb(204, 36, 29))
                        }
                    } else {
                        ("Local", egui::Color32::from_rgb(69, 133, 136))
                    };
                    ui.colored_label(status_color, status_text);

                    ui.separator();
                    ui.label("UTF-8");
                    ui.separator();
                    ui.label(format!("{}x{}", tab.cols, tab.rows));

                    // SSH server status
                    if let Some(ss) = &tab.server_status {
                        if !ss.os.is_empty() {
                            ui.separator();
                            let os_icon = match ss.os.as_str() {
                                "Linux" => "[L]",
                                "Darwin" => "[M]",
                                _ => "[?]",
                            };
                            let os_label = format!("{} {}", os_icon, ss.os);
                            let os_resp = ui.label(
                                egui::RichText::new(&os_label)
                                    .color(egui::Color32::from_rgb(184, 187, 38)),
                            );
                            os_resp.on_hover_text(format!(
                                "OS: {}\nKernel: {}\nHostname: {}",
                                ss.os, ss.kernel, ss.hostname
                            ));
                        }
                        if !ss.hostname.is_empty() {
                            ui.separator();
                            ui.label(
                                egui::RichText::new(&ss.hostname)
                                    .color(egui::Color32::from_rgb(250, 189, 47)),
                            );
                        }
                        if !ss.uptime.is_empty() {
                            ui.separator();
                            let up_resp = ui.label(
                                egui::RichText::new(format!("Up: {}", ss.uptime))
                                    .small()
                                    .color(egui::Color32::from_rgb(146, 131, 116)),
                            );
                            up_resp.on_hover_text(format!("Uptime: {}", ss.uptime));
                        }
                        if !ss.load_avg.is_empty() {
                            ui.separator();
                            let load_resp = ui.label(
                                egui::RichText::new(format!("Load: {}", ss.load_avg))
                                    .small()
                                    .color(egui::Color32::from_rgb(146, 131, 116)),
                            );
                            load_resp.on_hover_text(format!("Load Average (1/5/15): {}", ss.load_avg));
                        }
                        if ss.mem_total_mb > 0 {
                            ui.separator();
                            let pct = (ss.mem_used_mb as f64 / ss.mem_total_mb as f64 * 100.0) as u32;
                            let mem_color = if pct > 90 {
                                egui::Color32::from_rgb(204, 36, 29)  // red
                            } else if pct > 70 {
                                egui::Color32::from_rgb(250, 189, 47) // yellow
                            } else {
                                egui::Color32::from_rgb(152, 151, 26) // green
                            };
                            let mem_resp = ui.label(
                                egui::RichText::new(format!("Mem: {}%", pct))
                                    .small()
                                    .color(mem_color),
                            );
                            mem_resp.on_hover_text(format!(
                                "Memory: {} / {} MB ({}%)",
                                ss.mem_used_mb, ss.mem_total_mb, pct
                            ));
                        }
                        if !ss.disk_usage.is_empty() {
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!("Disk: {}", ss.disk_usage))
                                    .small()
                                    .color(egui::Color32::from_rgb(146, 131, 116)),
                            );
                        }
                        if ss.latency_ms > 0 {
                            ui.separator();
                            let lat_color = if ss.latency_ms > 500 {
                                egui::Color32::from_rgb(204, 36, 29)
                            } else if ss.latency_ms > 200 {
                                egui::Color32::from_rgb(250, 189, 47)
                            } else {
                                egui::Color32::from_rgb(152, 151, 26)
                            };
                            ui.label(
                                egui::RichText::new(format!("{}ms", ss.latency_ms))
                                    .small()
                                    .color(lat_color),
                            );
                        }
                        if !ss.connection_duration.is_empty() {
                            ui.separator();
                            ui.label(
                                egui::RichText::new(format!("Dur: {}", ss.connection_duration))
                                    .small()
                                    .color(egui::Color32::from_rgb(146, 131, 116)),
                            );
                        }
                    }
                } else {
                    ui.label("No active session");
                }

                // Right side
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Current local time (platform-native)
                    let now = {
                        let zdt = jiff::Zoned::now();
                        format!(
                            "{:02}:{:02}:{:02}",
                            zdt.hour(),
                            zdt.minute(),
                            zdt.second(),
                        )
                    };
                    ui.label(
                        egui::RichText::new(&now)
                            .small()
                            .monospace()
                            .color(egui::Color32::from_rgb(250, 189, 47)),
                    );
                    ui.separator();
                    ui.label(
                        egui::RichText::new("NexTerm v0.1.0")
                            .small()
                            .color(egui::Color32::from_rgb(146, 131, 116)),
                    );
                    // Request repaint every second so the clock ticks
                    ctx.request_repaint_after(std::time::Duration::from_secs(1));
                });
            });
        });
}

// ---------------------------------------------------------------------------
// Session panel (left sidebar) — tree of saved sessions
// ---------------------------------------------------------------------------

/// Assign a deterministic color to a group name for the left-bar indicator.
fn group_color(name: &str) -> egui::Color32 {
    const PALETTE: &[[u8; 3]] = &[
        [69, 133, 136],   // aqua
        [177, 98, 134],   // purple
        [214, 93, 14],    // orange
        [104, 157, 106],  // green
        [211, 134, 155],  // pink
        [152, 151, 26],   // yellow-green
        [204, 36, 29],    // red
        [69, 133, 136],   // teal
    ];
    let h: u32 = name.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let c = PALETTE[(h as usize) % PALETTE.len()];
    egui::Color32::from_rgb(c[0], c[1], c[2])
}

fn draw_session_panel(
    ctx: &egui::Context,
    state: &mut GuiState,
    actions: &mut Vec<GuiAction>,
    tabs: &[TabInfo],
) {
    let accent = egui::Color32::from_rgb(250, 189, 47);
    let gray = egui::Color32::from_rgb(146, 131, 116);
    let blue = egui::Color32::from_rgb(69, 133, 136);
    let dot_green = egui::Color32::from_rgb(142, 192, 124);
    let dot_red = egui::Color32::from_rgb(204, 36, 29);

    egui::SidePanel::left("session_panel")
        .default_width(SESSION_PANEL_WIDTH)
        .min_width(160.0)
        .max_width(400.0)
        .resizable(true)
        .show(ctx, |ui| {
            // ══════════════════════════════════════════════
            //  Header
            // ══════════════════════════════════════════════
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Sessions").size(14.0).strong().color(accent));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let add_btn = egui::Button::new(
                        egui::RichText::new("+").size(16.0).strong(),
                    )
                    .min_size(egui::vec2(22.0, 22.0));
                    if ui
                        .add(add_btn)
                        .on_hover_text("New SSH session")
                        .clicked()
                    {
                        state.ssh_editing_profile_id = None;
                        state.ssh_name.clear();
                        state.ssh_host.clear();
                        state.ssh_port = "22".to_string();
                        state.ssh_username.clear();
                        state.ssh_password.clear();
                        state.ssh_key_path.clear();
                        state.ssh_auth_mode = 0;
                        state.ssh_group = "Default".to_string();
                        state.show_ssh_dialog = true;
                    }
                    let import_btn = egui::Button::new(
                        egui::RichText::new("↓").size(14.0),
                    )
                    .min_size(egui::vec2(22.0, 22.0));
                    if ui
                        .add(import_btn)
                        .on_hover_text("Import from ~/.ssh/config")
                        .clicked()
                    {
                        actions.push(GuiAction::ImportSshConfig);
                    }
                });
            });
            ui.add_space(2.0);

            // ══════════════════════════════════════════════
            //  Filter bar
            // ══════════════════════════════════════════════
            ui.add(
                egui::TextEdit::singleline(&mut state.session_filter)
                    .hint_text("搜索过滤...")
                    .desired_width(ui.available_width())
                    .margin(egui::Margin::symmetric(6, 3)),
            );
            let filter_lower = state.session_filter.to_lowercase();
            let has_filter = !filter_lower.is_empty();

            // Count visible profiles for the filter badge
            let visible_count = if has_filter {
                state
                    .ssh_profiles
                    .iter()
                    .filter(|p| {
                        p.name.to_lowercase().contains(&filter_lower)
                            || p.host.to_lowercase().contains(&filter_lower)
                            || p.username.to_lowercase().contains(&filter_lower)
                            || p.group.to_lowercase().contains(&filter_lower)
                    })
                    .count()
            } else {
                state.ssh_profiles.len()
            };
            if has_filter {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{} 项匹配", visible_count))
                            .small()
                            .color(if visible_count > 0 { accent } else { dot_red }),
                    );
                    if ui
                        .small_button("x")
                        .on_hover_text("清除过滤")
                        .clicked()
                    {
                        state.session_filter.clear();
                    }
                });
            }

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);

            // ══════════════════════════════════════════════
            //  Group SSH profiles
            // ══════════════════════════════════════════════
            let mut groups: std::collections::BTreeMap<&str, Vec<(usize, &SshProfileEntry)>> =
                std::collections::BTreeMap::new();

            for (i, profile) in state.ssh_profiles.iter().enumerate() {
                if has_filter
                    && !profile.name.to_lowercase().contains(&filter_lower)
                    && !profile.host.to_lowercase().contains(&filter_lower)
                    && !profile.username.to_lowercase().contains(&filter_lower)
                    && !profile.group.to_lowercase().contains(&filter_lower)
                {
                    continue;
                }
                groups
                    .entry(profile.group.as_str())
                    .or_default()
                    .push((i, profile));
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    // ──────────────────────────────────────
                    //  Section 1 — Active Sessions (Outline)
                    // ──────────────────────────────────────
                    let active_header = egui::CollapsingHeader::new(
                        egui::RichText::new("> 活动会话")
                            .size(12.0)
                            .strong()
                            .color(dot_green),
                    )
                    .default_open(true);

                    active_header.show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        if tabs.is_empty() {
                            ui.label(
                                egui::RichText::new("  No open tabs")
                                    .small()
                                    .color(gray)
                                    .italics(),
                            );
                        }
                        for (i, tab) in tabs.iter().enumerate() {
                            // Filter active sessions too
                            if has_filter
                                && !tab.title.to_lowercase().contains(&filter_lower)
                            {
                                continue;
                            }
                            let dot_color = if tab.is_ssh {
                                if tab.is_connected {
                                    dot_green
                                } else {
                                    dot_red
                                }
                            } else {
                                blue
                            };
                            let type_label = if tab.is_ssh { "SSH" } else { "Shell" };

                            ui.horizontal(|ui| {
                                // Colored status dot
                                let (rect, _) =
                                    ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                                ui.painter()
                                    .circle_filled(rect.center(), 3.5, dot_color);

                                let label_text = format!(
                                    " {}  ({})",
                                    tab.title, type_label,
                                );
                                let resp = ui.selectable_label(
                                    tab.is_active,
                                    egui::RichText::new(label_text).size(11.5),
                                );
                                if resp.clicked() {
                                    actions.push(GuiAction::SwitchTab(i));
                                }
                                resp.on_hover_ui(|ui| {
                                    ui.label(
                                        egui::RichText::new(&tab.title)
                                            .strong()
                                            .color(accent),
                                    );
                                    ui.label(format!(
                                        "{}  |  {}x{}",
                                        type_label, tab.cols, tab.rows
                                    ));
                                });
                            });
                        }
                    });

                    ui.add_space(6.0);

                    // ──────────────────────────────────────
                    //  Section 2 — Local Shells
                    // ──────────────────────────────────────
                    let local_header = egui::CollapsingHeader::new(
                        egui::RichText::new("> 本地终端")
                            .size(12.0)
                            .strong()
                            .color(blue),
                    )
                    .default_open(true);

                    local_header.show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        if ui
                            .selectable_label(false, "   >  新建终端")
                            .clicked()
                        {
                            actions.push(GuiAction::NewTab);
                        }
                        if ui
                            .selectable_label(false, "   >  新建 WSL 终端")
                            .clicked()
                        {
                            actions.push(GuiAction::NewWslTab);
                        }
                    });

                    ui.add_space(6.0);

                    // ──────────────────────────────────────
                    //  Section 3 — SSH Saved Sessions (by group)
                    // ──────────────────────────────────────
                    if groups.is_empty() && state.ssh_profiles.is_empty() {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("暂无保存的会话")
                                    .color(gray)
                                    .italics(),
                            );
                            ui.add_space(4.0);
                            if ui.link("+ 添加 SSH 会话").clicked() {
                                state.show_ssh_dialog = true;
                            }
                        });
                    }

                    for (group_name, profiles) in &groups {
                        let gc = group_color(group_name);

                        // Group collapsing header with colored bar — profiles INSIDE
                        let id = ui.make_persistent_id(format!("ssh_grp_{}", group_name));
                        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                            .show_header(ui, |ui| {
                                // Left color bar
                                let (bar_rect, _) =
                                    ui.allocate_exact_size(egui::vec2(3.0, 16.0), egui::Sense::hover());
                                ui.painter().rect_filled(bar_rect, 1.0, gc);
                                ui.add_space(2.0);
                                ui.label(
                                    egui::RichText::new(format!("{}  ({})", group_name, profiles.len()))
                                        .size(12.0)
                                        .strong()
                                        .color(gc),
                                );
                            })
                            .body(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let mut profile_action = None;

                                for (idx, profile) in profiles {
                                    let selected = state.selected_profile == Some(*idx);

                                    let is_live = tabs.iter().any(|t| {
                                        t.is_ssh
                                            && t.title.contains(&profile.host)
                                            && t.is_connected
                                    });

                                    // Auth label — plain ASCII
                                    let auth_tag = match profile.auth_display.as_str() {
                                        "Key" => "[K]",
                                        "Agent" => "[A]",
                                        "Keyboard" => "[I]",
                                        _ => "[P]",
                                    };

                                    ui.horizontal(|ui| {
                                        // Tiny colored bar
                                        let (bar_rect, _) = ui.allocate_exact_size(
                                            egui::vec2(2.0, 16.0),
                                            egui::Sense::hover(),
                                        );
                                        if let Some(rgb) = profile.color_tag {
                                            ui.painter().rect_filled(
                                                bar_rect,
                                                0.0,
                                                egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
                                            );
                                        } else {
                                            ui.painter().rect_filled(bar_rect, 0.0, gc.linear_multiply(0.4));
                                        }

                                        // Live dot
                                        if is_live {
                                            let (dot_rect, _) = ui.allocate_exact_size(
                                                egui::vec2(6.0, 6.0),
                                                egui::Sense::hover(),
                                            );
                                            ui.painter()
                                                .circle_filled(dot_rect.center(), 2.5, dot_green);
                                        } else {
                                            ui.add_space(6.0);
                                        }

                                        let label = format!(
                                            " {} {}",
                                            auth_tag, profile.name,
                                        );
                                        let resp = ui.selectable_label(
                                            selected,
                                            egui::RichText::new(&label).size(11.5),
                                        );

                                        resp.clone().on_hover_ui(|ui| {
                                            ui.spacing_mut().item_spacing.y = 2.0;
                                            ui.label(
                                                egui::RichText::new(&profile.name)
                                                    .strong()
                                                    .color(accent),
                                            );
                                            ui.label(format!(
                                                "{}@{}:{}",
                                                profile.username, profile.host, profile.port
                                            ));
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "Auth: {} | Group: {}",
                                                    profile.auth_display, profile.group,
                                                ))
                                                .small()
                                                .color(gray),
                                            );
                                            if is_live {
                                                ui.label(
                                                    egui::RichText::new("* Connected")
                                                        .small()
                                                        .color(dot_green),
                                                );
                                            }
                                        });

                                        if resp.clicked() {
                                            state.selected_profile = Some(*idx);
                                        }
                                        if resp.double_clicked() {
                                            profile_action =
                                                Some(GuiAction::ConnectSavedProfile(*idx));
                                        }

                                        resp.context_menu(|ui| {
                                            if ui.button(">  连接").clicked() {
                                                profile_action =
                                                    Some(GuiAction::ConnectSavedProfile(*idx));
                                                ui.close_menu();
                                            }
                                            if ui.button("~  编辑...").clicked() {
                                                profile_action =
                                                    Some(GuiAction::EditSavedProfile(*idx));
                                                ui.close_menu();
                                            }
                                            ui.separator();
                                            if ui.button("x  删除").clicked() {
                                                profile_action =
                                                    Some(GuiAction::DeleteSavedProfile(*idx));
                                                ui.close_menu();
                                            }
                                        });
                                    });
                                }

                                if let Some(a) = profile_action {
                                    actions.push(a);
                                }
                            });

                        ui.add_space(2.0);
                    }

                    // ──────────────────────────────────────
                    //  Footer
                    // ──────────────────────────────────────
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} saved  |  {} active",
                                state.ssh_profiles.len(),
                                tabs.len(),
                            ))
                            .small()
                            .color(gray),
                        );
                    });
                });
        });
}

// ---------------------------------------------------------------------------
// Command history panel — floating window with search + click-to-execute
// ---------------------------------------------------------------------------

fn draw_history_panel(ctx: &egui::Context, state: &mut GuiState, actions: &mut Vec<GuiAction>) {
    let accent = egui::Color32::from_rgb(250, 189, 47);
    let gray = egui::Color32::from_rgb(146, 131, 116);

    egui::SidePanel::right("history_panel")
        .default_width(320.0)
        .min_width(200.0)
        .max_width(600.0)
        .resizable(true)
        .show(ctx, |ui| {
            // Header
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("History").size(14.0).strong().color(accent));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").on_hover_text("关闭").clicked() {
                        state.show_history_panel = false;
                    }
                    if ui.small_button("⟳").on_hover_text("刷新").clicked() {
                        actions.push(GuiAction::RefreshHistory);
                    }
                });
            });
            ui.separator();

            // Search bar
            ui.add(
                egui::TextEdit::singleline(&mut state.history_search)
                    .desired_width(ui.available_width())
                    .hint_text("搜索命令..."),
            );
            ui.add_space(4.0);

            let filter = state.history_search.to_lowercase();
            let filtered: Vec<&HistoryItem> = state
                .history_entries
                .iter()
                .filter(|e| filter.is_empty() || e.command.to_lowercase().contains(&filter))
                .collect();

            ui.label(
                egui::RichText::new(format!("{} 条记录", filtered.len()))
                    .small()
                    .color(gray),
            );
            ui.add_space(2.0);

            // History list
            egui::ScrollArea::vertical()
                .id_salt("history_list_scroll")
                .show(ui, |ui| {
                    for entry in &filtered {
                        let exit_color = if entry.exit_code == 0 {
                            egui::Color32::from_rgb(142, 192, 124) // green
                        } else {
                            egui::Color32::from_rgb(251, 73, 52) // red
                        };

                        // Format timestamp
                        let ts_str = {
                            let secs = entry.timestamp;
                            let hours = (secs / 3600) % 24;
                            let mins = (secs / 60) % 60;
                            format!("{:02}:{:02}", hours, mins)
                        };

                        let cmd_resp = ui.horizontal(|ui| {
                            // Exit status dot
                            let (dot_rect, _) = ui.allocate_exact_size(
                                egui::vec2(8.0, 8.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(dot_rect.center(), 3.5, exit_color);

                            // Command text (truncated, clickable)
                            let avail = ui.available_width() - 45.0;
                            let truncated = if entry.command.len() > 60 {
                                format!("{}…", &entry.command[..59])
                            } else {
                                entry.command.clone()
                            };
                            let cmd_label = egui::Label::new(
                                egui::RichText::new(&truncated)
                                    .size(12.0)
                                    .monospace()
                                    .color(egui::Color32::from_rgb(235, 219, 178)),
                            )
                            .wrap_mode(egui::TextWrapMode::Truncate)
                            .sense(egui::Sense::click());
                            let resp = ui.add_sized([avail, 16.0], cmd_label);
                            let clicked = resp.clicked();
                            resp.on_hover_text(&entry.command);

                            // Timestamp on the right
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    egui::RichText::new(&ts_str).size(10.0).color(gray),
                                );
                            });
                            clicked
                        });
                        if cmd_resp.inner {
                            actions.push(GuiAction::ExecHistoryCommand(entry.command.clone()));
                        }
                    }
                });
        });
}

// ---------------------------------------------------------------------------
// Settings window — tabbed: Appearance / Terminal / Keyboard
// ---------------------------------------------------------------------------

fn draw_settings_window(ctx: &egui::Context, state: &mut GuiState, actions: &mut Vec<GuiAction>) {
    let mut open = true;
    egui::Window::new("设置")
        .open(&mut open)
        .default_width(480.0)
        .default_height(360.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            // Tab selector at top
            ui.horizontal(|ui| {
                ui.selectable_value(&mut state.settings_tab, 0, "  外观  ");
                ui.selectable_value(&mut state.settings_tab, 1, "  终端  ");
                ui.selectable_value(&mut state.settings_tab, 2, "  快捷键  ");
            });
            ui.separator();

            match state.settings_tab {
                0 => draw_settings_appearance(ui, state),
                1 => draw_settings_terminal(ui, state),
                2 => draw_settings_keyboard(ui),
                _ => {}
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() {
                    actions.push(GuiAction::ApplySettings {
                        font_family: state.settings_font_family.clone(),
                        font_size: state.settings_font_size,
                        theme: state.settings_theme.clone(),
                        shell: state.settings_shell.clone(),
                        opacity: state.settings_opacity,
                        padding: state.settings_padding,
                        background_image: state.settings_background_image.clone(),
                        cursor_style: state.settings_cursor_style,
                        cursor_blink: state.settings_cursor_blink,
                    });
                }
                if ui.button("Close").clicked() {
                    state.show_settings = false;
                }
            });
        });
    if !open {
        state.show_settings = false;
    }
}

fn draw_settings_appearance(ui: &mut egui::Ui, state: &mut GuiState) {
    egui::Grid::new("settings_appearance")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .min_col_width(120.0)
        .show(ui, |ui| {
            ui.label("字体");
            ui.add(egui::TextEdit::singleline(&mut state.settings_font_family).desired_width(200.0));
            ui.end_row();

            ui.label("字号");
            ui.add(egui::Slider::new(&mut state.settings_font_size, 6.0..=72.0).suffix(" pt"));
            ui.end_row();

            ui.label("主题");
            egui::ComboBox::from_id_salt("theme_sel")
                .selected_text(&state.settings_theme)
                .width(200.0)
                .show_ui(ui, |ui| {
                    for name in &[
                        "gruvbox-dark",
                        "catppuccin-mocha",
                        "dracula",
                        "nord",
                        "solarized-dark",
                        "one-dark",
                    ] {
                        ui.selectable_value(&mut state.settings_theme, name.to_string(), *name);
                    }
                });
            ui.end_row();

            ui.label("透明度");
            ui.add(egui::Slider::new(&mut state.settings_opacity, 0.3..=1.0).show_value(true));
            ui.end_row();

            ui.label("内边距");
            ui.add(egui::Slider::new(&mut state.settings_padding, 0.0..=32.0).suffix(" px"));
            ui.end_row();

            ui.label("背景图片");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut state.settings_background_image)
                    .desired_width(160.0)
                    .hint_text("图片路径..."));
                if ui.button("浏览...").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("图片", &["png", "jpg", "jpeg", "bmp", "gif", "webp"])
                        .pick_file()
                    {
                        state.settings_background_image = path.to_string_lossy().to_string();
                    }
                }
                if !state.settings_background_image.is_empty() && ui.button("清除").clicked() {
                    state.settings_background_image.clear();
                }
            });
            ui.end_row();

            ui.label("光标样式");
            egui::ComboBox::from_id_salt("cursor_sel")
                .selected_text(match state.settings_cursor_style {
                    0 => "Block",
                    1 => "Beam",
                    2 => "Underline",
                    _ => "Beam",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.settings_cursor_style, 0, "Block");
                    ui.selectable_value(&mut state.settings_cursor_style, 1, "Beam");
                    ui.selectable_value(&mut state.settings_cursor_style, 2, "Underline");
                });
            ui.end_row();

            ui.label("光标闪烁");
            ui.checkbox(&mut state.settings_cursor_blink, "");
            ui.end_row();
        });
}

fn draw_settings_terminal(ui: &mut egui::Ui, state: &mut GuiState) {
    egui::Grid::new("settings_terminal")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .min_col_width(120.0)
        .show(ui, |ui| {
            ui.label("默认 Shell");
            egui::ComboBox::from_id_salt("shell_sel")
                .selected_text(&state.settings_shell)
                .width(200.0)
                .show_ui(ui, |ui| {
                    for name in &["auto", "pwsh.exe", "cmd.exe", "wsl", "bash"] {
                        ui.selectable_value(&mut state.settings_shell, name.to_string(), *name);
                    }
                });
            ui.end_row();

            ui.label("回滚行数");
            ui.add(egui::DragValue::new(&mut state.settings_scrollback).range(1000..=1_000_000));
            ui.end_row();
        });
}

fn draw_settings_keyboard(ui: &mut egui::Ui) {
    ui.label("键盘快捷键");
    ui.add_space(8.0);

    egui::Grid::new("shortcuts")
        .num_columns(2)
        .spacing([24.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            let shortcuts = [
                ("新建标签页", "Ctrl+Shift+T"),
                ("关闭标签页", "Ctrl+Shift+W"),
                ("复制", "Ctrl+Shift+C"),
                ("粘贴", "Ctrl+Shift+V"),
                ("查找", "Ctrl+F"),
                ("放大", "Ctrl+="),
                ("缩小", "Ctrl+-"),
                ("重置缩放", "Ctrl+0"),
                ("水平分屏", "Ctrl+Shift+D"),
                ("垂直分屏", "Ctrl+Shift+E"),
                ("切换会话面板", "Ctrl+Shift+B"),
                ("设置", "Ctrl+,"),
                ("全屏", "F11"),
            ];
            for (action, key) in &shortcuts {
                ui.label(*action);
                ui.label(egui::RichText::new(*key).monospace().strong());
                ui.end_row();
            }
        });
}

// ---------------------------------------------------------------------------
// SSH connection dialog — professional multi-section layout
// ---------------------------------------------------------------------------

fn draw_ssh_dialog(ctx: &egui::Context, state: &mut GuiState, actions: &mut Vec<GuiAction>) {
    let mut open = true;
    let is_editing = state.ssh_editing_profile_id.is_some();
    let dialog_title = if is_editing { "Edit SSH Connection" } else { "New SSH Connection" };
    egui::Window::new(dialog_title)
        .open(&mut open)
        .default_width(440.0)
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            egui::Grid::new("ssh_form")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .min_col_width(100.0)
                .show(ui, |ui| {
                    ui.label("Session Name");
                    ui.text_edit_singleline(&mut state.ssh_name);
                    ui.end_row();

                    ui.label("Group");
                    egui::ComboBox::from_id_salt("ssh_group")
                        .selected_text(&state.ssh_group)
                        .show_ui(ui, |ui| {
                            let existing: Vec<String> = state
                                .ssh_profiles
                                .iter()
                                .map(|p| p.group.clone())
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .collect();
                            for g in &existing {
                                ui.selectable_value(&mut state.ssh_group, g.clone(), g.as_str());
                            }
                            ui.selectable_value(
                                &mut state.ssh_group,
                                "Default".to_string(),
                                "Default",
                            );
                        });
                    ui.text_edit_singleline(&mut state.ssh_group);
                    ui.end_row();

                    ui.label("Host");
                    ui.text_edit_singleline(&mut state.ssh_host);
                    ui.end_row();

                    ui.label("Port");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.ssh_port)
                            .desired_width(60.0),
                    );
                    ui.end_row();

                    ui.label("Username");
                    ui.text_edit_singleline(&mut state.ssh_username);
                    ui.end_row();

                    ui.label("Authentication");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut state.ssh_auth_mode, 0, "Password");
                        ui.selectable_value(&mut state.ssh_auth_mode, 1, "Public Key");
                    });
                    ui.end_row();

                    if state.ssh_auth_mode == 0 {
                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.ssh_password).password(true),
                        );
                    } else {
                        ui.label("Key Path");
                        ui.text_edit_singleline(&mut state.ssh_key_path);
                    }
                    ui.end_row();
                });

            ui.separator();
            ui.horizontal(|ui| {
                let port = state.ssh_port.parse::<u16>().unwrap_or(22);
                let name = if state.ssh_name.is_empty() {
                    format!("{}@{}", state.ssh_username, state.ssh_host)
                } else {
                    state.ssh_name.clone()
                };

                if is_editing {
                    // ── Edit mode ──
                    let editing_id = state.ssh_editing_profile_id.clone().unwrap();
                    if ui.button("Update & Connect").clicked() {
                        actions.push(GuiAction::UpdateProfile {
                            id: editing_id.clone(),
                            name: name.clone(),
                            group: state.ssh_group.clone(),
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth_mode: state.ssh_auth_mode,
                            password: state.ssh_password.clone(),
                            key_path: state.ssh_key_path.clone(),
                        });
                        let auth = if state.ssh_auth_mode == 0 {
                            SshAuthInput::Password(state.ssh_password.clone())
                        } else {
                            SshAuthInput::KeyFile(state.ssh_key_path.clone())
                        };
                        actions.push(GuiAction::ConnectSsh {
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth,
                        });
                        state.show_ssh_dialog = false;
                        state.ssh_editing_profile_id = None;
                    }
                    if ui.button("Update").clicked() {
                        actions.push(GuiAction::UpdateProfile {
                            id: editing_id,
                            name,
                            group: state.ssh_group.clone(),
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth_mode: state.ssh_auth_mode,
                            password: state.ssh_password.clone(),
                            key_path: state.ssh_key_path.clone(),
                        });
                        state.show_ssh_dialog = false;
                        state.ssh_editing_profile_id = None;
                    }
                } else {
                    // ── New mode ──
                    if ui.button("Connect").clicked() {
                        let auth = if state.ssh_auth_mode == 0 {
                            SshAuthInput::Password(state.ssh_password.clone())
                        } else {
                            SshAuthInput::KeyFile(state.ssh_key_path.clone())
                        };
                        actions.push(GuiAction::ConnectSsh {
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth,
                        });
                    }
                    if ui.button("Save & Connect").clicked() {
                        actions.push(GuiAction::SaveProfile {
                            name: name.clone(),
                            group: state.ssh_group.clone(),
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth_mode: state.ssh_auth_mode,
                            password: state.ssh_password.clone(),
                            key_path: state.ssh_key_path.clone(),
                        });
                        let auth = if state.ssh_auth_mode == 0 {
                            SshAuthInput::Password(state.ssh_password.clone())
                        } else {
                            SshAuthInput::KeyFile(state.ssh_key_path.clone())
                        };
                        actions.push(GuiAction::ConnectSsh {
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth,
                        });
                        state.show_ssh_dialog = false;
                    }
                    if ui.button("Save").clicked() {
                        actions.push(GuiAction::SaveProfile {
                            name,
                            group: state.ssh_group.clone(),
                            host: state.ssh_host.clone(),
                            port,
                            username: state.ssh_username.clone(),
                            auth_mode: state.ssh_auth_mode,
                            password: state.ssh_password.clone(),
                            key_path: state.ssh_key_path.clone(),
                        });
                        state.show_ssh_dialog = false;
                    }
                }
                if ui.button("Cancel").clicked() {
                    state.show_ssh_dialog = false;
                    state.ssh_editing_profile_id = None;
                }
            });
        });
    if !open {
        state.show_ssh_dialog = false;
        state.ssh_editing_profile_id = None;
    }
}

// ---------------------------------------------------------------------------
// About window
// ---------------------------------------------------------------------------

fn draw_about_window(ctx: &egui::Context, state: &mut GuiState) {
    let mut open = true;
    egui::Window::new("About NexTerm")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(320.0)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.heading(
                    egui::RichText::new("NexTerm")
                        .size(24.0)
                        .strong()
                        .color(egui::Color32::from_rgb(250, 189, 47)),
                );
                ui.label("AI-native GPU-accelerated Terminal");
                ui.add_space(4.0);
                ui.label("Version 0.1.0");
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Terminal + SSH + SFTP + AI Agent")
                        .color(egui::Color32::from_rgb(146, 131, 116)),
                );
                ui.add_space(8.0);
                ui.hyperlink_to("GitHub Repository", "https://github.com/RigelNana/nexterm");
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Built with wgpu + egui + cosmic-text")
                        .small()
                        .color(egui::Color32::from_rgb(146, 131, 116)),
                );
            });
        });
    if !open {
        state.show_about = false;
    }
}

// ---------------------------------------------------------------------------
// Find bar (bottom overlay)
// ---------------------------------------------------------------------------

fn draw_find_bar(ctx: &egui::Context, state: &mut GuiState, actions: &mut Vec<GuiAction>) {
    let accent = egui::Color32::from_rgb(250, 189, 47);
    let gray = egui::Color32::from_rgb(146, 131, 116);
    let active_bg = egui::Color32::from_rgb(80, 73, 69);

    egui::TopBottomPanel::bottom("find_bar")
        .exact_height(38.0)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;

                // ---- Search icon ----
                ui.label(egui::RichText::new("Find:").size(12.0).strong().color(accent));

                // ---- Search input ----
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut state.find_query)
                        .desired_width(260.0)
                        .hint_text("Search...")
                        .margin(egui::Margin::symmetric(6, 3)),
                );

                // Auto-focus when find bar opens
                if state.find_last_query.is_empty() && state.find_query.is_empty() {
                    resp.request_focus();
                }

                // Escape to close
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    state.show_find_bar = false;
                    state.find_match_count = 0;
                    state.find_current_index = 0;
                    actions.push(GuiAction::ClearFind);
                    return;
                }

                // Enter = find next, Shift+Enter = find prev
                let enter_pressed = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter_pressed {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if shift {
                        actions.push(GuiAction::FindPrev {
                            query: state.find_query.clone(),
                            case_sensitive: state.find_case_sensitive,
                            whole_word: state.find_whole_word,
                            use_regex: state.find_use_regex,
                        });
                    } else {
                        actions.push(GuiAction::FindNext {
                            query: state.find_query.clone(),
                            case_sensitive: state.find_case_sensitive,
                            whole_word: state.find_whole_word,
                            use_regex: state.find_use_regex,
                        });
                    }
                    resp.request_focus();
                }

                // Auto-search on query or toggle change
                let query_changed = state.find_query != state.find_last_query;
                if query_changed {
                    state.find_last_query = state.find_query.clone();
                    if !state.find_query.is_empty() {
                        actions.push(GuiAction::FindAll {
                            query: state.find_query.clone(),
                            case_sensitive: state.find_case_sensitive,
                            whole_word: state.find_whole_word,
                            use_regex: state.find_use_regex,
                        });
                    } else {
                        state.find_match_count = 0;
                        state.find_current_index = 0;
                        actions.push(GuiAction::ClearFind);
                    }
                }

                ui.add_space(2.0);

                // ---- Toggle: Case Sensitive ----
                let cs_color = if state.find_case_sensitive { accent } else { gray };
                let cs_bg = if state.find_case_sensitive { active_bg } else { egui::Color32::TRANSPARENT };
                let cs_btn = egui::Button::new(
                    egui::RichText::new("Aa").size(11.0).strong().color(cs_color)
                ).fill(cs_bg).corner_radius(egui::CornerRadius::same(2)).min_size(egui::vec2(26.0, 20.0));
                if ui.add(cs_btn).on_hover_text("Match Case").clicked() {
                    state.find_case_sensitive = !state.find_case_sensitive;
                    state.find_last_query.clear(); // force re-search
                }

                // ---- Toggle: Whole Word ----
                let ww_color = if state.find_whole_word { accent } else { gray };
                let ww_bg = if state.find_whole_word { active_bg } else { egui::Color32::TRANSPARENT };
                let ww_btn = egui::Button::new(
                    egui::RichText::new("W").size(11.0).strong().color(ww_color)
                ).fill(ww_bg).corner_radius(egui::CornerRadius::same(2)).min_size(egui::vec2(26.0, 20.0));
                if ui.add(ww_btn).on_hover_text("Whole Word").clicked() {
                    state.find_whole_word = !state.find_whole_word;
                    state.find_last_query.clear();
                }

                // ---- Toggle: Regex ----
                let rx_color = if state.find_use_regex { accent } else { gray };
                let rx_bg = if state.find_use_regex { active_bg } else { egui::Color32::TRANSPARENT };
                let rx_btn = egui::Button::new(
                    egui::RichText::new(".*").size(11.0).strong().color(rx_color)
                ).fill(rx_bg).corner_radius(egui::CornerRadius::same(2)).min_size(egui::vec2(26.0, 20.0));
                if ui.add(rx_btn).on_hover_text("Use Regex").clicked() {
                    state.find_use_regex = !state.find_use_regex;
                    state.find_last_query.clear();
                }

                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                // ---- Match count ----
                if state.find_match_count > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            state.find_current_index + 1,
                            state.find_match_count
                        ))
                        .size(11.0)
                        .color(accent),
                    );
                } else if !state.find_query.is_empty() {
                    ui.label(
                        egui::RichText::new("No results")
                            .size(11.0)
                            .color(egui::Color32::from_rgb(204, 36, 29)),
                    );
                }

                ui.add_space(2.0);

                // ---- Nav buttons ----
                let nav_enabled = state.find_match_count > 0;
                if ui.add_enabled(nav_enabled, egui::Button::new(
                    egui::RichText::new("^").size(10.0)
                ).min_size(egui::vec2(22.0, 20.0))).on_hover_text("Previous  Shift+Enter").clicked() {
                    actions.push(GuiAction::FindPrev {
                        query: state.find_query.clone(),
                        case_sensitive: state.find_case_sensitive,
                        whole_word: state.find_whole_word,
                        use_regex: state.find_use_regex,
                    });
                }
                if ui.add_enabled(nav_enabled, egui::Button::new(
                    egui::RichText::new("v").size(10.0)
                ).min_size(egui::vec2(22.0, 20.0))).on_hover_text("Next  Enter").clicked() {
                    actions.push(GuiAction::FindNext {
                        query: state.find_query.clone(),
                        case_sensitive: state.find_case_sensitive,
                        whole_word: state.find_whole_word,
                        use_regex: state.find_use_regex,
                    });
                }

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                // ---- Find All ----
                if ui.add_enabled(!state.find_query.is_empty(), egui::Button::new(
                    egui::RichText::new("Find All").size(11.0)
                ).min_size(egui::vec2(0.0, 20.0))).clicked() {
                    actions.push(GuiAction::FindAll {
                        query: state.find_query.clone(),
                        case_sensitive: state.find_case_sensitive,
                        whole_word: state.find_whole_word,
                        use_regex: state.find_use_regex,
                    });
                }

                // ---- Close button (right-aligned) ----
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(
                        egui::RichText::new("X").size(12.0).color(gray)
                    ).frame(false)).on_hover_text("Close  Esc").clicked() {
                        state.show_find_bar = false;
                        state.find_match_count = 0;
                        state.find_current_index = 0;
                        actions.push(GuiAction::ClearFind);
                    }
                });
            });
        });
}

// ---------------------------------------------------------------------------
// Theme / Styling — Gruvbox Dark
// ---------------------------------------------------------------------------

fn apply_theme(ctx: &egui::Context) {
    use egui::{style::WidgetVisuals, Color32, CornerRadius, Stroke, Visuals};

    let mut visuals = Visuals::light();
    let bg = Color32::from_rgb(250, 250, 250);
    let bg1 = Color32::from_rgb(235, 235, 235);
    let bg2 = Color32::from_rgb(218, 218, 218);
    let fg = Color32::from_rgb(40, 40, 40);
    let accent = Color32::from_rgb(0, 120, 212);
    let gray = Color32::from_rgb(130, 130, 130);
    let cr = CornerRadius::same(4);

    visuals.panel_fill = Color32::from_rgb(245, 245, 245);
    visuals.window_fill = Color32::from_rgb(255, 255, 255);
    visuals.faint_bg_color = bg1;
    visuals.extreme_bg_color = Color32::from_rgb(255, 255, 255);
    visuals.override_text_color = Some(fg);
    visuals.selection.bg_fill = Color32::from_rgb(179, 215, 243);
    visuals.selection.stroke = Stroke::new(1.0, accent);
    visuals.window_stroke = Stroke::new(1.0, bg2);

    visuals.widgets.noninteractive = WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(0.5, bg2),
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: cr,
        expansion: 0.0,
    };
    visuals.widgets.inactive = WidgetVisuals {
        bg_fill: bg1,
        weak_bg_fill: Color32::from_rgb(240, 240, 240),
        bg_stroke: Stroke::new(0.5, bg2),
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: cr,
        expansion: 0.0,
    };
    visuals.widgets.hovered = WidgetVisuals {
        bg_fill: Color32::from_rgb(225, 238, 250),
        weak_bg_fill: Color32::from_rgb(225, 238, 250),
        bg_stroke: Stroke::new(1.0, accent),
        fg_stroke: Stroke::new(1.0, accent),
        corner_radius: cr,
        expansion: 1.0,
    };
    visuals.widgets.active = WidgetVisuals {
        bg_fill: accent,
        weak_bg_fill: accent,
        bg_stroke: Stroke::new(1.0, accent),
        fg_stroke: Stroke::new(1.0, Color32::WHITE),
        corner_radius: cr,
        expansion: 0.0,
    };
    visuals.widgets.open = WidgetVisuals {
        bg_fill: bg1,
        weak_bg_fill: bg1,
        bg_stroke: Stroke::new(1.0, gray),
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: cr,
        expansion: 0.0,
    };

    ctx.set_visuals(visuals);

    // Increase global font size for readability
    ctx.style_mut(|s| {
        for (_text_style, font_id) in s.text_styles.iter_mut() {
            font_id.size = (font_id.size * 1.20).max(13.0);
        }
    });

    // Slightly more spacious layout
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(7.0, 5.0);
    style.spacing.button_padding = egui::vec2(6.0, 3.0);
    style.spacing.window_margin = egui::Margin::same(10);
    ctx.set_style(style);
}

// ---------------------------------------------------------------------------
// SFTP file browser panel — left sidebar with path bar + detail table
// ---------------------------------------------------------------------------

/// Format file size into human-readable string.
fn fmt_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} K", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Format unix permissions (mode bits) into rwxrwxrwx string.
pub fn fmt_perms(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    let bits = [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ];
    for (bit, ch) in &bits {
        s.push(if mode & bit != 0 { *ch } else { '-' });
    }
    s
}

/// Format unix timestamp to a short date/time string.
pub fn fmt_mtime(ts: u64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    // Simple: show as "YYYY-MM-DD HH:MM"
    // We do a rough conversion without pulling in chrono
    let secs_per_min = 60u64;
    let secs_per_hour = 3600u64;
    let secs_per_day = 86400u64;
    // Days since epoch
    let mut days = ts / secs_per_day;
    let day_secs = ts % secs_per_day;
    let hour = day_secs / secs_per_hour;
    let min = (day_secs % secs_per_hour) / secs_per_min;
    // Compute year/month/day from days since epoch (1970-01-01)
    let mut year = 1970u64;
    loop {
        let ydays = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 366 } else { 365 };
        if days < ydays { break; }
        days -= ydays;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let mdays = [31, if leap {29} else {28}, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0usize;
    for (i, &md) in mdays.iter().enumerate() {
        if days < md { month = i; break; }
        days -= md;
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", year, month + 1, days + 1, hour, min)
}

fn draw_sftp_panel(
    ctx: &egui::Context,
    state: &mut GuiState,
    snap: &SftpBrowserSnapshot,
    actions: &mut Vec<GuiAction>,
) {
    let bg = egui::Color32::from_rgb(248, 248, 248);
    let header_color = egui::Color32::from_rgb(0, 120, 212);
    let label_color = egui::Color32::from_rgb(100, 100, 100);
    let _value_color = egui::Color32::from_rgb(40, 40, 40);
    let dir_color = egui::Color32::from_rgb(0, 95, 160);
    let file_color = egui::Color32::from_rgb(50, 50, 50);
    let link_color = egui::Color32::from_rgb(160, 60, 100);
    let border_color = egui::Color32::from_rgb(200, 200, 200);
    let fs: f32 = 13.0; // readable font size

    egui::TopBottomPanel::bottom("sftp_panel")
        .resizable(true)
        .default_height(300.0)
        .height_range(100.0..=600.0)
        .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin::same(6)))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;

            if !snap.initialized {
                ui.label(egui::RichText::new("Connecting to SFTP...").size(fs).color(label_color));
                return;
            }

            // Sync path input when directory changes
            if state.sftp_path_input != snap.current_path {
                state.sftp_path_input = snap.current_path.clone();
            }

            // ── Top bar: nav buttons + editable path + action buttons ──
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("SFTP").size(fs).strong().color(header_color));
                ui.separator();
                if ui.button(egui::RichText::new("^").size(fs)).on_hover_text("Go up").clicked() {
                    actions.push(GuiAction::SftpGoUp);
                }
                if ui.button(egui::RichText::new("~").size(fs)).on_hover_text("Go home").clicked() {
                    actions.push(GuiAction::SftpGoHome);
                }
                ui.separator();

                // Editable path input
                let path_resp = ui.add(
                    egui::TextEdit::singleline(&mut state.sftp_path_input)
                        .desired_width(ui.available_width() - 220.0)
                        .font(egui::TextStyle::Body)
                );
                if path_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let target = state.sftp_path_input.trim().to_string();
                    if !target.is_empty() {
                        actions.push(GuiAction::SftpNavigate(target));
                    }
                }

                ui.separator();
                // Action buttons
                if ui.button(egui::RichText::new("+Dir").size(fs)).on_hover_text("New directory").clicked() {
                    let name = state.sftp_new_name.trim().to_string();
                    if !name.is_empty() {
                        let full = format!("{}/{}", snap.current_path.trim_end_matches('/'), name);
                        actions.push(GuiAction::SftpMkdir(full));
                        state.sftp_new_name.clear();
                    }
                }
                if ui.button(egui::RichText::new("+File").size(fs)).on_hover_text("New file").clicked() {
                    let name = state.sftp_new_name.trim().to_string();
                    if !name.is_empty() {
                        let full = format!("{}/{}", snap.current_path.trim_end_matches('/'), name);
                        actions.push(GuiAction::SftpTouch(full));
                        state.sftp_new_name.clear();
                    }
                }
                if ui.button(egui::RichText::new("Upload").size(fs)).on_hover_text("Upload file").clicked() {
                    actions.push(GuiAction::SftpUpload);
                }
            });
            // Name input for mkdir/touch
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Name:").size(fs).color(label_color));
                ui.add(
                    egui::TextEdit::singleline(&mut state.sftp_new_name)
                        .desired_width(200.0)
                        .hint_text("new file or folder name")
                        .font(egui::TextStyle::Body)
                );
            });

            if let Some(err) = &snap.error {
                ui.label(egui::RichText::new(format!("Error: {err}")).size(fs).color(egui::Color32::from_rgb(204, 36, 29)));
            }

            // ── Transfer progress section (active uploads/downloads) ──
            if !snap.transfers.is_empty() {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Transfers").size(fs).strong().color(header_color));
                    let active = snap.transfers.iter().filter(|t| !t.done).count();
                    let done = snap.transfers.iter().filter(|t| t.done).count();
                    ui.label(
                        egui::RichText::new(format!("{} active, {} done", active, done))
                            .size(fs - 1.0)
                            .color(label_color),
                    );
                    if done > 0
                        && ui.small_button("Clear done")
                            .on_hover_text("Remove completed transfers")
                            .clicked()
                    {
                        actions.push(GuiAction::SftpClearTransfers);
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("sftp_transfers_scroll")
                    .max_height(80.0)
                    .show(ui, |ui| {
                        for t in &snap.transfers {
                            let name = t.remote_path
                                .rsplit('/')
                                .next()
                                .unwrap_or(&t.remote_path);
                            let frac = if t.total > 0 {
                                (t.bytes as f32 / t.total as f32).clamp(0.0, 1.0)
                            } else if t.done {
                                1.0
                            } else {
                                0.0
                            };
                            let arrow = if t.direction == "Upload" { "↑" } else { "↓" };
                            let status_color = if let Some(_) = &t.error {
                                egui::Color32::from_rgb(204, 36, 29)
                            } else if t.done {
                                egui::Color32::from_rgb(22, 160, 70)
                            } else {
                                header_color
                            };
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("{arrow} {name}"))
                                        .size(fs)
                                        .color(status_color),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} / {}",
                                        fmt_size(t.bytes),
                                        if t.total > 0 { fmt_size(t.total) } else { "?".into() },
                                    ))
                                    .size(fs - 1.0)
                                    .color(label_color),
                                );
                                let pb = egui::ProgressBar::new(frac)
                                    .desired_width(180.0)
                                    .show_percentage();
                                ui.add(pb);
                                if let Some(err) = &t.error {
                                    ui.label(
                                        egui::RichText::new(err)
                                            .size(fs - 1.0)
                                            .color(egui::Color32::from_rgb(204, 36, 29)),
                                    );
                                }
                            });
                        }
                    });
            }

            ui.separator();

            // ── Two-column layout with draggable splitter ──
            let avail = ui.available_size();
            let splitter_w: f32 = 6.0;
            let tree_w = (avail.x * state.sftp_tree_ratio).clamp(80.0, avail.x - 200.0);

            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                // ── Left: directory tree ──
                ui.allocate_ui_with_layout(
                    egui::vec2(tree_w, avail.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.label(egui::RichText::new("Directories").size(fs).strong().color(header_color));
                        ui.add_space(2.0);
                        egui::ScrollArea::vertical()
                            .id_salt("sftp_tree_scroll")
                            .max_height(avail.y - 22.0)
                            .show(ui, |ui| {
                                egui::Grid::new("sftp_dir_grid")
                                    .num_columns(1)
                                    .spacing([0.0, 1.0])
                                    .striped(true)
                                    .show(ui, |ui| {
                                        for entry in &snap.entries {
                                            if !entry.is_dir { continue; }
                                            let label = egui::RichText::new(format!("  {}", entry.name))
                                                .size(fs)
                                                .color(dir_color);
                                            if ui.add(egui::Label::new(label).sense(egui::Sense::click()))
                                                .on_hover_text(&entry.path)
                                                .clicked()
                                            {
                                                actions.push(GuiAction::SftpNavigate(entry.path.clone()));
                                            }
                                            ui.end_row();
                                        }
                                    });
                            });
                    },
                );

                // ── Draggable splitter ──
                let (splitter_rect, splitter_resp) = ui.allocate_exact_size(
                    egui::vec2(splitter_w, avail.y),
                    egui::Sense::drag(),
                );
                let splitter_color = if splitter_resp.hovered() || splitter_resp.dragged() {
                    header_color
                } else {
                    border_color
                };
                ui.painter().rect_filled(
                    egui::Rect::from_center_size(splitter_rect.center(), egui::vec2(2.0, avail.y)),
                    0.0,
                    splitter_color,
                );
                if splitter_resp.dragged() {
                    let delta = splitter_resp.drag_delta().x;
                    let new_ratio = (tree_w + delta) / avail.x;
                    state.sftp_tree_ratio = new_ratio.clamp(0.08, 0.50);
                }
                if splitter_resp.hovered() || splitter_resp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }

                // ── Right: file detail table (fills all remaining space) ──
                let actual_right_w = ui.available_width().max(200.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(actual_right_w, avail.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_width(actual_right_w);
                        egui::ScrollArea::vertical()
                            .id_salt("sftp_detail_scroll")
                            .max_height(avail.y)
                            .show(ui, |ui| {
                                ui.set_min_width(actual_right_w - 16.0);
                                egui::Grid::new("sftp_files_grid")
                                    .num_columns(8)
                                    .spacing([8.0, 1.0])
                                    .striped(true)
                                    .min_col_width(0.0)
                                    .show(ui, |ui| {
                                        // Header
                                        ui.label(egui::RichText::new(" ").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("Name").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("Size").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("Type").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("Modified").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("Perms").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("Owner/Group").size(fs).strong().color(label_color));
                                        ui.label(egui::RichText::new("").size(fs));
                                        ui.end_row();

                                        for entry in &snap.entries {
                                            let name_color = if entry.is_dir {
                                                dir_color
                                            } else if entry.file_type == "link" {
                                                link_color
                                            } else {
                                                file_color
                                            };
                                            let icon = if entry.is_dir { "D" } else if entry.file_type == "link" { "L" } else { "-" };

                                            ui.label(egui::RichText::new(icon).size(fs).color(name_color));
                                            if entry.is_dir {
                                                if ui.add(egui::Label::new(
                                                    egui::RichText::new(&entry.name).size(fs).color(name_color),
                                                ).sense(egui::Sense::click())).double_clicked() {
                                                    actions.push(GuiAction::SftpNavigate(entry.path.clone()));
                                                }
                                            } else {
                                                ui.label(egui::RichText::new(&entry.name).size(fs).color(name_color));
                                            }
                                            let size_str = if entry.is_dir { "-".to_string() } else { fmt_size(entry.size) };
                                            ui.label(egui::RichText::new(size_str).size(fs).color(label_color));
                                            ui.label(egui::RichText::new(&entry.file_type).size(fs).color(label_color));
                                            ui.label(egui::RichText::new(&entry.modified).size(fs).color(label_color));
                                            ui.label(egui::RichText::new(&entry.permissions).size(fs).color(label_color).monospace());
                                            ui.label(egui::RichText::new(&entry.group).size(fs).color(label_color));
                                            // Download button for files
                                            if !entry.is_dir {
                                                if ui.small_button("DL").on_hover_text("Download").clicked() {
                                                    actions.push(GuiAction::SftpDownload(entry.path.clone()));
                                                }
                                            } else {
                                                ui.label(egui::RichText::new("").size(fs));
                                            }
                                            ui.end_row();
                                        }
                                    });
                            });
                    },
                );
            });
        });
}

// ---------------------------------------------------------------------------
// System status panel — right sidebar with CPU/Mem, Disk, Net graphs
// ---------------------------------------------------------------------------

/// Format bytes/sec into human-readable string.
fn fmt_rate(bps: f64) -> String {
    if bps >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bps / 1_000_000_000.0)
    } else if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1} KB/s", bps / 1_000.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

/// Reformat a df -h size string to ensure G/T values have 2 decimal places.
/// e.g. "1.2T" → "1.20T", "500G" → "500.00G", "100M" → "100M" (unchanged).
fn fmt_disk_size(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return s.to_string();
    }
    let last = s.as_bytes()[s.len() - 1];
    if last == b'T' || last == b'G' {
        let num_str = &s[..s.len() - 1];
        if let Ok(val) = num_str.parse::<f64>() {
            return format!("{:.2}{}", val, last as char);
        }
    }
    s.to_string()
}

/// Format a memory value in MB into human-readable string with 2 decimal places.
fn fmt_mem_mb(mb: u64) -> String {
    if mb >= 1_048_576 {
        format!("{:.2} TB", mb as f64 / 1_048_576.0)
    } else if mb >= 1024 {
        format!("{:.2} GB", mb as f64 / 1024.0)
    } else {
        format!("{} MB", mb)
    }
}

fn draw_system_panel(ctx: &egui::Context, state: &mut GuiState, ss: &ServerStatusSnapshot) {
    let panel_width = 270.0;
    let bg = egui::Color32::from_rgb(248, 248, 248);
    let header_color = egui::Color32::from_rgb(0, 120, 212);
    let label_color = egui::Color32::from_rgb(100, 100, 100);
    let value_color = egui::Color32::from_rgb(40, 40, 40);
    let green = egui::Color32::from_rgb(22, 160, 70);
    let yellow = egui::Color32::from_rgb(200, 150, 0);
    let red = egui::Color32::from_rgb(210, 50, 45);
    let cyan = egui::Color32::from_rgb(0, 140, 170);
    let purple = egui::Color32::from_rgb(140, 60, 120);
    let bar_bg = egui::Color32::from_rgb(225, 225, 225);

    egui::SidePanel::right("system_panel")
        .default_width(panel_width)
        .min_width(200.0)
        .max_width(500.0)
        .resizable(true)
        .frame(egui::Frame::new().fill(bg).inner_margin(egui::Margin::same(8)))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;

            // ── Header ──
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("System Status").size(14.0).strong().color(header_color));
                if !ss.hostname.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(&ss.hostname).small().color(label_color));
                    });
                }
            });
            ui.separator();

            // ── CPU & Memory ──
            ui.label(egui::RichText::new("CPU / Memory").size(11.0).strong().color(value_color));
            egui::Grid::new("cpu_mem_grid")
                .num_columns(2)
                .spacing([8.0, 2.0])
                .show(ui, |ui| {
                    let bar_w = 110.0;
                    let bar_h = 12.0;

                    // CPU
                    ui.label(egui::RichText::new("CPU").small().color(label_color));
                    let cpu_color = if ss.cpu_pct > 90.0 { red } else if ss.cpu_pct > 70.0 { yellow } else { green };
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, bar_h), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, bar_bg);
                        let fill_w = bar_w * (ss.cpu_pct / 100.0).min(1.0);
                        if fill_w > 0.0 {
                            ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, bar_h)), 2.0, cpu_color);
                        }
                        ui.label(egui::RichText::new(format!("{:.0}%", ss.cpu_pct)).small().color(cpu_color));
                    });
                    ui.end_row();

                    // Memory
                    ui.label(egui::RichText::new("Mem").small().color(label_color));
                    let mem_pct = if ss.mem_total_mb > 0 { ss.mem_used_mb as f32 / ss.mem_total_mb as f32 * 100.0 } else { 0.0 };
                    let mem_color = if mem_pct > 90.0 { red } else if mem_pct > 70.0 { yellow } else { green };
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, bar_h), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, bar_bg);
                        let fill_w = bar_w * (mem_pct / 100.0).min(1.0);
                        if fill_w > 0.0 {
                            ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, bar_h)), 2.0, mem_color);
                        }
                        ui.label(egui::RichText::new(format!("{}/{}", fmt_mem_mb(ss.mem_used_mb), fmt_mem_mb(ss.mem_total_mb))).small().color(mem_color));
                    });
                    ui.end_row();

                    // Load
                    if !ss.load_avg.is_empty() {
                        ui.label(egui::RichText::new("Load").small().color(label_color));
                        ui.label(egui::RichText::new(&ss.load_avg).small().color(value_color));
                        ui.end_row();
                    }
                    // Uptime
                    if !ss.uptime.is_empty() {
                        ui.label(egui::RichText::new("Up").small().color(label_color));
                        ui.label(egui::RichText::new(&ss.uptime).small().color(value_color));
                        ui.end_row();
                    }
                    // Latency (true RTT via echo)
                    ui.label(egui::RichText::new("RTT").small().color(label_color));
                    let lat_color = if ss.latency_ms > 500 { red } else if ss.latency_ms > 200 { yellow } else { green };
                    ui.label(egui::RichText::new(format!("{}ms", ss.latency_ms)).small().color(lat_color));
                    ui.end_row();
                });

            ui.add_space(4.0);
            ui.separator();

            // ── Disks ──
            if !ss.disks.is_empty() {
                ui.label(egui::RichText::new("Disks").size(11.0).strong().color(value_color));
                egui::Grid::new("disk_grid")
                    .num_columns(4)
                    .spacing([4.0, 1.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Mount").small().strong().color(label_color));
                        ui.label(egui::RichText::new("Type").small().strong().color(label_color));
                        ui.label(egui::RichText::new("Used/Total").small().strong().color(label_color));
                        ui.label(egui::RichText::new("Use%").small().strong().color(label_color));
                        ui.end_row();

                        for d in &ss.disks {
                            ui.label(egui::RichText::new(&d.mount).small().color(value_color));
                            ui.label(egui::RichText::new(&d.fstype).small().color(label_color));
                            ui.label(egui::RichText::new(format!("{}/{}", fmt_disk_size(&d.used), fmt_disk_size(&d.total))).small().color(label_color));
                            let pct_num: f32 = d.use_pct.trim_end_matches('%').parse().unwrap_or(0.0);
                            let pct_color = if pct_num > 90.0 { red } else if pct_num > 70.0 { yellow } else { green };
                            ui.label(egui::RichText::new(&d.use_pct).small().color(pct_color));
                            ui.end_row();
                        }
                    });
                ui.add_space(4.0);
                ui.separator();
            }

            // ── Network ──
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Network").size(11.0).strong().color(value_color));

                // Interface selector
                if !ss.net_interfaces.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let current_label = match state.selected_net_if {
                            None => "All".to_string(),
                            Some(idx) => ss.net_interfaces.get(idx)
                                .map(|i| i.name.clone())
                                .unwrap_or_else(|| "All".to_string()),
                        };
                        egui::ComboBox::from_id_salt("net_if_sel")
                            .width(80.0)
                            .selected_text(egui::RichText::new(&current_label).small())
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(state.selected_net_if.is_none(), "All").clicked() {
                                    state.selected_net_if = None;
                                }
                                for (i, iface) in ss.net_interfaces.iter().enumerate() {
                                    if ui.selectable_label(state.selected_net_if == Some(i), &iface.name).clicked() {
                                        state.selected_net_if = Some(i);
                                    }
                                }
                            });
                    });
                }
            });

            // Pick the history to display
            let history: &[NetRateSnapshot] = match state.selected_net_if {
                Some(idx) => ss.net_interfaces.get(idx)
                    .map(|i| i.history.as_slice())
                    .unwrap_or(&ss.net_history),
                None => &ss.net_history,
            };

            if history.is_empty() {
                ui.label(egui::RichText::new("Collecting...").small().color(label_color));
            } else {
                // Current rate
                let last = history.last().unwrap();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("RX: {}", fmt_rate(last.rx_bps))).small().color(cyan));
                    ui.label(egui::RichText::new(format!("TX: {}", fmt_rate(last.tx_bps))).small().color(purple));
                });

                // Sparkline area chart
                let graph_h = 60.0;
                let avail_w = ui.available_width();
                let (rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, graph_h), egui::Sense::hover());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(240, 240, 240));

                let n = history.len();
                if n > 1 {
                    let max_val = history.iter()
                        .map(|s| s.rx_bps.max(s.tx_bps))
                        .fold(1.0_f64, f64::max);
                    let step = avail_w / (n.max(2) - 1) as f32;

                    // Helper to draw an area + line
                    let draw_series = |data: &[f64], color: egui::Color32, fill_alpha: u8| {
                        let points: Vec<egui::Pos2> = data.iter().enumerate().map(|(i, &v)| {
                            let x = rect.min.x + i as f32 * step;
                            let y = rect.max.y - (v / max_val) as f32 * graph_h;
                            egui::pos2(x, y.max(rect.min.y))
                        }).collect();
                        // Fill
                        let mut fill = vec![egui::pos2(rect.min.x, rect.max.y)];
                        fill.extend_from_slice(&points);
                        fill.push(egui::pos2(rect.min.x + (n - 1) as f32 * step, rect.max.y));
                        let fc = egui::Color32::from_rgba_premultiplied(color.r(), color.g(), color.b(), fill_alpha);
                        painter.add(egui::Shape::convex_polygon(fill, fc, egui::Stroke::NONE));
                        // Line
                        for w in points.windows(2) {
                            painter.line_segment([w[0], w[1]], egui::Stroke::new(1.5, color));
                        }
                    };

                    let rx_vals: Vec<f64> = history.iter().map(|s| s.rx_bps).collect();
                    let tx_vals: Vec<f64> = history.iter().map(|s| s.tx_bps).collect();
                    draw_series(&rx_vals, cyan, 40);
                    draw_series(&tx_vals, purple, 40);

                    painter.text(
                        egui::pos2(rect.max.x - 2.0, rect.min.y + 2.0),
                        egui::Align2::RIGHT_TOP,
                        fmt_rate(max_val),
                        egui::FontId::proportional(9.0),
                        label_color,
                    );
                }

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("── RX").small().color(cyan));
                    ui.label(egui::RichText::new("── TX").small().color(purple));
                });
            }

            // ── Top Processes ──
            if !ss.top_procs.is_empty() {
                ui.add_space(4.0);
                ui.separator();
                ui.label(egui::RichText::new("Top Processes").size(11.0).strong().color(value_color));

                // Sort processes
                let mut sorted_procs: Vec<&ProcessSnapshot> = ss.top_procs.iter().collect();
                match state.proc_sort_col {
                    1 => {
                        // Sort by memory
                        if state.proc_sort_asc {
                            sorted_procs.sort_by(|a, b| a.mem_kb.cmp(&b.mem_kb));
                        } else {
                            sorted_procs.sort_by(|a, b| b.mem_kb.cmp(&a.mem_kb));
                        }
                    }
                    _ => {
                        // Sort by CPU (default)
                        if state.proc_sort_asc {
                            sorted_procs.sort_by(|a, b| a.cpu_pct.partial_cmp(&b.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
                        } else {
                            sorted_procs.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
                        }
                    }
                }

                let arrow = if state.proc_sort_asc { " ▲" } else { " ▼" };
                egui::Grid::new("proc_grid")
                    .num_columns(3)
                    .spacing([6.0, 1.0])
                    .striped(true)
                    .show(ui, |ui| {
                        // Clickable column headers
                        let mem_label = if state.proc_sort_col == 1 { format!("Mem{arrow}") } else { "Mem".to_string() };
                        let cpu_label = if state.proc_sort_col == 0 { format!("CPU{arrow}") } else { "CPU".to_string() };
                        if ui.add(egui::Label::new(egui::RichText::new(mem_label).small().strong().color(label_color)).sense(egui::Sense::click())).clicked() {
                            if state.proc_sort_col == 1 {
                                state.proc_sort_asc = !state.proc_sort_asc;
                            } else {
                                state.proc_sort_col = 1;
                                state.proc_sort_asc = false;
                            }
                        }
                        if ui.add(egui::Label::new(egui::RichText::new(cpu_label).small().strong().color(label_color)).sense(egui::Sense::click())).clicked() {
                            if state.proc_sort_col == 0 {
                                state.proc_sort_asc = !state.proc_sort_asc;
                            } else {
                                state.proc_sort_col = 0;
                                state.proc_sort_asc = false;
                            }
                        }
                        ui.label(egui::RichText::new("Command").small().strong().color(label_color));
                        ui.end_row();
                        for p in &sorted_procs {
                            ui.label(egui::RichText::new(&p.mem_str).small().color(value_color));
                            let cpu_color = if p.cpu_pct > 50.0 { red } else if p.cpu_pct > 10.0 { yellow } else { green };
                            ui.label(egui::RichText::new(format!("{:.1}", p.cpu_pct)).small().color(cpu_color));
                            ui.label(egui::RichText::new(&p.name).small().color(label_color));
                            ui.end_row();
                        }
                    });
            }
            }); // ScrollArea
        });
}

// ---------------------------------------------------------------------------
// Agent panel — right sidebar with chat UI
// ---------------------------------------------------------------------------

const AGENT_PANEL_WIDTH: f32 = 340.0;

fn draw_agent_panel(
    ctx: &egui::Context,
    state: &mut GuiState,
    actions: &mut Vec<GuiAction>,
) {
    egui::SidePanel::right("agent_panel")
        .resizable(true)
        .default_width(AGENT_PANEL_WIDTH)
        .min_width(260.0)
        .max_width(600.0)
        .show(ctx, |ui| {
            // ── Header ──
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("AI Agent").size(14.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").clicked() {
                        actions.push(GuiAction::ToggleAgentPanel);
                    }
                    if ui.small_button("↺").on_hover_text("重置对话").clicked() {
                        actions.push(GuiAction::AgentReset);
                    }
                });
            });
            ui.separator();

            // ── Config section (collapsible) ──
            egui::CollapsingHeader::new(egui::RichText::new("配置").size(11.0))
                .default_open(state.agent_api_key.is_empty())
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Provider").size(11.0));
                        egui::ComboBox::from_id_salt("agent_provider")
                            .width(100.0)
                            .selected_text(&state.agent_provider_type)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut state.agent_provider_type, "openai".into(), "OpenAI");
                                ui.selectable_value(&mut state.agent_provider_type, "anthropic".into(), "Anthropic");
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Base URL").size(11.0));
                        ui.add(egui::TextEdit::singleline(&mut state.agent_base_url)
                            .desired_width(180.0)
                            .font(egui::TextStyle::Small));
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("API Key ").size(11.0));
                        ui.add(egui::TextEdit::singleline(&mut state.agent_api_key)
                            .desired_width(180.0)
                            .password(true)
                            .font(egui::TextStyle::Small));
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Model   ").size(11.0));
                        ui.add(egui::TextEdit::singleline(&mut state.agent_model_id)
                            .desired_width(180.0)
                            .font(egui::TextStyle::Small));
                    });
                    if ui.small_button("应用配置").clicked() {
                        actions.push(GuiAction::AgentConfigure {
                            provider_type: state.agent_provider_type.clone(),
                            base_url: state.agent_base_url.clone(),
                            api_key: state.agent_api_key.clone(),
                            model_id: state.agent_model_id.clone(),
                        });
                    }
                });
            ui.separator();

            // ── Chat messages area ──
            let available = ui.available_height() - 40.0; // reserve space for input
            egui::ScrollArea::vertical()
                .max_height(available.max(100.0))
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());

                    if state.agent_messages.is_empty() && state.agent_streaming_text.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.label(
                                egui::RichText::new("向 AI Agent 提问\n它可以在你的终端中执行命令")
                                    .size(12.0)
                                    .color(egui::Color32::from_gray(120)),
                            );
                        });
                    }

                    for msg in &state.agent_messages {
                        draw_agent_message(ui, msg);
                    }

                    // Show streaming text if agent is running
                    if !state.agent_streaming_text.is_empty() {
                        draw_agent_message(ui, &AgentChatMessage {
                            role: AgentChatRole::Assistant,
                            content: state.agent_streaming_text.clone(),
                        });
                    }

                    if state.agent_is_running && state.agent_streaming_text.is_empty() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(egui::RichText::new("思考中...").size(11.0).italics());
                        });
                    }
                });

            // ── Input bar ──
            ui.separator();
            ui.horizontal(|ui| {
                let input_response = ui.add(
                    egui::TextEdit::singleline(&mut state.agent_input)
                        .desired_width(ui.available_width() - 60.0)
                        .hint_text("输入消息...")
                        .font(egui::TextStyle::Body),
                );
                let enter_pressed = input_response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let send_clicked = if state.agent_is_running {
                    ui.add_enabled(true, egui::Button::new("取消"))
                        .clicked()
                } else {
                    ui.add_enabled(
                        !state.agent_input.trim().is_empty(),
                        egui::Button::new("发送"),
                    )
                    .clicked()
                };

                if state.agent_is_running && send_clicked {
                    actions.push(GuiAction::AgentCancel);
                } else if !state.agent_is_running
                    && (send_clicked || enter_pressed)
                    && !state.agent_input.trim().is_empty()
                {
                    let msg = state.agent_input.trim().to_string();
                    state.agent_messages.push(AgentChatMessage {
                        role: AgentChatRole::User,
                        content: msg.clone(),
                    });
                    state.agent_input.clear();
                    state.agent_is_running = true;
                    actions.push(GuiAction::AgentSendMessage(msg));
                    // Re-focus input
                    input_response.request_focus();
                }
            });
        });
}

fn draw_agent_message(ui: &mut egui::Ui, msg: &AgentChatMessage) {
    let (prefix, color, bg) = match msg.role {
        AgentChatRole::User => (
            "You",
            egui::Color32::from_rgb(100, 180, 255),
            egui::Color32::from_rgba_premultiplied(30, 60, 100, 40),
        ),
        AgentChatRole::Assistant => (
            "AI",
            egui::Color32::from_rgb(130, 220, 130),
            egui::Color32::from_rgba_premultiplied(30, 80, 40, 40),
        ),
        AgentChatRole::ToolCall => (
            "⚙",
            egui::Color32::from_rgb(200, 180, 100),
            egui::Color32::from_rgba_premultiplied(60, 50, 20, 40),
        ),
        AgentChatRole::Error => (
            "✗",
            egui::Color32::from_rgb(255, 100, 100),
            egui::Color32::from_rgba_premultiplied(80, 20, 20, 40),
        ),
    };

    egui::Frame::new()
        .fill(bg)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::same(6))
        .outer_margin(egui::Margin::symmetric(0, 2))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(egui::RichText::new(prefix).size(10.0).strong().color(color));
            ui.label(egui::RichText::new(&msg.content).size(12.0));
        });
}
