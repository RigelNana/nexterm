# NexTerm

**AI-Native GPU-Accelerated Terminal with SSH & SFTP**

> Warp's AI interaction × Xshell's SSH depth × Zilla's file sync — in one Rust binary.

## Features (Planned)

- **GPU-accelerated rendering** via `wgpu` — 60FPS+ with ligatures and shader effects
- **Block-based UI** — Warp-style command/output decomposition
- **SSH session manager** — tree-based groups, tags, thousands of servers
- **SSH tunneling** — local/remote/dynamic forwarding, ProxyJump, X11
- **SFTP file browser** — integrated sidebar, drag-drop, edit-in-place
- **Multi-Exec** — send commands to multiple servers simultaneously
- **AI Agent** — natural language → commands, error diagnosis, workflow automation
- **Cross-platform** — Windows, macOS, Linux

## Project Structure

```
nexterm/
├── app/                    # Main binary entry point
├── crates/
│   ├── nexterm-core/       # Event bus, pane/tab management
│   ├── nexterm-vte/        # VT100/VT520 terminal emulation
│   ├── nexterm-pty/        # Cross-platform PTY (ConPTY/Unix)
│   ├── nexterm-render/     # wgpu GPU rendering engine
│   ├── nexterm-ui/         # UI components (tabs, splits, blocks)
│   ├── nexterm-ssh/        # SSH connections, tunnels, ProxyJump
│   ├── nexterm-sftp/       # SFTP file browsing & transfer
│   ├── nexterm-session/    # Session persistence (SQLite)
│   ├── nexterm-keystore/   # Key management, OS Keychain
│   ├── nexterm-config/     # TOML config with hot-reload
│   ├── nexterm-history/    # Command history (SQLite FTS5)
│   ├── nexterm-theme/      # Theme engine
│   ├── nexterm-sync/       # Cross-device sync (optional)
│   └── nexterm-agent/      # AI Agent bridge → Agenium
└── lib/
    └── agent_engine/       # Agenium AI engine (submodule)
```

## Quick Start

```bash
# Build the project
cargo build -p nexterm

# Run with debug logging
RUST_LOG=info cargo run -p nexterm
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| GPU Rendering | wgpu + cosmic-text + swash |
| Terminal Emulation | vte (Alacritty parser) + portable-pty |
| SSH/SFTP | russh (pure Rust, async) |
| Data Storage | SQLite (rusqlite + FTS5) |
| Config | TOML + notify (hot-reload) |
| AI Engine | Agenium (agent-core, agent-provider, agent-tool, ...) |
| Async Runtime | Tokio |
| Window | winit |

## License

MIT OR Apache-2.0
