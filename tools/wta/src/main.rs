#[macro_use]
extern crate rust_i18n;

mod agent_check;
mod agent_hooks_installer;
mod agent_pane_origin;
mod agent_registry;
mod agent_sessions;
mod agent_source;
mod app;
mod clipboard_image;
mod command_recall;
mod commands;
mod coordinator;
mod cwd_util;
mod event;
mod helper;
mod history_loader;
#[cfg(test)]
#[path = "locale_parity_tests.rs"]
mod locale_parity_tests;
mod logging;
mod master;
mod osc52;
mod pane_context;
mod protocol;
mod resolve_command;
mod rtl;
mod session_history;
mod session_mgmt;
mod session_registry;
mod session_watcher;
mod shell;
mod team;
mod telemetry;
#[cfg(test)]
mod test_support;
mod theme;
mod ui;
mod ui_trace;
mod win32;
mod workspace;
mod wsl;
mod wsl_acp;

use agent_client_protocol as acp;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    cursor::{SetCursorStyle, Show},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use shell::wt_channel::{CliChannel, WtChannel};
use shell::ShellManager;
use wta::{compute, runtime_paths};

i18n!("locales", fallback = "en-US");

/// Normalize a detected OS locale to the closest available locale file.
/// Mimics Windows MRT behavior with script-aware affinity matching.
///
/// Examples:
///   - `de-AT` → `de-DE` (only one German variant available)
///   - `zh-HK` → `zh-TW` (Traditional Chinese affinity)
///   - `zh-SG` → `zh-CN` (Simplified Chinese affinity)
///   - `pt-MZ` → `pt-PT` (European Portuguese affinity)
///   - `fr-BE` → `fr-FR` (only one French variant available)
///   - `en-US` → `en-US` (exact match)
fn normalize_locale(locale: &str) -> String {
    let available = rust_i18n::available_locales!();

    // 1. Exact match (case-insensitive)
    if available.iter().any(|l| l.eq_ignore_ascii_case(locale)) {
        return locale.to_string();
    }

    // 2. Script/region affinity for languages with multiple variants.
    //    Aligns with Windows MRT language-distance behavior for our locale set.
    let affinity_target = match locale.to_lowercase().as_str() {
        // Chinese: script-based split
        "zh-hk" | "zh-mo" | "zh-hant" | "zh-hant-tw" | "zh-hant-hk" | "zh-hant-mo" => Some("zh-TW"),
        "zh-sg" | "zh-hans" | "zh-hans-cn" | "zh-hans-sg" => Some("zh-CN"),
        // English: Commonwealth regions → en-GB
        "en-au" | "en-nz" | "en-ie" | "en-in" | "en-sg" | "en-za" | "en-hk" | "en-my" | "en-ph"
        | "en-pk" | "en-ng" | "en-ke" | "en-gh" => Some("en-GB"),
        // Spanish: Latin American regions → es-MX
        "es-ar" | "es-co" | "es-cl" | "es-pe" | "es-ve" | "es-ec" | "es-gt" | "es-cu" | "es-bo"
        | "es-do" | "es-hn" | "es-py" | "es-sv" | "es-ni" | "es-cr" | "es-pa" | "es-uy"
        | "es-pr" | "es-us" | "es-419" => Some("es-MX"),
        // French: non-Canadian → fr-FR
        "fr-be" | "fr-ch" | "fr-lu" | "fr-mc" | "fr-sn" | "fr-ci" | "fr-ml" | "fr-cm" | "fr-mg"
        | "fr-cd" | "fr-dz" | "fr-tn" | "fr-ma" => Some("fr-FR"),
        // Portuguese: non-Brazilian → pt-PT
        "pt-ao" | "pt-mz" | "pt-gw" | "pt-tl" | "pt-cv" | "pt-st" => Some("pt-PT"),
        // Serbian: script-based split
        "sr-latn-ba" | "sr-latn-me" | "sr-latn-xk" => Some("sr-Latn-RS"),
        "sr-cyrl-ba" | "sr-cyrl-me" | "sr-cyrl-xk" => Some("sr-Cyrl-RS"),
        _ => None,
    };

    if let Some(target) = affinity_target {
        if available.iter().any(|l| l.eq_ignore_ascii_case(target)) {
            return target.to_string();
        }
    }

    // 3. Fallback: strip territory, find any locale with same language prefix.
    //    Safe for languages where we only have one regional variant (de, fr, ja, etc.)
    if let Some(lang) = locale.split('-').next() {
        let prefix = format!("{}-", lang.to_lowercase());
        if let Some(found) = available
            .iter()
            .find(|l| l.to_lowercase().starts_with(&prefix))
        {
            return found.to_string();
        }
    }

    "en-US".to_string()
}

// ─── CLI Definition ─────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "wta",
    version,
    about = "Windows Terminal Agent — ACP TUI client / tmux-like CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Initial prompt to send to the agent (ACP mode only)
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Agent CLI command (e.g. "copilot --acp --stdio")
    #[arg(long, default_value = agent_registry::DEFAULT_ACP_COMMAND)]
    agent: String,

    /// Canonical agent identifier (`copilot` / `claude` / `codex` / `gemini`
    /// / `opencode` / `custom:<name>`). When the host (Windows Terminal) launches wta it
    /// already knows which entry the user picked in settings, so it passes
    /// the original `acpAgent` value through here. wta uses this id as the
    /// authoritative identity for `current_agent_id` — driving the session-
    /// management view's CLI filter, the preflight check, etc.
    ///
    /// When omitted (manual `wta` runs, older host builds, tests) wta falls
    /// back to inferring the id by parsing the `--agent` command line via
    /// `agent_registry::resolve_agent_id_from_cmd`. That fallback works for
    /// bare names but is fragile for adapter-style launches (`npx … claude-
    /// code-acp`) and full-path launches, so the host should always pass
    /// `--agent-id` explicitly.
    #[arg(long)]
    agent_id: Option<String>,

    /// Per-tab ACP execution source (`host`, `wsl`, or managed `ssh`). Hidden because
    /// TerminalPage owns source compatibility checks.
    #[arg(long, hide = true, value_parser = ["host", "wsl", "ssh"])]
    agent_source: Option<String>,

    /// WSL distro paired with `--agent-source wsl`.
    #[arg(long, hide = true)]
    agent_wsl_distro: Option<String>,

    /// Canonical ComputeTarget id paired with `--agent-source ssh`.
    #[arg(long, hide = true)]
    agent_ssh_target: Option<String>,

    /// Stable wta-node runtime paired with `--agent-source ssh`.
    #[arg(long, hide = true)]
    agent_remote_session: Option<String>,

    /// Working-pane cwd captured when this helper was created.
    #[arg(long, hide = true)]
    agent_source_cwd: Option<String>,

    /// Master-only allowlist of agent ids a helper may request over the
    /// pipe (the GPO-filtered set; built by TerminalPage::
    /// _BuildSharedWtaExtraArgs from `FilteredAcpAgents()`). The master
    /// reconstructs a helper's requested agent command from its declared
    /// `agent_id` ONLY when that id is in this set — never executing a
    /// command string sent over the pipe. An id outside the set (or a
    /// custom/unknown id) falls back to `--agent` / `--agent-id`. An *absent*
    /// flag means "no host allowlist" (manual runs, older hosts): the master
    /// accepts any *known* agent id. A *present* flag is honored fail-closed —
    /// even when it filters down to nothing, every helper-selected id is then
    /// blocked (all panes fall back to the default) rather than widening back
    /// to accept-any. Helpers use the same list only to filter `/agent`;
    /// the master remains the authoritative enforcement point.
    #[arg(long, hide = true, value_name = "IDS", value_delimiter = ',')]
    allowed_agent_ids: Vec<String>,

    /// Boot-time hint from Windows Terminal: start directly on the auth screen
    /// for the given agent instead of attempting the initial ACP session. Used
    /// when FRE just installed Copilot, where the next expected action is
    /// signing in. Hidden — only Windows Terminal should pass it.
    #[arg(long, hide = true, value_name = "AGENT_ID")]
    initial_auth_agent: Option<String>,

    /// Model override for the ACP agent. Sent via ACP setSessionModel after
    /// handshake. Used by adapter-style launches (claude, codex via npx)
    /// where the model can't be passed on the command line; native ACP
    /// agents may use their own --model flag in `agent`.
    #[arg(long)]
    acp_model: Option<String>,

    /// Delegate agent CLI command (e.g. "codex")
    #[arg(long)]
    delegate_agent: Option<String>,

    /// Model override for the delegate agent
    #[arg(long)]
    delegate_model: Option<String>,

    /// Disable auto-fix on command failure
    #[arg(long)]
    no_autofix: bool,

    /// Host-enforced policy for agent reads (auto, prompt, or deny).
    #[arg(long, hide = true, default_value = "prompt", value_parser = ["auto", "prompt", "deny"])]
    confirmation_read_ops: String,

    /// Host-enforced policy for agent process/terminal creation.
    #[arg(long, hide = true, default_value = "prompt", value_parser = ["auto", "prompt", "deny"])]
    confirmation_create_ops: String,

    /// Host-enforced policy for agent input/mutation operations.
    #[arg(long, hide = true, default_value = "prompt", value_parser = ["auto", "prompt", "deny"])]
    confirmation_input_ops: String,

    /// Enter diagnostic setup mode with the given reason instead of connecting directly.
    /// Values: agent-missing, agent-error
    #[arg(long)]
    setup: Option<String>,

    /// Initial TUI view to show on startup. `chat` (default) starts in the
    /// chat view; `sessions` starts in the Agents (session list) view —
    /// equivalent to the user pressing Ctrl+Shift+/ right after the pane opens.
    /// Wired to WT's Ctrl+Shift+/ binding via TerminalPage.
    #[arg(long, value_enum, default_value_t = InitialView::Chat)]
    initial_view: InitialView,

    /// UI language override, passed by Windows Terminal from the
    /// `settings.json` `Language` field. When present, wta uses this
    /// directly for i18n instead of detecting the OS locale — ensuring
    /// the agent pane displays the same language as the Terminal chrome.
    /// When absent, wta falls back to `sys_locale` (automatic detection).
    #[arg(long)]
    language: Option<String>,

    /// Stable GUID of the WT tab that owns this wta process. Passed in by
    /// TerminalPage when spawning the agent pane (both _OpenOrReuseAgentPane
    /// and _AutoCreateHiddenAgentPane). Seeded into app_state.tab_id before
    /// ACP init, so the first AgentConnected binds the session under the
    /// real tab GUID instead of falling back to the implicit DEFAULT_TAB_ID
    /// placeholder. Hidden because nothing outside WT should be setting it.
    #[arg(long, hide = true)]
    owner_tab_id: Option<String>,

    /// Window ID of the WT window that owns this helper. Passed alongside
    /// `--owner-tab-id` because PID-based pane discovery is best-effort and
    /// may not find a newly spawned ConPTY helper before `/agent` is used.
    #[arg(long, hide = true)]
    owner_window_id: Option<String>,

    /// Boot-time hint: instead of letting the helper create a fresh ACP
    /// session via `session/new`, immediately resume the given session id
    /// via `session/load`. Used by the "Enter on Historical/Ended row in
    /// session manager" path: C++ spawns a new helper for the new
    /// agent pane and bundles the resume request via these flags so the
    /// resume is atomic — no separate `load_session` VT broadcast that
    /// could race the helper's pipe-attach.
    ///
    /// Pair with `--initial-load-cwd`. Hidden — only Windows Terminal
    /// should pass it. No-op outside `--connect-master` (only the helper
    /// boot path consumes it).
    #[arg(long, hide = true, value_name = "SESSION_ID")]
    initial_load_session_id: Option<String>,

    /// Working directory associated with `--initial-load-session-id`.
    /// Passed to the agent CLI via the ACP `session/load` request so the
    /// resumed conversation runs against the right repo root. Hidden.
    #[arg(long, hide = true, value_name = "PATH")]
    initial_load_cwd: Option<String>,

    /// Pre-warm mode: the helper is being spawned for a tab whose agent
    /// pane is *already stashed* on the C++ side (see TerminalPage::
    /// _AutoCreateHiddenAgentPaneShared autoStash path). Without this
    /// flag, the helper's `--owner-tab-id` startup branch seeds
    /// `tab.pane_open = true` and echoes back `agent_state_changed
    /// { pane_open: true }`, which C++ interprets as "user opened the
    /// pane" and unstashes it — defeating pre-warm. With this flag the
    /// helper seeds `tab.pane_open = false`, matching the C++ stash
    /// state. Hidden because only WT's pre-warm path should set it.
    #[arg(long, hide = true)]
    start_stashed: bool,

    /// Degraded-open mode: the helper is being spawned for a pane the user
    /// opened *while wta-master is known to be down* (it died unexpectedly and
    /// hasn't been recovered via /restart — see C++ `SharedWta::IsDegraded`).
    /// Rather than the helper retrying the dead master pipe for ~75s and
    /// showing a spinner, it comes up immediately in the disconnected state
    /// (the same transport-lost view an orphaned pane shows), so the user can
    /// /restart right there instead of hunting for another pane. Hidden — only
    /// WT's degraded-open path should set it.
    #[arg(long, hide = true)]
    assume_master_down: bool,

    // Legacy flags (hidden, backward compat)
    #[arg(long, hide = true)]
    info: bool,
    #[arg(long, hide = true)]
    test_pipe: bool,

    /// Output raw JSON instead of human-readable format
    #[arg(long, global = true)]
    json: bool,

    /// Run as the wta-master singleton (Z architecture). Listens on
    /// the named pipe whose name is passed here for wta-helper
    /// connections; owns the single ACP connection to the agent CLI
    /// subprocess; multiplexes per-helper ACP sessions onto it. Used
    /// by `SharedWta::AcquirePane` on the C++ side. Hidden — only
    /// Windows Terminal should spawn it.
    ///
    /// Pipe name is typically `\\.\pipe\wta-master-<GUID>`.
    #[arg(long, hide = true, value_name = "PIPE_NAME")]
    master: Option<String>,

    /// Connect to a wta-master singleton over the named pipe whose
    /// path is passed here, rather than spawning our own agent CLI
    /// subprocess. Used when this wta is acting as a per-pane helper
    /// in the helper+master architecture (see
    /// doc/specs/Multi-window-agent-pane.md). Hidden — only the C++
    /// side should pass it.
    ///
    /// Logically mutually exclusive with `--master`: a process can be
    /// either the master or a helper, never both. Enforced by clap so
    /// a misconfigured invocation fails fast instead of silently
    /// preferring `--master` (the previous behavior).
    #[arg(long, hide = true, value_name = "PIPE_NAME", conflicts_with = "master")]
    connect_master: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show Windows Terminal protocol connection info
    Info,

    /// Test protocol connection to Windows Terminal
    TestPipe,

    /// List all Windows Terminal windows
    #[command(alias = "lsw")]
    ListWindows,

    /// List tabs in a window
    #[command(alias = "lst")]
    ListTabs {
        /// Window ID (defaults to first window)
        #[arg(short = 'w', long)]
        window_id: Option<String>,
    },

    /// List panes in a tab
    #[command(alias = "lsp")]
    ListPanes {
        /// Tab ID (defaults to active tab)
        #[arg(short = 't', long)]
        tab_id: Option<String>,

        /// Window ID (used with tab_id)
        #[arg(short = 'w', long)]
        window_id: Option<String>,
    },

    /// Identify a command using the user's PowerShell profile
    ResolveCommand {
        /// Command name to identify (without arguments or a path)
        #[arg(value_parser = resolve_command::parse_non_empty)]
        token: String,

        /// PowerShell executable to use
        #[arg(
            long,
            default_value = "pwsh.exe",
            value_parser = resolve_command::parse_non_empty
        )]
        shell: String,
    },

    /// Create a new tab
    #[command(alias = "neww")]
    NewTab {
        /// Command to run in the new tab
        #[arg(short = 'c', long)]
        command: Option<String>,

        /// Working directory
        #[arg(short = 'd', long)]
        cwd: Option<String>,

        /// Tab title
        #[arg(short = 'n', long)]
        title: Option<String>,
    },

    /// Create a pane-local terminal surface (tab within a pane)
    #[command(alias = "news")]
    NewSurface {
        /// Target surface session ID (defaults to the active surface)
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Command to run in the new surface
        #[arg(short = 'c', long)]
        command: Option<String>,

        /// Terminal profile name or GUID
        #[arg(short = 'p', long)]
        profile: Option<String>,

        /// Working directory
        #[arg(short = 'd', long)]
        cwd: Option<String>,

        /// Create without switching the pane to the new surface
        #[arg(short = 'b', long)]
        background: bool,
    },

    /// Create a managed agent surface bound to a compute target
    #[command(alias = "newas")]
    NewAgentSurface {
        /// Target surface session ID (defaults to the active surface)
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Canonical ComputeTarget ID
        #[arg(long)]
        compute_target: String,

        /// Agent ID (codex, claude, gemini, copilot, or configured custom ID)
        #[arg(short = 'a', long)]
        agent: String,

        /// Create without switching the pane to the new surface
        #[arg(short = 'b', long)]
        background: bool,
    },

    /// Split the current pane
    #[command(alias = "splitw")]
    SplitPane {
        /// Target pane ID
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Split horizontally (panes side by side)
        // `-h` belongs to clap's standard help flag. Keeping horizontal on an
        // uppercase mnemonic makes the complete CLI tree valid in debug and
        // release builds instead of relying on release-only debug assertions.
        #[arg(short = 'H', long)]
        horizontal: bool,

        /// Split vertically (panes stacked)
        #[arg(short = 'v', long)]
        vertical: bool,

        /// Size as fraction (0.0-1.0)
        #[arg(short = 's', long)]
        size: Option<f64>,

        /// Command to run in the new pane
        #[arg(short = 'c', long)]
        command: Option<String>,
    },

    /// Preview or create a balanced multi-pane workspace
    Workspace {
        /// Working directory shared by the workspace panes
        #[arg(short = 'd', long, default_value = ".")]
        cwd: String,

        /// Workspace tab title
        #[arg(short = 'n', long)]
        title: Option<String>,

        /// Command for a pane; repeat once per pane (maximum 4)
        #[arg(long = "pane", required = true, value_name = "COMMAND")]
        panes: Vec<String>,

        /// Apply the plan to the current Intelligent Terminal instance
        #[arg(long)]
        apply: bool,
    },

    /// Declarative, persistent multi-agent workspaces
    #[command(name = "agent-workspace", alias = "aw", alias = "ws")]
    AgentWorkspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },

    /// Native multi-agent task coordination and pane control
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },

    /// Distributed agent and compute control plane
    Compute {
        #[command(subcommand)]
        action: ComputeAction,
    },

    /// Capture pane output (like tmux capture-pane -p)
    #[command(alias = "capturep")]
    CapturePane {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Maximum lines to capture
        #[arg(short = 'l', long)]
        max_lines: Option<u32>,

        /// Only return the most recent completed shell prompt
        /// (command + output). Requires OSC 133 shell integration.
        #[arg(long)]
        last_prompt: bool,
    },

    /// Close/kill a pane
    #[command(alias = "killp")]
    KillPane {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,
    },

    /// Show the currently active pane
    ActivePane,

    /// Show process status of a pane
    PaneStatus {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,
    },

    /// Wait for a pane's process to exit (delegates to `wtcli wait-for`)
    WaitFor {
        /// Target pane ID
        #[arg(short = 't', long)]
        target: String,

        /// Poll interval in milliseconds
        #[arg(long, default_value = "500")]
        interval: u64,

        /// Timeout in seconds (0 = wait forever)
        #[arg(long, default_value = "0")]
        timeout: u64,
    },

    /// Discover and print the WT COM CLSID used for protocol routing
    PipeId,

    /// Print shell commands to set WT_COM_CLSID
    #[command(alias = "setenv")]
    SetEnv {
        /// Shell syntax: bash (default), powershell, cmd
        #[arg(short = 's', long, default_value = "bash")]
        shell: String,
    },

    /// Listen for events from Windows Terminal (VT sequences, connection state changes)
    #[command(alias = "mon")]
    Listen {
        /// Filter by pane ID (show events from all panes if omitted)
        #[arg(short = 't', long)]
        target: Option<String>,
    },

    /// Open a configured delegate agent in a new tab (fire-and-forget). With a
    /// PROMPT, the prompt is baked into the agent's launch; omit PROMPT to open
    /// the agent interactively with no startup prompt.
    Delegate {
        /// The prompt to send to the delegate agent. Omit to open the agent
        /// interactively in a new tab with no startup prompt.
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,

        /// Agent CLI command (used to derive delegate agent commandline)
        #[arg(long, default_value = agent_registry::DEFAULT_ACP_COMMAND)]
        agent: String,

        /// Delegate agent CLI command (e.g. "codex")
        #[arg(long)]
        delegate_agent: Option<String>,

        /// Model override for the delegate agent
        #[arg(long)]
        delegate_model: Option<String>,

        /// Working directory for the delegate agent tab
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Manage the wt-agent-hooks bridge for supported CLI agents
    /// (Copilot / Claude / Gemini). See `agent_hooks_installer` for
    /// what each action does.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },

    /// Inspect sessions known to the shared wta-master.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },

    /// One-shot ACP handshake to read an agent's advertised model list.
    /// Spawned by the Settings UI when the user picks a new ACP agent so
    /// the model dropdown can populate before any real agent pane is
    /// rebuilt. Prints a single JSON object to stdout:
    ///
    ///   {"available_models":[{"id":"...","name":"...","description":"..."}],
    ///    "current_model_id":"..."}
    ///
    /// On error: non-zero exit, message on stderr.
    ProbeModels {
        /// Full agent cmdline, same shape as `--agent` (e.g.
        /// "copilot --acp --stdio" or "npx -y @agentclientprotocol/claude-agent-acp").
        #[arg(long)]
        agent: String,
    },

    /// List built-in ACP agents installed inside one WSL distro.
    /// Used by the per-profile Settings picker.
    #[command(hide = true)]
    ProbeAgentSources {
        #[arg(long)]
        wsl_distro: String,
    },

    /// Diagnostic: spawn an agent CLI, ACP `initialize`, then call
    /// `session/list` (`list_sessions`) and print what it returns.
    /// Used to evaluate whether ACP session enumeration can replace
    /// reading on-disk transcripts. Prints a pretty JSON object to
    /// stdout; on error: non-zero exit, message on stderr.
    ProbeSessions {
        /// Full agent cmdline, same shape as `--agent` (e.g.
        /// "copilot --acp --stdio" or "npx -y @agentclientprotocol/claude-agent-acp").
        #[arg(long)]
        agent: String,
    },

    /// Diagnostic: spawn an agent CLI, call ACP `session/list`, filter
    /// agent-pane-origin rows, and print the host history rows WTA would
    /// seed from the already-running master agent.
    ProbeHostSessions {
        /// Full agent cmdline, same shape as `--agent` (e.g.
        /// "copilot --acp --stdio" or "npx -y @agentclientprotocol/claude-agent-acp").
        #[arg(long)]
        agent: String,
    },

    /// Diagnostic: run the production WSL history scan
    /// (`wsl_acp::scan_running_distros_acp`) end-to-end against the
    /// currently-running distros and print the discovered sessions as
    /// JSON. Exercises the real `wsl.exe` spawn + ACP `session/list` path
    /// that seeds the `/sessions` view. Prints `[]` when no distro is
    /// running or none answer.
    ProbeWslSessions {
        /// Restrict to one CLI (`copilot` | `claude` | `codex`). Omitted
        /// scans the three ACP-capable built-ins (Gemini has no
        /// `session/list`).
        #[arg(long)]
        cli: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum WorkspaceAction {
    /// Compile a .agent-workspace.yaml without changing Terminal
    Plan {
        #[arg(default_value = ".agent-workspace.yaml")]
        manifest: PathBuf,
    },
    /// Create panes/worktrees and persist the runtime state
    Apply {
        #[arg(default_value = ".agent-workspace.yaml")]
        manifest: PathBuf,
    },
    /// Alias of apply, matching cmux-style workspace vocabulary
    Open {
        #[arg(default_value = ".agent-workspace.yaml")]
        manifest: PathBuf,
    },
    /// Recreate a workspace from its persisted manifest
    Restore {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        snapshot: Option<PathBuf>,
    },
    /// Write one of the built-in manifest templates
    Template {
        template: String,
        #[arg(short, long, default_value = "agent-workspace")]
        name: String,
        #[arg(short, long, default_value = ".agent-workspace.yaml")]
        output: PathBuf,
        #[arg(long)]
        force: bool,
        /// Print the manifest without writing a file
        #[arg(long)]
        stdout: bool,
    },
    /// List all persisted declarative workspaces rooted at a project
    List {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    /// Print persisted workspace/pane state
    Tree {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
    },
    /// Read the append-only timeline or one agent inbox
    Read {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        recipient: Option<String>,
        #[arg(long)]
        after_ms: Option<u64>,
    },
    /// Append a structured notification/message to the workspace
    Notify {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value = "message")]
        kind: String,
        #[arg(long, default_value = "{}")]
        payload: String,
    },
    /// Forward an existing event with correlation and hop-limit enforcement
    Forward {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        event_id: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        target: Option<String>,
    },
    /// Show event, sender, and unread-inbox metrics
    Status {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        recipient: Option<String>,
        #[arg(long)]
        after_ms: Option<u64>,
    },
    /// Collect sidebar metadata for an ad-hoc or declarative workspace
    Context {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        /// Prefer the persisted declarative workspace attached to this tab
        #[arg(long)]
        tab_id: Option<String>,
    },
    /// Collect a bounded, read-only Git status and diff
    InspectGit {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "262144")]
        max_bytes: usize,
    },
    /// Diagnose the native workspace control plane without changing state
    Doctor {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
    /// Send literal input to a logical workspace pane
    Send {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        target: String,
        text: String,
    },
    /// Focus a logical workspace pane
    Focus {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        target: String,
    },
    /// Read recent output without changing focus
    Peek {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        target: String,
        #[arg(short = 'l', long, default_value = "80")]
        max_lines: u32,
    },
    /// Wait for a logical workspace pane process to finish
    Wait {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        target: String,
        #[arg(long, default_value = "500")]
        interval: u64,
        #[arg(long, default_value = "0")]
        timeout: u64,
    },
    /// Close every live pane recorded for the workspace
    Close {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
    },
    /// Run the manifest's deterministic verification oracle
    Verify {
        #[arg(default_value = ".agent-workspace.yaml")]
        manifest: PathBuf,
    },
    /// Save a point-in-time copy of the runtime pane map
    Snapshot {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum TeamAction {
    /// Create a durable native team
    Create {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long, default_value = "leader")]
        leader: String,
        /// Stable native workspace/tab id this team belongs to
        #[arg(long)]
        workspace_id: Option<String>,
        #[arg(long, default_value = "120000")]
        stale_after_ms: u64,
        #[arg(long, default_value = "2")]
        max_attempts: u32,
    },
    /// Register a worker and, by default, launch its agent in a terminal pane
    AddWorker {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(long)]
        role: String,
        /// Delegate CLI command, such as codex, gemini or copilot
        #[arg(long, default_value = "codex")]
        agent: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        /// Only register the worker; do not create a terminal pane
        #[arg(long)]
        no_launch: bool,
        /// Split this pane instead of creating a new tab
        #[arg(long)]
        split_target: Option<String>,
        #[arg(long, default_value = "automatic")]
        direction: String,
    },
    /// Add a task with dependencies, retry policy, and exclusive paths
    AddTask {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        prompt: String,
        #[arg(long = "depends-on")]
        dependencies: Vec<String>,
        #[arg(long = "owns")]
        owns: Vec<String>,
        #[arg(long)]
        max_attempts: Option<u32>,
        #[arg(long, default_value = "leader")]
        actor: String,
    },
    /// Reserve a pending task for one worker without sending it
    Assign {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: String,
        #[arg(long, default_value = "leader")]
        actor: String,
    },
    /// Assign a task and send its complete prompt to the worker pane
    Dispatch {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: String,
        #[arg(long, default_value = "leader")]
        actor: String,
    },
    /// Atomically claim the requested or next runnable task
    Claim {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: Option<String>,
    },
    /// Move an assigned task into running state
    Start {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: String,
    },
    /// Refresh a worker lease, optionally asserting its current task
    Heartbeat {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: Option<String>,
    },
    /// Record a successful result and release ownership
    Complete {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: String,
        #[arg(long)]
        result: String,
    },
    /// Record a failure and release ownership
    Fail {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short, long)]
        task: String,
        #[arg(long)]
        error: String,
    },
    /// Return a failed task to pending when attempts remain
    Retry {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        task: String,
        #[arg(long, default_value = "leader")]
        actor: String,
    },
    /// Cancel a non-terminal task
    Cancel {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        task: String,
        #[arg(long)]
        reason: String,
        #[arg(long, default_value = "leader")]
        actor: String,
    },
    /// Print the complete team state
    Status {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        /// Refresh stale leases before printing
        #[arg(long)]
        reconcile: bool,
    },
    /// Print the append-only audit timeline
    Events {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long)]
        after_ms: Option<u64>,
    },
    /// Send literal text to a worker's terminal pane
    Send {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        text: String,
        /// Append Enter after the text
        #[arg(long)]
        enter: bool,
    },
    /// Focus a worker's pane
    Focus {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
    },
    /// Read recent output from a worker without focusing it
    Peek {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        worker: String,
        #[arg(short = 'l', long, default_value = "80")]
        max_lines: u32,
    },
    /// Mark workers with expired heartbeat leases as stale
    Reconcile {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long, default_value = "leader")]
        actor: String,
    },
    /// Request graceful shutdown or force a terminal state
    Shutdown {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
        #[arg(long, default_value = "leader")]
        actor: String,
        #[arg(long)]
        force: bool,
        /// Also close all recorded worker panes
        #[arg(long)]
        close_panes: bool,
    },
    /// Check schema, leases, dependency readiness and ownership conflicts
    Doctor {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(short, long)]
        name: String,
    },
    /// Launch a real two-agent smoke test in two terminal panes
    E2e {
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "codex")]
        agent: String,
        /// Optional different CLI for the second worker
        #[arg(long)]
        agent_two: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        model_two: Option<String>,
        /// Agent process cwd; the team state remains under --root
        #[arg(long)]
        worker_cwd: Option<PathBuf>,
        /// Wait for both real agents to report success; 0 returns after dispatch
        #[arg(long, default_value = "180")]
        wait_seconds: u64,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeAction {
    /// Discover and manage execution targets
    Target {
        #[command(subcommand)]
        action: ComputeTargetAction,
    },
    /// Inspect stable runtime environments hosted by compute targets.
    Environment {
        #[command(subcommand)]
        action: ComputeEnvironmentAction,
    },
    /// Inspect policy-only access paths; public endpoint kinds remain disabled.
    Endpoint {
        #[command(subcommand)]
        action: ComputeEndpointAction,
    },
    /// Inspect or explicitly reset the one connection supervisor per environment.
    Connection {
        #[command(subcommand)]
        action: ComputeConnectionAction,
    },
    /// Bind native terminal surfaces to plain terminals or managed agents
    Binding {
        #[command(subcommand)]
        action: ComputeBindingAction,
    },
    /// Manage workspace placement, trust and secret policies
    Policy {
        #[command(subcommand)]
        action: ComputePolicyAction,
    },
    /// Manage the lifecycle descriptor for an SSH-backed native workspace
    RemoteWorkspace {
        #[command(subcommand)]
        action: ComputeRemoteWorkspaceAction,
    },
    /// Attach, detach, resume or stop the managed session for a binding
    Session {
        #[command(subcommand)]
        action: ComputeSessionAction,
    },
    /// Preview, explain, pin or unpin sticky placement
    Place {
        #[command(subcommand)]
        action: ComputePlaceAction,
    },
    /// Run an explicit build/test/lint job; normal PTY commands stay local
    Exec {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        workspace: String,
        #[arg(long, default_value = "build")]
        class: String,
        #[arg(long, default_value = "auto")]
        target: String,
        #[arg(long, default_value = ".")]
        cwd: String,
        #[arg(long, default_value = "900000")]
        timeout_ms: u64,
        #[arg(long, default_value = "user")]
        requested_by: String,
        #[arg(long)]
        idempotent: bool,
        #[arg(long)]
        destructive: bool,
        #[arg(long = "env")]
        environment_allowlist: Vec<String>,
        #[arg(long = "output")]
        declared_outputs: Vec<String>,
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Inspect and control routed jobs
    Job {
        #[command(subcommand)]
        action: ComputeJobAction,
    },
    /// Create, inspect, verify and materialize immutable snapshots
    Snapshot {
        #[command(subcommand)]
        action: ComputeSnapshotAction,
    },
    /// Upload files through SSH with node-side hash verification
    Transfer {
        #[command(subcommand)]
        action: ComputeTransferAction,
    },
    /// Route browser HTTP/HTTPS/WebSocket traffic through one SSH workspace.
    Proxy {
        #[command(subcommand)]
        action: ComputeProxyAction,
    },
    /// Manage isolated native browser surfaces backed by an SSH proxy.
    Browser {
        #[command(subcommand)]
        action: ComputeBrowserAction,
    },
    /// Inspect and mutate files inside an explicitly scoped remote workspace root.
    File {
        #[command(subcommand)]
        action: ComputeFileAction,
    },
    /// Capture and reconcile WTA runtime identities alongside native layout restore.
    Restore {
        #[command(subcommand)]
        action: ComputeRestoreAction,
    },
    /// Preview or apply a transactional HomeTarget handoff
    Handoff {
        #[command(subcommand)]
        action: ComputeHandoffAction,
    },
    /// Inspect or revoke broker leases
    Lease {
        #[command(subcommand)]
        action: ComputeLeaseAction,
    },
    /// Bootstrap and inspect a portable wta-node
    Node {
        #[command(subcommand)]
        action: ComputeNodeAction,
    },
    /// Issue scoped relay capabilities and project remote events into the UI.
    Relay {
        #[command(subcommand)]
        action: ComputeRelayAction,
    },
    /// Diagnose targets/surfaces/agents without exposing credentials.
    Doctor {
        #[command(subcommand)]
        action: ComputeDoctorAction,
    },
    /// Export a deterministic, redacted support bundle.
    Evidence {
        #[command(subcommand)]
        action: ComputeEvidenceAction,
    },
    /// Print the append-only compute audit timeline
    Events {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        subject: Option<String>,
    },
    /// Aggregate target capacity, active bindings, leases and jobs
    Top,
}

#[derive(Subcommand, Debug)]
enum ComputeTargetAction {
    /// Enumerate local, WSL and concrete OpenSSH targets
    Discover {
        /// Persist discovered targets in the canonical store
        #[arg(long)]
        save: bool,
    },
    Add {
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        ssh_alias: Option<String>,
        #[arg(long)]
        wsl_distro: Option<String>,
        #[arg(long)]
        azure_resource_id: Option<String>,
        #[arg(long, default_value = "unknown")]
        os: String,
        #[arg(long, default_value = "unknown")]
        arch: String,
        #[arg(long, default_value = "development")]
        trust: String,
        #[arg(long, default_value = "1")]
        agent_slots: u32,
        #[arg(long, default_value = "1")]
        build_slots: u32,
        #[arg(long, default_value = "0")]
        memory_bytes: u64,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long = "project")]
        project_allowlist: Vec<String>,
        #[arg(long, default_value = "{}")]
        metadata: String,
    },
    Get {
        id: String,
    },
    List,
    /// Replace a target from a versioned JSON document
    Update {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    Remove {
        id: String,
        #[arg(long, default_value = "user")]
        actor: String,
    },
    Probe {
        id: String,
    },
    /// Read the server key and command preview without modifying known_hosts
    PreviewTrust {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Explicitly accept a first-use SSH host key, then probe it
    Trust {
        id: String,
    },
    /// Explicitly start an allowlisted Azure VM target
    Start {
        id: String,
        #[arg(long)]
        allow_production: bool,
    },
    /// Explicitly deallocate an Azure VM after active-use checks
    Deallocate {
        id: String,
        #[arg(long)]
        allow_production: bool,
    },
    /// Read the configured cost/budget policy without mutating Azure
    Cost {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeEnvironmentAction {
    Get {
        id: String,
    },
    List {
        #[arg(long)]
        target: Option<String>,
    },
    /// Reconcile the stable environment from a verified installed wta-node.
    Reconcile {
        #[arg(long)]
        target: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeEndpointAction {
    Get {
        id: String,
    },
    List {
        #[arg(long)]
        environment: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeConnectionAction {
    Get {
        environment: String,
    },
    List,
    /// Select the preferred endpoint and enter connecting/reconnecting state.
    Prepare {
        environment: String,
        #[arg(long, default_value = "ssh_forward")]
        preferred: String,
    },
    /// Explicitly clear retry/block state without opening a transport.
    Reset {
        environment: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeRemoteWorkspaceAction {
    /// Register a native workspace after trust, probe and node bootstrap pass
    Create {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        window: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        target: String,
        /// Confirm first-use host-key trust and enable the selected target
        #[arg(long)]
        accept_host_key: bool,
    },
    Get {
        id: String,
    },
    List,
    /// Mark the workspace reconnecting; the transport supervisor consumes it
    Reconnect {
        id: String,
    },
    Close {
        id: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeBindingAction {
    Create {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        window: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        pane: String,
        #[arg(long)]
        surface: String,
        #[arg(long, default_value = "plain_terminal")]
        kind: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
        /// Stable wta-node runtime identity for a managed remote surface.
        #[arg(long)]
        remote_session: Option<String>,
        #[arg(long, default_value = "0")]
        focus_generation: u64,
    },
    Get {
        id: String,
    },
    List {
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Replace a binding from a versioned JSON document
    Update {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    Delete {
        id: String,
    },
    /// Idempotently remove the binding owned by one terminal surface.
    /// GUID-like identities accept either WinRT's braced lower-case form or
    /// the COM protocol's unbraced upper-case form.
    DeleteSurface {
        #[arg(long)]
        window: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        surface: String,
    },
    /// Persist proof that the exact managed runtime is still alive.
    Heartbeat {
        id: String,
        #[arg(long, default_value = "runtime")]
        actor: String,
    },
    /// Fail stale managed bindings left in `creating` without live evidence.
    Reconcile {
        #[arg(long, default_value = "120000")]
        stale_after_ms: u64,
        #[arg(long, default_value = "binding.reconcile")]
        actor: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputePolicyAction {
    Get {
        workspace: String,
    },
    List,
    /// Import and validate a versioned policy JSON document
    Set {
        #[arg(long)]
        file: PathBuf,
    },
    /// Import the repository-local .intelligent-terminal/compute-policy.json
    Import {
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Export a policy to the repository-local, secret-free policy file
    Export {
        workspace: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        force: bool,
    },
    Delete {
        workspace: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeSessionAction {
    Attach {
        binding: String,
        #[arg(long)]
        remote_session: String,
        #[arg(long)]
        acp_session: Option<String>,
    },
    Detach {
        binding: String,
    },
    Resume {
        binding: String,
    },
    /// Reconcile the local binding with the exact persistent runtime.
    Reconcile {
        binding: String,
    },
    Stop {
        binding: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputePlaceAction {
    Preview {
        #[arg(long)]
        workspace: String,
        #[arg(long, default_value = "interactive_agent")]
        workload: String,
        #[arg(long, default_value = "balanced")]
        policy: String,
        #[arg(long)]
        preferred: Option<String>,
        #[arg(long)]
        os: Option<String>,
        #[arg(long)]
        arch: Option<String>,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long, default_value = "0")]
        minimum_memory_bytes: u64,
        #[arg(long, default_value = "development")]
        required_trust: String,
        #[arg(long)]
        production_targets_allowed: bool,
    },
    Explain {
        #[arg(long)]
        workspace: String,
        #[arg(long, default_value = "interactive_agent")]
        workload: String,
        #[arg(long, default_value = "balanced")]
        policy: String,
    },
    Pin {
        #[arg(long)]
        binding: String,
        #[arg(long)]
        target: String,
    },
    Unpin {
        #[arg(long)]
        binding: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeJobAction {
    Get {
        id: String,
    },
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    Logs {
        id: String,
    },
    Cancel {
        id: String,
    },
    Retry {
        id: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    Artifacts {
        id: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeSnapshotAction {
    Create {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "user")]
        created_by: String,
        #[arg(long = "include-ignored")]
        include_ignored: Vec<String>,
    },
    Inspect {
        id: String,
    },
    Verify {
        id: String,
    },
    List,
    Materialize {
        id: String,
        #[arg(long)]
        destination: PathBuf,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeTransferAction {
    Upload {
        #[arg(long)]
        target: String,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    Download {
        #[arg(long)]
        target: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    Get {
        id: String,
    },
    List,
    /// Request cancellation; the uploader observes the shared marker and
    /// removes the incomplete remote payload.
    Cancel {
        id: String,
    },
    /// Retry a terminal transfer as a new auditable transfer.
    Retry {
        id: String,
    },
    /// Delete one completed/cancelled transfer record.
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeProxyAction {
    /// Start a loopback-only SOCKS5 endpoint through an SSH target.
    Open {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        surface: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        allow_production: bool,
    },
    Get {
        id: String,
    },
    List {
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
    /// Reconcile stale state after an unclean supervisor or client exit.
    Reconcile {
        #[arg(long, default_value = "30000")]
        stale_after_ms: u64,
    },
    /// Stop the exact worker-owned SSH process.
    Close {
        id: String,
    },
    Delete {
        id: String,
    },
    /// Internal detached supervisor. It owns the SSH child and stop marker.
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeBrowserAction {
    /// Allocate an isolated profile and surface-scoped SSH proxy.
    Open {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        remote_workspace: String,
        #[arg(long)]
        surface: String,
        #[arg(long, default_value = "https://example.com")]
        url: String,
        #[arg(long, default_value_t = true)]
        persistent: bool,
        #[arg(long)]
        allow_production: bool,
    },
    Get {
        id: String,
    },
    List {
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        surface: Option<String>,
    },
    Navigate {
        id: String,
        url: String,
    },
    Back {
        id: String,
    },
    Forward {
        id: String,
    },
    /// Native WebView2 host reports successful controller creation.
    Ready {
        id: String,
    },
    /// Native WebView2 host reports a fail-closed renderer error.
    Fail {
        id: String,
        #[arg(long)]
        error: String,
    },
    Reconcile {
        #[arg(long, default_value = "30000")]
        stale_after_ms: u64,
    },
    /// Reuse or restart the exact surface proxy before native controller restore.
    Recover {
        id: String,
        #[arg(long)]
        allow_production: bool,
    },
    Close {
        id: String,
    },
    Delete {
        id: String,
        #[arg(long)]
        delete_profile: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeFileAction {
    Roots {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        include_revoked: bool,
    },
    /// Add an explicit project/worktree/home/admin root to the broker policy.
    Authorize {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        label: String,
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "project")]
        source: String,
        #[arg(long)]
        writable: bool,
        #[arg(long)]
        deletable: bool,
        /// Required for HOME or administrator-configured broad roots.
        #[arg(long)]
        acknowledge_wide_scope: bool,
    },
    /// Revoke a root immediately; later operations using its ID fail closed.
    Revoke { id: String },
    List {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long, default_value = "")]
        path: String,
        #[arg(long, default_value = "0")]
        offset: u64,
        #[arg(long, default_value = "200")]
        limit: u64,
    },
    Stat {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long, default_value = "")]
        path: String,
    },
    Read {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "262144")]
        max_bytes: u64,
    },
    Download {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    Mkdir {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        recursive: bool,
        /// Required acknowledgement for a remote filesystem mutation.
        #[arg(long)]
        allow_destructive: bool,
    },
    Rename {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        destination: String,
        #[arg(long)]
        overwrite: bool,
        /// Required acknowledgement for a remote filesystem mutation.
        #[arg(long)]
        allow_destructive: bool,
    },
    Remove {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long)]
        root: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        recursive: bool,
        /// Required acknowledgement for a remote filesystem mutation.
        #[arg(long)]
        allow_destructive: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeRestoreAction {
    Capture {
        #[arg(long)]
        window: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        focused_surface: Option<String>,
    },
    Get {
        id: String,
    },
    List,
    Latest {
        #[arg(long)]
        window: String,
        #[arg(long)]
        workspace: String,
    },
    Plan {
        id: String,
    },
    Apply {
        id: String,
        #[arg(long)]
        allow_production: bool,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeHandoffAction {
    Preview {
        #[arg(long)]
        binding: String,
        #[arg(long)]
        target: String,
    },
    Apply {
        #[arg(long)]
        binding: String,
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "user")]
        actor: String,
    },
    Rollback {
        #[arg(long)]
        binding: String,
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "user")]
        actor: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeLeaseAction {
    List {
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    Revoke {
        id: String,
        #[arg(long, default_value = "user")]
        actor: String,
        #[arg(long, default_value = "revoked by user")]
        reason: String,
    },
    Heartbeat {
        id: String,
        #[arg(long, default_value = "60000")]
        ttl_ms: u64,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeNodeAction {
    /// Run the line-delimited JSON-RPC bridge on stdin/stdout
    Bridge,
    /// Print this machine's node handshake
    Status,
    Doctor,
    /// Copy this WTA build to an SSH target and verify its SHA-256
    Bootstrap {
        target: String,
    },
    /// Replace the remote helper only when its verified version differs
    Upgrade {
        target: String,
    },
    /// Render the exact persistent-PTY SSH command used by a terminal surface
    PtyCommand {
        target: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        attach: bool,
    },
    /// Read live PID, memory and CPU-time metrics for one persistent PTY.
    PtyStatus {
        target: String,
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeRelayAction {
    /// Issue a short-lived capability on the remote node.
    Issue {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        surface: Option<String>,
        #[arg(long = "operation")]
        operations: Vec<String>,
        #[arg(long, default_value = "300000")]
        ttl_ms: u64,
    },
    /// Revoke a capability before its expiry.
    Revoke {
        #[arg(long)]
        target: String,
        #[arg(long)]
        token: Option<String>,
    },
    /// Pump remote events into the authenticated Terminal Protocol event bus.
    Pump {
        #[arg(long)]
        target: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        surface: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long, default_value = "0")]
        after_sequence: u64,
        #[arg(long, default_value = "750")]
        interval_ms: u64,
        #[arg(long)]
        once: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ComputeDoctorAction {
    Ssh { target: String },
    Surface { binding: String },
    Agent { agent: String },
}

#[derive(Subcommand, Debug)]
enum ComputeEvidenceAction {
    Export {
        #[arg(long)]
        output: PathBuf,
        /// Required acknowledgement: evidence is always redacted.
        #[arg(long)]
        redact: bool,
    },
}

/// Subcommands for `wta sessions`.
#[derive(Subcommand, Debug)]
enum SessionsAction {
    /// List sessions in the master registry.
    List {
        /// Override the wta-master named pipe path.
        #[arg(long, value_name = "PIPE_NAME")]
        master: Option<String>,

        /// Restrict the list to a session origin. `all` (default) shows
        /// every row — that matches the historical debug behavior.
        /// `shell` shows only user-started shell-pane sessions (the
        /// MVP sessions default). `agent-pane` shows only sessions that
        /// WTA spawned for an Intelligent Terminal agent pane.
        #[arg(long, value_enum, default_value_t = SessionsOriginArg::All)]
        origin: SessionsOriginArg,
    },
}

/// CLI value for `wta sessions list --origin`. Mirrors
/// [`agent_sessions::OriginFilter`] but lives in `main.rs` so the
/// clap derive can attach `ValueEnum` without polluting the library
/// crate with clap as a dependency.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum SessionsOriginArg {
    /// Shell-pane sessions only (Class B). Matches the MVP sessions picker.
    Shell,
    /// Agent-pane sessions only (Class A). Hidden from the MVP sessions
    /// picker; surfaced here for debugging.
    AgentPane,
    /// Every row in the registry — historical debug default.
    All,
}

impl SessionsOriginArg {
    fn to_filter(self) -> agent_sessions::OriginFilter {
        match self {
            SessionsOriginArg::Shell => agent_sessions::OriginFilter::ShellOnly,
            SessionsOriginArg::AgentPane => agent_sessions::OriginFilter::AgentPaneOnly,
            SessionsOriginArg::All => agent_sessions::OriginFilter::All,
        }
    }
}

/// Subcommands for `wta hooks`.
#[derive(Subcommand, Debug)]
enum HooksAction {
    /// (Re-)install the wt-agent-hooks bridge. Installs for all supported
    /// CLIs by default, or a single CLI with `--cli`.
    Install {
        /// Which CLI to install for. Default: `all`.
        #[arg(long, value_enum, default_value_t = HooksCliFilter::All)]
        cli: HooksCliFilter,
    },

    /// Print per-CLI install state. Returns JSON with `--json`,
    /// or a human-readable table by default.
    Status,

    /// Uninstall the bridge for one or all CLIs. Best-effort: missing
    /// CLIs are skipped at info level. With `--json` returns a structured
    /// per-CLI result report.
    Uninstall {
        /// Which CLI(s) to uninstall for. Default: `all`.
        #[arg(long, value_enum, default_value_t = HooksCliFilter::All)]
        cli: HooksCliFilter,
    },
}

/// `--cli` filter for `wta hooks uninstall`.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum HooksCliFilter {
    All,
    Copilot,
    Claude,
    Gemini,
    Codex,
    #[value(name = "opencode")]
    OpenCode,
}

impl HooksCliFilter {
    fn into_scope(self) -> agent_hooks_installer::CliScope {
        use agent_hooks_installer::{CliKind, CliScope};
        match self {
            HooksCliFilter::All => CliScope::All,
            HooksCliFilter::Copilot => CliScope::One(CliKind::Copilot),
            HooksCliFilter::Claude => CliScope::One(CliKind::Claude),
            HooksCliFilter::Gemini => CliScope::One(CliKind::Gemini),
            HooksCliFilter::Codex => CliScope::One(CliKind::Codex),
            HooksCliFilter::OpenCode => CliScope::One(CliKind::OpenCode),
        }
    }
}

/// `--initial-view` selector. Drives whether the TUI starts in the chat
/// view (default) or jumps straight to the Agents (session list) view.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum InitialView {
    Chat,
    Sessions,
}

// ─── Entry Point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Detect and set the system locale for i18n.
    // normalize_locale() maps unmatched regions to the canonical variant (e.g., de-AT → de-DE).
    //
    // Priority:
    //   1. --language flag (passed by Windows Terminal from settings.json Language)
    //      — aligns with C++ side's PrimaryLanguageOverride behavior
    //   2. sys_locale (GetUserPreferredUILanguages — automatic OS detection)
    //      — aligns with C++ side's MRT fallback when Language is empty
    let cli = Cli::parse();
    protocol::acp::client::set_operation_policies(
        &cli.confirmation_read_ops,
        &cli.confirmation_create_ops,
        &cli.confirmation_input_ops,
    );

    // Initialize file logging exactly once, as the very first thing after
    // arg parsing, so even early-startup failures (locale, ETW registration,
    // legacy-flag dispatch) are captured. The global tracing subscriber can
    // only be set once per process, so every mode routes through here — the
    // per-mode handlers below no longer init their own. The appender's guard
    // is held in a global and flushed via `logging::shutdown_flush()` on every
    // exit path (see the calls below and before each `process::exit`).
    logging::init(&process_label(&cli));
    // Log + flush on console teardown signals (pane/tab/window close, logoff,
    // shutdown) so a torn-down helper isn't a silent disappearance. Installed
    // process-wide; see `install_ctrl_handler` for coverage limits — notably
    // the master is job-killed (KILL_ON_JOB_CLOSE) and won't observe these, so
    // *this handler* doesn't trace routine master teardown. That teardown is
    // still logged, just by the C++ parent: `SharedWta` records both the
    // deliberate job-close and an unexpected exit to terminal-agent-pane.log.
    logging::install_ctrl_handler();
    // Record panics to disk (+ a synchronous wta-panic.log backstop) so a
    // panic isn't a silent death — stderr is invisible for a ConPTY helper /
    // CREATE_NO_WINDOW master. Chains the default hook; semantics unchanged.
    logging::install_panic_hook();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "=== wta starting ===");

    let locale = cli
        .language
        .clone()
        .or_else(|| sys_locale::get_locale())
        .unwrap_or_else(|| "en-US".to_string());
    rust_i18n::set_locale(&normalize_locale(&locale));

    // Register WTA's own ETW TraceLogging provider once per process. WTA uses
    // its own provider (`Microsoft.Windows.Terminal.WTA`), separate from the
    // C++ side. See tools/wta/src/telemetry.rs.
    telemetry::register();

    // Legacy flags first (backward compat)
    if cli.test_pipe {
        let r = run_test_pipe().await;
        if let Err(err) = &r {
            tracing::error!(error = ?err, "wta exiting with error");
        }
        logging::shutdown_flush();
        return r;
    }
    if cli.info {
        let r = run_info_mode().await;
        if let Err(err) = &r {
            tracing::error!(error = ?err, "wta exiting with error");
        }
        logging::shutdown_flush();
        return r;
    }
    let json_mode = cli.json;

    let result = match cli.command {
        // Subcommand aliases for legacy modes
        Some(Command::Info) => run_info_mode().await,
        Some(Command::TestPipe) => run_test_pipe().await,

        // ── List commands ──
        Some(Command::ListWindows) => {
            let result = wt_call("list_windows", json!({})).await?;
            print_output(&result, json_mode, format_windows_human);
            Ok(())
        }
        Some(Command::ListTabs { window_id }) => {
            let channel = connect_channel().await?;
            let wid = match window_id {
                Some(id) => id,
                None => get_first_window_id(&channel).await?,
            };
            let result = channel
                .request("list_tabs", json!({ "window_id": wid }))
                .await?;
            print_output(&result, json_mode, format_tabs_human);
            Ok(())
        }
        Some(Command::ListPanes { tab_id, window_id }) => {
            let channel = connect_channel().await?;
            let tid = match tab_id {
                Some(id) => id,
                None => {
                    let wid = match window_id {
                        Some(id) => id,
                        None => get_first_window_id(&channel).await?,
                    };
                    get_first_tab_id(&channel, &wid).await?
                }
            };
            let result = channel
                .request("list_panes", json!({ "tab_id": tid }))
                .await?;
            print_output(&result, json_mode, format_panes_human);
            Ok(())
        }

        // ── Profile-aware command resolution ──
        Some(Command::ResolveCommand { token, shell }) => {
            let result = resolve_command::resolve(&token, &shell).await;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", resolve_command::format_human(&result));
            }
            Ok(())
        }

        // ── Create/split ──
        Some(Command::NewTab {
            command,
            cwd,
            title,
        }) => {
            let mut params = json!({});
            if let Some(c) = command {
                params["command"] = json!(c);
            }
            if let Some(d) = cwd {
                params["cwd"] = json!(d);
            }
            if let Some(t) = title {
                params["title"] = json!(t);
            }
            let result = wt_call("create_tab", params).await?;
            print_output(&result, json_mode, format_created_tab);
            Ok(())
        }
        Some(Command::NewSurface {
            target,
            command,
            profile,
            cwd,
            background,
        }) => {
            let channel = connect_channel().await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let mut params = json!({
                "session_id": pane_id,
                "background": background,
            });
            if let Some(c) = command {
                params["commandline"] = json!(c);
            }
            if let Some(p) = profile {
                params["profile"] = json!(p);
            }
            if let Some(d) = cwd {
                params["cwd"] = json!(d);
            }
            let result = channel.request("create_surface", params).await?;
            print_output(&result, json_mode, format_created_pane);
            Ok(())
        }
        Some(Command::NewAgentSurface {
            target,
            compute_target,
            agent,
            background,
        }) => {
            let channel = connect_channel().await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let result = channel
                .request(
                    "create_managed_surface",
                    json!({
                        "session_id": pane_id,
                        "compute_target": compute_target,
                        "agent_id": agent,
                        "background": background,
                    }),
                )
                .await?;
            print_output(&result, json_mode, format_created_pane);
            Ok(())
        }
        Some(Command::SplitPane {
            target,
            horizontal,
            vertical,
            size,
            command,
        }) => {
            let channel = connect_channel().await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let split_dir = if horizontal {
                "horizontal"
            } else if vertical {
                "vertical"
            } else {
                "automatic"
            };
            let mut params = json!({
                "session_id": pane_id,
                "direction": split_dir,
            });
            if let Some(s) = size {
                params["size"] = json!(s);
            }
            if let Some(c) = command {
                params["command"] = json!(c);
            }
            let result = channel.request("split_pane", params).await?;
            print_output(&result, json_mode, format_created_pane);
            Ok(())
        }
        Some(Command::Workspace {
            cwd,
            title,
            panes,
            apply,
        }) => {
            let plan = workspace::build_plan(&cwd, title, panes)?;
            if apply {
                let channel = connect_channel().await?;
                let result = workspace::apply_plan(&channel, &plan).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            }
            Ok(())
        }
        Some(Command::AgentWorkspace { action }) => run_agent_workspace(action, json_mode).await,
        Some(Command::Team { action }) => run_team(action).await,
        Some(Command::Compute { action }) => run_compute(action, json_mode).await,

        // ── Capture pane ──
        Some(Command::CapturePane {
            target,
            max_lines,
            last_prompt,
        }) => {
            let channel = connect_channel().await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let mut params = json!({ "session_id": pane_id });
            if let Some(n) = max_lines {
                params["max_lines"] = json!(n);
            }
            if last_prompt {
                params["source"] = json!("last_prompt");
            }
            let result = channel.request("read_pane_output", params).await?;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(output) = result.get("content").and_then(|v| v.as_str()) {
                print!("{}", output);
            }
            Ok(())
        }

        // ── Kill pane ──
        Some(Command::KillPane { target }) => {
            let channel = connect_channel().await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            channel
                .request("close_pane", json!({ "session_id": pane_id }))
                .await?;
            if !json_mode {
                println!("{}", t!("output.pane_closed", pane_id = pane_id));
            }
            Ok(())
        }

        // ── Active pane ──
        Some(Command::ActivePane) => {
            let result = wt_call("get_active_pane", json!({})).await?;
            print_output(&result, json_mode, format_active_pane);
            Ok(())
        }

        // ── Pane status ──
        Some(Command::PaneStatus { target }) => {
            let channel = connect_channel().await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let result = channel
                .request("get_process_status", json!({ "session_id": pane_id }))
                .await?;
            print_output(&result, json_mode, format_pane_status);
            Ok(())
        }

        // ── Wait for ──
        // Delegate to `wtcli wait-for` so the poll loop runs inside a single
        // wtcli process (one COM handshake) instead of re-spawning wtcli per
        // tick through CliChannel.
        Some(Command::WaitFor {
            target,
            interval,
            timeout,
        }) => {
            let wtcli = shell::wt_channel::resolve_wtcli_path();
            let interval_str = interval.to_string();
            let timeout_str = timeout.to_string();
            let output = tokio::process::Command::new(&wtcli)
                .args([
                    "--json",
                    "wait-for",
                    "-t",
                    &target,
                    "--interval",
                    &interval_str,
                    "--timeout",
                    &timeout_str,
                ])
                .output()
                .await
                .with_context(|| t!("error.wtcli_wait_for_spawn").into_owned())?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "{}",
                    t!("error.wtcli_wait_for_failed", stderr = stderr.trim())
                );
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let trimmed = stdout.trim();
            if !trimmed.is_empty() {
                let val: serde_json::Value = serde_json::from_str(trimmed)
                    .with_context(|| t!("error.wtcli_wait_for_parse").into_owned())?;
                print_output(&val, json_mode, format_pane_status);
            }
            Ok(())
        }

        // ── Pipe discovery ──
        Some(Command::PipeId) => run_pipe_id(json_mode),

        // ── Set environment variables ──
        Some(Command::SetEnv { shell }) => run_set_env(&shell),

        // ── Delegate prompt to new tab agent ──
        Some(Command::Delegate {
            prompt,
            agent,
            delegate_agent,
            delegate_model,
            cwd,
        }) => {
            run_delegate(
                prompt.as_deref(),
                &agent,
                delegate_agent.as_deref(),
                delegate_model.as_deref(),
                cwd.as_deref(),
            )
            .await
        }

        // ── Listen for events ──
        Some(Command::Listen { target }) => run_listen(target.as_deref()).await,

        // ── Master session registry CLI ──
        Some(Command::Sessions { action }) => match action {
            SessionsAction::List { master, origin } => {
                run_sessions_list(master, origin.to_filter(), json_mode).await
            }
        },

        // ── Manage agent hooks (install/status/uninstall) ──
        Some(Command::Hooks { action }) => match action {
            HooksAction::Install { cli } => run_hooks_install(cli),
            HooksAction::Status => run_hooks_status(json_mode),
            HooksAction::Uninstall { cli } => run_hooks_uninstall(cli, json_mode),
        },

        // ── ACP model list probe ──
        Some(Command::ProbeModels { agent }) => run_probe_models(&agent).await,
        Some(Command::ProbeAgentSources { wsl_distro }) => {
            run_probe_agent_sources(&wsl_distro).await
        }

        // ── ACP session/list probe (diagnostic) ──
        Some(Command::ProbeSessions { agent }) => run_probe_sessions(&agent).await,

        // ── Filtered host ACP history probe (diagnostic) ──
        Some(Command::ProbeHostSessions { agent }) => run_probe_host_sessions(&agent).await,

        // ── WSL ACP history-scan probe (diagnostic) ──
        Some(Command::ProbeWslSessions { cli }) => run_probe_wsl_sessions(cli.as_deref()).await,

        // ── No subcommand: a singleton-service mode, or an error. There
        //    is no standalone/default ACP TUI mode — the direct agent-spawn
        //    path was removed, so bare `wta` always runs as a WT-launched
        //    agent pane via `--connect-master`:
        //    - `--master <pipe>`: wta-master (Z architecture; owns
        //      agent CLI, serves helper connections over named pipe)
        //    - `--connect-master <pipe>`: wta-helper (Z architecture;
        //      per-pane child that speaks ACP to master over the pipe)
        //    - neither: error — there is no standalone agent mode.
        None => {
            if let Some(pipe_name) = cli.master.clone() {
                master::run_master_mode(cli, pipe_name).await
            } else if let Some(pipe_name) = cli.connect_master.clone() {
                helper::run_helper_mode(cli, pipe_name).await
            } else {
                Err(anyhow::anyhow!(
                    "wta has no standalone agent mode: it runs as a Windows \
                     Terminal agent pane (launched by WT with --connect-master) \
                     or via a subcommand (delegate, hooks, sessions, …)"
                ))
            }
        }
    };

    // Last-resort diagnostic: any propagated failure (named-pipe connect,
    // agent spawn, ACP initialize, etc.) is otherwise only printed to stderr
    // and lost. Log it to file so connection failures are always recoverable
    // from the logs. Mode-specific context (target=master / target=helper)
    // is added closer to the source in run_master_mode / the helper path.
    if let Err(err) = &result {
        tracing::error!(error = ?err, "wta exiting with error");
    }
    // Flush the file appender before returning (its guard lives in a global,
    // not a local, so it is not dropped automatically on return).
    logging::shutdown_flush();
    result
}

/// Pick the log file label for this process from its launch mode. Drives the
/// `wta-<label>.log` filename in [`logging::init`]. Singleton-service modes are
/// selected by flags (`--master` / `--connect-master`); everything else by the
/// subcommand. Short-lived `wtcli`-style commands all share `cli`.
fn process_label(cli: &Cli) -> String {
    if cli.master.is_some() {
        return "main_master".to_string();
    }
    if cli.connect_master.is_some() {
        // Per-PID so concurrent per-tab helpers don't interleave into one
        // file (and can be reclaimed individually — see logging::housekeeping).
        return format!("main_helper-{}", std::process::id());
    }
    // Legacy diagnostic flags are short-lived clients, not the TUI.
    if cli.test_pipe || cli.info {
        return "cli".to_string();
    }
    match &cli.command {
        None => "main".to_string(),
        Some(Command::Delegate { .. }) => "delegate".to_string(),
        Some(Command::ProbeModels { .. }) => "probe".to_string(),
        Some(Command::ProbeAgentSources { .. }) => "probe".to_string(),
        Some(Command::ProbeSessions { .. }) => "probe".to_string(),
        Some(Command::ProbeHostSessions { .. }) => "probe".to_string(),
        Some(Command::ProbeWslSessions { .. }) => "probe".to_string(),
        Some(Command::Hooks {
            action: HooksAction::Install { .. },
        }) => "install-hooks".to_string(),
        // All other subcommands are short-lived wtcli-style clients.
        Some(_) => "cli".to_string(),
    }
}

/// Drive [`protocol::acp::probe::probe_models`] on a tokio `LocalSet`
/// (the ACP client connection is `!Send`), serialize the result to
/// stdout, force-exit. See exit notes below.
async fn run_probe_models(agent: &str) -> Result<()> {
    // Logging is initialized in `main()` (file, not stderr — the Settings UI
    // captures our stdout for the JSON payload and stderr would pollute it).
    tracing::info!("probe-models start: agent={}", agent);

    let local = tokio::task::LocalSet::new();
    let result = match local
        .run_until(protocol::acp::probe::probe_models(agent))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("probe-models failed: {:#}", e);
            eprintln!("probe-models failed: {:#}", e);
            let _ = std::io::Write::flush(&mut std::io::stderr());
            // Flush the file appender — process::exit skips the guard drop.
            logging::shutdown_flush();
            // See exit rationale below.
            std::process::exit(1);
        }
    };
    tracing::info!(
        "probe-models ok: {} model(s), current={:?}",
        result.available_models.len(),
        result.current_model_id
    );
    let payload = serde_json::to_string(&result).context("serialize probe result")?;
    println!("{}", payload);

    // Force-exit before the tokio runtime tries to drop. The agent we
    // spawned is e.g. `cmd /c npx ...`; kill_on_drop kills cmd but
    // the npx → node grandchildren survive as orphans. Tokio's IOCP
    // reactor stays blocked on handles those orphans inherited and
    // the runtime drop hangs for ~35s. Runtime cleanup is meaningless
    // for a one-shot CLI — the caller is blocked on our process
    // handle, exit now. Orphan grandchildren self-exit shortly after
    // when they notice their pipes are broken.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // Flush the file appender — process::exit skips the guard drop.
    logging::shutdown_flush();
    std::process::exit(0);
}

#[derive(serde::Serialize)]
struct AgentSourceProbeEntry {
    id: &'static str,
    display_name: &'static str,
}

#[derive(serde::Serialize)]
struct AgentSourceProbeResult {
    wsl_distro: String,
    agents: Vec<AgentSourceProbeEntry>,
}

async fn run_probe_agent_sources(wsl_distro: &str) -> Result<()> {
    let distro = wsl_distro.trim();
    anyhow::ensure!(!distro.is_empty(), "--wsl-distro must not be empty");

    use futures::StreamExt as _;
    let agents = futures::stream::iter(crate::agent_registry::KNOWN_AGENTS)
        .map(|profile| async move {
            crate::agent_check::wsl_agent_available(distro, profile.id)
                .await
                .then_some(AgentSourceProbeEntry {
                    id: profile.id,
                    display_name: profile.display_name,
                })
        })
        .buffer_unordered(crate::agent_registry::KNOWN_AGENTS.len())
        .filter_map(async move |entry| entry)
        .collect()
        .await;

    println!(
        "{}",
        serde_json::to_string(&AgentSourceProbeResult {
            wsl_distro: distro.to_string(),
            agents,
        })
        .context("serialize agent source probe")?
    );
    Ok(())
}

/// Drive [`protocol::acp::probe::probe_sessions`] on a tokio `LocalSet`
/// (the ACP client connection is `!Send`), print the result as pretty
/// JSON to stdout, force-exit. Diagnostic-only: evaluates whether an
/// agent CLI answers ACP `session/list` and what it returns.
async fn run_probe_sessions(agent: &str) -> Result<()> {
    tracing::info!("probe-sessions start: agent={}", agent);

    let local = tokio::task::LocalSet::new();
    let result = match local
        .run_until(protocol::acp::probe::probe_sessions(agent))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("probe-sessions failed: {:#}", e);
            eprintln!("probe-sessions failed: {:#}", e);
            let _ = std::io::Write::flush(&mut std::io::stderr());
            logging::shutdown_flush();
            std::process::exit(1);
        }
    };
    tracing::info!(
        "probe-sessions ok: list_ok={} sessions={} err={:?}",
        result.list_sessions_ok,
        result.sessions.len(),
        result.list_sessions_error
    );
    let payload = serde_json::to_string_pretty(&result).context("serialize session probe")?;
    println!("{payload}");

    // Same force-exit rationale as run_probe_models (orphan npx/node
    // grandchildren keep the tokio reactor blocked on drop).
    let _ = std::io::Write::flush(&mut std::io::stdout());
    logging::shutdown_flush();
    std::process::exit(0);
}

/// Diagnostic host-history smoke test: run one ACP CLI, fetch
/// `session/list`, apply the production Class-A filter, and print the
/// rows in the same compact shape used by the WSL probe.
async fn run_probe_host_sessions(agent: &str) -> Result<()> {
    use crate::agent_sessions::{CliSource, SessionLocation};
    use std::time::Duration;

    tracing::info!("probe-host-sessions start: agent={}", agent);

    // Resolve the CliSource from the agent command so the probe labels and
    // classifies rows the way production seeding does (which uses the real
    // `state.cli_source`), instead of assuming Copilot for every agent.
    let cli_source = CliSource::parse(Some(crate::agent_registry::resolve_agent_id_from_cmd(
        agent,
    )));

    let local = tokio::task::LocalSet::new();
    let rows = match local
        .run_until(async {
            let mut spawned = crate::protocol::acp::spawn::spawn_agent_process(agent, None)?;
            let label = format!("host:{}", crate::session_history::cli_label(&cli_source));
            let init_timeout = Duration::from_secs(if spawned.is_npx { 25 } else { 10 });
            let result = crate::protocol::acp::session_list::fetch_session_list(
                &mut spawned.child,
                &label,
                init_timeout,
                Duration::from_secs(10),
            )
            .await;
            let _ = spawned.child.start_kill();
            let (_init, list_result) = result?;
            // session/list unsupported (e.g. `Method not found`) is the production
            // "empty history, no fallback" case — surface it as `[]` + exit 0, not a
            // diagnostic failure. A genuine spawn/init error still propagates above.
            let sessions = list_result.unwrap_or_else(|e| {
                tracing::info!("probe-host-sessions: session/list unavailable ({e}); returning []");
                Vec::new()
            });
            let idx = crate::agent_pane_origin::load_default_set();
            Ok::<_, anyhow::Error>(crate::session_history::classify_and_map(
                &sessions,
                &idx,
                SessionLocation::Host,
                &cli_source,
            ))
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // Same force-exit rationale as run_probe_sessions: orphan npx/node
            // grandchildren keep the tokio reactor blocked ~35s on drop.
            tracing::error!("probe-host-sessions failed: {:#}", e);
            eprintln!("probe-host-sessions failed: {:#}", e);
            let _ = std::io::Write::flush(&mut std::io::stderr());
            logging::shutdown_flush();
            std::process::exit(1);
        }
    };

    let json: Vec<_> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "key": r.key,
                "cli": format!("{:?}", r.cli_source),
                "title": r.title,
                "cwd": r.cwd.to_string_lossy(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json).context("serialize host session probe")?
    );

    tracing::info!("probe-host-sessions ok: {} row(s)", rows.len());
    let _ = std::io::Write::flush(&mut std::io::stdout());
    logging::shutdown_flush();
    std::process::exit(0);
}

/// Drive the production WSL ACP history scan
/// ([`wsl_acp::scan_running_distros_acp`]) on a tokio `LocalSet` (the ACP
/// connection is `!Send`) and print the discovered sessions as JSON.
/// Diagnostic-only: exercises the real `wsl.exe` spawn + `session/list`
/// path that seeds the `/sessions` view.
async fn run_probe_wsl_sessions(cli: Option<&str>) -> Result<()> {
    use crate::agent_sessions::CliSource;
    tracing::info!("probe-wsl-sessions start: cli={:?}", cli);

    let filter: Option<CliSource> = match cli {
        None => None,
        Some("copilot") => Some(CliSource::Copilot),
        Some("claude") => Some(CliSource::Claude),
        Some("codex") => Some(CliSource::Codex),
        Some("gemini") => Some(CliSource::Gemini),
        Some("opencode") => Some(CliSource::OpenCode),
        Some(other) => {
            // Reject unknown values rather than silently widening to "scan all"
            // (Unknown → clis_to_scan → every built-in), which would make the
            // diagnostic's output contradict the requested restriction.
            anyhow::bail!(
                "unknown --cli value {other:?}; expected one of: copilot, claude, codex, gemini, opencode"
            );
        }
    };

    let local = tokio::task::LocalSet::new();
    let rows = local
        .run_until(crate::wsl_acp::scan_running_distros_acp(filter.as_ref()))
        .await;

    let json: Vec<_> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "key": r.key,
                "cli": format!("{:?}", r.cli_source),
                "title": r.title,
                "cwd": r.cwd.to_string_lossy(),
                "distro": r.location.distro(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json).context("serialize WSL session probe")?
    );

    tracing::info!("probe-wsl-sessions ok: {} row(s)", rows.len());
    // Force-exit like the other probes: a distro CLI may leave orphan
    // grandchildren that keep the tokio reactor blocked on drop.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    logging::shutdown_flush();
    std::process::exit(0);
}

// ─── Hooks subcommand handlers ──────────────────────────────────────────────

fn run_hooks_install(cli: HooksCliFilter) -> Result<()> {
    // Logging is initialized in `main()`; the install attempt is observable in
    // %LOCALAPPDATA%\IntelligentTerminal\logs\wta-install-hooks.log.
    let scope = cli.into_scope();
    agent_hooks_installer::ensure_installed_scoped(scope);

    // Verify the install actually landed by checking on-disk status.
    // ensure_installed_scoped is fire-and-forget (silent on failure),
    // so we inspect the result independently. `status_scoped(scope)`
    // skips the Node-CLI spawns for CLIs outside the requested scope —
    // a `--cli copilot` install no longer pays for `claude plugin list`
    // and `gemini extensions list` (each ~1-3s of Node startup).
    let report = agent_hooks_installer::status_scoped(scope);
    let failed: Vec<&str> = report
        .clis
        .iter()
        .filter(|c| {
            let in_scope = match scope {
                agent_hooks_installer::CliScope::All => true,
                agent_hooks_installer::CliScope::One(kind) => c.name == kind.name(),
            };
            // A CLI is "failed" if it's in scope, present on the machine
            // (cli_found), but hooks are not installed.
            in_scope && c.binary_on_path && !c.plugin_installed
        })
        .map(|c| c.name)
        .collect();

    if failed.is_empty() {
        println!("{}", t!("hooks.install_attempted"));
        Ok(())
    } else {
        let names = failed.join(", ");
        tracing::error!(target: "agent_hooks", clis = %names, "hooks install verification failed");
        anyhow::bail!("hooks installation failed for: {}", names)
    }
}

fn run_hooks_status(json_mode: bool) -> Result<()> {
    let report = agent_hooks_installer::status();
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .unwrap_or_else(|_| serde_json::to_string(&report).unwrap_or_default())
        );
    } else {
        format_hooks_status_human(&report);
    }
    Ok(())
}

fn run_hooks_uninstall(cli: HooksCliFilter, json_mode: bool) -> Result<()> {
    let report = agent_hooks_installer::uninstall(cli.into_scope());
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .unwrap_or_else(|_| serde_json::to_string(&report).unwrap_or_default())
        );
    } else {
        format_hooks_uninstall_human(&report);
    }
    if report.succeeded() {
        Ok(())
    } else {
        anyhow::bail!("one or more hook uninstall steps failed")
    }
}

fn format_hooks_status_human(r: &agent_hooks_installer::StatusReport) {
    let path_suffix = r
        .bundle_source
        .path
        .as_deref()
        .map(|p| format!(" ({})", p))
        .unwrap_or_default();
    println!(
        "{}",
        t!(
            "hooks.bundle_source",
            source = r.bundle_source.kind,
            path_suffix = path_suffix,
        )
    );
    println!();
    for c in &r.clis {
        let summary = if !c.binary_on_path {
            t!("hooks.cli_not_on_path").into_owned()
        } else if c.plugin_installed && c.plugin_enabled && c.marketplace_path_valid {
            t!("hooks.installed").into_owned()
        } else if c.plugin_installed && !c.marketplace_path_valid {
            t!("hooks.marketplace_path_stale").into_owned()
        } else if c.plugin_installed {
            t!("hooks.installed_but_disabled").into_owned()
        } else {
            t!("hooks.not_installed").into_owned()
        };
        let detail = format!(
            "marketplace={}, path_valid={}, plugin={}, enabled={}{}",
            yn(c.marketplace_registered),
            yn(c.marketplace_path_valid),
            yn(c.plugin_installed),
            yn(c.plugin_enabled),
            c.detection_fallback
                .map(|m| format!(", detection={}", m))
                .unwrap_or_default(),
        );
        println!("  {:<10} {:<28}  ({})", c.name, summary, detail);
        if let Some(p) = c.marketplace_path.as_deref() {
            println!("    path: {}", p);
        }
    }
}

fn format_hooks_uninstall_human(r: &agent_hooks_installer::UninstallReport) {
    for c in &r.clis {
        let summary = if !c.attempted {
            t!("hooks.uninstall_skipped").into_owned()
        } else {
            let plugin = c
                .plugin_uninstalled
                .map(|b| if b { "ok" } else { "failed" })
                .unwrap_or("-");
            let mkt = c
                .marketplace_removed
                .map(|b| if b { "ok" } else { "failed" })
                .unwrap_or("-");
            format!(
                "plugin={} marketplace={} staging={}",
                plugin,
                mkt,
                if c.staging_dir_removed {
                    "ok"
                } else {
                    "failed"
                },
            )
        };
        println!("  {:<10} {}", c.name, summary);
        for m in &c.messages {
            println!("    \u{00b7} {}", m);
        }
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

// ─── Helper: connect to WT COM protocol (no debug channel, no ShellManager) ─────────

async fn run_compute(action: ComputeAction, json_mode: bool) -> Result<()> {
    use compute::model::*;
    use compute::store::now_ms;
    use serde_json::Value;
    use uuid::Uuid;

    let store = compute::ComputeStore::package_default()?;
    let output = match action {
        ComputeAction::Target { action } => match action {
            ComputeTargetAction::Discover { save } => {
                let mut targets = discover_compute_targets()?;
                if save {
                    targets = targets
                        .into_iter()
                        .map(|target| store.upsert_target("discovery", target))
                        .collect::<Result<Vec<_>>>()?;
                }
                serde_json::to_value(targets)?
            }
            ComputeTargetAction::Add {
                id,
                name,
                provider,
                ssh_alias,
                wsl_distro,
                azure_resource_id,
                os,
                arch,
                trust,
                agent_slots,
                build_slots,
                memory_bytes,
                capabilities,
                project_allowlist,
                metadata,
            } => {
                let target = ComputeTarget {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    id,
                    display_name: name,
                    provider: parse_provider(&provider)?,
                    endpoint: TargetEndpoint {
                        ssh_alias,
                        wsl_distro,
                        azure_resource_id,
                    },
                    os,
                    arch,
                    capabilities,
                    toolchains: std::collections::BTreeMap::new(),
                    trust_tier: parse_trust(&trust)?,
                    project_allowlist,
                    agent_slots,
                    build_slots,
                    memory_bytes,
                    cost_policy: Value::Null,
                    power_policy: Value::Null,
                    health: TargetHealth::Unknown,
                    last_probe_at_ms: None,
                    disabled: false,
                    metadata: serde_json::from_str(&metadata)
                        .context("--metadata must be valid JSON")?,
                };
                serde_json::to_value(store.upsert_target("user", target)?)?
            }
            ComputeTargetAction::Get { id } => serde_json::to_value(store.get_target(&id)?)?,
            ComputeTargetAction::List => serde_json::to_value(store.list_targets()?)?,
            ComputeTargetAction::Update { id, file } => {
                let mut target: ComputeTarget = serde_json::from_slice(&std::fs::read(&file)?)
                    .with_context(|| format!("invalid target JSON: {}", file.display()))?;
                if target.id != id {
                    bail!("target document id {} does not match {id}", target.id);
                }
                target.schema_version = COMPUTE_SCHEMA_VERSION;
                serde_json::to_value(store.upsert_target("user", target)?)?
            }
            ComputeTargetAction::Remove { id, actor } => {
                serde_json::to_value(store.remove_target(&actor, &id)?)?
            }
            ComputeTargetAction::Enable { id } => {
                serde_json::to_value(store.set_target_disabled("user", &id, false)?)?
            }
            ComputeTargetAction::Disable { id } => {
                serde_json::to_value(store.set_target_disabled("user", &id, true)?)?
            }
            ComputeTargetAction::Probe { id } => {
                serde_json::to_value(probe_compute_target(&store, &id, false)?)?
            }
            ComputeTargetAction::PreviewTrust { id } => {
                let target = store.get_target(&id)?;
                let alias = target
                    .endpoint
                    .ssh_alias
                    .as_deref()
                    .context("SSH target has no alias")?;
                serde_json::to_value(compute::ssh::preview_trust(alias)?)?
            }
            ComputeTargetAction::Trust { id } => {
                serde_json::to_value(probe_compute_target(&store, &id, true)?)?
            }
            ComputeTargetAction::Start {
                id,
                allow_production,
            } => azure_target_lifecycle(&store, &id, "start", allow_production)?,
            ComputeTargetAction::Deallocate {
                id,
                allow_production,
            } => azure_target_lifecycle(&store, &id, "deallocate", allow_production)?,
            ComputeTargetAction::Cost { id } => {
                let target = store.get_target(&id)?;
                if target.provider != ProviderKind::Azure {
                    bail!("target {id} is not an Azure target");
                }
                json!({
                    "target_id": id,
                    "cost_policy": target.cost_policy,
                    "power_policy": target.power_policy,
                    "estimated_hourly_usd": target.metadata.get("estimated_hourly_usd"),
                    "mutates_azure": false,
                })
            }
        },
        ComputeAction::Environment { action } => match action {
            ComputeEnvironmentAction::Get { id } => {
                serde_json::to_value(store.get_environment(&id)?)?
            }
            ComputeEnvironmentAction::List { target } => {
                let environments = store
                    .list_environments()?
                    .into_iter()
                    .filter(|environment| {
                        target
                            .as_deref()
                            .is_none_or(|value| environment.target_id == value)
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(environments)?
            }
            ComputeEnvironmentAction::Reconcile { target } => {
                let target = store.get_target(&target)?;
                serde_json::to_value(reconcile_execution_environment(
                    &store,
                    &target,
                    LaunchMethod::SshManaged,
                )?)?
            }
        },
        ComputeAction::Endpoint { action } => match action {
            ComputeEndpointAction::Get { id } => serde_json::to_value(store.get_endpoint(&id)?)?,
            ComputeEndpointAction::List { environment } => {
                serde_json::to_value(store.list_endpoints(environment.as_deref())?)?
            }
        },
        ComputeAction::Connection { action } => match action {
            ComputeConnectionAction::Get { environment } => {
                serde_json::to_value(store.get_connection_supervisor(&environment)?)?
            }
            ComputeConnectionAction::List => {
                serde_json::to_value(store.list_connection_supervisors()?)?
            }
            ComputeConnectionAction::Prepare {
                environment,
                preferred,
            } => {
                let environment_record = store.get_environment(&environment)?;
                let preferred = parse_endpoint_kind(&preferred)?;
                if matches!(
                    preferred,
                    AccessEndpointKind::Tailscale
                        | AccessEndpointKind::AuthenticatedWss
                        | AccessEndpointKind::Relay
                ) {
                    bail!("public/overlay endpoint kinds are disabled in this release");
                }
                let permit = compute::connection::begin_for_target(
                    &store,
                    &environment_record.target_id,
                    Some(preferred),
                )?;
                json!({
                    "environment_id": permit.environment.environment_id,
                    "target_id": permit.environment.target_id,
                    "endpoint_id": permit.endpoint.endpoint_id,
                    "endpoint_kind": permit.endpoint.kind,
                    "state": permit.supervisor.state,
                    "generation": permit.supervisor.generation,
                    "opens_transport": false,
                })
            }
            ComputeConnectionAction::Reset { environment } => {
                serde_json::to_value(compute::connection::disconnect(&store, &environment)?)?
            }
        },
        ComputeAction::RemoteWorkspace { action } => match action {
            ComputeRemoteWorkspaceAction::Create {
                id,
                window,
                workspace,
                target,
                accept_host_key,
            } => {
                let initial_target = store.get_target(&target)?;
                if !matches!(
                    initial_target.provider,
                    ProviderKind::Ssh | ProviderKind::Azure
                ) {
                    bail!("remote workspace target {target} must be SSH-backed");
                }
                if initial_target.disabled && !accept_host_key {
                    let preview = initial_target
                        .endpoint
                        .ssh_alias
                        .as_deref()
                        .map(compute::ssh::preview_trust)
                        .transpose()?;
                    bail!(
                        "remote workspace target {target} is disabled; review trust preview and retry with --accept-host-key: {}",
                        serde_json::to_string(&preview)?
                    );
                }
                if accept_host_key {
                    probe_compute_target(&store, &target, true)?;
                    store.set_target_disabled("remote-workspace.create", &target, false)?;
                } else if initial_target.health != TargetHealth::Healthy {
                    probe_compute_target(&store, &target, false)?;
                }
                let probed_target = store.get_target(&target)?;
                let node_is_current = compute::installation::from_target(&probed_target)
                    .is_ok_and(|installation| installation.version == env!("CARGO_PKG_VERSION"));
                if !node_is_current {
                    bootstrap_compute_node(&store, &target, false)?;
                }
                let compute_target = store.get_target(&target)?;
                if compute_target.disabled || compute_target.health != TargetHealth::Healthy {
                    bail!(
                        "remote workspace target {target} must be enabled and healthy before creation"
                    );
                }
                let environment = reconcile_execution_environment(
                    &store,
                    &compute_target,
                    LaunchMethod::SshManaged,
                )?;
                let installation = compute::installation::from_target(&compute_target)?;
                let session = RemoteWorkspaceSession {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    remote_workspace_id: id
                        .unwrap_or_else(|| format!("remote-workspace-{}", Uuid::new_v4())),
                    window_id: window,
                    workspace_id: workspace,
                    target_id: target,
                    environment_id: Some(environment.environment_id),
                    preferred_endpoint_kind: Some(AccessEndpointKind::SshForward),
                    state: RemoteWorkspaceState::Ready,
                    reconnect_policy: ReconnectPolicy::default(),
                    reconnect_attempt: 0,
                    transport_session_id: None,
                    node_id: compute_target
                        .metadata
                        .get("node_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    last_error: None,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    metadata: json!({
                        "node_version": installation.version,
                        "node_sha256": installation.sha256,
                    }),
                };
                serde_json::to_value(store.upsert_remote_workspace("user", session)?)?
            }
            ComputeRemoteWorkspaceAction::Get { id } => {
                serde_json::to_value(store.get_remote_workspace(&id)?)?
            }
            ComputeRemoteWorkspaceAction::List => {
                serde_json::to_value(store.list_remote_workspaces()?)?
            }
            ComputeRemoteWorkspaceAction::Reconnect { id } => {
                let mut workspace = store.get_remote_workspace(&id)?;
                if matches!(
                    workspace.state,
                    RemoteWorkspaceState::Closing | RemoteWorkspaceState::Closed
                ) {
                    bail!("remote workspace {id} is closing or closed");
                }
                workspace.state = RemoteWorkspaceState::Reconnecting;
                workspace.reconnect_attempt = workspace.reconnect_attempt.saturating_add(1);
                workspace.last_error = None;
                serde_json::to_value(store.upsert_remote_workspace("user.reconnect", workspace)?)?
            }
            ComputeRemoteWorkspaceAction::Close { id } => {
                let mut workspace = store.get_remote_workspace(&id)?;
                let cleanup = stop_remote_workspace_sessions(&store, &workspace)?;
                workspace.state = RemoteWorkspaceState::Closed;
                workspace.transport_session_id = None;
                workspace.metadata["cleanup"] = cleanup;
                serde_json::to_value(store.upsert_remote_workspace("user.close", workspace)?)?
            }
            ComputeRemoteWorkspaceAction::Delete { id } => {
                serde_json::to_value(store.remove_remote_workspace("user", &id)?)?
            }
        },
        ComputeAction::Binding { action } => match action {
            ComputeBindingAction::Create {
                id,
                window,
                workspace,
                pane,
                surface,
                kind,
                target,
                agent,
                adapter,
                worktree,
                remote_session,
                focus_generation,
            } => {
                let kind = parse_binding_kind(&kind)?;
                let existing = store.find_surface_binding(&window, &workspace, &surface)?;
                let binding_id = id
                    .or_else(|| existing.as_ref().map(|binding| binding.binding_id.clone()))
                    .unwrap_or_else(|| format!("binding-{}", Uuid::new_v4()));
                let mut writer_lease_id = None;
                if kind == BindingKind::ManagedAgent
                    && existing
                        .as_ref()
                        .is_none_or(|binding| binding.writer_lease_id.is_none())
                {
                    let target_id = target
                        .as_deref()
                        .context("managed_agent binding requires --target")?;
                    let subject = worktree.as_deref().unwrap_or(&binding_id);
                    let writer = store.acquire_lease(
                        "binding.create",
                        LeaseKind::Writer,
                        subject,
                        Some(target_id),
                        &workspace,
                        &binding_id,
                        120_000,
                    )?;
                    if let Err(error) = store.acquire_lease(
                        "binding.create",
                        LeaseKind::AgentSlot,
                        &binding_id,
                        Some(target_id),
                        &workspace,
                        &binding_id,
                        120_000,
                    ) {
                        let _ = store.revoke_lease(
                            "binding.create",
                            &writer.lease_id,
                            "agent slot acquisition failed",
                        );
                        return Err(error);
                    }
                    writer_lease_id = Some(writer.lease_id);
                }
                let now = now_ms();
                let created_at_ms = existing
                    .as_ref()
                    .map_or(now, |binding| binding.created_at_ms);
                if writer_lease_id.is_none() {
                    writer_lease_id = existing
                        .as_ref()
                        .and_then(|binding| binding.writer_lease_id.clone());
                }
                let environment_id = target
                    .as_deref()
                    .and_then(|target_id| {
                        compute::connection::environment_for_target(&store, target_id).ok()
                    })
                    .map(|environment| environment.environment_id)
                    .or_else(|| {
                        existing
                            .as_ref()
                            .and_then(|binding| binding.environment_id.clone())
                    });
                let preferred_endpoint_kind = if environment_id.is_some() {
                    Some(AccessEndpointKind::SshForward)
                } else {
                    existing
                        .as_ref()
                        .and_then(|binding| binding.preferred_endpoint_kind)
                };
                let binding = SurfaceBinding {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    binding_id,
                    window_id: window,
                    workspace_id: workspace,
                    pane_id: pane,
                    surface_id: surface,
                    focus_generation,
                    kind,
                    agent_id: agent,
                    adapter_kind: adapter,
                    acp_session_id: existing
                        .as_ref()
                        .and_then(|binding| binding.acp_session_id.clone()),
                    remote_session_id: remote_session.or_else(|| {
                        existing
                            .as_ref()
                            .and_then(|binding| binding.remote_session_id.clone())
                    }),
                    environment_id,
                    preferred_endpoint_kind,
                    home_target_id: target,
                    worktree_id: worktree,
                    writer_lease_id,
                    state: if kind == BindingKind::ManagedAgent {
                        BindingState::Creating
                    } else {
                        BindingState::Ready
                    },
                    created_at_ms,
                    updated_at_ms: now,
                    metadata: Value::Null,
                };
                serde_json::to_value(store.upsert_binding("user", binding)?)?
            }
            ComputeBindingAction::Get { id } => serde_json::to_value(store.get_binding(&id)?)?,
            ComputeBindingAction::List { workspace, target } => {
                let bindings = store
                    .list_bindings()?
                    .into_iter()
                    .filter(|binding| {
                        workspace
                            .as_deref()
                            .is_none_or(|value| binding.workspace_id == value)
                            && target.as_deref().is_none_or(|value| {
                                binding.home_target_id.as_deref() == Some(value)
                            })
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(bindings)?
            }
            ComputeBindingAction::Update { id, file } => {
                let binding: SurfaceBinding = serde_json::from_slice(&std::fs::read(&file)?)
                    .with_context(|| format!("invalid binding JSON: {}", file.display()))?;
                if binding.binding_id != id {
                    bail!(
                        "binding document id {} does not match {id}",
                        binding.binding_id
                    );
                }
                serde_json::to_value(store.upsert_binding("user", binding)?)?
            }
            ComputeBindingAction::Delete { id } => {
                serde_json::to_value(store.remove_binding("user", &id)?)?
            }
            ComputeBindingAction::DeleteSurface {
                window,
                workspace,
                surface,
            } => serde_json::to_value(store.remove_surface_binding(
                "terminal.surface_closed",
                &window,
                &workspace,
                &surface,
            )?)?,
            ComputeBindingAction::Heartbeat { id, actor } => {
                serde_json::to_value(store.heartbeat_binding_runtime(&actor, &id)?)?
            }
            ComputeBindingAction::Reconcile {
                stale_after_ms,
                actor,
            } => serde_json::to_value(
                store.reconcile_stale_managed_bindings(&actor, stale_after_ms)?,
            )?,
        },
        ComputeAction::Policy { action } => match action {
            ComputePolicyAction::Get { workspace } => {
                serde_json::to_value(store.get_policy(&workspace)?)?
            }
            ComputePolicyAction::List => serde_json::to_value(store.list_policies()?)?,
            ComputePolicyAction::Set { file } => {
                let policy: WorkspaceComputePolicy = serde_json::from_slice(&std::fs::read(&file)?)
                    .with_context(|| format!("invalid policy JSON: {}", file.display()))?;
                serde_json::to_value(store.upsert_policy("user", policy)?)?
            }
            ComputePolicyAction::Import { root } => {
                let path = root
                    .join(".intelligent-terminal")
                    .join("compute-policy.json");
                let policy: WorkspaceComputePolicy = serde_json::from_slice(&std::fs::read(&path)?)
                    .with_context(|| format!("invalid policy JSON: {}", path.display()))?;
                serde_json::to_value(store.upsert_policy("policy.import", policy)?)?
            }
            ComputePolicyAction::Export {
                workspace,
                root,
                force,
            } => {
                let policy = store.get_policy(&workspace)?;
                let path = root
                    .join(".intelligent-terminal")
                    .join("compute-policy.json");
                if path.exists() && !force {
                    bail!(
                        "policy file already exists: {}; pass --force to replace it",
                        path.display()
                    );
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let temp = path.with_extension(format!("json.{}.tmp", Uuid::new_v4()));
                {
                    let mut file = std::fs::File::create(&temp)?;
                    serde_json::to_writer_pretty(&mut file, &policy)?;
                    file.write_all(b"\n")?;
                    file.flush()?;
                    file.sync_all()?;
                }
                std::fs::rename(&temp, &path)?;
                json!({"ok": true, "workspace_id": workspace, "path": path})
            }
            ComputePolicyAction::Delete { workspace } => {
                serde_json::to_value(store.remove_policy("user", &workspace)?)?
            }
        },
        ComputeAction::Session { action } => {
            if let ComputeSessionAction::Resume { binding }
            | ComputeSessionAction::Reconcile { binding } = &action
            {
                let reconciled = compute::diagnostics::reconcile_surface(&store, binding)?;
                if reconciled.state == BindingState::Disconnected {
                    bail!(
                        "managed surface {} is disconnected; keep its terminal open to let the verified reconnect wrapper reattach",
                        reconciled.binding_id
                    );
                }
                serde_json::to_value(reconciled)?
            } else if let ComputeSessionAction::Stop { binding } = &action {
                serde_json::to_value(compute::diagnostics::stop_surface(&store, binding)?)?
            } else {
                let (id, state, remote_session, acp_session) = match action {
                    ComputeSessionAction::Attach {
                        binding,
                        remote_session,
                        acp_session,
                    } => (
                        binding,
                        BindingState::Ready,
                        Some(remote_session),
                        acp_session,
                    ),
                    ComputeSessionAction::Detach { binding } => {
                        (binding, BindingState::Detached, None, None)
                    }
                    ComputeSessionAction::Resume { .. }
                    | ComputeSessionAction::Reconcile { .. }
                    | ComputeSessionAction::Stop { .. } => unreachable!(),
                };
                let mut binding = store.get_binding(&id)?;
                if binding.kind != BindingKind::ManagedAgent {
                    bail!("binding {id} is not a managed agent");
                }
                binding.state = state;
                if remote_session.is_some() {
                    binding.remote_session_id = remote_session;
                }
                if acp_session.is_some() {
                    binding.acp_session_id = acp_session;
                }
                let updated = store.upsert_binding("session", binding)?;
                let updated = if updated.state == BindingState::Ready {
                    store.heartbeat_binding_runtime("session.attach", &updated.binding_id)?
                } else {
                    updated
                };
                serde_json::to_value(updated)?
            }
        }
        ComputeAction::Place { action } => match action {
            ComputePlaceAction::Preview {
                workspace,
                workload,
                policy,
                preferred,
                os,
                arch,
                capabilities,
                minimum_memory_bytes,
                required_trust,
                production_targets_allowed,
            } => serde_json::to_value(compute::placement::decide(
                &store,
                &PlacementRequest {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    request_id: Uuid::new_v4().to_string(),
                    workspace_id: workspace,
                    workload: parse_workload(&workload)?,
                    requirements: PlacementRequirements {
                        os,
                        arch,
                        capabilities,
                        toolchains: std::collections::BTreeMap::new(),
                        minimum_memory_bytes,
                        project_identity: String::new(),
                    },
                    candidate_policy: parse_placement_policy(&policy)?,
                    preferred_target_id: preferred,
                    excluded_target_ids: Vec::new(),
                    production_targets_allowed,
                    required_trust_tier: parse_trust(&required_trust)?,
                },
            )?)?,
            ComputePlaceAction::Explain {
                workspace,
                workload,
                policy,
            } => serde_json::to_value(compute::placement::decide(
                &store,
                &PlacementRequest {
                    schema_version: COMPUTE_SCHEMA_VERSION,
                    request_id: Uuid::new_v4().to_string(),
                    workspace_id: workspace,
                    workload: parse_workload(&workload)?,
                    requirements: PlacementRequirements::default(),
                    candidate_policy: parse_placement_policy(&policy)?,
                    preferred_target_id: None,
                    excluded_target_ids: Vec::new(),
                    production_targets_allowed: false,
                    required_trust_tier: TrustTier::Development,
                },
            )?)?,
            ComputePlaceAction::Pin { binding, target } => {
                store.get_target(&target)?;
                let mut binding = store.get_binding(&binding)?;
                binding.home_target_id = Some(target);
                binding.metadata["placement"] = json!("manual_pin");
                serde_json::to_value(store.upsert_binding("placement.pin", binding)?)?
            }
            ComputePlaceAction::Unpin { binding } => {
                let mut binding = store.get_binding(&binding)?;
                binding.metadata["placement"] = json!("sticky_auto");
                serde_json::to_value(store.upsert_binding("placement.unpin", binding)?)?
            }
        },
        ComputeAction::Exec {
            root,
            workspace,
            class,
            target,
            cwd,
            timeout_ms,
            requested_by,
            idempotent,
            destructive,
            environment_allowlist,
            declared_outputs,
            snapshot,
            argv,
        } => {
            let request = ExecutionRequest {
                schema_version: COMPUTE_SCHEMA_VERSION,
                request_id: Uuid::new_v4().to_string(),
                workspace_id: workspace,
                class: parse_workload(&class)?,
                argv,
                cwd_relative: cwd,
                snapshot_id: snapshot,
                requirements: PlacementRequirements::default(),
                target_policy: target,
                environment_allowlist,
                declared_outputs,
                idempotency_key: None,
                idempotent,
                destructive,
                timeout_ms,
                requested_by,
            };
            serde_json::to_value(compute::execution::execute(&store, &root, request).await?)?
        }
        ComputeAction::Job { action } => match action {
            ComputeJobAction::Get { id } => serde_json::to_value(store.get_job(&id)?)?,
            ComputeJobAction::List { state, target } => {
                let jobs = store
                    .list_jobs()?
                    .into_iter()
                    .filter(|job| {
                        state.as_deref().is_none_or(|value| {
                            format!("{:?}", job.state).eq_ignore_ascii_case(value)
                        }) && target.as_deref().is_none_or(|value| job.target_id == value)
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(jobs)?
            }
            ComputeJobAction::Logs { id } => compute::execution::logs(&store, &id)?,
            ComputeJobAction::Cancel { id } => {
                serde_json::to_value(compute::execution::cancel(&store, "user", &id)?)?
            }
            ComputeJobAction::Retry { id, root } => {
                let prior = store.get_job(&id)?;
                if !prior.state.is_terminal() {
                    bail!("job {id} is not terminal");
                }
                if !prior.request.idempotent || prior.request.destructive {
                    bail!("job {id} is not safe to retry automatically");
                }
                let mut request = prior.request;
                request.request_id = Uuid::new_v4().to_string();
                serde_json::to_value(compute::execution::execute(&store, &root, request).await?)?
            }
            ComputeJobAction::Artifacts { id } => {
                serde_json::to_value(store.get_job(&id)?.artifacts)?
            }
            ComputeJobAction::Delete { id } => {
                serde_json::to_value(store.delete_job("user", &id)?)?
            }
        },
        ComputeAction::Snapshot { action } => match action {
            ComputeSnapshotAction::Create {
                root,
                created_by,
                include_ignored,
            } => serde_json::to_value(compute::snapshot::create(
                &store,
                &root,
                &created_by,
                &include_ignored,
            )?)?,
            ComputeSnapshotAction::Inspect { id } => {
                serde_json::to_value(store.get_snapshot(&id)?)?
            }
            ComputeSnapshotAction::Verify { id } => {
                serde_json::to_value(compute::snapshot::verify(&store, &id)?)?
            }
            ComputeSnapshotAction::List => serde_json::to_value(store.list_snapshots()?)?,
            ComputeSnapshotAction::Materialize { id, destination } => {
                compute::snapshot::materialize(&store, &id, &destination)?;
                json!({"ok": true, "snapshot_id": id, "destination": destination})
            }
            ComputeSnapshotAction::Delete { id } => {
                serde_json::to_value(store.delete_snapshot("user", &id)?)?
            }
        },
        ComputeAction::Transfer { action } => match action {
            ComputeTransferAction::Upload {
                target,
                source,
                name,
                workspace,
                surface,
            } => serde_json::to_value(compute::transfer::upload(
                &store,
                &target,
                &source,
                name.as_deref(),
                workspace,
                surface,
            )?)?,
            ComputeTransferAction::Download {
                target,
                source,
                destination,
                overwrite,
                workspace,
                surface,
            } => serde_json::to_value(compute::transfer::download(
                &store,
                &target,
                &source,
                &destination,
                overwrite,
                workspace,
                surface,
            )?)?,
            ComputeTransferAction::Get { id } => serde_json::to_value(store.get_transfer(&id)?)?,
            ComputeTransferAction::List => serde_json::to_value(store.list_transfers()?)?,
            ComputeTransferAction::Cancel { id } => {
                serde_json::to_value(store.request_transfer_cancel("user", &id)?)?
            }
            ComputeTransferAction::Retry { id } => {
                let previous = store.get_transfer(&id)?;
                if !matches!(
                    previous.state,
                    TransferState::Failed | TransferState::Cancelled
                ) {
                    bail!("only failed or cancelled transfers can be retried");
                }
                match previous.direction {
                    TransferDirection::Upload => serde_json::to_value(compute::transfer::upload(
                        &store,
                        &previous.target_id,
                        Path::new(&previous.source_path),
                        Some(&previous.display_name),
                        previous.workspace_id,
                        previous.surface_id,
                    )?)?,
                    TransferDirection::Download => {
                        let destination = previous
                            .local_path
                            .as_deref()
                            .context("download retry has no local destination")?;
                        serde_json::to_value(compute::transfer::download(
                            &store,
                            &previous.target_id,
                            &previous.source_path,
                            Path::new(destination),
                            previous.overwrite,
                            previous.workspace_id,
                            previous.surface_id,
                        )?)?
                    }
                }
            }
            ComputeTransferAction::Delete { id } => {
                serde_json::to_value(store.remove_transfer("user", &id)?)?
            }
        },
        ComputeAction::Proxy { action } => match action {
            ComputeProxyAction::Open {
                target,
                workspace,
                surface,
                port,
                allow_production,
            } => serde_json::to_value(compute::proxy::open(
                &store,
                &target,
                &workspace,
                surface,
                port,
                allow_production,
            )?)?,
            ComputeProxyAction::Get { id } => serde_json::to_value(store.get_proxy(&id)?)?,
            ComputeProxyAction::List { workspace, target } => {
                let proxies = store
                    .list_proxies()?
                    .into_iter()
                    .filter(|proxy| {
                        workspace
                            .as_deref()
                            .is_none_or(|value| proxy.workspace_id == value)
                            && target
                                .as_deref()
                                .is_none_or(|value| proxy.target_id == value)
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(proxies)?
            }
            ComputeProxyAction::Reconcile { stale_after_ms } => {
                serde_json::to_value(compute::proxy::reconcile(
                    &store,
                    std::time::Duration::from_millis(stale_after_ms),
                )?)?
            }
            ComputeProxyAction::Close { id } => {
                serde_json::to_value(compute::proxy::close(&store, &id)?)?
            }
            ComputeProxyAction::Delete { id } => {
                serde_json::to_value(store.remove_proxy("user", &id)?)?
            }
            ComputeProxyAction::Worker { id } => {
                compute::proxy::worker(&store, &id)?;
                json!({"ok": true, "proxy_id": id})
            }
        },
        ComputeAction::Browser { action } => match action {
            ComputeBrowserAction::Open {
                id,
                remote_workspace,
                surface,
                url,
                persistent,
                allow_production,
            } => serde_json::to_value(compute::browser::open(
                &store,
                id.as_deref(),
                &remote_workspace,
                &surface,
                &url,
                persistent,
                allow_production,
            )?)?,
            ComputeBrowserAction::Get { id } => serde_json::to_value(store.get_browser(&id)?)?,
            ComputeBrowserAction::List { workspace, surface } => {
                let browsers = store
                    .list_browsers()?
                    .into_iter()
                    .filter(|browser| {
                        workspace
                            .as_deref()
                            .is_none_or(|value| browser.workspace_id == value)
                            && surface
                                .as_deref()
                                .is_none_or(|value| browser.surface_id == value)
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(browsers)?
            }
            ComputeBrowserAction::Navigate { id, url } => {
                serde_json::to_value(compute::browser::navigate(&store, &id, &url)?)?
            }
            ComputeBrowserAction::Back { id } => {
                serde_json::to_value(compute::browser::move_history(&store, &id, -1)?)?
            }
            ComputeBrowserAction::Forward { id } => {
                serde_json::to_value(compute::browser::move_history(&store, &id, 1)?)?
            }
            ComputeBrowserAction::Ready { id } => serde_json::to_value(
                compute::browser::set_state(&store, &id, BrowserSurfaceState::Ready, None)?,
            )?,
            ComputeBrowserAction::Fail { id, error } => serde_json::to_value(
                compute::browser::set_state(&store, &id, BrowserSurfaceState::Failed, Some(error))?,
            )?,
            ComputeBrowserAction::Reconcile { stale_after_ms } => {
                serde_json::to_value(compute::browser::reconcile(
                    &store,
                    std::time::Duration::from_millis(stale_after_ms),
                )?)?
            }
            ComputeBrowserAction::Recover {
                id,
                allow_production,
            } => serde_json::to_value(compute::browser::recover(&store, &id, allow_production)?)?,
            ComputeBrowserAction::Close { id } => {
                serde_json::to_value(compute::browser::close(&store, &id)?)?
            }
            ComputeBrowserAction::Delete { id, delete_profile } => {
                serde_json::to_value(store.remove_browser("user", &id, delete_profile)?)?
            }
        },
        ComputeAction::File { action } => match action {
            ComputeFileAction::Roots {
                target,
                workspace,
                binding,
                include_revoked,
            } => {
                let policies = store.list_file_root_policies(
                    Some(&workspace),
                    Some(&target),
                    binding.as_deref(),
                    include_revoked,
                )?;
                json!({
                    "workspace_id": workspace,
                    "target_id": target,
                    "binding_id": binding,
                    "roots": policies.iter().map(public_file_root_policy).collect::<Vec<_>>(),
                })
            }
            ComputeFileAction::Authorize {
                id,
                target,
                workspace,
                binding,
                label,
                path,
                source,
                writable,
                deletable,
                acknowledge_wide_scope,
            } => {
                let target_record = store.get_target(&target)?;
                if !matches!(
                    target_record.provider,
                    compute::ProviderKind::Ssh | compute::ProviderKind::Azure
                ) {
                    bail!("remote file roots require an SSH-backed target");
                }
                if target_record.disabled || target_record.health != compute::TargetHealth::Healthy
                {
                    bail!("remote file roots require an enabled, healthy target");
                }
                let source = parse_file_root_source(&source)?;
                for required in [
                    "files.read",
                    if writable {
                        "files.write"
                    } else {
                        "files.read"
                    },
                    if deletable {
                        "files.delete"
                    } else {
                        "files.read"
                    },
                ] {
                    if !target_record
                        .capabilities
                        .iter()
                        .any(|capability| capability == required)
                    {
                        bail!("target {target} does not advertise {required}");
                    }
                }
                if source == compute::RemoteFileRootSource::Admin
                    && !target_record
                        .capabilities
                        .iter()
                        .any(|capability| capability == "files.admin_roots")
                {
                    bail!("target {target} does not advertise files.admin_roots");
                }
                let policy = compute::RemoteFileRootPolicy {
                    schema_version: compute::COMPUTE_SCHEMA_VERSION,
                    root_id: id.unwrap_or_else(|| format!("file-root-{}", Uuid::new_v4())),
                    workspace_id: workspace,
                    target_id: target,
                    binding_id: binding,
                    label,
                    canonical_path: path,
                    readable: true,
                    writable,
                    deletable,
                    source,
                    trust_tier: target_record.trust_tier,
                    wide_scope_acknowledged: acknowledge_wide_scope,
                    active: true,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    revoked_at_ms: None,
                };
                let policy = store.save_file_root_policy("user", policy)?;
                public_file_root_policy(&policy)
            }
            ComputeFileAction::Revoke { id } => {
                let policy = store.revoke_file_root_policy("user", &id)?;
                public_file_root_policy(&policy)
            }
            ComputeFileAction::Download {
                target,
                workspace,
                binding,
                root,
                path,
                destination,
                overwrite,
            } => {
                let transfer_id = format!("transfer-{}", Uuid::new_v4());
                let prepared = run_remote_file_operation(
                    &store,
                    &target,
                    &workspace,
                    binding.as_deref(),
                    &root,
                    "file.prepare_download",
                    json!({
                        "path": path,
                        "transfer_id": transfer_id,
                    }),
                )
                .await?;
                let state = prepared
                    .get("state")
                    .and_then(Value::as_str)
                    .context("remote file download response has no state")?;
                if state != "downloading" {
                    bail!("remote file download was not prepared");
                }
                let download = compute::transfer::NodeDownload {
                    schema_version: prepared
                        .get("schema_version")
                        .and_then(Value::as_u64)
                        .and_then(|value| u16::try_from(value).ok())
                        .context("remote file download response has no schema version")?,
                    transfer_id: prepared
                        .get("transfer_id")
                        .and_then(Value::as_str)
                        .context("remote file download response has no transfer ID")?
                        .to_string(),
                    display_name: prepared
                        .get("display_name")
                        .and_then(Value::as_str)
                        .context("remote file download response has no display name")?
                        .to_string(),
                    expected_size: prepared
                        .get("expected_size")
                        .and_then(Value::as_u64)
                        .context("remote file download response has no size")?,
                    expected_sha256: prepared
                        .get("expected_sha256")
                        .and_then(Value::as_str)
                        .context("remote file download response has no digest")?
                        .to_string(),
                    source_path: String::new(),
                    snapshot_path: String::new(),
                    state: compute::TransferState::Downloading,
                };
                let surface_id = binding
                    .as_deref()
                    .map(|id| store.get_binding(id))
                    .transpose()?
                    .map(|binding| binding.surface_id);
                let transfer = compute::transfer::download_prepared(
                    &store,
                    &target,
                    &format!("{root}:{path}"),
                    &destination,
                    overwrite,
                    Some(workspace),
                    surface_id,
                    download,
                )?;
                json!({
                    "transfer_id": transfer.transfer_id,
                    "state": transfer.state,
                    "display_name": transfer.display_name,
                    "size_bytes": transfer.size_bytes,
                    "bytes_transferred": transfer.bytes_transferred,
                    "sha256": transfer.sha256,
                    "local_path": transfer.local_path,
                })
            }
            operation => {
                let (target, workspace, binding, root, method, params) = match operation {
                    ComputeFileAction::List {
                        target,
                        workspace,
                        binding,
                        root,
                        path,
                        offset,
                        limit,
                    } => (
                        target,
                        workspace,
                        binding,
                        root,
                        "file.list_directory",
                        json!({
                            "path": path,
                            "offset": offset,
                            "limit": limit,
                        }),
                    ),
                    ComputeFileAction::Stat {
                        target,
                        workspace,
                        binding,
                        root,
                        path,
                    } => (
                        target,
                        workspace,
                        binding,
                        root,
                        "file.stat",
                        json!({ "path": path }),
                    ),
                    ComputeFileAction::Read {
                        target,
                        workspace,
                        binding,
                        root,
                        path,
                        max_bytes,
                    } => (
                        target,
                        workspace,
                        binding,
                        root,
                        "file.read_text",
                        json!({
                            "path": path,
                            "max_bytes": max_bytes,
                        }),
                    ),
                    ComputeFileAction::Mkdir {
                        target,
                        workspace,
                        binding,
                        root,
                        path,
                        recursive,
                        allow_destructive,
                    } => (
                        target,
                        workspace,
                        binding,
                        root,
                        "file.create_directory",
                        json!({
                            "path": path,
                            "recursive": recursive,
                            "allow_destructive": allow_destructive,
                        }),
                    ),
                    ComputeFileAction::Rename {
                        target,
                        workspace,
                        binding,
                        root,
                        source,
                        destination,
                        overwrite,
                        allow_destructive,
                    } => (
                        target,
                        workspace,
                        binding,
                        root,
                        "file.rename",
                        json!({
                            "source": source,
                            "destination": destination,
                            "overwrite": overwrite,
                            "allow_destructive": allow_destructive,
                        }),
                    ),
                    ComputeFileAction::Remove {
                        target,
                        workspace,
                        binding,
                        root,
                        path,
                        recursive,
                        allow_destructive,
                    } => (
                        target,
                        workspace,
                        binding,
                        root,
                        "file.remove",
                        json!({
                            "path": path,
                            "recursive": recursive,
                            "allow_destructive": allow_destructive,
                        }),
                    ),
                    ComputeFileAction::Roots { .. }
                    | ComputeFileAction::Authorize { .. }
                    | ComputeFileAction::Revoke { .. }
                    | ComputeFileAction::Download { .. } => unreachable!(),
                };
                run_remote_file_operation(
                    &store,
                    &target,
                    &workspace,
                    binding.as_deref(),
                    &root,
                    method,
                    params,
                )
                .await?
            }
        },
        ComputeAction::Restore { action } => match action {
            ComputeRestoreAction::Capture {
                window,
                workspace,
                focused_surface,
            } => serde_json::to_value(compute::restore::capture(
                &store,
                &window,
                &workspace,
                focused_surface,
            )?)?,
            ComputeRestoreAction::Get { id } => {
                serde_json::to_value(store.get_restore_snapshot(&id)?)?
            }
            ComputeRestoreAction::List => serde_json::to_value(store.list_restore_snapshots()?)?,
            ComputeRestoreAction::Latest { window, workspace } => {
                serde_json::to_value(compute::restore::latest_plan(&store, &window, &workspace)?)?
            }
            ComputeRestoreAction::Plan { id } => {
                serde_json::to_value(compute::restore::plan(&store, &id)?)?
            }
            ComputeRestoreAction::Apply {
                id,
                allow_production,
            } => serde_json::to_value(compute::restore::apply(&store, &id, allow_production)?)?,
            ComputeRestoreAction::Delete { id } => {
                serde_json::to_value(store.remove_restore_snapshot("user", &id)?)?
            }
        },
        ComputeAction::Handoff { action } => {
            let (binding_id, target_id, actor, apply) = match action {
                ComputeHandoffAction::Preview { binding, target } => {
                    (binding, target, "preview".to_string(), false)
                }
                ComputeHandoffAction::Apply {
                    binding,
                    target,
                    actor,
                }
                | ComputeHandoffAction::Rollback {
                    binding,
                    target,
                    actor,
                } => (binding, target, actor, true),
            };
            let binding = store.get_binding(&binding_id)?;
            let target = store.get_target(&target_id)?;
            let preview = json!({
                "binding_id": binding_id,
                "surface_id": binding.surface_id,
                "from_target_id": binding.home_target_id,
                "to_target_id": target.id,
                "preserves_surface_identity": true,
                "preserves_writer_lease": binding.writer_lease_id,
                "requires_new_agent_slot": binding.kind == BindingKind::ManagedAgent,
                "apply": apply,
            });
            if !apply {
                preview
            } else {
                let new_slot = if binding.kind == BindingKind::ManagedAgent {
                    Some(store.acquire_lease(
                        &actor,
                        LeaseKind::AgentSlot,
                        &binding.binding_id,
                        Some(&target.id),
                        &binding.workspace_id,
                        &binding.binding_id,
                        120_000,
                    )?)
                } else {
                    None
                };
                let old_target = binding.home_target_id.clone();
                let mut updated = binding.clone();
                updated.home_target_id = Some(target.id.clone());
                updated.state = BindingState::Reconnecting;
                updated.metadata["handoff"] = json!({
                    "from": old_target,
                    "to": target.id,
                    "at_ms": now_ms(),
                });
                if let Err(error) = store.upsert_binding(&actor, updated.clone()) {
                    if let Some(lease) = new_slot {
                        let _ = store.revoke_lease(&actor, &lease.lease_id, "handoff rollback");
                    }
                    return Err(error);
                }
                for lease in store.list_leases()?.into_iter().filter(|lease| {
                    lease.kind == LeaseKind::AgentSlot
                        && lease.subject_id == binding.binding_id
                        && lease.state == LeaseState::Active
                        && lease.target_id == old_target
                }) {
                    let _ = store.revoke_lease(&actor, &lease.lease_id, "handoff committed");
                }
                serde_json::to_value(updated)?
            }
        }
        ComputeAction::Lease { action } => match action {
            ComputeLeaseAction::List { target, workspace } => {
                let leases = store
                    .list_leases()?
                    .into_iter()
                    .filter(|lease| {
                        target
                            .as_deref()
                            .is_none_or(|value| lease.target_id.as_deref() == Some(value))
                            && workspace
                                .as_deref()
                                .is_none_or(|value| lease.workspace_id == value)
                    })
                    .collect::<Vec<_>>();
                serde_json::to_value(leases)?
            }
            ComputeLeaseAction::Revoke { id, actor, reason } => {
                serde_json::to_value(store.revoke_lease(&actor, &id, &reason)?)?
            }
            ComputeLeaseAction::Heartbeat { id, ttl_ms } => {
                serde_json::to_value(store.heartbeat_lease("heartbeat", &id, ttl_ms)?)?
            }
        },
        ComputeAction::Node { action } => match action {
            ComputeNodeAction::Bridge => {
                compute::node::bridge_stdio().await?;
                return Ok(());
            }
            ComputeNodeAction::Status => serde_json::to_value(compute::node::handshake()?)?,
            ComputeNodeAction::Doctor => compute_node_doctor()?,
            ComputeNodeAction::Bootstrap { target } => {
                bootstrap_compute_node(&store, &target, false)?
            }
            ComputeNodeAction::Upgrade { target } => bootstrap_compute_node(&store, &target, true)?,
            ComputeNodeAction::PtyCommand {
                target,
                session,
                attach,
            } => persistent_pty_command(&store, &target, &session, attach)?,
            ComputeNodeAction::PtyStatus { target, session } => {
                let mut client = compute::node_client::RemoteNodeClient::connect(
                    &store,
                    &target,
                    "persistent_generic_pty_v1",
                )
                .await?;
                let result = client
                    .request("pty.status", json!({ "session_id": session }))
                    .await?;
                client.close().await?;
                result
            }
        },
        ComputeAction::Relay { action } => match action {
            ComputeRelayAction::Issue {
                target,
                workspace,
                surface,
                operations,
                ttl_ms,
            } => {
                let operations = if operations.is_empty() {
                    vec!["notify", "status", "progress", "focus", "list"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                } else {
                    operations
                };
                let mut client =
                    compute::relay_client::RemoteRelayClient::connect(&store, &target).await?;
                let result = client
                    .request(
                        "relay.capability.issue",
                        json!({
                            "scope": {
                                "workspace_id": workspace,
                                "surface_id": surface,
                            },
                            "operations": operations,
                            "ttl_ms": ttl_ms,
                        }),
                    )
                    .await?;
                client.close().await?;
                result
            }
            ComputeRelayAction::Revoke { target, token } => {
                let token = relay_token(token)?;
                let mut client =
                    compute::relay_client::RemoteRelayClient::connect(&store, &target).await?;
                let result = client
                    .request("relay.capability.revoke", json!({ "token": token }))
                    .await?;
                client.close().await?;
                result
            }
            ComputeRelayAction::Pump {
                target,
                workspace,
                surface,
                token,
                after_sequence,
                interval_ms,
                once,
            } => {
                let token = relay_token(token)?;
                let mut client =
                    compute::relay_client::RemoteRelayClient::connect(&store, &target).await?;
                let channel = connect_channel().await?;
                let mut cursor = after_sequence;
                let mut delivered = 0_u64;
                loop {
                    let response = client
                        .request(
                            "relay.list",
                            json!({
                                "authorization": {
                                    "token": &token,
                                    "nonce": Uuid::new_v4().to_string(),
                                },
                                "scope": {
                                    "workspace_id": &workspace,
                                    "surface_id": &surface,
                                },
                                "after_sequence": cursor,
                                "limit": 200,
                            }),
                        )
                        .await?;
                    if let Some(events) = response.get("events").and_then(Value::as_array) {
                        for event in events {
                            channel
                                .request("send_event", relay_event_envelope(event)?)
                                .await?;
                            delivered = delivered.saturating_add(1);
                        }
                    }
                    cursor = response
                        .get("last_sequence")
                        .and_then(Value::as_u64)
                        .unwrap_or(cursor);
                    if once {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(interval_ms.max(100))) => {}
                        _ = tokio::signal::ctrl_c() => break,
                    }
                }
                let _ = client.close().await;
                json!({
                    "ok": true,
                    "target_id": target,
                    "workspace_id": workspace,
                    "surface_id": surface,
                    "last_sequence": cursor,
                    "delivered": delivered,
                })
            }
        },
        ComputeAction::Doctor { action } => match action {
            ComputeDoctorAction::Ssh { target } => {
                compute::diagnostics::doctor_ssh(&store, &target)?
            }
            ComputeDoctorAction::Surface { binding } => {
                compute::diagnostics::doctor_surface(&store, &binding)?
            }
            ComputeDoctorAction::Agent { agent } => {
                compute::diagnostics::doctor_agent(&store, &agent)?
            }
        },
        ComputeAction::Evidence { action } => match action {
            ComputeEvidenceAction::Export { output, redact } => {
                if !redact {
                    bail!("evidence export requires --redact");
                }
                compute::diagnostics::export_redacted(&store, &output)?
            }
        },
        ComputeAction::Events { kind, subject } => {
            let events = store
                .events()?
                .into_iter()
                .filter(|event| {
                    kind.as_deref().is_none_or(|value| event.kind == value)
                        && subject
                            .as_deref()
                            .is_none_or(|value| event.subject_id.as_deref() == Some(value))
                })
                .collect::<Vec<_>>();
            serde_json::to_value(events)?
        }
        ComputeAction::Top => {
            let targets = store.list_targets()?;
            let bindings = store.list_bindings()?;
            let leases = store.list_leases()?;
            let jobs = store.list_jobs()?;
            let remote_workspaces = store.list_remote_workspaces()?;
            let browsers = store.list_browsers()?;
            let proxies = store.list_proxies()?;
            let transfers = store.list_transfers()?;
            json!({
                "targets": targets,
                "bindings": bindings,
                "remote_workspaces": remote_workspaces,
                "browsers": browsers,
                "proxies": proxies,
                "transfers": transfers,
                "active_leases": leases.iter().filter(|lease| lease.state == LeaseState::Active).collect::<Vec<_>>(),
                "active_jobs": jobs.iter().filter(|job| !job.state.is_terminal()).collect::<Vec<_>>(),
                "summary": {
                    "targets": targets.len(),
                    "managed_agents": bindings.iter().filter(|binding| binding.kind == BindingKind::ManagedAgent).count(),
                    "remote_workspaces": remote_workspaces.iter().filter(|workspace| workspace.state != RemoteWorkspaceState::Closed).count(),
                    "active_browsers": browsers.iter().filter(|browser| !browser.state.is_terminal()).count(),
                    "active_proxies": proxies.iter().filter(|proxy| !proxy.state.is_terminal()).count(),
                    "active_transfers": transfers.iter().filter(|transfer| !matches!(transfer.state, TransferState::Succeeded | TransferState::Failed | TransferState::Cancelled)).count(),
                    "active_leases": leases.iter().filter(|lease| lease.state == LeaseState::Active).count(),
                    "active_jobs": jobs.iter().filter(|job| !job.state.is_terminal()).count(),
                }
            })
        }
    };
    print_compute_output(&output, json_mode);
    Ok(())
}

fn relay_token(explicit: Option<String>) -> Result<String> {
    let token = explicit
        .or_else(|| std::env::var("WTA_RELAY_TOKEN").ok())
        .context("relay token is required via --token or WTA_RELAY_TOKEN")?;
    if token.len() > 16 * 1024 || !token.contains('.') {
        bail!("relay token is malformed");
    }
    Ok(token)
}

fn relay_event_envelope(event: &serde_json::Value) -> Result<serde_json::Value> {
    let scope = event
        .get("scope")
        .and_then(serde_json::Value::as_object)
        .context("relay event has no scope")?;
    let workspace_id = scope
        .get("workspace_id")
        .and_then(serde_json::Value::as_str)
        .context("relay event has no workspace_id")?;
    let surface_id = scope
        .get("surface_id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let kind = event
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .context("relay event has no kind")?;
    if !matches!(kind, "notify" | "status" | "progress" | "focus") {
        bail!("unsupported relay event kind: {kind}");
    }
    let payload = event
        .get("payload")
        .filter(|value| value.is_object())
        .cloned()
        .context("relay event payload must be an object")?;
    Ok(serde_json::json!({
        "type": "event",
        "method": "remote_relay_event",
        "params": {
            "workspace_id": workspace_id,
            "surface_id": surface_id,
            "kind": kind,
            "payload": payload,
            "event_id": event.get("event_id"),
            "sequence": event.get("sequence"),
            "timestamp_ms": event.get("timestamp_ms"),
        }
    }))
}

fn print_compute_output(value: &serde_json::Value, _json_mode: bool) {
    // Stable JSON is the canonical representation for both agents and the
    // native UI. Keeping the human path valid JSON prevents hidden behavior
    // drift; richer tables can be layered over the same object later.
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

fn stop_remote_workspace_sessions(
    store: &compute::ComputeStore,
    workspace: &compute::RemoteWorkspaceSession,
) -> Result<serde_json::Value> {
    let target = store.get_target(&workspace.target_id)?;
    let alias = target
        .endpoint
        .ssh_alias
        .as_deref()
        .context("remote workspace target has no SSH alias")?;
    compute::ssh::validate_alias(alias)?;
    let installation = compute::installation::from_target(&target)?;
    let active = format!("$HOME/{}", installation.active_path);
    let ssh = compute::ssh::find_ssh_executable()?;
    let mut stopped = Vec::new();
    let mut failed = Vec::new();
    for mut binding in store
        .list_bindings()?
        .into_iter()
        .filter(|binding| binding.workspace_id == workspace.workspace_id)
    {
        let Some(session_id) = binding.remote_session_id.clone() else {
            continue;
        };
        let pty = std::process::Command::new(&ssh)
            .arg(alias)
            .arg("--")
            .arg(&active)
            .arg("pty")
            .arg("stop")
            .arg("--session")
            .arg(&session_id)
            .output()?;
        let acp = if binding.kind == compute::BindingKind::ManagedAgent {
            Some(
                std::process::Command::new(&ssh)
                    .arg(alias)
                    .arg("--")
                    .arg(&active)
                    .arg("acp")
                    .arg("stop")
                    .arg("--session")
                    .arg(&session_id)
                    .output()?,
            )
        } else {
            None
        };
        if pty.status.success() && acp.as_ref().is_none_or(|output| output.status.success()) {
            binding.state = compute::BindingState::Stopped;
            store.upsert_binding("remote-workspace.close", binding)?;
            stopped.push(session_id);
        } else {
            failed.push(json!({
                "session_id": session_id,
                "pty_error": String::from_utf8_lossy(&pty.stderr).trim(),
                "acp_error": acp.as_ref().map(|output| String::from_utf8_lossy(&output.stderr).trim().to_string()),
            }));
        }
    }
    Ok(json!({"stopped": stopped, "failed": failed}))
}

fn parse_provider(value: &str) -> Result<compute::ProviderKind> {
    match value.to_ascii_lowercase().as_str() {
        "local" => Ok(compute::ProviderKind::Local),
        "wsl" => Ok(compute::ProviderKind::Wsl),
        "ssh" => Ok(compute::ProviderKind::Ssh),
        "azure" => Ok(compute::ProviderKind::Azure),
        _ => bail!("unknown provider {value:?}; expected local, wsl, ssh or azure"),
    }
}

fn parse_trust(value: &str) -> Result<compute::TrustTier> {
    match value.to_ascii_lowercase().as_str() {
        "personal" => Ok(compute::TrustTier::Personal),
        "development" | "dev" => Ok(compute::TrustTier::Development),
        "restricted" => Ok(compute::TrustTier::Restricted),
        "production" | "prod" => Ok(compute::TrustTier::Production),
        _ => bail!("unknown trust tier {value:?}"),
    }
}

fn parse_binding_kind(value: &str) -> Result<compute::BindingKind> {
    match value.to_ascii_lowercase().as_str() {
        "plain_terminal" | "plain" | "terminal" => Ok(compute::BindingKind::PlainTerminal),
        "managed_agent" | "agent" => Ok(compute::BindingKind::ManagedAgent),
        _ => bail!("unknown binding kind {value:?}"),
    }
}

fn parse_file_root_source(value: &str) -> Result<compute::RemoteFileRootSource> {
    match value.to_ascii_lowercase().as_str() {
        "project" => Ok(compute::RemoteFileRootSource::Project),
        "worktree" => Ok(compute::RemoteFileRootSource::Worktree),
        "explicit_home" | "explicit-home" | "home" => {
            Ok(compute::RemoteFileRootSource::ExplicitHome)
        }
        "admin" => Ok(compute::RemoteFileRootSource::Admin),
        _ => bail!(
            "unknown remote file root source {value:?}; expected project, worktree, explicit_home or admin"
        ),
    }
}

fn parse_endpoint_kind(value: &str) -> Result<compute::AccessEndpointKind> {
    match value.to_ascii_lowercase().as_str() {
        "ssh_forward" | "ssh-forward" | "ssh" => Ok(compute::AccessEndpointKind::SshForward),
        "private_network" | "private-network" | "private" => {
            Ok(compute::AccessEndpointKind::PrivateNetwork)
        }
        "tailscale" => Ok(compute::AccessEndpointKind::Tailscale),
        "authenticated_wss" | "authenticated-wss" | "wss" => {
            Ok(compute::AccessEndpointKind::AuthenticatedWss)
        }
        "relay" => Ok(compute::AccessEndpointKind::Relay),
        _ => bail!(
            "unknown endpoint kind {value:?}; expected ssh_forward, private_network, tailscale, authenticated_wss or relay"
        ),
    }
}

fn public_file_root_policy(policy: &compute::RemoteFileRootPolicy) -> Value {
    json!({
        "id": policy.root_id,
        "workspace_id": policy.workspace_id,
        "target_id": policy.target_id,
        "binding_id": policy.binding_id,
        "label": policy.label,
        "readable": policy.readable,
        "writable": policy.writable,
        "deletable": policy.deletable,
        "source": policy.source,
        "trust_tier": policy.trust_tier,
        "broad_access": matches!(
            policy.source,
            compute::RemoteFileRootSource::ExplicitHome | compute::RemoteFileRootSource::Admin
        ),
        "active": policy.active,
        "created_at_ms": policy.created_at_ms,
        "updated_at_ms": policy.updated_at_ms,
        "revoked_at_ms": policy.revoked_at_ms,
    })
}

async fn run_remote_file_operation(
    store: &compute::ComputeStore,
    target_id: &str,
    workspace_id: &str,
    binding_id: Option<&str>,
    root_id: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let policy = store.get_file_root_policy(root_id)?;
    if !policy.active {
        bail!("remote file root {root_id} has been revoked");
    }
    if policy.target_id != target_id
        || !compute::store::terminal_identity_eq(&policy.workspace_id, workspace_id)
        || policy.binding_id.as_deref() != binding_id
    {
        bail!("remote file root is not authorized for this workspace, binding or target");
    }
    match method {
        "file.list_directory" | "file.stat" | "file.read_text" | "file.prepare_download"
            if !policy.readable =>
        {
            bail!("remote file root does not grant files.read")
        }
        "file.create_directory" | "file.rename" if !policy.writable => {
            bail!("remote file root does not grant files.write")
        }
        "file.remove" if !policy.deletable => {
            bail!("remote file root does not grant files.delete")
        }
        _ => {}
    }

    let encode = |value: &str| URL_SAFE_NO_PAD.encode(value.as_bytes());
    let mut client = compute::node_client::RemoteNodeClient::connect(
        store,
        target_id,
        "remote_file_explorer_v1",
    )
    .await?;
    client
        .request(
            "file.open_root",
            json!({
                "root_id": policy.root_id,
                "workspace_id": policy.workspace_id,
                "binding_id": policy.binding_id,
                "canonical_path_b64": encode(&policy.canonical_path),
                "readable": policy.readable,
                "writable": policy.writable,
                "deletable": policy.deletable,
                "source": policy.source,
                "wide_scope_acknowledged": policy.wide_scope_acknowledged,
            }),
        )
        .await?;

    let mut params = params;
    params["root_id"] = json!(root_id);
    params["workspace_id"] = json!(workspace_id);
    if let Some(binding_id) = binding_id {
        params["binding_id"] = json!(binding_id);
    }
    for (source, encoded_key) in [
        ("path", "path_b64"),
        ("source", "source_b64"),
        ("destination", "destination_b64"),
    ] {
        let value = params
            .get(source)
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(value) = value {
            params[encoded_key] = json!(encode(&value));
        }
        if let Some(object) = params.as_object_mut() {
            object.remove(source);
        }
    }

    let result = client.request(method, params).await;
    let _ = client
        .request("file.close_root", json!({ "root_id": root_id }))
        .await;
    let close_result = client.close().await;
    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn parse_workload(value: &str) -> Result<compute::WorkloadClass> {
    match value.to_ascii_lowercase().as_str() {
        "interactive_agent" | "agent" => Ok(compute::WorkloadClass::InteractiveAgent),
        "build" => Ok(compute::WorkloadClass::Build),
        "test" => Ok(compute::WorkloadClass::Test),
        "lint" => Ok(compute::WorkloadClass::Lint),
        "browser" => Ok(compute::WorkloadClass::Browser),
        "gpu" => Ok(compute::WorkloadClass::Gpu),
        _ => bail!("unknown workload class {value:?}"),
    }
}

fn parse_placement_policy(value: &str) -> Result<compute::PlacementPolicy> {
    match value.to_ascii_lowercase().as_str() {
        "local_first" | "local-first" => Ok(compute::PlacementPolicy::LocalFirst),
        "balanced" => Ok(compute::PlacementPolicy::Balanced),
        "cost_first" | "cost-first" => Ok(compute::PlacementPolicy::CostFirst),
        "performance" => Ok(compute::PlacementPolicy::Performance),
        _ => bail!("unknown placement policy {value:?}"),
    }
}

fn discover_compute_targets() -> Result<Vec<compute::ComputeTarget>> {
    use compute::model::*;
    let mut targets = vec![ComputeTarget {
        schema_version: COMPUTE_SCHEMA_VERSION,
        id: "local".to_string(),
        display_name: "This computer".to_string(),
        provider: ProviderKind::Local,
        endpoint: TargetEndpoint::default(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        capabilities: vec![
            "terminal".to_string(),
            "managed_agent".to_string(),
            "build".to_string(),
        ],
        toolchains: std::collections::BTreeMap::new(),
        trust_tier: TrustTier::Personal,
        project_allowlist: Vec::new(),
        agent_slots: std::thread::available_parallelism()
            .map(|value| value.get().max(1) as u32)
            .unwrap_or(1),
        build_slots: std::thread::available_parallelism()
            .map(|value| (value.get() / 2).max(1) as u32)
            .unwrap_or(1),
        memory_bytes: 0,
        cost_policy: json!({"hourly_usd": 0}),
        power_policy: serde_json::Value::Null,
        health: TargetHealth::Healthy,
        last_probe_at_ms: Some(compute::store::now_ms()),
        disabled: false,
        metadata: json!({"discovered": true}),
    }];
    targets.extend(
        wsl::installed_distros()
            .into_iter()
            .map(|distro| ComputeTarget {
                schema_version: COMPUTE_SCHEMA_VERSION,
                id: format!(
                    "wsl:{}",
                    distro
                        .chars()
                        .map(|ch| if ch.is_ascii_alphanumeric() {
                            ch.to_ascii_lowercase()
                        } else {
                            '-'
                        })
                        .collect::<String>()
                ),
                display_name: format!("WSL — {distro}"),
                provider: ProviderKind::Wsl,
                endpoint: TargetEndpoint {
                    wsl_distro: Some(distro),
                    ..Default::default()
                },
                os: "linux".to_string(),
                arch: std::env::consts::ARCH.to_string(),
                capabilities: vec!["terminal".to_string(), "build".to_string()],
                toolchains: std::collections::BTreeMap::new(),
                trust_tier: TrustTier::Personal,
                project_allowlist: Vec::new(),
                agent_slots: 1,
                build_slots: 1,
                memory_bytes: 0,
                cost_policy: json!({"hourly_usd": 0}),
                power_policy: serde_json::Value::Null,
                health: TargetHealth::Unknown,
                last_probe_at_ms: None,
                disabled: false,
                metadata: json!({"discovered": true}),
            }),
    );
    if compute::ssh::find_ssh_executable().is_ok() {
        targets.extend(compute::ssh::discover_targets()?);
    }
    Ok(targets)
}

fn probe_compute_target(
    store: &compute::ComputeStore,
    id: &str,
    trust: bool,
) -> Result<compute::ComputeTarget> {
    use compute::model::*;
    let mut target = store.get_target(id)?;
    let now = compute::store::now_ms();
    match target.provider {
        ProviderKind::Local => {
            target.health = TargetHealth::Healthy;
        }
        ProviderKind::Wsl => {
            let distro = target
                .endpoint
                .wsl_distro
                .as_deref()
                .context("WSL target has no distro")?;
            let output = std::process::Command::new("wsl.exe")
                .args(["-d", distro, "--", "true"])
                .output()?;
            target.health = if output.status.success() {
                TargetHealth::Healthy
            } else {
                TargetHealth::Unreachable
            };
        }
        ProviderKind::Ssh | ProviderKind::Azure
            if target.endpoint.ssh_alias.as_deref().is_some() =>
        {
            let alias = target
                .endpoint
                .ssh_alias
                .as_deref()
                .context("SSH target has no alias")?;
            let probe = compute::ssh::probe(alias, trust)?;
            target.health = probe.health;
            target.metadata["ssh"] = serde_json::to_value(&probe)?;
            if probe.health == TargetHealth::Healthy {
                if let Ok((os, arch)) = compute::ssh::probe_platform(alias) {
                    target.os = os;
                    target.arch = arch;
                }
            }
            if trust && probe.health == TargetHealth::Healthy {
                target.metadata["trusted_at_ms"] = json!(now);
            }
        }
        ProviderKind::Ssh => {
            target.health = TargetHealth::Unreachable;
            target.metadata["probe_error"] = json!("SSH target has no alias");
        }
        ProviderKind::Azure => {
            target.health = TargetHealth::Unknown;
            target.metadata["probe_error"] =
                json!("Azure target is not configured with an SSH alias");
        }
    }
    target.last_probe_at_ms = Some(now);
    store.upsert_target(
        if trust {
            "target.trust"
        } else {
            "target.probe"
        },
        target,
    )
}

fn compute_node_doctor() -> Result<serde_json::Value> {
    let handshake = compute::node::handshake()?;
    Ok(json!({
        "ok": true,
        "handshake": handshake,
        "tools": {
            "git": which::which("git").ok().map(|path| path.to_string_lossy().into_owned()),
            "ssh": which::which("ssh").ok().map(|path| path.to_string_lossy().into_owned()),
            "codex": which::which("codex").ok().map(|path| path.to_string_lossy().into_owned()),
        }
    }))
}

fn azure_target_lifecycle(
    store: &compute::ComputeStore,
    id: &str,
    operation: &str,
    allow_production: bool,
) -> Result<serde_json::Value> {
    use compute::model::{LeaseState, ProviderKind, TrustTier};
    let target = store.get_target(id)?;
    if target.provider != ProviderKind::Azure {
        bail!("target {id} is not an Azure target");
    }
    if target.trust_tier == TrustTier::Production && !allow_production {
        bail!("production target requires --allow-production for explicit lifecycle actions");
    }
    if operation == "start" {
        let estimated = target
            .metadata
            .get("estimated_hourly_usd")
            .and_then(serde_json::Value::as_f64);
        let budget = target
            .cost_policy
            .get("max_hourly_usd")
            .and_then(serde_json::Value::as_f64);
        if let (Some(estimated), Some(budget)) = (estimated, budget) {
            if estimated > budget {
                bail!(
                    "Azure target estimate ${estimated:.4}/h exceeds configured budget ${budget:.4}/h"
                );
            }
        }
    } else {
        let active_lease = store.list_leases()?.into_iter().find(|lease| {
            lease.target_id.as_deref() == Some(id) && lease.state == LeaseState::Active
        });
        let active_job = store
            .list_jobs()?
            .into_iter()
            .find(|job| job.target_id == id && !job.state.is_terminal());
        if let Some(lease) = active_lease {
            bail!(
                "cannot deallocate target {id}; active lease {} belongs to {}",
                lease.lease_id,
                lease.owner
            );
        }
        if let Some(job) = active_job {
            bail!(
                "cannot deallocate target {id}; active job {} is {:?}",
                job.job_id,
                job.state
            );
        }
    }
    let resource_id = target
        .endpoint
        .azure_resource_id
        .as_deref()
        .context("Azure target has no resource id")?;
    if !resource_id.starts_with("/subscriptions/") || !resource_id.contains("/virtualMachines/") {
        bail!("Azure target resource id is not an allowlisted VM resource id");
    }
    let az = which::which("az.exe")
        .or_else(|_| which::which("az"))
        .context("Azure CLI is not installed")?;
    let output = std::process::Command::new(az)
        .args(["vm", operation, "--ids", resource_id, "--output", "json"])
        .output()?;
    if !output.status.success() {
        bail!(
            "az vm {operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    store.append_event(compute::model::ComputeEvent::new(
        &format!("azure.{operation}"),
        "user",
        Some(id.to_string()),
        None,
        json!({"resource_id": resource_id}),
    ))?;
    Ok(json!({
        "ok": true,
        "target_id": id,
        "operation": operation,
        "resource_id": resource_id,
        "azure_response": serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap_or(serde_json::Value::Null),
    }))
}

fn reconcile_execution_environment(
    store: &compute::ComputeStore,
    target: &compute::ComputeTarget,
    launch_method: compute::LaunchMethod,
) -> Result<compute::ExecutionEnvironment> {
    let node_id = target
        .metadata
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .context("verified node identity is unavailable")?;
    let installation = compute::installation::from_target(target)?;
    let environment_id = format!("environment-{node_id}");

    for mut stale in store
        .list_environments()?
        .into_iter()
        .filter(|environment| {
            environment.target_id == target.id
                && environment.environment_id != environment_id
                && environment.lifecycle_state != compute::EnvironmentLifecycleState::Retired
        })
    {
        stale.lifecycle_state = compute::EnvironmentLifecycleState::Retired;
        store.save_environment("environment.reconcile", stale)?;
    }

    let environment = store.save_environment(
        "environment.reconcile",
        compute::ExecutionEnvironment {
            schema_version: compute::COMPUTE_SCHEMA_VERSION,
            environment_id: environment_id.clone(),
            target_id: target.id.clone(),
            runtime_version: installation.version,
            protocol_version: compute::COMPUTE_PROTOCOL_VERSION,
            os: target.os.clone(),
            arch: target.arch.clone(),
            capabilities: target.capabilities.clone(),
            lifecycle_state: if target.disabled || target.health != compute::TargetHealth::Healthy {
                compute::EnvironmentLifecycleState::Offline
            } else {
                compute::EnvironmentLifecycleState::Ready
            },
            launch_method,
            created_at_ms: 0,
            updated_at_ms: 0,
            metadata: json!({
                "node_id": node_id,
                "transport": "ssh_stdio",
            }),
        },
    )?;
    let endpoint_id = format!("endpoint-{node_id}-ssh");
    store.save_endpoint(
        "environment.reconcile",
        compute::AccessEndpoint {
            schema_version: compute::COMPUTE_SCHEMA_VERSION,
            endpoint_id,
            environment_id: environment_id.clone(),
            kind: compute::AccessEndpointKind::SshForward,
            reachability: compute::EndpointReachability::SshRequired,
            health: if target.health == compute::TargetHealth::Healthy && !target.disabled {
                compute::EndpointHealth::Healthy
            } else {
                compute::EndpointHealth::Unreachable
            },
            priority: 10,
            enabled: true,
            created_at_ms: 0,
            updated_at_ms: 0,
            metadata: json!({
                "bootstrap": "ssh_stdio",
                "public_listener": false,
            }),
        },
    )?;
    compute::connection::ensure_supervisor(
        store,
        &environment_id,
        compute::AccessEndpointKind::SshForward,
    )?;
    Ok(environment)
}

fn bootstrap_compute_node(
    store: &compute::ComputeStore,
    target_id: &str,
    upgrade: bool,
) -> Result<serde_json::Value> {
    use compute::model::ProviderKind;
    let mut target = store.get_target(target_id)?;
    if !matches!(target.provider, ProviderKind::Ssh | ProviderKind::Azure) {
        bail!("node bootstrap currently requires an SSH-backed target");
    }
    let alias = target
        .endpoint
        .ssh_alias
        .clone()
        .context("SSH target has no alias")?;
    compute::ssh::resolve_alias(&alias)?;
    let executable = find_node_artifact(&target)?;
    let local_hash = compute::snapshot::sha256_file(&executable)?;
    let windows_target = target.os.eq_ignore_ascii_case("windows");
    let layout = compute::installation::layout_for(&target)?;
    let remote_dir = &layout.version_dir;
    let remote_file = &layout.version_path;
    let ssh = compute::ssh::find_ssh_executable()?;
    let mkdir = if windows_target {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(format!(
                "New-Item -ItemType Directory -Force -Path '{}','{}' | Out-Null",
                remote_dir, layout.active_dir
            ))
            .output()?
    } else {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("mkdir")
            .arg("-p")
            .arg(remote_dir)
            .arg(&layout.root)
            .output()?
    };
    if !mkdir.status.success() {
        bail!(
            "failed to create remote node directory: {}",
            String::from_utf8_lossy(&mkdir.stderr).trim()
        );
    }

    // Do not replace an identical versioned binary. On Unix, atomically
    // replacing the file behind a live daemon makes /proc/self/exe resolve to
    // a deleted inode and forces an unnecessary daemon rollover on the next
    // request. Bootstrap remains fully verified, but is idempotent when the
    // exact SHA-256 is already installed.
    let installed_hash = if windows_target {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(format!(
                "(Get-FileHash -Algorithm SHA256 '{}').Hash",
                remote_file
            ))
            .output()?
    } else {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("sha256sum")
            .arg(remote_file)
            .output()?
    };
    let installed_digest = String::from_utf8_lossy(&installed_hash.stdout)
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let version_is_current = installed_hash.status.success() && installed_digest == local_hash;

    if !version_is_current {
        let scp = which::which("scp.exe")
            .or_else(|_| which::which("scp"))
            .context("OpenSSH scp was not found")?;
        let copied = std::process::Command::new(scp)
            .arg(&executable)
            .arg(format!("{alias}:{remote_file}.incoming"))
            .output()?;
        if !copied.status.success() {
            bail!(
                "failed to upload node helper: {}",
                String::from_utf8_lossy(&copied.stderr).trim()
            );
        }
        // Use fixed, product-generated paths and argv. No user-provided shell
        // fragment is interpolated into the remote verification command.
        let verify = if windows_target {
            std::process::Command::new(&ssh)
                .arg(&alias)
                .arg("--")
                .arg("powershell.exe")
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(format!(
                    "(Get-FileHash -Algorithm SHA256 '{}.incoming').Hash",
                    remote_file
                ))
                .output()?
        } else {
            std::process::Command::new(&ssh)
                .arg(&alias)
                .arg("--")
                .arg("sha256sum")
                .arg(format!("{remote_file}.incoming"))
                .output()?
        };
        if !verify.status.success() {
            bail!(
                "remote SHA-256 verification failed: {}",
                String::from_utf8_lossy(&verify.stderr).trim()
            );
        }
        let remote_hash = String::from_utf8_lossy(&verify.stdout)
            .split_ascii_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if remote_hash != local_hash {
            bail!("remote helper hash mismatch: expected {local_hash}, received {remote_hash}");
        }
        let install = if windows_target {
            std::process::Command::new(&ssh)
                .arg(&alias)
                .arg("--")
                .arg("powershell.exe")
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(format!(
                    "Move-Item -LiteralPath '{}.incoming' -Destination '{}' -Force",
                    remote_file, remote_file
                ))
                .output()?
        } else {
            std::process::Command::new(&ssh)
                .arg(&alias)
                .arg("--")
                .arg("mv")
                .arg(format!("{remote_file}.incoming"))
                .arg(remote_file)
                .output()?
        };
        if !install.status.success() {
            bail!(
                "failed to activate verified node helper: {}",
                String::from_utf8_lossy(&install.stderr).trim()
            );
        }
        if !windows_target {
            let chmod = std::process::Command::new(&ssh)
                .arg(&alias)
                .arg("--")
                .arg("chmod")
                .arg("700")
                .arg(remote_file)
                .output()?;
            if !chmod.status.success() {
                bail!("failed to mark remote helper executable");
            }
        }
    }
    let activate = if windows_target {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(format!(
                "Copy-Item -LiteralPath '{}' -Destination '{}.incoming' -Force; Move-Item -LiteralPath '{}.incoming' -Destination '{}' -Force",
                remote_file, layout.active_path, layout.active_path, layout.active_path
            ))
            .output()?
    } else {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("ln")
            .arg("-sfn")
            .arg(format!("versions/{}", env!("CARGO_PKG_VERSION")))
            .arg(format!("{}/current", layout.root))
            .output()?
    };
    if !activate.status.success() {
        bail!(
            "failed to atomically activate verified node helper: {}",
            String::from_utf8_lossy(&activate.stderr).trim()
        );
    }

    let active_hash = if windows_target {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(format!(
                "(Get-FileHash -Algorithm SHA256 '{}').Hash",
                layout.active_path
            ))
            .output()?
    } else {
        std::process::Command::new(&ssh)
            .arg(&alias)
            .arg("--")
            .arg("sha256sum")
            .arg(&layout.active_path)
            .output()?
    };
    let active_digest = String::from_utf8_lossy(&active_hash.stdout)
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !active_hash.status.success() || active_digest != local_hash {
        bail!(
            "active node helper verification failed: expected {local_hash}, received {active_digest}"
        );
    }

    let status = std::process::Command::new(&ssh)
        .arg(&alias)
        .arg("--")
        .arg(&layout.active_path)
        .arg("status")
        .output()?;
    if !status.status.success() {
        bail!(
            "active node helper handshake failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    let handshake: compute::NodeHandshake = serde_json::from_slice(&status.stdout)
        .context("active node helper returned an invalid handshake")?;
    if handshake.node_version != env!("CARGO_PKG_VERSION")
        || handshake.protocol_version != compute::COMPUTE_PROTOCOL_VERSION
    {
        bail!(
            "active node helper handshake mismatch: node version {}, protocol {}",
            handshake.node_version,
            handshake.protocol_version
        );
    }

    let installation = compute::installation::NodeInstallation::new(
        &target,
        &layout,
        handshake.state_root.clone(),
        local_hash.clone(),
    );
    compute::installation::record(&mut target, &installation)?;
    target.capabilities = handshake.capabilities.clone();
    target.metadata["node_id"] = json!(handshake.node_id);
    target.metadata["node_rpc_methods"] = json!(handshake.rpc_methods);
    let target = store.upsert_target("node.bootstrap", target)?;
    let environment =
        reconcile_execution_environment(store, &target, compute::LaunchMethod::SshManaged)?;

    Ok(json!({
        "ok": true,
        "target_id": target_id,
        "ssh_alias": alias,
        "remote_path": layout.version_path,
        "active_path": layout.active_path,
        "sha256": local_hash,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": handshake.protocol_version,
        "environment_id": environment.environment_id,
        "capabilities": handshake.capabilities,
        "operation": if upgrade { "upgrade" } else { "bootstrap" },
        "bridge_command": format!("{} bridge", layout.active_path),
    }))
}

fn persistent_pty_command(
    store: &compute::ComputeStore,
    target_id: &str,
    session_id: &str,
    attach: bool,
) -> Result<serde_json::Value> {
    compute::session::validate_session_id(session_id)?;
    let target = store.get_target(target_id)?;
    if target.disabled || target.health != compute::TargetHealth::Healthy {
        bail!("PTY target {target_id} must be enabled and healthy");
    }
    if !target.os.eq_ignore_ascii_case("linux") {
        bail!("persistent PTY currently requires a Linux wta-node target");
    }
    let alias = target
        .endpoint
        .ssh_alias
        .as_deref()
        .context("PTY target has no SSH alias")?;
    compute::ssh::validate_alias(alias)?;
    let installation = compute::installation::from_target(&target)?;
    let resolved = compute::ssh::resolve_alias(alias)?;
    let mut arguments = vec![
        "-t".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
    ];
    arguments.extend(compute::transport::default_keepalive_args(&resolved));
    arguments.push(alias.to_string());
    arguments.push("--".to_string());
    let operation = if attach { "attach" } else { "start" };
    let mut remote = format!(
        "exec \"$HOME/{}\" pty {operation} --session {}",
        installation.active_path,
        crate::coordinator::sh_quote(session_id)
    );
    if !attach {
        remote.push_str(" -- \"${SHELL:-/bin/sh}\" -l");
    }
    arguments.push(remote);
    let commandline = reconnecting_ssh_commandline(&arguments)?;
    Ok(json!({
        "target_id": target_id,
        "session_id": session_id,
        "operation": operation,
        "commandline": commandline,
        "node_version": installation.version,
        "node_sha256": installation.sha256,
        "keepalive_injected": compute::transport::default_keepalive_args(&resolved),
        "reconnect_delays_seconds": compute::ReconnectPolicy::default().delays_seconds,
    }))
}

fn reconnecting_ssh_commandline(arguments: &[String]) -> Result<String> {
    use base64::Engine as _;

    if arguments.is_empty() {
        bail!("reconnecting SSH command requires arguments");
    }
    let quote_ps = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let argv = arguments
        .iter()
        .map(|argument| quote_ps(argument))
        .collect::<Vec<_>>()
        .join(",");
    let delays = compute::ReconnectPolicy::default()
        .delays_seconds
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // Encode the wrapper as UTF-16LE for powershell.exe -EncodedCommand.
    // Target data remains argv literals and is never reparsed as script.
    let script = format!(
        "$ErrorActionPreference='Continue';\
         $ssh={};\
         $argv=@({argv});\
         $delays=@({delays});\
         $attempt=0;\
         while($true){{\
           if($attempt -gt 0){{\
             $index=[Math]::Min($attempt-1,$delays.Count-1);\
             $delay=$delays[$index];\
             Write-Host \"`r`n[Intelligent Terminal] Connection lost. Reattaching in $delay second(s)...\" -ForegroundColor DarkYellow;\
             Start-Sleep -Seconds $delay\
           }};\
           & $ssh @argv;\
           $code=$LASTEXITCODE;\
           if($code -eq 0){{exit 0}};\
           $attempt++\
         }}",
        quote_ps(
            compute::ssh::find_ssh_executable()?
                .to_string_lossy()
                .as_ref()
        )
    );
    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    Ok(format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
        crate::coordinator::quote_windows_commandline_arg(&encoded)
    ))
}

fn find_node_artifact(target: &compute::ComputeTarget) -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let executable_name = if target.os.eq_ignore_ascii_case("windows") {
        "wta-node.exe"
    } else if target.os.eq_ignore_ascii_case("linux") && target.arch == "x86_64" {
        "wta-node-linux-x64"
    } else {
        bail!(
            "no packaged wta-node artifact for {}/{}",
            target.os,
            target.arch
        );
    };
    let mut candidates = vec![current.with_file_name(executable_name)];
    for ancestor in current.ancestors().take(7) {
        candidates.push(ancestor.join("remote").join("linux-x64").join("wta-node"));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| format!("wta-node artifact {executable_name} is not available"))
}

async fn run_agent_workspace(action: WorkspaceAction, _json_mode: bool) -> Result<()> {
    match action {
        WorkspaceAction::Plan { manifest } => {
            let definition = workspace::WorkspaceManifest::load(&manifest)?;
            let plan = workspace::build_declarative_plan(&definition, &manifest)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        WorkspaceAction::Apply { manifest } | WorkspaceAction::Open { manifest } => {
            let result = apply_workspace_manifest(&manifest).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        WorkspaceAction::Restore {
            root,
            name,
            snapshot,
        } => {
            let store = workspace::WorkspaceStore::open(&root, &name)?;
            let runtime = match snapshot {
                Some(path) => store.load_snapshot(&path)?,
                None => store.load_runtime()?,
            };
            let persisted_manifest = PathBuf::from(&runtime.manifest_path);
            let manifest_path = if persisted_manifest.is_absolute() || persisted_manifest.exists() {
                persisted_manifest
            } else {
                Path::new(&runtime.root).join(persisted_manifest)
            };
            let result = apply_workspace_manifest(&manifest_path).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        WorkspaceAction::Template {
            template,
            name,
            output,
            force,
            stdout,
        } => {
            let contents = workspace::render_template(&template, &name)?;
            if stdout {
                print!("{contents}");
                return Ok(());
            }
            if output.exists() && !force {
                bail!(
                    "refusing to overwrite {}; pass --force to replace it",
                    output.display()
                );
            }
            if let Some(parent) = output
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&output, contents)
                .with_context(|| format!("failed to write template {}", output.display()))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "template": template,
                    "path": output,
                }))?
            );
        }
        WorkspaceAction::List { root } => {
            let runtimes = workspace::WorkspaceStore::discover_all(&root)?;
            println!("{}", serde_json::to_string_pretty(&runtimes)?);
        }
        WorkspaceAction::Tree { root, name } => {
            let runtime = workspace::WorkspaceStore::open(&root, &name)?.load_runtime()?;
            println!("{}", serde_json::to_string_pretty(&runtime)?);
        }
        WorkspaceAction::Read {
            root,
            name,
            recipient,
            after_ms,
        } => {
            let store = workspace::WorkspaceStore::open(&root, &name)?;
            let events = match recipient {
                Some(recipient) => store.inbox(&recipient, after_ms)?,
                None => store
                    .events()?
                    .into_iter()
                    .filter(|event| after_ms.is_none_or(|after| event.timestamp_ms > after))
                    .collect(),
            };
            println!("{}", serde_json::to_string_pretty(&events)?);
        }
        WorkspaceAction::Notify {
            root,
            name,
            source,
            target,
            kind,
            payload,
        } => {
            let store = workspace::WorkspaceStore::open(&root, &name)?;
            let mut runtime = store.load_runtime()?;
            let manifest_path = PathBuf::from(&runtime.manifest_path);
            let manifest = workspace::WorkspaceManifest::load(&manifest_path)?;
            if target.as_deref() == Some("*") && !manifest.messaging.allow_broadcast {
                bail!("workspace messaging policy rejects broadcasts");
            }
            if let Some(recipient) = target.as_deref().filter(|target| *target != "*") {
                if !runtime.panes.contains_key(recipient) {
                    bail!("unknown workspace message target: {recipient}");
                }
            }
            let payload: serde_json::Value =
                serde_json::from_str(&payload).context("--payload must be valid JSON")?;
            let event = workspace::WorkspaceEvent::new(
                &runtime.workspace_id,
                &kind,
                &source,
                target.clone(),
                payload,
                manifest.messaging.max_hops,
            )?;
            store.append_event(&event)?;
            if let Ok(channel) = connect_channel().await {
                let _ = channel
                    .request("send_event", event.protocol_envelope()?)
                    .await;
            }
            if let Some(target) = target.as_deref().filter(|target| *target != "*") {
                if let Some(pane) = runtime.panes.get_mut(target) {
                    pane.activity = workspace::PaneActivity::Attention;
                    pane.last_notification = Some(event.id.clone());
                    store.save_runtime(&mut runtime)?;
                }
            }
            println!("{}", serde_json::to_string_pretty(&event)?);
        }
        WorkspaceAction::Forward {
            root,
            name,
            event_id,
            source,
            target,
        } => {
            let store = workspace::WorkspaceStore::open(&root, &name)?;
            let runtime = store.load_runtime()?;
            let manifest = workspace::WorkspaceManifest::load(Path::new(&runtime.manifest_path))?;
            if target.as_deref() == Some("*") && !manifest.messaging.allow_broadcast {
                bail!("workspace messaging policy rejects broadcasts");
            }
            if let Some(recipient) = target.as_deref().filter(|target| *target != "*") {
                if !runtime.panes.contains_key(recipient) {
                    bail!("unknown workspace message target: {recipient}");
                }
            }
            let original = store
                .events()?
                .into_iter()
                .find(|event| event.id == event_id)
                .ok_or_else(|| anyhow::anyhow!("unknown workspace event id: {event_id}"))?;
            if original.workspace_id != runtime.workspace_id {
                bail!(
                    "event {} belongs to workspace {}, not {}",
                    original.id,
                    original.workspace_id,
                    runtime.workspace_id
                );
            }
            let forwarded = original.forwarded(&source, target)?;
            store.append_event(&forwarded)?;
            if let Ok(channel) = connect_channel().await {
                let _ = channel
                    .request("send_event", forwarded.protocol_envelope()?)
                    .await;
            }
            println!("{}", serde_json::to_string_pretty(&forwarded)?);
        }
        WorkspaceAction::Status {
            root,
            name,
            recipient,
            after_ms,
        } => {
            let store = workspace::WorkspaceStore::open(&root, &name)?;
            let mut runtime = store.load_runtime()?;
            let refreshed_panes = if let Ok(channel) = connect_channel().await {
                let refreshed = workspace::refresh_workspace_runtime(&channel, &mut runtime).await;
                if refreshed > 0 {
                    store.save_runtime(&mut runtime)?;
                }
                refreshed
            } else {
                0
            };
            let metrics = store.metrics(recipient.as_deref(), after_ms)?;
            let context = workspace::collect_context(&runtime).await;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "workspace": runtime,
                    "metrics": metrics,
                    "context": context,
                    "refreshed_panes": refreshed_panes,
                }))?
            );
        }
        WorkspaceAction::Context { root, tab_id } => {
            let persisted = workspace::WorkspaceStore::discover(&root, tab_id.as_deref())?;
            let runtime = persisted.clone().unwrap_or_else(|| {
                workspace::WorkspaceRuntime::new("Ad hoc", Path::new(""), &root)
            });
            let context = workspace::collect_context(&runtime).await;
            let is_persisted = !runtime.manifest_path.is_empty();
            let (metrics, events) = if let Some(runtime) = persisted {
                let store = workspace::WorkspaceStore::open(&root, &runtime.name)?;
                (Some(store.metrics(None, None)?), store.events_tail(40)?)
            } else {
                (None, Vec::new())
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "workspace": runtime,
                    "context": context,
                    "metrics": metrics,
                    "events": events,
                    "persisted": is_persisted,
                }))?
            );
        }
        WorkspaceAction::InspectGit { root, max_bytes } => {
            let inspection = workspace::inspect_git(&root, Some(max_bytes)).await;
            println!("{}", serde_json::to_string_pretty(&inspection)?);
        }
        WorkspaceAction::Doctor { root } => {
            let root_exists = root.is_dir();
            let git = probe_command("git", &["--version"], &root).await;
            let github = probe_command("gh", &["--version"], &root).await;
            let protocol = match connect_channel().await {
                Ok(channel) => match channel.request("list_windows", json!({})).await {
                    Ok(value) => json!({
                        "ok": true,
                        "windows": value.get("windows").and_then(|value| value.as_array()).map_or(0, |values| values.len()),
                    }),
                    Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
                },
                Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
            };
            let workspaces = if root_exists {
                match workspace::WorkspaceStore::discover_all(&root) {
                    Ok(values) => json!({"ok": true, "count": values.len()}),
                    Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
                }
            } else {
                json!({"ok": false, "error": "root directory does not exist"})
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": root_exists
                        && git["ok"].as_bool().unwrap_or(false)
                        && protocol["ok"].as_bool().unwrap_or(false),
                    "version": env!("CARGO_PKG_VERSION"),
                    "root": root,
                    "checks": {
                        "root": {"ok": root_exists},
                        "git": git,
                        "github_cli": github,
                        "terminal_protocol": protocol,
                        "workspace_store": workspaces,
                        "embedded_webview": {
                            "ok": true,
                            "enabled": false,
                            "policy": "fail_closed_external_browser_only"
                        }
                    }
                }))?
            );
        }
        WorkspaceAction::Send {
            root,
            name,
            target,
            text,
        } => {
            let runtime = workspace::WorkspaceStore::open(&root, &name)?.load_runtime()?;
            let channel = connect_channel().await?;
            workspace::send_to_workspace_pane(&channel, &runtime, &target, &text).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"ok": true, "target": target}))?
            );
        }
        WorkspaceAction::Focus { root, name, target } => {
            let runtime = workspace::WorkspaceStore::open(&root, &name)?.load_runtime()?;
            let pane = runtime
                .panes
                .get(&target)
                .ok_or_else(|| anyhow::anyhow!("unknown logical pane id: {target}"))?;
            let channel = connect_channel().await?;
            let result = channel
                .request("focus_pane", json!({ "session_id": pane.session_id }))
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        WorkspaceAction::Peek {
            root,
            name,
            target,
            max_lines,
        } => {
            let runtime = workspace::WorkspaceStore::open(&root, &name)?.load_runtime()?;
            let pane = runtime
                .panes
                .get(&target)
                .ok_or_else(|| anyhow::anyhow!("unknown logical pane id: {target}"))?;
            let channel = connect_channel().await?;
            let result = channel
                .request(
                    "read_pane_output",
                    json!({
                        "session_id": pane.session_id,
                        "source": "scrollback",
                        "max_lines": max_lines,
                    }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        WorkspaceAction::Wait {
            root,
            name,
            target,
            interval,
            timeout,
        } => {
            let runtime = workspace::WorkspaceStore::open(&root, &name)?.load_runtime()?;
            let channel = connect_channel().await?;
            let status =
                workspace::wait_for_workspace_pane(&channel, &runtime, &target, interval, timeout)
                    .await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        WorkspaceAction::Close { root, name } => {
            let runtime = workspace::WorkspaceStore::open(&root, &name)?.load_runtime()?;
            let channel = connect_channel().await?;
            workspace::close_workspace(&channel, &runtime).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"ok": true, "workspace": name}))?
            );
        }
        WorkspaceAction::Verify { manifest } => {
            let definition = workspace::WorkspaceManifest::load(&manifest)?;
            let plan = workspace::build_declarative_plan(&definition, &manifest)?;
            let result = workspace::run_verifier(&plan).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            if result.winner.is_none() {
                bail!("workspace verification oracle failed");
            }
        }
        WorkspaceAction::Snapshot { root, name } => {
            let store = workspace::WorkspaceStore::open(&root, &name)?;
            let runtime = store.load_runtime()?;
            let snapshot = store.create_snapshot(&runtime)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"ok": true, "snapshot": snapshot}))?
            );
        }
    }
    Ok(())
}

async fn run_team(action: TeamAction) -> Result<()> {
    match action {
        TeamAction::Create {
            root,
            name,
            leader,
            workspace_id,
            stale_after_ms,
            max_attempts,
        } => {
            if !root.is_dir() {
                bail!("team root is not a directory: {}", root.display());
            }
            // When invoked from a terminal pane, bind the team to the native
            // workspace automatically. Explicit --workspace-id remains useful
            // for automation outside the focused window. If no protocol is
            // available we preserve the legacy unbound/team-only behavior.
            let workspace_id = match workspace_id {
                Some(id) => Some(id),
                None => match connect_channel().await {
                    Ok(channel) => channel
                        .request("get_active_pane", json!({}))
                        .await
                        .ok()
                        .and_then(|pane| {
                            pane.get("workspace_id")
                                .and_then(serde_json::Value::as_str)
                                .filter(|id| !id.is_empty())
                                .map(str::to_string)
                        }),
                    Err(_) => None,
                },
            };
            let (store, state) = team::TeamStore::create(
                &root,
                &name,
                &leader,
                workspace_id.as_deref(),
                stale_after_ms,
                max_attempts,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "team": state,
                    "store": store.directory(),
                }))?
            );
        }
        TeamAction::AddWorker {
            root,
            name,
            worker,
            role,
            agent,
            model,
            cwd,
            capabilities,
            no_launch,
            split_target,
            direction,
        } => {
            let cwd = cwd.unwrap_or_else(|| root.clone());
            let store = team::TeamStore::open(&root, &name)?;
            let leader = store.load()?.leader;
            let registered = store.add_worker(
                &leader,
                &worker,
                &role,
                &agent,
                model.clone(),
                &cwd,
                capabilities,
            )?;
            if no_launch {
                println!("{}", serde_json::to_string_pretty(&registered)?);
            } else {
                let launched = launch_team_worker(
                    &store,
                    &root,
                    &name,
                    &registered,
                    &agent,
                    model.as_deref(),
                    split_target.as_deref(),
                    &direction,
                    None,
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&launched)?);
            }
        }
        TeamAction::AddTask {
            root,
            name,
            id,
            title,
            prompt,
            dependencies,
            owns,
            max_attempts,
            actor,
        } => {
            let task = team::TeamStore::open(&root, &name)?.add_task(
                &actor,
                id.as_deref(),
                &title,
                &prompt,
                dependencies,
                owns,
                max_attempts,
            )?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Assign {
            root,
            name,
            worker,
            task,
            actor,
        } => {
            let task = team::TeamStore::open(&root, &name)?.assign_task(&actor, &task, &worker)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Dispatch {
            root,
            name,
            worker,
            task,
            actor,
        } => {
            let store = team::TeamStore::open(&root, &name)?;
            let task = store.assign_task(&actor, &task, &worker)?;
            let worker = store.worker(&worker)?;
            let pane = worker
                .pane_session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("worker {} has no terminal pane", worker.id))?;
            let prompt = team::task_dispatch_prompt(&root, &name, &worker, &task);
            connect_channel()
                .await?
                .request(
                    "send_input",
                    json!({"session_id": pane, "text": format!("{prompt}\r")}),
                )
                .await
                .with_context(|| {
                    format!(
                        "task {} was assigned, but delivery to worker {} failed",
                        task.id, worker.id
                    )
                })?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "task": task,
                    "worker": worker.id,
                    "pane_session_id": pane,
                    "delivered": true,
                }))?
            );
        }
        TeamAction::Claim {
            root,
            name,
            worker,
            task,
        } => {
            let task = team::TeamStore::open(&root, &name)?.claim_task(&worker, task.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Start {
            root,
            name,
            worker,
            task,
        } => {
            let task = team::TeamStore::open(&root, &name)?.start_assigned_task(&worker, &task)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Heartbeat {
            root,
            name,
            worker,
            task,
        } => {
            let worker =
                team::TeamStore::open(&root, &name)?.heartbeat(&worker, task.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&worker)?);
        }
        TeamAction::Complete {
            root,
            name,
            worker,
            task,
            result,
        } => {
            let task =
                team::TeamStore::open(&root, &name)?.complete_task(&worker, &task, &result)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Fail {
            root,
            name,
            worker,
            task,
            error,
        } => {
            let task = team::TeamStore::open(&root, &name)?.fail_task(&worker, &task, &error)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Retry {
            root,
            name,
            task,
            actor,
        } => {
            let task = team::TeamStore::open(&root, &name)?.retry_task(&actor, &task)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Cancel {
            root,
            name,
            task,
            reason,
            actor,
        } => {
            let task = team::TeamStore::open(&root, &name)?.cancel_task(&actor, &task, &reason)?;
            println!("{}", serde_json::to_string_pretty(&task)?);
        }
        TeamAction::Status {
            root,
            name,
            reconcile,
        } => {
            let store = team::TeamStore::open(&root, &name)?;
            let stale = if reconcile {
                store.reconcile("status")?
            } else {
                Vec::new()
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "team": store.load()?,
                    "stale_workers_marked": stale,
                }))?
            );
        }
        TeamAction::Events {
            root,
            name,
            after_ms,
        } => {
            let events = team::TeamStore::open(&root, &name)?
                .events()?
                .into_iter()
                .filter(|event| after_ms.is_none_or(|after| event.timestamp_ms > after))
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&events)?);
        }
        TeamAction::Send {
            root,
            name,
            worker,
            text,
            enter,
        } => {
            let worker = team::TeamStore::open(&root, &name)?.worker(&worker)?;
            let pane = worker
                .pane_session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("worker {} has no terminal pane", worker.id))?;
            let text = if enter { format!("{text}\r") } else { text };
            let result = connect_channel()
                .await?
                .request("send_input", json!({"session_id": pane, "text": text}))
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        TeamAction::Focus { root, name, worker } => {
            let worker = team::TeamStore::open(&root, &name)?.worker(&worker)?;
            let pane = worker
                .pane_session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("worker {} has no terminal pane", worker.id))?;
            let result = connect_channel()
                .await?
                .request("focus_pane", json!({"session_id": pane}))
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        TeamAction::Peek {
            root,
            name,
            worker,
            max_lines,
        } => {
            let worker = team::TeamStore::open(&root, &name)?.worker(&worker)?;
            let pane = worker
                .pane_session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("worker {} has no terminal pane", worker.id))?;
            let result = connect_channel()
                .await?
                .request(
                    "read_pane_output",
                    json!({
                        "session_id": pane,
                        "source": "scrollback",
                        "max_lines": max_lines,
                    }),
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        TeamAction::Reconcile { root, name, actor } => {
            let stale = team::TeamStore::open(&root, &name)?.reconcile(&actor)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"stale_workers": stale}))?
            );
        }
        TeamAction::Shutdown {
            root,
            name,
            actor,
            force,
            close_panes,
        } => {
            let store = team::TeamStore::open(&root, &name)?;
            let before = store.load()?;
            let channel = connect_channel().await.ok();
            let mut pane_failures = Vec::new();
            if let Some(channel) = channel.as_ref() {
                for worker in before.workers.values() {
                    let Some(pane) = worker.pane_session_id.as_deref() else {
                        continue;
                    };
                    if !force {
                        let message = "The team leader requested shutdown. Finish or fail your current task, then exit.\r";
                        if let Err(error) = channel
                            .request("send_input", json!({"session_id": pane, "text": message}))
                            .await
                        {
                            pane_failures.push(format!("notify {}: {error:#}", worker.id));
                        }
                    }
                    if close_panes {
                        if let Err(error) = channel
                            .request("close_pane", json!({"session_id": pane}))
                            .await
                        {
                            pane_failures.push(format!("close {}: {error:#}", worker.id));
                        }
                    }
                }
            } else if close_panes {
                pane_failures.push("terminal protocol is unavailable".to_string());
            }
            let state = store.shutdown(&actor, force)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "team": state,
                    "close_panes": close_panes,
                    "pane_failures": pane_failures,
                }))?
            );
        }
        TeamAction::Doctor { root, name } => {
            let report = team::TeamStore::open(&root, &name)?.doctor()?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        TeamAction::E2e {
            root,
            name,
            agent,
            agent_two,
            model,
            model_two,
            worker_cwd,
            wait_seconds,
        } => {
            let worker_cwd = worker_cwd.unwrap_or_else(|| root.clone());
            if !worker_cwd.is_dir() {
                bail!(
                    "E2E worker cwd is not a directory: {}",
                    worker_cwd.display()
                );
            }
            let name = name.unwrap_or_else(|| {
                format!("e2e-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
            });
            let (store, _) = team::TeamStore::create(&root, &name, "e2e-leader", None, 180_000, 2)?;
            for (id, title) in [
                ("agent-one", "Inspect native team state"),
                ("agent-two", "Inspect native team audit log"),
            ] {
                store.add_task(
                    "e2e-leader",
                    Some(id),
                    title,
                    "Use read-only commands to inspect this team. Return the team name and your worker id, then record that result with `wta team complete`.",
                    Vec::new(),
                    Vec::new(),
                    Some(2),
                )?;
            }
            let agent_two = agent_two.unwrap_or_else(|| agent.clone());
            let worker_specs = [
                ("agent-one", agent.as_str(), model.as_deref()),
                (
                    "agent-two",
                    agent_two.as_str(),
                    model_two.as_deref().or(model.as_deref()),
                ),
            ];
            let mut launched = Vec::new();
            for (id, worker_agent, worker_model) in worker_specs {
                let worker = store.add_worker(
                    "e2e-leader",
                    id,
                    "independent smoke-test agent",
                    worker_agent,
                    worker_model.map(str::to_string),
                    &worker_cwd,
                    vec!["team-status".to_string()],
                )?;
                let task = store.assign_task("e2e-leader", id, id)?;
                let launch_prompt = format!(
                    "You are worker `{}` in Intelligent Terminal team `{}`. Role: {}.\n\
                     This is an isolated coordination smoke test. Execute the assigned \
                     read-only task and update the native team state exactly as instructed.\n\n{}",
                    worker.id,
                    name,
                    worker.role,
                    team::task_dispatch_prompt(&root, &name, &worker, &task)
                );
                launched.push(
                    launch_team_worker(
                        &store,
                        &root,
                        &name,
                        &worker,
                        worker_agent,
                        worker_model,
                        None,
                        "automatic",
                        Some(&launch_prompt),
                    )
                    .await?,
                );
            }
            let started = tokio::time::Instant::now();
            let final_state = loop {
                let state = store.load()?;
                let succeeded = ["agent-one", "agent-two"].iter().all(|id| {
                    state.tasks.get(*id).map(|task| task.status)
                        == Some(team::TaskStatus::Succeeded)
                });
                let failed = ["agent-one", "agent-two"].iter().any(|id| {
                    state.tasks.get(*id).is_some_and(|task| {
                        matches!(
                            task.status,
                            team::TaskStatus::Failed | team::TaskStatus::Cancelled
                        )
                    })
                });
                if succeeded || failed || wait_seconds == 0 {
                    break state;
                }
                if started.elapsed() >= std::time::Duration::from_secs(wait_seconds) {
                    bail!(
                        "real two-agent E2E timed out after {} seconds; inspect with `wta team status --root \"{}\" --name \"{}\"`",
                        wait_seconds,
                        root.display(),
                        name
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            };
            let success = ["agent-one", "agent-two"].iter().all(|id| {
                final_state.tasks.get(*id).map(|task| task.status)
                    == Some(team::TaskStatus::Succeeded)
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": success,
                    "team": final_state,
                    "launched_workers": launched,
                    "verification": format!(
                        "wta team status --root \"{}\" --name \"{}\" --reconcile",
                        root.display(),
                        name
                    ),
                }))?
            );
            if wait_seconds > 0 && !success {
                bail!("one or more real agents reported failure");
            }
        }
    }
    Ok(())
}

async fn launch_team_worker(
    store: &team::TeamStore,
    root: &Path,
    team_name: &str,
    worker: &team::TeamWorker,
    agent_command: &str,
    model: Option<&str>,
    split_target: Option<&str>,
    direction: &str,
    startup_prompt: Option<&str>,
) -> Result<team::TeamWorker> {
    let runtime =
        crate::coordinator::default_delegate_agent_runtimes(Some(agent_command), None, model)
            .into_iter()
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!("no delegate runtime was produced for {agent_command}")
            })?;
    let default_prompt = team::worker_bootstrap_prompt(root, team_name, worker);
    let prompt = startup_prompt.unwrap_or(&default_prompt);
    let agent_commandline = crate::coordinator::build_delegate_launch_commandline_with_session(
        &runtime,
        Some(prompt),
        None,
    )?;
    // A terminal-agent worker may update its own team task store, but it must
    // not inherit the host-wide COM capability and drive arbitrary panes.
    // Coordinator mutations that need terminal control stay in the trusted
    // parent WTA process.
    let commandline = format!(
        "cmd.exe /d /s /c \"set WT_COM_CLSID=&& set WT_PROTOCOL_TOKEN=&& {agent_commandline}\""
    );
    let channel = connect_channel().await?;
    let result = match split_target {
        Some(target) => channel
            .request(
                "split_pane",
                json!({
                    "session_id": target,
                    "direction": direction,
                    "commandline": commandline,
                    "cwd": worker.cwd,
                }),
            )
            .await
            .with_context(|| format!("failed to launch team worker {} in a split", worker.id))?,
        None => channel
            .request(
                "create_tab",
                json!({
                    "commandline": commandline,
                    "cwd": worker.cwd,
                    "title": format!("{} · {}", team_name, worker.id),
                }),
            )
            .await
            .with_context(|| format!("failed to launch team worker {} in a tab", worker.id))?,
    };
    let pane_session_id = team_protocol_id(&result, "session_id")
        .or_else(|| team_protocol_id(&result, "pane_id"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "terminal launch response for worker {} is missing a pane id: {}",
                worker.id,
                result
            )
        })?;
    let leader = store.load()?.leader;
    store.set_worker_pane(&leader, &worker.id, &pane_session_id)
}

fn team_protocol_id(value: &serde_json::Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(serde_json::Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(serde_json::Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

async fn apply_workspace_manifest(
    manifest_path: &std::path::Path,
) -> Result<workspace::ApplyResult> {
    let manifest_path = manifest_path
        .canonicalize()
        .with_context(|| format!("failed to resolve manifest {}", manifest_path.display()))?;
    let definition = workspace::WorkspaceManifest::load(&manifest_path)?;
    let plan = workspace::build_declarative_plan(&definition, &manifest_path)?;
    let channel = connect_channel().await?;
    workspace::apply_declarative_plan(&channel, &definition, &manifest_path, &plan).await
}

async fn connect_channel() -> Result<CliChannel> {
    CliChannel::connect().await
}

async fn probe_command(program: &str, args: &[&str], cwd: &Path) -> serde_json::Value {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let result = match command.spawn() {
        Ok(child) => {
            tokio::time::timeout(std::time::Duration::from_secs(3), child.wait_with_output()).await
        }
        Err(error) => {
            return json!({"ok": false, "error": error.to_string()});
        }
    };
    match result {
        Ok(Ok(output)) => {
            let text = if output.stdout.is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            } else {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            };
            json!({
                "ok": output.status.success(),
                "version": text.lines().next().unwrap_or_default(),
                "exit_code": output.status.code(),
            })
        }
        Ok(Err(error)) => json!({"ok": false, "error": error.to_string()}),
        Err(_) => json!({"ok": false, "error": "probe timed out"}),
    }
}

/// Single-shot: connect + call + return JSON
async fn wt_call(method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let channel = connect_channel().await?;
    channel.request(method, params).await
}

/// Resolve -t target: Some(id) -> use it, None -> get_active_pane fallback
async fn resolve_pane_id(channel: &CliChannel, target: &Option<String>) -> Result<String> {
    match target {
        Some(id) => Ok(id.clone()),
        None => {
            let result = channel.request("get_active_pane", json!({})).await?;
            let pane_id = result
                .get("session_id")
                .and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("{}", t!("error.no_active_pane")))?;
            Ok(pane_id)
        }
    }
}

/// Get the first window ID from list_windows.
async fn get_first_window_id(channel: &CliChannel) -> Result<String> {
    let result = channel.request("list_windows", json!({})).await?;
    first_window_id_from_result(&result)
}

fn first_window_id_from_result(result: &serde_json::Value) -> Result<String> {
    result
        .get("windows")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|w| w.get("window_id"))
        .and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("{}", t!("output.no_windows_in_list")))
}

/// Get the first tab ID from a window.
async fn get_first_tab_id(channel: &CliChannel, window_id: &str) -> Result<String> {
    let result = channel
        .request("list_tabs", json!({ "window_id": window_id }))
        .await?;
    result
        .get("tabs")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|t| match t.get("tab_id") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("{}", t!("output.no_tabs_in_window", window_id = window_id)))
}

// ─── sessions CLI helpers ───────────────────────────────────────────────────

const MASTER_NOT_RUNNING: &str = "wta-master not running. Start Windows Terminal first.";

async fn run_sessions_list(
    master_override: Option<String>,
    origin_filter: agent_sessions::OriginFilter,
    json_mode: bool,
) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    let sessions = local
        .run_until(fetch_sessions_from_master(master_override))
        .await?;
    // Origin filter is applied client-side: master always returns the
    // full registry so this command can act as the debug eye-of-god
    // view (default `--origin all`). `--origin shell` matches what
    // the MVP sessions picker shows; `--origin agent-pane` surfaces the
    // rows MVP sessions hides.
    let mut filtered: Vec<session_registry::SessionInfo> = sessions
        .into_iter()
        .filter(|s| origin_filter.matches_opt(s.origin.as_ref()))
        .collect();
    // Match the `/sessions` picker, which renders newest-activity-first.
    // `None` (no timestamp) sorts last.
    filtered.sort_by(|a, b| b.last_activity_at_ms.cmp(&a.last_activity_at_ms));
    if json_mode {
        print!("{}", format_sessions_json_lines(&filtered)?);
    } else {
        print!("{}", format_sessions_table(&filtered));
    }
    Ok(())
}

async fn fetch_sessions_from_master(
    master_override: Option<String>,
) -> Result<Vec<session_registry::SessionInfo>> {
    let pipe_name = resolve_master_pipe(master_override).await?;
    let pipe = open_master_pipe_for_cli(&pipe_name).await?;
    let (read_half, write_half) = tokio::io::split(pipe);
    let outgoing = write_half.compat_write();
    let incoming = read_half.compat();
    let (conn, handle_io) = crate::protocol::acp::conn::spawn_client(
        acp::Client.builder().name("wta-sessions"),
        crate::protocol::acp::conn::byte_streams(outgoing, incoming),
    );
    tokio::task::spawn_local(async move {
        let _ = handle_io.await;
    });

    let init_started = std::time::Instant::now();
    let init_result = conn
        .initialize(
            acp::schema::v1::InitializeRequest::new(acp::schema::ProtocolVersion::V1)
                .client_capabilities(acp::schema::v1::ClientCapabilities::new())
                .client_info(
                    acp::schema::v1::Implementation::new("wta-sessions", env!("CARGO_PKG_VERSION"))
                        .title("Windows Terminal Agent sessions CLI"),
                ),
        )
        .await;
    telemetry::log_acp_initialize_complete(
        init_started.elapsed().as_secs_f64() * 1000.0,
        init_result.is_ok(),
        "SessionsCli",
        if init_result.is_ok() { "" } else { "AcpError" },
        init_result
            .as_ref()
            .err()
            .map(|e| e.code.into())
            .unwrap_or(0),
    );
    init_result.map_err(|_| anyhow::anyhow!(MASTER_NOT_RUNNING))?;

    let req = session_registry::build_sessions_list_request(false);
    let resp = conn
        .ext_method(req)
        .await
        .map_err(|_| anyhow::anyhow!(MASTER_NOT_RUNNING))?;
    let parsed = session_registry::parse_sessions_list_response(&resp.0)
        .context("parse sessions/list response")?;
    Ok(parsed.sessions)
}

/// Best-effort: register a WTA-launched CLI session with `wta-master` as a
/// *born-bound* row — bound to its pane, with no hooks involved. Sends a
/// `SessionStarted` over the `intellterm.wta/session_born_bound` method, which
/// the master turns into a Class-B (`origin = Unknown`) row whose
/// `pane_session_id` is the pane we just created and records as binding-only
/// (so the file watcher may still supply activity/status when no hook is
/// installed). Best-effort: if master is unreachable there is no registry to
/// populate, so the registration is dropped (logged at `warn`) and the tab
/// still opens normally.
async fn register_launched_session_with_master(
    session_id: &str,
    pane_session_id: &str,
    cli_id: &str,
    cwd: Option<&str>,
    wsl_distro: Option<&str>,
) {
    let event = crate::agent_sessions::SessionEvent::SessionStarted {
        key: session_id.to_string(),
        cli_source: crate::agent_sessions::CliSource::from(
            crate::session_registry::SessionHookCliSource::Known(cli_id.to_string()),
        ),
        pane_session_id: pane_session_id.to_string(),
        cwd: cwd.map(std::path::PathBuf::from).unwrap_or_default(),
        // Empty title: the master refreshes the row's title from the CLI's
        // on-disk session artefacts once they appear.
        title: String::new(),
    };
    // A WSL delegate carries its distro so the master stamps the row
    // `Wsl { distro }` → the session view shows the `[WSL-<distro>]` prefix.
    let req = match wsl_distro {
        Some(distro) => session_registry::build_born_bound_request_wsl(&event, distro),
        None => session_registry::build_born_bound_request(&event),
    };

    // Own LocalSet so the `spawn_local` transport works regardless of how the
    // delegate's runtime was set up (mirrors `run_sessions_list`).
    let local = tokio::task::LocalSet::new();
    let result: Result<()> = local
        .run_until(async move {
            let pipe_name = resolve_master_pipe(None).await?;
            let pipe = open_master_pipe_for_cli(&pipe_name).await?;
            let (read_half, write_half) = tokio::io::split(pipe);
            let outgoing = write_half.compat_write();
            let incoming = read_half.compat();
            let (conn, handle_io) = crate::protocol::acp::conn::spawn_client(
                acp::Client.builder().name("wta-delegate"),
                crate::protocol::acp::conn::byte_streams(outgoing, incoming),
            );
            tokio::task::spawn_local(async move {
                let _ = handle_io.await;
            });

            conn.initialize(
                acp::schema::v1::InitializeRequest::new(acp::schema::ProtocolVersion::V1)
                    .client_capabilities(acp::schema::v1::ClientCapabilities::new())
                    .client_info(
                        acp::schema::v1::Implementation::new(
                            "wta-delegate",
                            env!("CARGO_PKG_VERSION"),
                        )
                        .title("Windows Terminal Agent delegate"),
                    ),
            )
            .await
            .map_err(|_| anyhow::anyhow!(MASTER_NOT_RUNNING))?;

            conn.ext_method(req)
                .await
                .map_err(|_| anyhow::anyhow!(MASTER_NOT_RUNNING))?;
            Ok(())
        })
        .await;

    if let Err(e) = result {
        tracing::warn!(
            target: "delegate",
            error = %e,
            "register born-bound session with master failed (best-effort)"
        );
    }
}

async fn resolve_master_pipe(master_override: Option<String>) -> Result<String> {
    if let Some(pipe) = master_override.filter(|s| !s.trim().is_empty()) {
        return Ok(pipe);
    }

    for attempt in 0..2 {
        if let Some(path) = runtime_paths::master_pipe_file_path() {
            if let Ok(contents) = std::fs::read_to_string(path) {
                let pipe = contents.trim();
                if !pipe.is_empty() {
                    return Ok(pipe.to_string());
                }
            }
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    Err(anyhow::anyhow!(MASTER_NOT_RUNNING))
}

async fn open_master_pipe_for_cli(
    pipe_name: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    for attempt in 0..2 {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(pipe_name) {
            Ok(pipe) => return Ok(pipe),
            Err(_) if attempt == 0 => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await
            }
            Err(_) => return Err(anyhow::anyhow!(MASTER_NOT_RUNNING)),
        }
    }
    Err(anyhow::anyhow!(MASTER_NOT_RUNNING))
}

fn format_sessions_json_lines(sessions: &[session_registry::SessionInfo]) -> Result<String> {
    let mut out = String::new();
    for session in sessions {
        out.push_str(&serde_json::to_string(session)?);
        out.push('\n');
    }
    Ok(out)
}

fn format_sessions_table(sessions: &[session_registry::SessionInfo]) -> String {
    let mut out = String::new();
    if sessions.is_empty() {
        out.push_str("No sessions.\n");
        return out;
    }
    out.push_str(&format!(
        "{:<4} {:<24} {:<10} {:<10} {:<10} {:<16} {:<20} {:<20} {}\n",
        "#", "SESSION", "STATUS", "CLI", "ORIGIN", "LOCATION", "PANE", "UPDATED", "TITLE"
    ));
    for (i, session) in sessions.iter().enumerate() {
        let sid = session.session_id.to_string();
        let short_sid = if sid.len() > 24 {
            &sid[..24]
        } else {
            sid.as_str()
        };
        out.push_str(&format!(
            "{:<4} {:<24} {:<10} {:<10} {:<10} {:<16} {:<20} {:<20} {}\n",
            i + 1,
            short_sid,
            status_label(session.status.as_ref()),
            cli_source_label(session.cli_source.as_ref()),
            origin_label(session.origin.as_ref()),
            location_label(&session.location),
            session.pane_session_id.as_deref().unwrap_or("-"),
            updated_label(session),
            session.title.as_deref().unwrap_or("-"),
        ));
    }
    out
}

fn status_label(status: Option<&agent_sessions::AgentStatus>) -> String {
    status
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn cli_source_label(source: Option<&agent_sessions::CliSource>) -> String {
    match source {
        Some(agent_sessions::CliSource::Claude) => "Claude".to_string(),
        Some(agent_sessions::CliSource::Codex) => "Codex".to_string(),
        Some(agent_sessions::CliSource::Copilot) => "Copilot".to_string(),
        Some(agent_sessions::CliSource::Gemini) => "Gemini".to_string(),
        Some(agent_sessions::CliSource::OpenCode) => "OpenCode".to_string(),
        Some(agent_sessions::CliSource::Unknown(s)) if !s.is_empty() => s.clone(),
        _ => "-".to_string(),
    }
}

/// Render a `SessionOrigin` for the `wta sessions list` table. `None`
/// is the on-the-wire representation for "field absent" (legacy rows
/// or notification paths that don't carry origin) — we print `-`
/// rather than fabricating an origin so the operator can tell
/// "untagged" from "shell".
fn origin_label(origin: Option<&agent_sessions::SessionOrigin>) -> &'static str {
    match origin {
        Some(agent_sessions::SessionOrigin::AgentPane) => "AgentPane",
        Some(agent_sessions::SessionOrigin::Unknown) => "Shell",
        None => "-",
    }
}

/// Render a `SessionLocation` for the `wta sessions list` table: `host`
/// for Windows-profile sessions, `wsl:<distro>` for sessions discovered
/// inside a WSL distro.
fn location_label(location: &agent_sessions::SessionLocation) -> String {
    match location {
        agent_sessions::SessionLocation::Host => "host".to_string(),
        agent_sessions::SessionLocation::Wsl { distro } => format!("wsl:{distro}"),
    }
}

/// Render the UPDATED column. Prefers the `updated_at` ISO string (set for
/// live sessions); for history-scanned rows that only carry an epoch-ms
/// `last_activity_at_ms`, formats that as a `YYYY-MM-DD HH:MM` UTC stamp so
/// the column isn't blank. `-` when neither is available.
fn updated_label(s: &session_registry::SessionInfo) -> String {
    if let Some(u) = s.updated_at.as_deref() {
        return u.to_string();
    }
    match s.last_activity_at_ms {
        Some(ms) => format_epoch_ms_utc(ms),
        None => "-".to_string(),
    }
}

/// Format epoch milliseconds as `YYYY-MM-DD HH:MM` (UTC) without pulling in a
/// date crate. Uses Howard Hinnant's `civil_from_days` algorithm.
fn format_epoch_ms_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hour, min) = (tod / 3600, (tod % 3600) / 60);
    // civil_from_days: days since 1970-01-01 -> (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}")
}

// ─── Output helpers ─────────────────────────────────────────────────────────

fn print_output(val: &serde_json::Value, json_mode: bool, formatter: fn(&serde_json::Value)) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string())
        );
    } else {
        formatter(val);
    }
}

fn format_windows_human(val: &serde_json::Value) {
    if let Some(windows) = val.get("windows").and_then(|v| v.as_array()) {
        if windows.is_empty() {
            println!("{}", t!("output.no_windows"));
            return;
        }
        println!("{}", t!("output.header.windows"));
        for w in windows {
            let id = json_str_or_num(w, "window_id");
            let title = w.get("title").and_then(|v| v.as_str()).unwrap_or("-");
            let focused = w
                .get("is_focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            println!(
                "{:<12} {:<30} {}",
                id,
                title,
                if focused { "*" } else { "" }
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    }
}

fn format_tabs_human(val: &serde_json::Value) {
    if let Some(tabs) = val.get("tabs").and_then(|v| v.as_array()) {
        if tabs.is_empty() {
            println!("{}", t!("output.no_tabs"));
            return;
        }
        println!("{}", t!("output.header.tabs"));
        for t in tabs {
            let id = json_str_or_num(t, "tab_id");
            let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("-");
            let focused = t
                .get("is_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            println!(
                "{:<10} {:<30} {}",
                id,
                title,
                if focused { "*" } else { "" }
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    }
}

fn format_panes_human(val: &serde_json::Value) {
    if let Some(panes) = val.get("panes").and_then(|v| v.as_array()) {
        if panes.is_empty() {
            println!("{}", t!("output.no_panes"));
            return;
        }
        println!("{}", t!("output.header.panes"));
        for p in panes {
            let id = json_str_or_num(p, "session_id");
            let pid = p
                .get("pid")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let active = p
                .get("is_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let size = p.get("size");
            let rows = size
                .and_then(|s| s.get("rows"))
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let cols = size
                .and_then(|s| s.get("columns"))
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<10} {:<8} {:<8} {:<10} {}",
                id,
                pid,
                if active { "*" } else { "" },
                rows,
                cols
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    }
}

fn format_active_pane(val: &serde_json::Value) {
    let id = json_str_or_num(val, "session_id");
    let tab = json_str_or_num(val, "tab_id");
    let win = json_str_or_num(val, "window_id");
    println!(
        "{}",
        t!("output.active_pane", pane = id, tab = tab, window = win)
    );
}

fn format_pane_status(val: &serde_json::Value) {
    let state = val
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let running = state == "running";
    let exit_code = val
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    let pid = val
        .get("pid")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    if running {
        println!("{}", t!("output.pane_running", pid = pid));
    } else {
        println!("{}", t!("output.pane_exited", code = exit_code, pid = pid));
    }
}

fn format_created_tab(val: &serde_json::Value) {
    let tab_id = json_str_or_num(val, "tab_id");
    let pane_id = json_str_or_num(val, "session_id");
    println!(
        "{}",
        t!("output.created_tab", tab_id = tab_id, pane_id = pane_id)
    );
}

fn format_created_pane(val: &serde_json::Value) {
    let pane_id = json_str_or_num(val, "session_id");
    println!("{}", t!("output.created_pane", pane_id = pane_id));
}

/// Extract a field that may be string or number from JSON.
fn json_str_or_num(val: &serde_json::Value, key: &str) -> String {
    match val.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "-".to_string(),
    }
}

// ─── pipe-id / set-env: surface the inherited WT_COM_CLSID env var ─────────

fn run_pipe_id(json_mode: bool) -> Result<()> {
    let clsid = std::env::var("WT_COM_CLSID")
        .map_err(|_| anyhow::anyhow!("{}", t!("error.wt_com_clsid_not_set")))?;
    if json_mode {
        let val = json!({ "connection_id": clsid, "env": "WT_COM_CLSID" });
        println!("{}", serde_json::to_string_pretty(&val)?);
    } else {
        println!("{}", clsid);
    }
    Ok(())
}

fn run_set_env(shell_type: &str) -> Result<()> {
    let clsid = std::env::var("WT_COM_CLSID")
        .map_err(|_| anyhow::anyhow!("{}", t!("error.wt_com_clsid_not_set")))?;

    match shell_type {
        "bash" | "sh" | "zsh" => {
            println!("export WT_COM_CLSID='{}'", clsid);
            eprintln!("# Run: eval \"$(wta set-env)\"");
        }
        "powershell" | "pwsh" | "ps" => {
            println!("$env:WT_COM_CLSID = '{}'", clsid);
            eprintln!("# Run: wta set-env -s powershell | Invoke-Expression");
        }
        "cmd" => {
            println!("set WT_COM_CLSID={}", clsid);
            eprintln!("REM Run in a for /f loop or copy-paste");
        }
        "fish" => {
            println!("set -gx WT_COM_CLSID '{}'", clsid);
            eprintln!("# Run: wta set-env -s fish | source");
        }
        other => {
            bail!("{}", t!("error.unknown_shell_type", shell = other));
        }
    }

    Ok(())
}

// ─── Listen mode ────────────────────────────────────────────────────────────

async fn run_listen(pane_filter: Option<&str>) -> Result<()> {
    let channel = connect_channel().await?;
    let arc_channel = std::sync::Arc::new(channel);

    // Subscribe to events and start the background reader.
    let mut event_rx = arc_channel.subscribe_events();
    arc_channel.start_reader().await;

    // Send any request to trigger lazy page event registration on the server.
    let _ = arc_channel.request("get_capabilities", json!({})).await;

    eprintln!("Connected. Listening for events... (Ctrl+C to stop)");
    if let Some(pane) = pane_filter {
        eprintln!("Filtering: pane_id={}", pane);
    }

    while let Some(msg) = event_rx.recv().await {
        // Only print events, skip responses.
        if msg.get("type").and_then(|v| v.as_str()) != Some("event") {
            continue;
        }

        // Optional pane_id filter.
        if let Some(filter) = pane_filter {
            let pane_id = msg
                .get("params")
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str());
            if pane_id != Some(filter) {
                continue;
            }
        }

        // Re-serialize to guarantee compact single-line JSON (safe for jq piping).
        println!("{}", serde_json::to_string(&msg).unwrap_or_default());
    }

    eprintln!("Event stream closed.");
    Ok(())
}

// ─── Delegate prompt to new tab agent ────────────────────────────────────────

async fn run_delegate(
    prompt: Option<&str>,
    agent_cmd: &str,
    delegate_agent_cmd: Option<&str>,
    delegate_model: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    // Log the prompt length, not the text — the prompt is user content.
    tracing::info!(
        prompt_chars = prompt.map(|p| p.chars().count()),
        agent = agent_cmd,
        "run_delegate started"
    );
    tracing::trace!(target: "delegate.content", prompt = ?prompt, "run_delegate prompt");

    let (debug_tx, _) = tokio::sync::mpsc::unbounded_channel::<app::DebugMessage>();
    let channel = match connect_to_wt_protocol(debug_tx).await {
        Ok(ch) => {
            tracing::info!("WT protocol connected");
            ch
        }
        Err(e) => {
            tracing::warn!(error = %e, "WT protocol connection FAILED");
            return Err(e);
        }
    };
    let shell_mgr = ShellManager::new()
        .with_wt_channel(Arc::new(channel) as Arc<dyn shell::wt_channel::WtChannel>);

    match delegate_with_context(
        &shell_mgr,
        prompt,
        agent_cmd,
        delegate_agent_cmd,
        delegate_model,
        cwd,
    )
    .await
    {
        Ok(()) => {
            tracing::info!("delegate OK");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "delegate FAILED");
            Err(e)
        }
    }
}

/// The WSL distro backing the delegate's active pane, if any — i.e. its shell,
/// reported via `OSC 9001;ShellType`, is `wsl:<distro>` with a **non-empty**
/// distro name (e.g. `wsl:Ubuntu`). The shipped Bash shell integration only
/// emits `wsl:<distro>` when `$WSL_DISTRO_NAME` is set (otherwise it reports
/// `bash`), so a bare `wsl:` never occurs in practice; rejecting it defensively
/// keeps us from ever building a `wsl -d "" …` command. Returns `None` when the
/// pane is missing, has no `shell` field, or the shell is anything else
/// (PowerShell, cmd, …).
/// Whether the delegate agent CLI is actually available inside `distro`.
///
/// PR375 routes a `?<prompt>` from a WSL pane into the distro
/// (`wsl -d <distro> -- bash -lc "<agent> …"`), but the agent may be installed
/// only on the Windows host — the Settings UI verifies the host CLI, never the
/// distro. Probe the distro under a **login** shell (`bash -lc`): the shipped
/// integration and the common CLI installs (npm-global, snap, `~/.local/bin`)
/// only put the agent on the login PATH, so a non-login `bash -c` would miss it.
/// The probe resolves the agent's PATH location and accepts it only when it is a
/// native Linux install — a Windows CLI leaking in via `appendWindowsPath`
/// (resolving under `/mnt/…`) is rejected, so it falls back to the host CLI that
/// can actually run it (see [`wsl_agent_probe_script`]). Returns `false` on any
/// spawn/exec error or timeout so the caller falls back to the known-good
/// Windows host CLI instead of launching a doomed in-distro command that would
/// silently drop the prompt.
async fn wsl_delegate_agent_available(distro: &str, agent_exe: &str) -> bool {
    crate::agent_check::find_wsl_exe(distro, agent_exe)
        .await
        .is_some()
}

/// Whether the delegate agent should be treated as launchable for the active
/// pane's *target* environment.
///
/// `host_launchable` comes from [`crate::coordinator::delegate_command_launchable`],
/// which only inspects the Windows PATH. `wsl_agent_available` is true when the
/// active pane is a WSL distro **and** the agent CLI is installed inside it (see
/// [`wsl_delegate_agent_available`]). Either path makes the delegate
/// launchable: the Windows host, or the in-distro CLI. Without the WSL term a
/// Copilot/Claude installed only in the distro would be treated as
/// non-launchable and silently drop its `?<prompt>` text; with it, a WSL pane
/// whose distro lacks the CLI still falls through to the host term rather than
/// being force-routed into a doomed in-distro launch. The prompt-enrichment and
/// session-pin gates in `delegate_with_context` both key off this value.
fn delegate_launchable_for_target(host_launchable: bool, wsl_agent_available: bool) -> bool {
    host_launchable || wsl_agent_available
}

/// Max bytes of captured terminal context baked into a delegate prompt.
///
/// The enriched prompt rides the `wt_create_tab` commandline (base64-encoded).
/// Windows caps a process commandline at ~32,767 chars, and base64 inflates by
/// 4/3, so an unbounded 30-line capture from a very wide pane could overflow it
/// and fail the launch with "filename or extension is too long". Capping the
/// context keeps the encoded commandline comfortably under that limit; the user
/// prompt itself is assumed small.
const MAX_DELEGATE_CONTEXT_BYTES: usize = 12 * 1024;

/// Trim captured terminal context to at most `max_bytes`, including the
/// truncation marker, while keeping the **tail** (most recent output). Cuts on a
/// UTF-8 char boundary. If the marker does not fit, returns only the valid tail.
fn cap_delegate_context(context: &str, max_bytes: usize) -> String {
    if context.len() <= max_bytes {
        return context.to_string();
    }
    const TRUNCATION_MARKER: &str = "…(truncated)\n";
    let marker = if TRUNCATION_MARKER.len() <= max_bytes {
        TRUNCATION_MARKER
    } else {
        ""
    };
    let tail_bytes = max_bytes - marker.len();
    let mut start = context.len() - tail_bytes;
    while start < context.len() && !context.is_char_boundary(start) {
        start += 1;
    }
    format!("{marker}{}", &context[start..])
}

/// Shared delegation logic: enrich the prompt with the active pane's recent
/// output (when available), build the delegate-agent commandline, and create a
/// new tab to launch it. WT's GetActivePane already resolves the agent pane to
/// the user's working pane, so a single query is enough.
async fn delegate_with_context(
    shell_mgr: &ShellManager,
    prompt: Option<&str>,
    agent_cmd: &str,
    delegate_agent_cmd: Option<&str>,
    delegate_model: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    let delegate_agents = crate::coordinator::default_delegate_agent_runtimes(
        delegate_agent_cmd,
        Some(agent_cmd),
        delegate_model,
    );
    let runtime = delegate_agents
        .first()
        .ok_or_else(|| anyhow::anyhow!("no delegate agent configured"))?;

    // Pre-flight: can the configured delegate agent actually be launched? A
    // misconfigured / nonexistent command still gets its own tab and stays
    // there showing the real failure — cmd's "'<agent>' is not recognized …",
    // then WT's "[process exited with code 1] … press Enter to restart" — just
    // like mistyping a command in any shell. WT keeps a non-zero-exit pane open
    // under closeOnExit=automatic, so there's nothing to "fix" for the common
    // case; we do NOT open a second, canned-message tab.
    //
    // The flag is only used to keep a doomed launch OUT of the prompt-baking
    // path below. Baking the active pane's output into `cmd /c <agent>
    // -i "<context>"` is fragile: a stray `"`/`&` in that arbitrary text can
    // unbalance cmd's quote tracking so cmd runs a trailing token and exits 0,
    // which — under closeOnExit=automatic — closes the pane before the error is
    // readable (the original "flash shut"). A bare `cmd /c <agent>` instead
    // fails cleanly with a non-zero code and stays put.
    let launchable = crate::coordinator::delegate_command_launchable(&runtime.commandline);

    // A WSL pane runs the agent *inside the distro* (`wsl -d <distro> -- …`), so
    // the Windows-host launchable check does not apply to it. Fetch the active
    // pane up front so the gate below and the WSL branch further down can see
    // it. See `delegate_launchable_for_target`.
    let active = shell_mgr.wt_get_active_pane().await.ok();

    // If the active pane is a WSL distro, prefer running the agent inside it —
    // but only when the agent CLI is actually installed there. Otherwise, fall
    // back to the Windows host CLI (which the Settings UI already verified is
    // installed): an in-distro launch would just print "<agent>: command not
    // found" and drop the prompt. Probe the distro once, up front, so the
    // launchable gate, the WSL branch, and the host fallback all agree.
    let wsl_distro: Option<String> =
        crate::agent_source::active_pane_wsl_distro(active.as_ref()).map(str::to_string);
    let wsl_agent_available = match wsl_distro.as_deref() {
        Some(distro) => {
            let agent_exe =
                crate::coordinator::split_windows_commandline(runtime.commandline.trim())
                    .into_iter()
                    .next()
                    .unwrap_or_default();
            let available = wsl_delegate_agent_available(distro, &agent_exe).await;
            if !available {
                tracing::info!(
                    target: "delegate",
                    distro,
                    agent = %agent_exe,
                    "delegate agent not available in WSL distro — falling back to Windows host CLI",
                );
            }
            available
        }
        None => false,
    };

    let launchable_for_target = delegate_launchable_for_target(launchable, wsl_agent_available);

    if !launchable_for_target {
        // Log only the executable (first token), never the full commandline: a
        // custom agent command can embed tokens/credentials that shouldn't land
        // in the log. The full commandline stays trace-only (below).
        let exe = crate::coordinator::split_windows_commandline(&runtime.commandline)
            .into_iter()
            .next()
            .unwrap_or_default();
        tracing::warn!(
            target: "delegate",
            agent = %exe,
            "delegate agent not launchable — opening its tab with the bare command so the real error stays visible",
        );
    }

    // Pin a session id we choose, so the launched CLI writes its session under a
    // known id and we can bind it to the pane without hooks. Only for agents that
    // advertise `--session-id` (Copilot/Claude/Gemini); `None` otherwise. We
    // identify the agent with `resolve_agent_id_from_cmd` (not a naive
    // `split_whitespace`) so quoted/space-containing paths and adapter launches
    // resolve correctly -- and so this decision matches the one the command
    // builder makes when it appends the flag, keeping the pinned id and the
    // actual launch flag in agreement. A non-launchable command will never
    // produce a session, so skip pinning (and the born-bound registration
    // below). A WSL pane is launchable via the distro, so it pins like any
    // other supported agent.
    let pinned_session_id: Option<String> = if launchable_for_target {
        crate::agent_registry::lookup_profile_by_id(
            crate::agent_registry::resolve_agent_id_from_cmd(&runtime.commandline),
        )
        .new_session_id_flag
        .map(|_| uuid::Uuid::new_v4().to_string())
    } else {
        None
    };

    // ── Enriched prompt ──────────────────────────────────────────────────
    // Bake the active pane's output into the prompt when the agent is
    // launchable for the target environment — the Windows pre-flight, or a WSL
    // pane that will run the agent inside the distro. A non-launchable agent
    // stays in the bare-command path so its failure is clean and visible.
    let enriched_prompt: Option<String> = match prompt {
        Some(prompt) if !prompt.trim().is_empty() && launchable_for_target => {
            let active_pane_id = active
                .as_ref()
                .and_then(|v| v.get("session_id"))
                .and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                });

            let pane_context = if let Some(ref pane_id) = active_pane_id {
                match shell_mgr.wt_read_pane_output(pane_id, Some(30)).await {
                    Ok(value) => value
                        .get("content")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string()),
                    Err(_) => None,
                }
            } else {
                None
            };

            // The `## Terminal Context (pane …)` heading is built from
            // `TERMINAL_CONTEXT_TITLE_MARKER` (the single source of truth) so the
            // master-side title filter (`host_titles_via_acp`) can recognise —
            // and skip — this injected block if an agent CLI echoes the first
            // user message back as a `session/list` title.
            Some(match (pane_context, active_pane_id) {
                (Some(context), Some(pane_id)) => format!(
                    "{}\n\n{}{})\n```\n{}\n```",
                    prompt,
                    crate::session_registry::TERMINAL_CONTEXT_TITLE_MARKER,
                    pane_id,
                    cap_delegate_context(&context, MAX_DELEGATE_CONTEXT_BYTES)
                ),
                _ => prompt.to_string(),
            })
        }
        _ => None,
    };

    // ── Windows-native commandline (fallback for non-WSL) ────────────────
    let commandline = crate::coordinator::build_delegate_launch_commandline_with_session(
        runtime,
        enriched_prompt.as_deref(),
        pinned_session_id.as_deref(),
    )?;

    // ── WSL delegate path ───────────────────────────────────────────────────
    // Taken only when the active pane is a WSL distro AND the agent CLI is
    // installed inside it (`wsl_agent_available`). Build a WSL-native command
    // that runs the agent CLI inside the distro (using the Linux toolchain and
    // filesystem). When the distro lacks the CLI we fall through to the Windows
    // host path below, which sanitizes the pane's POSIX cwd to the Windows home.
    //
    // Delivery (see `build_wsl_delegate_commandline`): the prompt rides as an
    // inline base64 payload decoded in-distro — base64's alphabet has no shell
    // syntax characters and no `%`, so it survives WT's `ExpandEnvironmentStringsW`
    // and the `wsl.exe` interop's expansion pass. The bash command is escaped for
    // that pass, then wrapped once for Windows `CommandLineToArgvW`:
    //   1. build_wsl_delegate_commandline() → base64-inline bash command,
    //      pre-escaped for the wsl.exe expansion pass (`\`, `$`, backtick)
    //   2. quote_windows_commandline_arg() → Windows CommandLineToArgvW escaping
    //      → embed in format!("bash -lc {}")
    //   3. → wsl -d <distro> --cd "<cwd>" -- bash -lc <escaped>
    //
    // Composability works because the two layers have disjoint special
    // characters: ' is special to bash, " is special to Windows.
    if wsl_agent_available {
        // `wsl_agent_available` implies both `wsl_distro` and `active` are set
        // (it is derived from them above); the `if let` is a defensive guard
        // that falls through to the host path in the impossible None case.
        if let (Some(distro), Some(active_pane)) = (wsl_distro.as_deref(), active.as_ref()) {
            let wsl_agent_cmd = crate::coordinator::build_wsl_delegate_commandline(
                runtime,
                enriched_prompt.as_deref(),
                pinned_session_id.as_deref(),
            )?;
            let escaped = crate::coordinator::quote_windows_commandline_arg(&wsl_agent_cmd);
            let login_invocation = format!("bash -lc {}", escaped);
            let distro_arg = crate::coordinator::quote_windows_commandline_arg(distro);
            let wsl_cwd = active_pane
                .get("cwd")
                .and_then(|v| v.as_str())
                .filter(|s| s.starts_with('/') && !s.contains('"'));
            let wsl_commandline = match wsl_cwd {
                Some(cwd) => {
                    format!("wsl -d {distro_arg} --cd \"{cwd}\" -- {login_invocation}")
                }
                None => format!("wsl -d {distro_arg} -- {login_invocation}"),
            };

            tracing::debug!("delegate_with_context: launching in WSL ({distro})");
            tracing::trace!(
                target: "delegate.content",
                commandline = %wsl_commandline,
                "wsl delegate commandline",
            );

            let create_resp = shell_mgr
                .wt_create_tab(Some(&wsl_commandline), None, None, None)
                .await?;
            let pane_guid = create_resp
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            tracing::info!(
                target: "delegate",
                pane_guid = ?pane_guid,
                pinned = ?pinned_session_id,
                distro,
                "delegate WSL tab created",
            );

            // Born-bound registration for the WSL delegate session — but only
            // when WSL sessions are enabled. The whole WSL surface is gated on
            // `WTA_WSL_SESSIONS`; with it off we must not surface *any* WSL
            // session, born-bound delegate rows included (the master-side
            // historical WSL scan is already gated, so skipping this registration
            // keeps a `?<prompt>` WSL delegate out of the session view). The tab
            // still opens and the CLI still runs — it's just untracked, exactly
            // like every other WSL session while the flag is off.
            //
            // The distro is threaded through so the master stamps the row
            // `Wsl { distro }` → the session view shows the `[WSL-<distro>]`
            // prefix.
            if crate::history_loader::wsl_sessions_enabled() {
                if let (Some(sid), Some(pane)) =
                    (pinned_session_id.as_deref(), pane_guid.as_deref())
                {
                    register_launched_session_with_master(
                        sid,
                        pane,
                        &runtime.id,
                        wsl_cwd.or(cwd),
                        Some(distro),
                    )
                    .await;
                }
            }
            return Ok(());
        }
    }

    // ── Windows (existing) path ────────────────────────────────────────────
    // The delegate always launches a Windows agent CLI (Copilot/Claude/Gemini).
    // If the active pane is WSL, `cwd` is a POSIX path (e.g. "/home/user") that
    // a Windows process can't use as a working directory — sanitize it to the
    // Windows home so the CLI still launches.
    tracing::debug!("delegate_with_context: launching");
    tracing::trace!(target: "delegate.content", commandline, "delegate_with_context commandline");

    let windows_home = std::env::var("USERPROFILE").ok();
    let sanitized_cwd =
        crate::coordinator::sanitize_windows_agent_cwd(cwd, windows_home.as_deref());

    let create_resp = shell_mgr
        .wt_create_tab(Some(&commandline), sanitized_cwd.as_deref(), None, None)
        .await?;
    let pane_guid = create_resp
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    tracing::info!(
        target: "delegate",
        pane_guid = ?pane_guid,
        pinned = ?pinned_session_id,
        "delegate tab created",
    );

    // Born-bound registration: WTA created this tab and pinned the CLI's
    // session id, so we know (session id, pane) at launch. Tell master to
    // bind them with no hooks (best-effort). Only when both are known —
    // i.e. a pinnable agent (Copilot/Claude/Gemini) whose tab was created.
    if let (Some(sid), Some(pane)) = (pinned_session_id.as_deref(), pane_guid.as_deref()) {
        register_launched_session_with_master(sid, pane, &runtime.id, cwd, None).await;
    }

    Ok(())
}

// ─── Default ACP TUI mode ───────────────────────────────────────────────────

/// Drive the standard ACP TUI but use `pipe_name` as the ACP transport
/// (helper mode). The helper attaches to wta-master over the supplied
/// named pipe and forwards ACP traffic over it.
pub(crate) async fn run_default_tui_over_pipe(mut cli: Cli, pipe_name: String) -> Result<()> {
    tracing::info!(target: "helper", pipe = %pipe_name, "=== wta-helper starting (TUI) ===");
    let agent_source = crate::agent_source::AgentSource::from_wire(
        cli.agent_source.as_deref(),
        cli.agent_wsl_distro.as_deref(),
        cli.agent_ssh_target.as_deref(),
        cli.agent_remote_session.as_deref(),
    );
    cli.agent_source_cwd =
        crate::agent_source::resolve_source_cwd(&agent_source, cli.agent_source_cwd.as_deref())
            .await;

    // Debug channel for the helper TUI.
    let (debug_tx, debug_rx) = tokio::sync::mpsc::unbounded_channel::<app::DebugMessage>();

    let mut shell_mgr = ShellManager::new().with_agent_source(agent_source);
    let mut wt_event_rx = None;
    let mut wt_protocol_channel: Option<Arc<CliChannel>> = None;
    let wt_connected = match connect_to_wt_protocol(debug_tx.clone()).await {
        Ok(channel) => {
            tracing::info!(target: "helper", "Connected to WT COM protocol — subscribing to events");
            wt_event_rx = Some(channel.subscribe_events());
            let cli_arc = Arc::new(channel);
            wt_protocol_channel = Some(Arc::clone(&cli_arc));
            shell_mgr =
                shell_mgr.with_wt_channel(cli_arc.clone() as Arc<dyn shell::wt_channel::WtChannel>);
            true
        }
        Err(e) => {
            tracing::warn!(target: "helper", error = %e, "NO WT protocol connection");
            false
        }
    };
    let shell_mgr = Arc::new(shell_mgr);

    let pane_identity = if wt_connected {
        discover_pane_identity(&shell_mgr).await
    } else {
        None
    };

    // Connection failures to wta-master (pipe connect give-up, ACP initialize
    // timeout/failure) are logged at their source (target=helper) and again in
    // `run_acp_tui_mode`'s exit branch, which `process::exit`s rather than
    // returning Err — so there's no point wrapping the result here.
    run_acp_tui_mode(
        cli,
        shell_mgr,
        wt_connected,
        debug_rx,
        pane_identity,
        wt_event_rx,
        wt_protocol_channel,
        pipe_name,
    )
    .await
}

// ─── Existing functions (preserved) ─────────────────────────────────────────

/// Discover our own pane identity by matching our PID against WT's pane list.
async fn discover_pane_identity(shell_mgr: &ShellManager) -> Option<(String, String, String)> {
    let our_pid = std::process::id();

    // WT IDs may arrive as JSON strings or numbers (COM returns numeric) — accept both.
    fn id_str(v: Option<&serde_json::Value>) -> Option<String> {
        match v {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    }

    let windows = shell_mgr.wt_list_windows().await.ok()?;
    let windows_arr = windows.get("windows")?.as_array()?;

    for win in windows_arr {
        let window_id = match id_str(win.get("window_id")) {
            Some(w) => w,
            None => continue,
        };
        let tabs = shell_mgr.wt_list_tabs(&window_id).await.ok()?;
        let tabs_arr = tabs.get("tabs")?.as_array()?;

        for tab in tabs_arr {
            let tab_id_str = match id_str(tab.get("tab_id")) {
                Some(t) => t,
                None => continue,
            };
            let panes = shell_mgr
                .wt_list_panes(&tab_id_str, Some(&window_id))
                .await
                .ok()?;
            let panes_arr = panes.get("panes")?.as_array()?;

            for pane in panes_arr {
                if let Some(pid) = pane.get("pid").and_then(|v| v.as_u64()) {
                    if pid == our_pid as u64 {
                        let pane_id = match id_str(pane.get("session_id")) {
                            Some(p) => p,
                            None => continue,
                        };
                        return Some((pane_id, tab_id_str.clone(), window_id.to_string()));
                    }
                }
            }
        }
    }
    None
}

struct TuiRestoreGuard {
    armed: bool,
}

impl TuiRestoreGuard {
    fn new() -> Self {
        Self { armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TuiRestoreGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        // Agent panes start with alternate-scroll enabled, so restore that known state.
        let _ = write!(stdout, "\x1b[?1007h");
        let _ = stdout.flush();
        let _ = execute!(
            stdout,
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen,
            Show
        );
    }
}

async fn run_acp_tui_mode(
    cli: Cli,
    shell_mgr: Arc<ShellManager>,
    wt_connected: bool,
    debug_rx: tokio::sync::mpsc::UnboundedReceiver<app::DebugMessage>,
    pane_identity: Option<(String, String, String)>,
    wt_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>,
    wt_protocol_channel: Option<Arc<CliChannel>>,
    connect_master_pipe: String,
) -> Result<()> {
    enable_raw_mode()?;
    let mut restore_guard = TuiRestoreGuard::new();
    let mut stdout = io::stdout();
    // Keep mouse capture off so native click-drag selection continues to work.
    // Disable xterm alternate-scroll mode while the TUI is active so wheel
    // events are not translated into the Up/Down keys used by input history.
    execute!(stdout, EnterAlternateScreen)?;
    write!(stdout, "\x1b[?1007l")?;
    stdout.flush()?;
    // Deliberately do NOT emit `OSC 11` to force a background color: the pane
    // must inherit the profile's color scheme background so it tracks the
    // user's theme like any other pane (#234). Cells render on the terminal's
    // default (scheme) background; `Color::Reset` resolves to it.
    // Steady block (DECSCUSR Ps=2): solid filled rectangle, no blink.
    // Survives the alt-screen swap; restored on exit below.
    execute!(stdout, SetCursorStyle::SteadyBlock)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_acp_app(
        &mut terminal,
        cli,
        shell_mgr,
        wt_connected,
        debug_rx,
        pane_identity,
        wt_event_rx,
        wt_protocol_channel,
        connect_master_pipe,
    )
    .await;

    disable_raw_mode()?;
    // WT does not implement xterm private-mode save/restore (`?1007s`/`?1007r`).
    // Agent panes start with alternate-scroll enabled, so restore that known state.
    write!(terminal.backend_mut(), "\x1b[?1007h")?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    restore_guard.disarm();

    if let Err(e) = result {
        // This is the real exit point for a TUI/helper failure (connection
        // failures to wta-master propagate up to here). `process::exit` below
        // bypasses both `main()`'s catch-all and any caller's wrapper, so log
        // it here before exiting — it lands in this process's log file
        // (wta-main_helper-{pid}.log in helper mode).
        tracing::error!(error = ?e, "wta TUI exiting with error");
        eprintln!("Error: {e:?}");
        // Flush the file appender — process::exit skips the guard drop.
        logging::shutdown_flush();
        std::process::exit(1);
    }
    Ok(())
}

async fn run_test_pipe() -> Result<()> {
    use shell::wt_channel::WtChannel;

    println!("Connecting to Windows Terminal protocol...");
    let channel = connect_channel().await?;
    println!("Connected and authenticated!\n");

    let result: serde_json::Value = channel
        .request("list_windows", serde_json::json!({}))
        .await?;
    println!("list_windows:");
    println!("{}\n", serde_json::to_string_pretty(&result)?);

    let result: serde_json::Value = channel
        .request("get_capabilities", serde_json::json!({}))
        .await?;
    println!("get_capabilities:");
    println!("{}", serde_json::to_string_pretty(&result)?);

    Ok(())
}

/// Try to connect to the WT protocol via the inherited WT_COM_CLSID env var.
async fn connect_to_wt_protocol(
    debug_tx: tokio::sync::mpsc::UnboundedSender<app::DebugMessage>,
) -> Result<shell::wt_channel::CliChannel> {
    use shell::wt_channel::CliChannel;
    let channel = CliChannel::connect().await?;
    Ok(channel.with_debug_sender(debug_tx))
}

/// Show Windows Terminal protocol connection info and pane identity.
async fn run_info_mode() -> Result<()> {
    use shell::wt_channel::WtChannel;

    println!("Windows Terminal Protocol Info");
    println!("========================================");

    let clsid = match std::env::var("WT_COM_CLSID") {
        Ok(v) => v,
        Err(_) => {
            println!("  Status: Not running inside Windows Terminal");
            println!("  (WT_COM_CLSID not set)");
            return Ok(());
        }
    };

    println!("  COM CLSID: {}", clsid);
    println!("  Source: WT_COM_CLSID env var");
    println!();

    let channel = match CliChannel::connect().await {
        Ok(ch) => ch,
        Err(e) => {
            println!("  Connection failed: {}", e);
            return Ok(());
        }
    };

    let our_pid = std::process::id();
    let mut pane_info: Option<(String, String, String)> = None;
    let mut total_windows = 0u32;
    let mut total_tabs = 0u32;
    let mut total_panes = 0u32;

    if let Ok(windows) = channel.request("list_windows", serde_json::json!({})).await {
        if let Some(windows_arr) = windows.get("windows").and_then(|v| v.as_array()) {
            total_windows = windows_arr.len() as u32;

            for win in windows_arr {
                let window_id = match win.get("window_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => continue,
                };

                if let Ok(tabs) = channel
                    .request("list_tabs", serde_json::json!({ "window_id": window_id }))
                    .await
                {
                    if let Some(tabs_arr) = tabs.get("tabs").and_then(|v| v.as_array()) {
                        total_tabs += tabs_arr.len() as u32;

                        for tab in tabs_arr {
                            let tab_id_str = match tab.get("tab_id") {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(serde_json::Value::Number(n)) => n.to_string(),
                                _ => continue,
                            };

                            if let Ok(panes) = channel
                                .request("list_panes", serde_json::json!({ "tab_id": tab_id_str }))
                                .await
                            {
                                if let Some(panes_arr) =
                                    panes.get("panes").and_then(|v| v.as_array())
                                {
                                    total_panes += panes_arr.len() as u32;

                                    for pane in panes_arr {
                                        if let Some(pid) = pane.get("pid").and_then(|v| v.as_u64())
                                        {
                                            if pid == our_pid as u64 {
                                                let pane_id = match pane.get("session_id") {
                                                    Some(serde_json::Value::String(s)) => s.clone(),
                                                    Some(serde_json::Value::Number(n)) => {
                                                        n.to_string()
                                                    }
                                                    _ => "?".to_string(),
                                                };
                                                pane_info = Some((
                                                    pane_id,
                                                    tab_id_str.clone(),
                                                    window_id.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some((pane_id, tab_id, window_id)) = pane_info {
        println!("Current Pane (PID {}):", our_pid);
        println!("  Window ID: {}", window_id);
        println!("  Tab ID:    {}", tab_id);
        println!("  Pane ID:   {}", pane_id);
    } else {
        println!("Current Pane (PID {}): not found in WT pane list", our_pid);
    }

    println!();
    println!("Summary:");
    println!(
        "  Windows: {}, Tabs: {}, Panes: {}",
        total_windows, total_tabs, total_panes
    );

    Ok(())
}

fn spawn_restart_agent_stack_forwarder(
    mut restart_rx: tokio::sync::mpsc::UnboundedReceiver<protocol::acp::client::RestartRequest>,
) {
    tokio::task::spawn_local(async move {
        while let Some(req) = restart_rx.recv().await {
            tracing::info!(
                target: "helper",
                new_agent = ?req.agent_cmd,
                "restart requested before ACP task is running; asking WT to force-restart the agent stack"
            );
            let evt = serde_json::json!({
                "type": "event",
                "method": "restart_agent_stack",
                "params": {},
            });
            crate::app::send_wt_protocol_event(evt.to_string());
        }
    });
}

async fn run_acp_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: Cli,
    shell_mgr: Arc<ShellManager>,
    mut wt_connected: bool,
    mut debug_rx: tokio::sync::mpsc::UnboundedReceiver<app::DebugMessage>,
    pane_identity: Option<(String, String, String)>,
    wt_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>,
    wt_protocol_channel: Option<Arc<CliChannel>>,
    connect_master_pipe: String,
) -> Result<()> {
    let agent_cmd = cli.agent.clone();
    let agent_source = crate::agent_source::AgentSource::from_wire(
        cli.agent_source.as_deref(),
        cli.agent_wsl_distro.as_deref(),
        cli.agent_ssh_target.as_deref(),
        cli.agent_remote_session.as_deref(),
    );
    let agent_source_cwd = cli.agent_source_cwd.clone();

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();

            let evt_tx = event_tx.clone();
            tokio::task::spawn_local(event::read_crossterm_events(evt_tx));

            let dbg_event_tx = event_tx.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = debug_rx.recv().await {
                    let _ = dbg_event_tx.send(app::AppEvent::DebugPipeMessage(msg));
                }
            });

            // Start the background protocol reader and trigger lazy event registration.
            // start_reader() claims stdout/stderr streams and must complete before any requests.
            // get_capabilities triggers _ensurePageEventsRegistered() on the WT server.
            if let Some(ref protocol_ch) = wt_protocol_channel {
                tracing::info!("start_reader: starting...");
                protocol_ch.start_reader().await;
                tracing::info!("start_reader: done, sending get_capabilities...");
                match protocol_ch.request("get_capabilities", serde_json::json!({})).await {
                    Ok(v) => tracing::info!(result = %v, "get_capabilities OK"),
                    Err(e) => {
                        tracing::warn!(error = %e, "get_capabilities FAILED");
                        wt_connected = false;
                        let _ = event_tx.send(app::AppEvent::WtProtocolFailure(format!(
                            "Terminal controls unavailable: installed components are incompatible. Repair or reinstall Intelligent Terminal. Details: {e}"
                        )));
                    }
                }
            } else {
                tracing::warn!("no wt_pipe_channel — events won't work");
            }

            // Background WT event reader: forwards push events from the protocol channel to the TUI.
            if let Some(mut wt_rx) = wt_event_rx {
                tracing::info!("wt_event_rx: starting background reader task");
                let wt_event_tx = event_tx.clone();
                tokio::task::spawn_local(async move {
                    while let Some(event_json) = wt_rx.recv().await {
                        let method = event_json
                            .get("method")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // The full event envelope carries `vt_sequence` (raw
                        // terminal output/scrollback) — keep it out of debug;
                        // log only the method there, full JSON at trace.
                        tracing::debug!(method = %method, "wt_event_rx: received event");
                        if method == "agent_paste_text" {
                            let mut redacted = event_json.clone();
                            let paste_len = redacted
                                .get("params")
                                .and_then(|p| p.get("text"))
                                .and_then(|v| v.as_str())
                                .map(str::len);
                            if let Some(paste_len) = paste_len {
                                if let Some(params) = redacted.get_mut("params").and_then(|v| v.as_object_mut()) {
                                    params.insert(
                                        "text".to_string(),
                                        serde_json::json!(format!("<redacted {} bytes>", paste_len)),
                                    );
                                }
                            }
                            tracing::trace!(target: "wt_event.content", event = %redacted, "wt_event_rx: full event");
                        } else {
                            tracing::trace!(target: "wt_event.content", event = %event_json, "wt_event_rx: full event");
                        }

                        let params = event_json
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        // Read `pane_id` (current name) with a fallback
                        // to `session_id` (the old name before the
                        // per-tab autofix routing PR renamed it). The
                        // C++ TerminalPage side now emits `pane_id` for
                        // `connection_state` / `vt_sequence`, but the
                        // wtcli `send-event` builder
                        // (`BuildSendEventJson`) was missed in that
                        // rename pass — `agent_event` envelopes from
                        // hook bridge still carried `session_id`.
                        // Without this fallback every hook event
                        // arrived with `pane_id = ""`, and downstream
                        // `route_agent_event_to_registry` collided all
                        // sessions on the empty-string key in
                        // `active_by_pane`, triggering spurious
                        // orphan-handover demotions whenever a second
                        // session started in the same window (e.g.
                        // session A → Ended the moment session B's
                        // first hook fires). Keep the fallback even
                        // after wtcli is fixed so an old wtcli build
                        // can talk to a new wta without surprises.
                        let pane_id = params
                            .get("pane_id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .or_else(|| params.get("session_id").and_then(|v| v.as_str()))
                            .unwrap_or("")
                            .to_string();
                        let tab_id = params
                            .get("tab_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let _ = wt_event_tx.send(app::AppEvent::WtEvent {
                            method,
                            pane_id,
                            tab_id,
                            params,
                        });
                    }
                });
            }

            let shell_mgr_for_recs = Arc::clone(&shell_mgr);

            // Cancel channel for Ctrl+C handling: App produces, ACP client
            // task consumes (one listener task inside the ACP client loop).
            let (cancel_tx, cancel_rx) = tokio::sync::mpsc::unbounded_channel();
            // /new channel: App emits a NewSessionForTab, the ACP client
            // drops the cached SessionId for that tab and re-issues
            // new_session(). The resulting SessionAttached event flows
            // back through event_tx like the lazy-create path.
            let (new_session_tx, new_session_rx) = tokio::sync::mpsc::unbounded_channel();
            // load_session channel: App emits a LoadSessionForTab in
            // response to WT's `load_session` event (the back-half of
            // the session management view's Shift+Enter -> "resume in
            // new tab's agent pane" flow). The ACP client calls
            // `conn.load_session` and binds the rehydrated session to
            // the tab via SessionAttached.
            let (load_session_tx, load_session_rx) = tokio::sync::mpsc::unbounded_channel();
            // Clone for the boot-time initial-load injection below. The
            // primary `load_session_tx` is moved into `App::new` further
            // down; this clone is used once (if `--initial-load-session-id`
            // was passed) to synthesize a LoadSessionForTab as soon as the
            // helper has finished its owner_tab_id seed. The receiver in
            // `run_acp_client_over_pipe` then drives `session/load` through
            // its standard runtime arm — no race vs. a separate VT
            // `load_session` broadcast.
            let initial_load_tx = load_session_tx.clone();
            // /restart channel: App emits a RestartRequest, the ACP client
            // kills the agent child process, drops the connection, and
            // respawns from scratch. State is cleaned up on both sides.
            let (restart_tx, restart_rx) = tokio::sync::mpsc::unbounded_channel();
            // reset_tab_session channel: App emits a DropSessionRequest when
            // WT tells us to release a tab's binding (Ctrl+C×2 hide path).
            // ACP client removes the SessionId from tab_to_session and
            // cancels any in-flight prompt for it; the next prompt on that
            // tab lazily creates a fresh session.
            let (drop_session_tx, drop_session_rx) = tokio::sync::mpsc::unbounded_channel();
            // tab-drag rename channel: App emits a RenameSessionRequest when
            // WT mints a new stable tab id for an existing tab (cross-window
            // tab drag). ACP client rekeys tab_to_session so the next prompt
            // on the dragged tab finds the existing ACP SessionId — without
            // this the agent loses turn context after a drag.
            let (rename_session_tx, rename_session_rx) =
                tokio::sync::mpsc::unbounded_channel();
            // Helper mode always speaks to wta-master, so the session-hook
            // channel is always live.
            let (session_hook_tx, session_hook_rx) = tokio::sync::mpsc::unbounded_channel();
            let (master_ext_tx, master_ext_rx) = tokio::sync::mpsc::unbounded_channel();

            // Seed the process-wide owner tab StableId so `inject_wta_pane_meta`
            // stamps `_meta.wta.owner_tab_id` on every session/new + session/load.
            // Master needs it to address `restart_agent_pane` crash-recovery
            // events by the same StableId C++ routes per-tab events with.
            protocol::acp::client::set_helper_owner_tab_id(cli.owner_tab_id.as_deref());

            let explicit_agent_id = cli
                .agent_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let canonical_agent_id: String = explicit_agent_id
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| {
                    agent_registry::resolve_agent_id_from_cmd(&agent_cmd).to_string()
                });
            let canonical_agent_source = if explicit_agent_id.is_some() {
                "--agent-id"
            } else {
                "resolved-from-cmd"
            };
            let initial_load_requested = cli
                .initial_load_session_id
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let initial_auth_agent = match cli
                .initial_auth_agent
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(requested) if cli.assume_master_down => {
                    tracing::warn!(
                        target: "initial_auth",
                        requested_agent = %requested,
                        "--initial-auth-agent ignored because --assume-master-down is active"
                    );
                    None
                }
                Some(requested) if cli.start_stashed => {
                    tracing::warn!(
                        target: "initial_auth",
                        requested_agent = %requested,
                        "--initial-auth-agent ignored because --start-stashed is active"
                    );
                    None
                }
                Some(requested) if cli.setup.is_some() => {
                    tracing::warn!(
                        target: "initial_auth",
                        requested_agent = %requested,
                        "--initial-auth-agent ignored because --setup is active"
                    );
                    None
                }
                Some(requested) if initial_load_requested => {
                    tracing::warn!(
                        target: "initial_auth",
                        requested_agent = %requested,
                        "--initial-auth-agent ignored because --initial-load-session-id is active"
                    );
                    None
                }
                Some(requested) => {
                    let requested_agent = requested.to_ascii_lowercase();
                    if requested_agent != canonical_agent_id {
                        tracing::warn!(
                            target: "initial_auth",
                            requested_agent = %requested_agent,
                            current_agent = %canonical_agent_id,
                            "--initial-auth-agent ignored because it does not match the effective agent"
                        );
                        None
                    } else if requested_agent != "copilot" {
                        tracing::warn!(
                            target: "initial_auth",
                            requested_agent = %requested_agent,
                            "--initial-auth-agent ignored for unsupported agent"
                        );
                        None
                    } else {
                        Some(requested_agent)
                    }
                }
                None => None,
            };
            let start_in_initial_auth = initial_auth_agent.as_deref() == Some("copilot");

            // Spawn the ACP client. In helper mode (`--connect-master <pipe>`)
            // master owns the agent lifecycle, so normal panes spawn the
            // pipe-attached variant immediately. FRE-installed Copilot is the
            // exception: `--initial-auth-agent copilot` starts on Auth and lets
            // `LoginComplete` spawn the first pipe client after sign-in.
            if cli.assume_master_down {
                // Degraded open: master is known down, so don't even try the
                // (dead) pipe — go straight to the disconnected view that an
                // orphaned pane shows, where /restart is the one available
                // command. /restart routes via wtcli→COM (not the dead pipe),
                // so it recovers the whole stack from right here.
                tracing::info!(
                    target: "helper",
                    "assume-master-down: starting in disconnected state (master is degraded)"
                );
                let _ = event_tx.send(app::AppEvent::AgentError {
                    session_id: None,
                    failure: protocol::acp::failure::AgentFailure::TransportLost,
                    message: t!("connection.lost").into_owned(),
                });
                // Keep the /restart path alive even with no master: /restart
                // doesn't talk to master, it asks the C++ side (via wtcli->COM)
                // to force-restart the whole agent stack — which respawns
                // master and reconnects EVERY pane. So we must keep consuming
                // `restart_rx` and forward it as a `restart_agent_stack` event.
                // The other receivers (prompt/new_session/…) genuinely have no
                // master to reach, so they're dropped; they're re-created when
                // /restart reopens this pane fresh.
                spawn_restart_agent_stack_forwarder(restart_rx);
                // The remaining receivers have no master to forward to. They
                // get re-created when /restart respawns the stack and reopens
                // this pane fresh.
                drop((
                    prompt_rx,
                    cancel_rx,
                    new_session_rx,
                    load_session_rx,
                    drop_session_rx,
                    rename_session_rx,
                    session_hook_rx,
                    master_ext_rx,
                ));
            } else if start_in_initial_auth {
                tracing::info!(
                    target: "initial_auth",
                    agent_id = %canonical_agent_id,
                    "starting helper on auth screen; initial ACP task skipped"
                );
                // The Auth screen's LoginComplete path uses
                // `set_master_pipe_acp_params` below and `try_start_acp` to
                // create fresh channels and reconnect through the master pipe
                // after login. Dropping the boot channels here avoids an
                // explicit initial ACP race and makes the startup ordering
                // independent from tokio task polling.
                //
                // Keep the /restart path alive even though no ACP task is
                // running yet. The boot App holds the sole restart sender; when
                // LoginComplete calls `try_start_acp`, it replaces that sender
                // with a fresh channel and this forwarder exits.
                spawn_restart_agent_stack_forwarder(restart_rx);
                drop((
                    prompt_rx,
                    cancel_rx,
                    new_session_rx,
                    load_session_rx,
                    drop_session_rx,
                    rename_session_rx,
                    session_hook_rx,
                    master_ext_rx,
                ));
            } else {
                let pipe_name = connect_master_pipe.clone();
                let event_tx_for_pipe = event_tx.clone();
                let shell_mgr_for_pipe = Arc::clone(&shell_mgr);
                let acp_model = cli.acp_model.clone();
                // Per-tab agent identity passed through to the multi-agent
                // master via the initialize handshake. The helper has had
                // this on its `Cli` all along; pre-multi-agent it dropped
                // it (master owned the single agent CLI).
                let agent_id = cli.agent_id.clone();
                let agent_source_for_client = agent_source.clone();
                let source_cwd = agent_source_cwd.clone();
                let owner_tab = cli.owner_tab_id.clone();
                let initial_load_sid = cli.initial_load_session_id.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = protocol::acp::client::run_acp_client_over_pipe(
                        pipe_name,
                        acp_model,
                        agent_id,
                        agent_source_for_client,
                        source_cwd,
                        owner_tab,
                        initial_load_sid,
                        event_tx_for_pipe.clone(),
                        prompt_rx,
                        cancel_rx,
                        new_session_rx,
                        load_session_rx,
                        drop_session_rx,
                        rename_session_rx,
                        restart_rx,
                        session_hook_rx,
                        master_ext_rx,
                        shell_mgr_for_pipe,
                        wt_connected,
                        false, // post_login_reconnect: first connection, no authenticate needed
                    )
                    .await
                    {
                        tracing::error!(
                            target: "helper",
                            error = %e,
                            "run_acp_client_over_pipe failed"
                        );
                        // Recover the typed classification: an auth error
                        // attached at the handshake `new_session` site survives
                        // the `?`-collapse into `anyhow` via downcast, so it
                        // still routes to the sign-in screen; other handshake
                        // failures fall back to `HandshakeFailed`. The raw
                        // `{e:#}` is also in the log above for diagnosis.
                        let failure = protocol::acp::failure::classify_anyhow(
                            &e,
                            protocol::acp::failure::HandshakeStage::Initialize,
                        );
                        let _ = event_tx_for_pipe.send(app::AppEvent::AgentError {
                            session_id: None,
                            failure,
                            message: format!("helper ACP transport failed: {e:#}"),
                        });
                    }
                });
            }

            let (recommendation_tx, recommendation_rx) = tokio::sync::mpsc::unbounded_channel();
            let (permission_tx, _permission_rx) = tokio::sync::mpsc::unbounded_channel();
            let debug_capture_enabled = Arc::new(AtomicBool::new(false));
            let (_ui_event_tx, ui_event_rx) = tokio::sync::mpsc::unbounded_channel();

            // Spawn the recommendation executor so selected choices actually run.
            let rec_event_tx = event_tx.clone();
            // Shared so a runtime `agent_config_changed` settings update can
            // hot-swap the configured delegate agent/model in place (handled
            // in App::handle_event) without restarting the agent pane. The
            // executor snapshots it per choice; the App rebuilds it on change.
            let delegate_agents = Arc::new(std::sync::Mutex::new(
                crate::coordinator::default_delegate_agent_runtimes(
                    cli.delegate_agent.as_deref(),
                    Some(cli.agent.as_str()),
                    cli.delegate_model.as_deref(),
                ),
            ));
            tokio::spawn(crate::coordinator::run_recommendation_executor(
                recommendation_rx,
                rec_event_tx,
                shell_mgr_for_recs,
                Arc::clone(&delegate_agents),
            ));

            let autofix_enabled = !cli.no_autofix;
            let mut app_state = app::App::new(prompt_tx, recommendation_tx, permission_tx, cancel_tx, new_session_tx, load_session_tx, drop_session_tx, rename_session_tx, restart_tx, master_ext_tx, debug_capture_enabled, wt_connected, autofix_enabled, Arc::clone(&shell_mgr));
            app_state.set_allowed_agent_ids(cli.allowed_agent_ids.clone());
            // Seed the hot-updatable runtime agent config: the shared
            // delegate runtime table, the helper's own agent_cmd (needed to
            // re-derive the delegate commandline when only the delegate
            // agent/model change), and the configured acp-model override
            // (re-applied to future sessions so /new stays on the model).
            app_state.set_runtime_agent_config(
                Arc::clone(&delegate_agents),
                cli.agent.clone(),
                cli.acp_model.clone(),
            );
            app_state.set_session_hook_tx(session_hook_tx);

            // Pipe-mode reconnect pre-stash. In helper mode the initial
            // `run_acp_client_over_pipe` task fails immediately with
            // `Authentication required` if the user is in FRE (not yet
            // logged in). The post-login `LoginComplete` handler fires
            // `try_start_acp`; without this stash it would have no master
            // pipe to reconnect with and could not resume the agent pane
            // — breaking every `intellterm.wta/...`
            // ext-method (e.g. `sessions/list` — session view would stay
            // empty on the first tab forever). With the stash in place,
            // `try_start_acp` sees `master_pipe_name = Some(...)` and
            // routes the reconnect back through master.
            //
            // No effect when the initial connection succeeds: the
            // stashed params just sit unused for the helper's lifetime.
            app_state.set_master_pipe_acp_params(
                connect_master_pipe.clone(),
                agent_cmd.clone(),
                cli.acp_model.clone(),
                agent_source.clone(),
                agent_source_cwd.clone(),
                cli.owner_tab_id.clone(),
                Arc::clone(&shell_mgr),
                wt_connected,
            );

            if cli.setup.is_none() {
                app_state.current_agent_id = canonical_agent_id.clone();
                app_state.current_agent_source = agent_source.clone();
                tracing::info!(
                    target: "agents_view_filter",
                    agent_id = %canonical_agent_id,
                    agent_cmd = %agent_cmd,
                    source = canonical_agent_source,
                    "current_agent_id assigned",
                );
            }
            if start_in_initial_auth {
                app_state.show_copilot_auth_screen();
            }

            // ── Preflight: check the agent CLI before connecting ──────────
            // Skip preflight when FRE is active — FRE has its own agent
            // selection + auth flow and doesn't need the preflight wizard.
            if cli.setup.is_none() && !start_in_initial_auth {
                let agent_id = canonical_agent_id.as_str();
                let preflight_result = if agent_id.starts_with("custom:")
                    || !agent_registry::is_known_id(agent_id)
                {
                    // Custom/unknown agents: command is opaque (`.cmd`, `node script.js`,
                    // shell function, …); a PATH probe would lie. The real spawn produces
                    // the authoritative error via `ConnectionFailed`, so skip preflight.
                    app::PreflightResult::passed_for_custom_agent(&canonical_agent_id)
                } else {
                    let status =
                        agent_check::check_agent_in_source(agent_id, &agent_source).await;
                    app::PreflightResult {
                        agent_id: canonical_agent_id.clone(),
                        display_name: status.display_name.clone(),
                        cli_status: if status.cli_found {
                            app::CheckStatus::Passed
                        } else {
                            app::CheckStatus::Failed("Not found on PATH".to_string())
                        },
                        cli_path: status.cli_path.clone(),
                        // Authentication is checked by the ACP handshake rather
                        // than by a local credential-store preflight.
                        auth_status: app::CheckStatus::Skipped,
                        install_hint: status.install_hint.clone(),
                        install_url: String::new(),
                        auth_hint: status.auth_hint.clone(),
                    }
                };
                tracing::info!(
                    target: "preflight",
                    agent_id = %preflight_result.agent_id,
                    cli = ?preflight_result.cli_status,
                    auth = ?preflight_result.auth_status,
                    "preflight done (via agent_check)"
                );
                let _ = event_tx.send(app::AppEvent::PreflightComplete(preflight_result));
            }

            // ── install-hooks request channel ─────────────────────────────
            // The Settings UI / in-TUI install button signals via this
            // channel; main.rs runs `agent_hooks_installer::ensure_installed`
            // off the UI thread so the TUI stays responsive.
            let (install_req_tx, mut install_req_rx) =
                tokio::sync::mpsc::unbounded_channel::<()>();
            tokio::task::spawn_local(async move {
                while let Some(()) = install_req_rx.recv().await {
                    tracing::info!(target: "install_hooks", "received install request");
                    // Run the (potentially slow, IO-bound) installer on the
                    // blocking pool so we don't park the LocalSet.
                    let _ = tokio::task::spawn_blocking(|| {
                        agent_hooks_installer::ensure_installed();
                    })
                    .await;
                }
            });
            app_state.set_install_request_tx(install_req_tx);

            // Wire the agent_event channel so dispatch_resume's split-pane
            // background callback can post AgentSessionEvent (specifically
            // ResumePaneAssigned) back into the event loop.
            app_state.set_agent_event_tx(event_tx.clone());

            // Seed `app_state.tab_id` + `pane_open` from `--owner-tab-id`
            // BEFORE the `--initial-view` block + the `project_active_tab_state`
            // emit below. Two failure modes if we don't:
            //   1. `current_tab_mut` in the --initial-view block falls back
            //      to DEFAULT_TAB_ID — the view setting lands on the wrong
            //      tab, the echo C++ receives doesn't match any real tab
            //      and is dropped.
            //   2. The initial echo has `pane_open=false` (default), which
            //      C++'s `OnAgentStateChanged` interprets as "hide" and
            //      stashes the just-spawned agent pane.
            // The full seed block further down (which logs + redundantly
            // sets the same fields) becomes idempotent now.
            //
            // `--start-stashed` inverts (2): in the pre-warm path the
            // C++ side has *already stashed* the pane after spawning the
            // helper, so the helper must seed `pane_open = false` to
            // match. Without this, helper echoes `pane_open=true`, C++
            // sees a stashed pane and a `pane_open=true` echo, and
            // restores the pane — defeating pre-warm.
            if let Some(ref owner_tab_id) = cli.owner_tab_id {
                if !owner_tab_id.is_empty() && app_state.tab_id.is_none() {
                    let tab = app_state
                        .tab_sessions
                        .entry(owner_tab_id.clone())
                        .or_default();
                    tab.pane_open = !cli.start_stashed;
                    app_state.tab_id = Some(owner_tab_id.clone());
                    app_state.owner_tab_id = Some(owner_tab_id.clone());
                }
            }

            // Plan-C boot-time initial-load: if WT spawned us with
            // `--initial-load-session-id` (+ optional `--initial-load-cwd`)
            // synthesize an `AppEvent::WtEvent { method:"load_session" }`
            // and queue it on `event_tx`. The App's event loop will pick
            // it up after startup and route it through the same handler
            // that the runtime `wt_event` path uses (app.rs ~4039) —
            // which:
            //   1) clears the tab's chat and sets `loading_session=true`,
            //      so the chunk handlers ACCEPT replay chunks during the
            //      ensuing `session/load`. Going through the channel
            //      directly (the old design) skipped this, and the
            //      master DID route the replay chunks back to the
            //      helper, but the App's AgentMessageChunk handler
            //      dropped them because `turn.is_in_flight() == false`
            //      and `loading_session == false` — user-visible
            //      symptom: "Session loaded." footer with no past
            //      content above.
            //   2) emits a "Resuming session …" system message so the
            //      user has a visible cue while the load is in flight,
            //   3) forwards into the same `load_session_tx` channel the
            //      runtime arm uses, which drives `conn.load_session`
            //      on the ACP client side — atomically replacing the
            //      bootstrap session created by `session/new` moments
            //      earlier.
            //
            // This replaces the prior race-prone design where C++
            // broadcast a separate `load_session` VT event right after
            // spawning the helper — which often landed in the wrong
            // helper because the new helper's pipe attach hadn't yet
            // completed.
            //
            // Pair-only: both flags meaningless without `--owner-tab-id`
            // (the load_session handler routes by tab id), so we
            // silently skip if owner_tab_id is unset. Logged so a
            // misconfigured spawn is easy to diagnose.
            if let Some(ref sid) = cli.initial_load_session_id {
                if !sid.is_empty() {
                    let tab_id_opt = app_state
                        .owner_tab_id
                        .clone()
                        .or_else(|| cli.owner_tab_id.clone());
                    match tab_id_opt {
                        Some(tab_id) if !tab_id.is_empty() => {
                            let cwd = cli
                                .initial_load_cwd
                                .as_deref()
                                .map(str::to_string)
                                .filter(|s| !s.is_empty())
                                .and_then(|s| {
                                    let v = crate::cwd_util::validate_starting_directory(&s);
                                    if v.is_none() {
                                        tracing::warn!(
                                            target: "acp_load_session",
                                            "--initial-load-cwd refers to a missing directory; dropping from load_session params",
                                        );
                                    }
                                    v
                                });
                            tracing::info!(
                                target: "acp_load_session",
                                session_id = sid,
                                tab_id = %tab_id,
                                "queueing boot-time initial load_session via AppEvent::WtEvent"
                            );
                            let mut params = serde_json::Map::new();
                            params.insert(
                                "tab_id".to_string(),
                                serde_json::Value::String(tab_id.clone()),
                            );
                            params.insert(
                                "session_id".to_string(),
                                serde_json::Value::String(sid.clone()),
                            );
                            if let Some(cwd_str) = cwd {
                                params.insert(
                                    "cwd".to_string(),
                                    serde_json::Value::String(cwd_str),
                                );
                            }
                            let _ = event_tx.send(app::AppEvent::WtEvent {
                                method: "load_session".to_string(),
                                pane_id: String::new(),
                                tab_id: Some(tab_id),
                                params: serde_json::Value::Object(params),
                            });
                        }
                        _ => {
                            tracing::warn!(
                                target: "acp_load_session",
                                "--initial-load-session-id given without --owner-tab-id; ignoring"
                            );
                        }
                    }
                }
            }
            // `initial_load_tx` is no longer used (the runtime
            // `load_session_tx` path is now reached via the App's
            // WtEvent handler) but we still need to drop the cloned
            // sender so the receiver future inside the ACP client loop
            // doesn't keep an extra producer alive past shutdown.
            drop(initial_load_tx);

            // Apply --initial-view: if `sessions`, jump straight into the
            // agent session view (mirrors the Chat→Agents toggle). Wired to
            // WT's Ctrl+Shift+/ binding via `--initial-view sessions` on
            // the wta cmdline. `open_agents_view_for_tab` fires the
            // `session/list` refetch to master that populates the view.
            //
            // Skip in setup mode: --setup takes the diagnostic path and the user
            // shouldn't be dropped into an empty session list.
            if cli.setup.is_none()
                && !start_in_initial_auth
                && cli.initial_view == InitialView::Sessions
            {
                tracing::info!(target: "initial_view", "starting in agent session view");
                let tab_id = app_state
                    .tab_id
                    .clone()
                    .unwrap_or_else(|| app::DEFAULT_TAB_ID.to_string());
                app_state.open_agents_view_for_tab(tab_id);
            }

            // Project the initial active-tab state to C++ once, after the
            // --initial-view block has had its say. Without this push,
            // C++'s `_agentSessionsViewActive` and `Tab.AgentPaneOpen`
            // mirrors (single writer lives in `OnAgentStateChanged`)
            // would stay on their defaults until the user's first
            // interaction, leaving the bar mislabelled in the
            // `--initial-view sessions` case and the pane-open flag
            // out of sync with the seeded `pane_open=true` on the
            // owner tab. Cheap and idempotent.
            //
            // Safe before the `Setup` mode block below: that block runs
            // its own UI and doesn't read the view flag; if we end up in
            // setup mode the initial "chat" emission is harmless.
            if wt_connected {
                app_state.project_active_tab_state();
            }

            // NOTE: the helper no longer scans on-disk history at all. The
            // session view renders from master's `session/list` snapshot, and
            // master performs the single CLI-filtered scan at its startup.
            // See doc/specs/per-cli-history-filtering.md.

            // Enter setup mode if --setup <reason> was passed.
            tracing::info!("cli.setup = {:?}", cli.setup);
            if let Some(ref reason_str) = cli.setup {
                tracing::info!("Entering diagnostic setup mode: reason={}", reason_str);
                let reason = app::SetupReason::from_str(reason_str);

                app_state.mode = app::AppMode::Setup;
                let options = app::build_setup_options(&reason, None);
                let title = reason.title().to_string();
                let subtitle = "Fix the issue to continue".to_string();
                app_state.setup = Some(app::SetupState {
                    reason,
                    selected_index: 0,
                    preflight: app::PreflightResult {
                        agent_id: String::new(),
                        display_name: String::new(),
                        cli_status: app::CheckStatus::Skipped,
                        cli_path: None,
                        auth_status: app::CheckStatus::Skipped,
                        install_hint: String::new(),
                        install_url: String::new(),
                        auth_hint: String::new(),
                    },
                    install_in_progress: false,
                    install_log: Vec::new(),
                    install_error: None,
                    options,
                    title,
                    subtitle,
                });
            }

            app_state.set_event_tx(event_tx.clone());

            // The helper does not scan on-disk history: master performs the
            // single (CLI-filtered) scan and the session view renders from
            // its `session/list` snapshot. See
            // doc/specs/per-cli-history-filtering.md.

            if let Some((pane_id, _tab_id, window_id)) = pane_identity {
                app_state.pane_id = Some(pane_id);
                // discover_pane_identity returns the legacy unstable tab
                // index, not the GUID — ignore it. The stable owner-tab GUID
                // is passed by WT via --owner-tab-id (see below) and seeded
                // directly into app_state.tab_id.
                app_state.window_id = Some(window_id);
            }

            // WT knows the owning window authoritatively when it creates the
            // helper. Prefer that seed over best-effort PID discovery so
            // outbound per-window events work from the first render.
            if let Some(owner_window_id) = cli
                .owner_window_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                tracing::info!(
                    target: "tab_session",
                    window_id = %owner_window_id,
                    "seeded app_state.window_id from --owner-window-id"
                );
                app_state.window_id = Some(owner_window_id.to_string());
            }

            // Seed tab_id from --owner-tab-id (passed by TerminalPage when
            // spawning the agent pane). With this set, AgentConnected binds
            // the initial session under the correct GUID immediately, and
            // tab_changed events later are plain switches — no implicit
            // DEFAULT_TAB_ID placeholder, no migration heuristics. Falls
            // back to None for non-pane invocations (manual `wta` runs, the
            // `wta delegate` subcommand), where the legacy DEFAULT_TAB_ID
            // path handles routing.
            //
            // Materialize the matching `tab_sessions` entry alongside the
            // tab_id assignment — `current_tab()` borrows immutably and
            // expects the active key to already be present, so without
            // pre-inserting we'd panic on the first render before any
            // event has had a chance to lazy-create it.
            if let Some(owner_tab_id) = cli.owner_tab_id.clone() {
                if !owner_tab_id.is_empty() {
                    tracing::info!(
                        target: "tab_session",
                        tab_id = %owner_tab_id,
                        "seeded app_state.tab_id from --owner-tab-id"
                    );
                    let tab = app_state
                        .tab_sessions
                        .entry(owner_tab_id.clone())
                        .or_default();
                    // wta is the source of truth for "does this tab want
                    // the pane visible". The pane is being spawned right
                    // now for this owner tab; under the normal user-
                    // initiated open the user wants it visible, so default
                    // pane_open=true. The exception is `--start-stashed`
                    // (pre-warm path) where C++ has already stashed the
                    // pane — see comment on the earlier seed block.
                    tab.pane_open = !cli.start_stashed;
                    app_state.tab_id = Some(owner_tab_id.clone());

                    // Publish an initial chip-target state for this tab so
                    // the C++ side can sync regardless of which transitions
                    // it has seen so far. At startup no Send card is
                    // selected, so the published target is `None` — i.e.
                    // "release any override, fall back to the source-of-
                    // agent flag". This is harmless when the C++ side is
                    // already in that state and load-bearing in the race
                    // where the agent pane was just restored from a stash
                    // and the chip-visibility hook on the C++ side hasn't
                    // run with the right `previousActive` yet.
                    app_state.recompute_chip_override_initial(&owner_tab_id);
                }
            }

            // ── source-pane context (autofix attribution) ─────────────────
            app_state.source_session_id = std::env::var("WTA_SOURCE_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty());
            app_state.source_cwd = std::env::var("WTA_SOURCE_CWD")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| agent_source_cwd.clone());

            // ── env-gated raw agent_event chat logging (diagnostics) ──────
            app_state.log_agent_events = std::env::var("WTA_LOG_AGENT_EVENT")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);

            // If a prompt was passed via CLI arg (e.g., from command palette creating
            // a new agent pane), delegate it to a new tab agent on startup.
            if let Some(ref initial_prompt) = cli.prompt {
                if !initial_prompt.is_empty() {
                    app_state.delegate_to_tab_agent(initial_prompt);
                }
            }

            app_state.run(terminal, event_rx, ui_event_rx).await
        })
        .await
}

#[cfg(test)]
mod cli_tests;

#[cfg(test)]
mod delegate_context_tests {
    use super::cap_delegate_context;

    #[test]
    fn cap_returns_short_context_unchanged() {
        let ctx = "small output";
        assert_eq!(cap_delegate_context(ctx, 1024), ctx);
    }

    #[test]
    fn cap_keeps_tail_and_marks_truncation() {
        let ctx: String = (0..5000u32)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let out = cap_delegate_context(&ctx, 1000);
        assert!(out.starts_with("…(truncated)\n"));
        // keeps the tail (most recent output)
        assert!(out.ends_with(&ctx[ctx.len() - 100..]));
        assert!(out.len() <= 1000);
    }

    #[test]
    fn cap_is_char_boundary_safe() {
        // Each '⭐' is 3 bytes; cutting must land on a char boundary (no panic).
        let ctx: String = std::iter::repeat('⭐').take(500).collect();
        let out = cap_delegate_context(&ctx, 100);
        assert!(out.len() <= 100);
        assert!(out.ends_with('⭐'));
        assert!(out
            .chars()
            .all(|c| c == '⭐' || "…(truncated)\n".contains(c)));
    }

    #[test]
    fn cap_omits_marker_when_limit_is_too_small() {
        assert_eq!(cap_delegate_context("prefix-tail", 4), "tail");
    }
}

#[cfg(test)]
mod remote_transport_command_tests {
    use super::reconnecting_ssh_commandline;
    use base64::Engine as _;

    #[test]
    fn reconnect_wrapper_preserves_argv_and_caps_backoff() {
        let commandline = reconnecting_ssh_commandline(&[
            "-t".into(),
            "devbox".into(),
            "--".into(),
            "exec \"$HOME/.local/node\" pty start --session surface-1".into(),
        ])
        .unwrap();
        let encoded = commandline.split_whitespace().last().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let words = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let script = String::from_utf16(&words).unwrap();
        assert!(script.contains("$delays=@(3,6,12,24,48,60)"));
        assert!(script.contains("'devbox'"));
        assert!(script.contains("'exec \"$HOME/.local/node\" pty start --session surface-1'"));
        assert!(script.contains("if($code -eq 0){exit 0}"));
    }
}
