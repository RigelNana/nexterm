//! Terminal-specific tools registered with the Agenium agent.
//!
//! These tools allow the AI to:
//! - Execute commands in a terminal pane
//! - Read/write files on local or remote systems
//! - Query SSH session state
//! - Interact with the SFTP file browser

/// Tool names exposed to the AI agent.
pub mod tool_names {
    pub const EXECUTE_COMMAND: &str = "execute_command";
    pub const READ_TERMINAL_OUTPUT: &str = "read_terminal_output";
    pub const SSH_CONNECT: &str = "ssh_connect";
    pub const SSH_EXEC: &str = "ssh_exec";
    pub const SFTP_LIST: &str = "sftp_list";
    pub const SFTP_READ: &str = "sftp_read_file";
    pub const SFTP_WRITE: &str = "sftp_write_file";
}

// TODO: implement each tool as an `agent_tool::Tool` trait impl
// Example skeleton:
//
// pub struct ExecuteCommandTool { ... }
//
// #[async_trait]
// impl Tool for ExecuteCommandTool {
//     fn definition(&self) -> ToolDefinition { ... }
//     async fn execute(&self, params: Value) -> Result<ToolResult> { ... }
// }
