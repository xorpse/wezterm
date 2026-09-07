use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use mux::domain::{DomainId, DomainState};
use mux::localpane::PaneNotifHandler;
use mux::pane::{
    CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, WithPaneLines,
    impl_get_lines_via_with_lines, impl_get_logical_lines_via_get_lines,
};
use mux::renderable::{
    RenderableDimensions, StableCursorPosition, terminal_for_each_logical_line_in_stable_range_mut,
    terminal_get_cursor_position, terminal_get_dimensions, terminal_get_dirty_lines,
    terminal_with_lines_mut,
};
use mux::{Mux, MuxNotification};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    KeyCode, KeyModifiers, MouseEvent, StableRowIndex, Terminal, TerminalConfiguration,
    TerminalSize,
};

use crate::relay::{Notification, RelayConnection};

struct ChannelWriter {
    tx: flume::Sender<Vec<u8>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct RelayPane {
    pane_id: PaneId,
    domain_id: DomainId,
    pty_id: String,
    cwd: String,
    parent_tab_id: String,
    owns_pty: bool,
    terminal: Mutex<Terminal>,
    writer: Mutex<Box<dyn Write + Send>>,
    connection: Mutex<RelayConnection>,
    me: Mutex<Weak<RelayPane>>,
    last_size: Mutex<TerminalSize>,
    dead: AtomicBool,
    killed: AtomicBool,
    held: AtomicBool,
    io_generation: AtomicU64,
}

impl RelayPane {
    pub fn new(
        pane_id: PaneId,
        domain_id: DomainId,
        pty_id: String,
        cwd: String,
        parent_tab_id: String,
        owns_pty: bool,
        size: TerminalSize,
        connection: RelayConnection,
    ) -> (Arc<RelayPane>, flume::Receiver<Vec<u8>>) {
        let (input_tx, input_rx) = flume::unbounded::<Vec<u8>>();
        let term_config = Arc::new(config::TermConfig::new());
        let mut terminal = Terminal::new(
            size,
            term_config,
            "orca-relay",
            "1.0",
            Box::new(ChannelWriter {
                tx: input_tx.clone(),
            }),
        );
        terminal.set_notification_handler(Box::new(PaneNotifHandler::new(pane_id)));
        let pane = Arc::new(RelayPane {
            pane_id,
            domain_id,
            pty_id,
            cwd,
            parent_tab_id,
            owns_pty,
            terminal: Mutex::new(terminal),
            writer: Mutex::new(Box::new(ChannelWriter { tx: input_tx })),
            connection: Mutex::new(connection),
            me: Mutex::new(Weak::new()),
            last_size: Mutex::new(size),
            dead: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            held: AtomicBool::new(false),
            io_generation: AtomicU64::new(0),
        });
        *pane.me.lock() = Arc::downgrade(&pane);
        (pane, input_rx)
    }

    pub fn parent_tab_id(&self) -> &str {
        &self.parent_tab_id
    }

    pub fn pty_id(&self) -> &str {
        &self.pty_id
    }

    pub fn start_io(
        &self,
        output_rx: flume::Receiver<Notification>,
        input_rx: flume::Receiver<Vec<u8>>,
    ) {
        self.spawn_output(output_rx);

        let weak = self.me.lock().clone();
        promise::spawn::spawn(async move {
            while let Ok(bytes) = input_rx.recv_async().await {
                let Some(pane) = weak.upgrade() else {
                    break;
                };
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let connection = pane.connection.lock().clone();
                let _ = connection.input(&pane.pty_id, &text).await;
            }
        })
        .detach();
    }

    fn spawn_output(&self, output_rx: flume::Receiver<Notification>) {
        let generation = self.io_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let weak = self.me.lock().clone();
        promise::spawn::spawn_into_main_thread(async move {
            let mut exited = false;
            while let Ok(notification) = output_rx.recv_async().await {
                let Some(pane) = weak.upgrade() else {
                    return;
                };
                if pane.io_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                match notification.method.as_str() {
                    "pty.data" | "pty.replay" => {
                        if let Some(data) = notification
                            .params
                            .get("data")
                            .and_then(|value| value.as_str())
                        {
                            pane.terminal.lock().advance_bytes(data.as_bytes());
                            Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id));
                        }
                        if notification.method == "pty.data" {
                            let connection = pane.connection.lock().clone();
                            let _ = connection
                                .ack_data(&pane.pty_id, &notification.params)
                                .await;
                        }
                    }
                    "pty.exit" => {
                        exited = true;
                        break;
                    }
                    _ => {}
                }
            }
            let Some(pane) = weak.upgrade() else {
                return;
            };
            if pane.io_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            if exited {
                pane.declare_dead();
            } else {
                // The route closed without a pty.exit — the ssh channel dropped,
                // but the daemon keeps the PTY in grace. Mark held; the domain's
                // poller reconnects and rebuilds this pane from the live list.
                pane.held.store(true, Ordering::Relaxed);
                Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id));
            }
        })
        .detach();
    }

    fn declare_dead(&self) {
        if self.dead.swap(true, Ordering::Relaxed) {
            return;
        }
        self.connection.lock().unroute_pty(&self.pty_id);
        let pane_id = self.pane_id;
        Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane_id));
        let mux = Mux::get();
        match config::configuration().exit_behavior {
            config::ExitBehavior::Hold => mux.prune_dead_windows(),
            config::ExitBehavior::Close | config::ExitBehavior::CloseOnCleanExit => {
                mux.remove_pane(pane_id)
            }
        }
    }

    fn shutdown_remote(&self) {
        if !self.owns_pty || self.killed.swap(true, Ordering::Relaxed) {
            return;
        }
        let connection = self.connection.lock().clone();
        let pty_id = self.pty_id.clone();
        promise::spawn::spawn(async move {
            let _ = connection.shutdown_pty(&pty_id).await;
        })
        .detach();
    }
}

impl Pane for RelayPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        terminal_get_cursor_position(&mut self.terminal.lock())
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.terminal.lock().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.terminal.lock(), lines, seqno)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        terminal_for_each_logical_line_in_stable_range_mut(
            &mut self.terminal.lock(),
            lines,
            for_line,
        )
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        terminal_with_lines_mut(&mut self.terminal.lock(), lines, with_lines)
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        impl_get_lines_via_with_lines(self, lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.terminal.lock())
    }

    fn get_title(&self) -> String {
        let title = self.terminal.lock().get_title().to_owned();
        if self.held.load(Ordering::Relaxed) && !self.dead.load(Ordering::Relaxed) {
            return format!("⌁ {title}");
        }
        title
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        self.terminal.lock().send_paste(text)
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer.as_mut();
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.terminal.lock().resize(size);
        *self.last_size.lock() = size;
        // A subscribed pane shares the app's PTY; only the owner may drive its
        // size, so never resize a terminal we do not own out from under the app.
        if self.owns_pty {
            let connection = self.connection.lock().clone();
            let pty_id = self.pty_id.clone();
            let cols = size.cols as u16;
            let rows = size.rows as u16;
            promise::spawn::spawn(async move {
                let _ = connection.resize_pty(&pty_id, cols, rows).await;
            })
            .detach();
        }
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        self.terminal.lock().key_down(key, mods)
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        self.terminal.lock().key_up(key, mods)
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        self.terminal.lock().mouse_event(event)
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.terminal.lock().perform_actions(actions)
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        self.terminal.lock().get_keyboard_encoding()
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    fn kill(&self) {
        self.dead.store(true, Ordering::Relaxed);
        let detached = Mux::get()
            .get_domain(self.domain_id)
            .is_none_or(|domain| domain.state() == DomainState::Detached);
        if !detached {
            self.shutdown_remote();
        }
    }

    fn palette(&self) -> ColorPalette {
        self.terminal.lock().palette()
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.terminal.lock().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.lock().get_config())
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.terminal.lock().user_vars().clone()
    }

    fn is_mouse_grabbed(&self) -> bool {
        self.terminal.lock().is_mouse_grabbed()
    }

    fn is_alt_screen_active(&self) -> bool {
        self.terminal.lock().is_alt_screen_active()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        if let Some(url) = self.terminal.lock().get_current_dir().cloned() {
            return Some(url);
        }
        Url::from_file_path(&self.cwd).ok()
    }
}
