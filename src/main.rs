//! A vim-like normal mode for the herdr sidebar: `j` and `k` walk the spaces
//! and agents as one list, `h` and `l` walk the tabs of whichever workspace the
//! cursor is in, `0`-`9` jump to a pane on the map, and herdr's focus follows.
//!
//! This runs as a session-modal popup. Any other placement would be pushed
//! behind the pane we just focused, taking our input and our drawing with it.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event};
use crossterm::terminal;

use herdr_normal_mode::herdr::Herdr;
use herdr_normal_mode::{
    Action, Layout, POPUP_HEIGHT, POPUP_WIDTH, PaneRef, Row, Tabs, build_rows, handle_key,
    initial_cursor, move_cursor, pane_at, pane_in_tab, popup_rows, resolve_active_ids, row_by_key,
};

fn main() {
    // No socket means no herdr to drive, and nowhere to say so.
    let Some(herdr) = Herdr::connect() else {
        return;
    };

    let context_json = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok();
    // No pane fallback on purpose: `HERDR_PANE_ID` is this popup's own id.
    let env_workspace_id = std::env::var("HERDR_WORKSPACE_ID").ok();
    let (active_pane_id, active_workspace_id) =
        resolve_active_ids(context_json.as_deref(), env_workspace_id.as_deref());
    let own_pane_id = std::env::var("HERDR_PANE_ID").ok();

    let lists = herdr.lists();
    let rows = build_rows(&lists.workspaces, &lists.agents, own_pane_id.as_deref());
    // The popup is a pane too. Naming it when asking for a layout would draw
    // the popup rather than the tab behind it, so it is dropped here the same
    // way it is dropped from the rows.
    let panes: Vec<PaneRef> = lists
        .panes
        .into_iter()
        .filter(|pane| Some(pane.pane_id.as_str()) != own_pane_id.as_deref())
        .collect();

    // Nothing to walk means herdr answered with no workspaces at all, which it
    // cannot do while we are running inside one of them. Close again rather
    // than carry a reporting path for it.
    if rows.is_empty() {
        return;
    }

    let mut tabs = Tabs::new(&lists.workspaces, &lists.tabs);
    // A reopen names the row it left the cursor on. It wins over the focus,
    // which says only where the last move landed: focus on an agent would put
    // the cursor on the agent row even when it was walking the spaces.
    let handed_over = std::env::var("HERDR_NORMAL_MODE_CURSOR")
        .ok()
        .and_then(|key| row_by_key(&rows, &key));
    let start = handed_over.or_else(|| {
        initial_cursor(&rows, active_pane_id.as_deref(), active_workspace_id.as_deref())
    });
    // Nothing is done with a failure. Every error `run` can return comes from
    // the terminal itself, which is the only place a report could go, and herdr
    // tears the popup down the moment we exit — so there is nowhere to say it
    // and nothing the user could do about it. Dropping it here is also what
    // keeps a `?` inside `run` from skipping the restore below and stranding
    // the terminal in raw mode.
    let _ = run(&herdr, &rows, &panes, &mut tabs, start);
    let _ = restore();
}

fn run(
    herdr: &Herdr,
    rows: &[Row],
    panes: &[PaneRef],
    tabs: &mut Tabs,
    start: Option<usize>,
) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    // No alternate screen: the popup is its own surface and gets torn down
    // with us.
    print!("\x1b[?25l\x1b[2J\x1b[H");

    let mut cursor = start.unwrap_or(0);

    // The row herdr is focused on, as far as we know. `None` when the cursor
    // started somewhere we could not match, so the first settle really sends a
    // focus. We never re-read herdr's own `focused` flags — following our own
    // focus changes would be a loop.
    let followed = start;

    // The map on screen, and the tab it is of. Drawn empty first: the layout
    // needs a round trip, and the mode row should not wait on it.
    let mut layout: Option<Layout> = None;
    draw(&layout)?;
    settle_map(herdr, rows, cursor, panes, tabs, &mut layout)?;

    loop {
        // Settle the focus once the burst is over. Holding `j` queues keys
        // faster than herdr can answer them, so a burst costs one round trip
        // at the row it lands on instead of one per keystroke. Doing this at
        // the top of the loop means every path back here settles, not just
        // the one that moved the cursor.
        if followed != Some(cursor)
            && !event::poll(Duration::ZERO)?
            // Only count the row as reached if herdr accepted it. A pane that
            // died while the popup was open should be retried, not written off.
            && herdr.focus_row(&rows[cursor])
        {
            // The popup is replaced from here: the new one re-reads the lists,
            // so the tab ring and the map start from where this move landed.
            return Ok(Herdr::reopen(&rows[cursor].key()));
        }

        match event::read()? {
            Event::Key(key) => match handle_key(&key) {
                Action::Move(delta) => {
                    // The cursor tracks the keystroke without waiting on
                    // herdr; the focus and the map catch up at the settle
                    // above.
                    cursor = move_cursor(cursor, rows.len(), delta);
                }
                Action::Tab(delta) => {
                    // `followed` is deliberately left alone. Stepping a tab
                    // moves the focus off the row the cursor is on, which is
                    // the point — marking the row as unreached would send it a
                    // focus again at the top of the loop and undo the step.
                    // Coming back to this row later still re-focuses it, and
                    // for a space that lands on the tab we stepped to.
                    if let Some(workspace_id) = rows[cursor].workspace_id()
                        && let Some(tab_id) = tabs.step(workspace_id, delta)
                        && herdr.focus_tab(tab_id)
                    {
                        return Ok(Herdr::reopen(&rows[cursor].key()));
                    }
                    settle_map(herdr, rows, cursor, panes, tabs, &mut layout)?;
                }
                Action::Pane(index) => {
                    // The map does not change: it draws the shape of the tab,
                    // and jumping within it moves the focus but not the shape.
                    // `followed` is left alone for the same reason as above.
                    if let Some(layout) = layout.as_ref()
                        && let Some(pane_id) = pane_at(layout, index)
                        && herdr.focus_pane(pane_id)
                    {
                        return Ok(Herdr::reopen(&rows[cursor].key()));
                    }
                }
                Action::Quit => {
                    // A burst that ended in a quit never reached the settle
                    // above; land on the row the cursor stopped on.
                    if followed != Some(cursor) {
                        herdr.focus_row(&rows[cursor]);
                    }
                    break;
                }
                Action::Ignore => {}
            },
            // A resize can clear the pane out from under us, so put the rows
            // back.
            Event::Resize(..) => draw(&layout)?,
            _ => {}
        }
    }

    Ok(())
}

/// Bring the map up to date with the tab the cursor now points at, and draw
/// again only if that tab is not the one already drawn — every other keystroke
/// leaves the same shape on screen.
fn settle_map(
    herdr: &Herdr,
    rows: &[Row],
    cursor: usize,
    panes: &[PaneRef],
    tabs: &Tabs,
    layout: &mut Option<Layout>,
) -> io::Result<()> {
    let target = rows[cursor]
        .workspace_id()
        .and_then(|workspace_id| tabs.current(workspace_id))
        .map(str::to_string);
    if layout.as_ref().map(|held| held.tab_id.as_str()) == target.as_deref() {
        return Ok(());
    }
    *layout = target
        .as_deref()
        .and_then(|tab_id| pane_in_tab(panes, tab_id))
        .and_then(|pane_id| herdr.layout(pane_id));
    draw(layout)
}

/// Every row, each placed by absolute position. A newline would do instead, but
/// only until the pane is exactly as tall as what we draw — then the last one
/// scrolls the first row away.
///
/// The rows are laid out against the pane herdr actually gave us, not against
/// the manifest. Writing past the last row does not fail: the terminal clamps
/// the cursor and the row lands on top of the one above, which is how the keys
/// went missing when the manifest was read one row too generously.
fn draw(layout: &Option<Layout>) -> io::Result<()> {
    let (width, height) = viewport();
    let mut stdout = io::stdout();
    for (index, row) in popup_rows(layout.as_ref(), width, height)
        .iter()
        .enumerate()
    {
        write!(stdout, "\x1b[{};1H\x1b[2K{row}", index + 1)?;
    }
    stdout.flush()
}

/// The pane's own size. A pane that will not say how big it is gets the size the
/// manifest asks for, which is the size it was going to be anyway.
fn viewport() -> (usize, usize) {
    terminal::size()
        .map(|(columns, rows)| (columns as usize, rows as usize))
        .unwrap_or((POPUP_WIDTH, POPUP_HEIGHT))
}

fn restore() -> io::Result<()> {
    print!("\x1b[?25h");
    io::stdout().flush()?;
    terminal::disable_raw_mode()
}
