//! The herdr side of the plugin: one connection to herdr's own socket, and the
//! handful of calls the popup makes over it.
//!
//! The socket is here rather than the `herdr` CLI because focusing a pane by id
//! is only offered as an API method — `herdr pane focus` moves by direction.
//! Everything else came along because one transport beats two, and because a
//! keystroke no longer costs a process.
//!
//! Failures are swallowed on purpose — a popup that panics leaves the terminal
//! in raw mode, and a missed focus is recoverable while that is not.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Agent, Layout, PaneRef, Row, Tab, Workspace, parse_agent_list, parse_pane_layout,
    parse_pane_list, parse_tab_list, parse_workspace_list,
};

/// The lists the popup reads when it opens and never again: re-reading would
/// mostly show it where it moved the focus itself.
pub struct Lists {
    pub workspaces: Vec<Workspace>,
    pub agents: Vec<Agent>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<PaneRef>,
}

/// Where herdr's socket is. Not a held-open connection: the server answers one
/// request per connection and closes it, so a second write on the same stream
/// gets a broken pipe. Every call therefore opens its own — still far below what
/// shelling out to the `herdr` CLI cost, which was a whole process doing this
/// same connect itself.
pub struct Herdr {
    socket: String,
}

impl Herdr {
    /// herdr hands every plugin pane the socket to talk back on. There is no
    /// fallback: with no socket there is nothing to drive, and no way to say so.
    pub fn connect() -> Option<Self> {
        Some(Self {
            socket: std::env::var("HERDR_SOCKET_PATH").ok()?,
        })
    }

    /// One request, one reply line, handed back whole. The reply carries the
    /// same `{"result": …}` envelope the CLI printed, which is why moving to the
    /// socket left every parser alone.
    ///
    /// The method doubles as the request id: the connection carries one request,
    /// so there is nothing to tell apart.
    fn call(&self, method: &str, params: Value) -> Option<String> {
        let mut requests = UnixStream::connect(&self.socket).ok()?;
        let request = json!({"id": method, "method": method, "params": params});
        writeln!(requests, "{request}").ok()?;
        requests.flush().ok()?;
        let mut reply = String::new();
        BufReader::new(requests).read_line(&mut reply).ok()?;
        Some(reply)
    }

    /// Whether herdr took the call. A refusal comes back as a reply rather than
    /// as a failure to send, so it has to be read out of the answer.
    fn accepted(&self, method: &str, params: Value) -> bool {
        self.call(method, params).is_some_and(|reply| {
            serde_json::from_str::<Reply>(&reply).is_ok_and(|reply| reply.result.is_some())
        })
    }

    /// The four lists in a row, a connection each.
    pub fn lists(&self) -> Lists {
        Lists {
            workspaces: self
                .call("workspace.list", json!({}))
                .map(|reply| parse_workspace_list(&reply))
                .unwrap_or_default(),
            agents: self
                .call("agent.list", json!({}))
                .map(|reply| parse_agent_list(&reply))
                .unwrap_or_default(),
            tabs: self
                .call("tab.list", json!({}))
                .map(|reply| parse_tab_list(&reply))
                .unwrap_or_default(),
            panes: self
                .call("pane.list", json!({}))
                .map(|reply| parse_pane_list(&reply))
                .unwrap_or_default(),
        }
    }

    /// How the panes of one tab tile it, named by one of the panes in it.
    ///
    /// Asked for by pane and not left to default, because the default is the
    /// tab the focus is in — and the focus is ours, on a popup that is not in
    /// any tab.
    pub fn layout(&self, pane_id: &str) -> Option<Layout> {
        self.call("pane.layout", json!({"pane_id": pane_id}))
            .and_then(|reply| parse_pane_layout(&reply))
    }

    /// Move herdr's focus onto the row under the cursor.
    ///
    /// Reports whether herdr accepted it. The rows are read once and never
    /// refreshed, so a row can outlive its pane; the caller has to know a focus
    /// failed or it would mark that row as reached and never try again.
    pub fn focus_row(&self, row: &Row) -> bool {
        match row {
            Row::Space { workspace_id } => {
                self.accepted("workspace.focus", json!({"workspace_id": workspace_id}))
            }
            // `agent.focus` and not `pane.focus`: an agent row is reached the
            // way herdr reaches an agent, whatever else that carries with it.
            Row::Agent { pane_id, .. } => self.accepted("agent.focus", json!({"target": pane_id})),
        }
    }

    pub fn focus_tab(&self, tab_id: &str) -> bool {
        self.accepted("tab.focus", json!({"tab_id": tab_id}))
    }

    /// Jump to the pane a digit named. This is the call the CLI does not offer.
    pub fn focus_pane(&self, pane_id: &str) -> bool {
        self.accepted("pane.focus", json!({"pane_id": pane_id}))
    }

    /// Hand the mode over to a fresh popup, with the cursor on `cursor_key`.
    ///
    /// A popup belongs to the view it was opened over: once the focus moves to
    /// another tab or workspace it stays drawn but stops taking input. So after
    /// every focus change this popup is dropped and a new one opened over the
    /// view we landed in. The caller exits right after, which is what closes
    /// this one; the opener is a detached helper in its own session, because
    /// it has to outlive us, and it retries until herdr has let the old popup
    /// go.
    pub fn reopen(cursor_key: &str) {
        const SCRIPT: &str = r#"sleep 0.05
n=0
while [ "$n" -lt 40 ]; do
  "${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin "$HERDR_PLUGIN_ID" \
    --entrypoint normal --env "HERDR_NORMAL_MODE_CURSOR=$HERDR_NORMAL_MODE_CURSOR" \
    >/dev/null 2>&1 && exit 0
  n=$((n + 1))
  sleep 0.05
done"#;
        let mut helper = Command::new("sh");
        helper
            .args(["-c", SCRIPT])
            .env("HERDR_NORMAL_MODE_CURSOR", cursor_key)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: `setsid` is async-signal-safe and touches no memory of ours.
        unsafe {
            helper.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let _ = helper.spawn();
    }
}

/// What herdr answered, when all we need from it is whether it agreed.
#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    result: Option<Value>,
}
