//! Block-based UI: Warp-style command/output block decomposition.

use uuid::Uuid;

/// A single block: represents one command execution and its output.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: Uuid,
    /// The command string entered by the user.
    pub command: String,
    /// The captured output text.
    pub output: String,
    /// Exit code of the command (if known).
    pub exit_code: Option<i32>,
    /// Timestamp of execution.
    pub timestamp: u64,
    /// Whether the block is collapsed in the UI.
    pub collapsed: bool,
}

impl Block {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            command: command.into(),
            output: String::new(),
            exit_code: None,
            timestamp: 0,
            collapsed: false,
        }
    }

    /// Append output data to this block.
    pub fn append_output(&mut self, data: &str) {
        self.output.push_str(data);
    }

    /// Finalize the block with an exit code.
    pub fn finalize(&mut self, exit_code: i32) {
        self.exit_code = Some(exit_code);
    }
}

/// Manages the list of blocks for a pane.
#[derive(Debug, Default)]
pub struct BlockManager {
    pub blocks: Vec<Block>,
    /// The currently active (in-progress) block, if any.
    pub active: Option<Block>,
}

impl BlockManager {
    pub fn start_block(&mut self, command: impl Into<String>) {
        if let Some(prev) = self.active.take() {
            self.blocks.push(prev);
        }
        self.active = Some(Block::new(command));
    }

    pub fn append_output(&mut self, data: &str) {
        if let Some(block) = &mut self.active {
            block.append_output(data);
        }
    }

    pub fn finish_block(&mut self, exit_code: i32) {
        if let Some(mut block) = self.active.take() {
            block.finalize(exit_code);
            self.blocks.push(block);
        }
    }
}
