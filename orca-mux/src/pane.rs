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
use orca_client::{
    OrcaClient, TerminalHandle, TerminalStreamEvent, TerminalSummary, TerminalWriter, id_selector,
};
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

use crate::domain::OrcaDomain;

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

pub struct TerminalBinding {
    pub terminal: String,
    pub worktree_selector: String,
    pub worktree_path: String,
    pub parent_tab_id: String,
    pub leaf_id: String,
}

impl TerminalBinding {
    pub fn from_summary(summary: &TerminalSummary) -> TerminalBinding {
        TerminalBinding {
            terminal: summary.handle.clone(),
            worktree_selector: id_selector(&summary.worktree_id),
            worktree_path: summary.worktree_path.clone(),
            parent_tab_id: summary.tab_id.clone(),
            leaf_id: summary.leaf_id.clone(),
        }
    }

    pub fn session_tab_id(&self) -> String {
        format!("{}::{}", self.parent_tab_id, self.leaf_id)
    }
}

pub struct OrcaTerminalPane {
    pane_id: PaneId,
    domain_id: DomainId,
    binding: TerminalBinding,
    terminal: Mutex<Terminal>,
    writer: Mutex<Box<dyn Write + Send>>,
    remote: Mutex<TerminalWriter>,
    client: OrcaClient,
    me: Mutex<Weak<OrcaTerminalPane>>,
    io_generation: AtomicU64,
    last_size: Mutex<TerminalSize>,
    agent_state: Mutex<Option<String>>,
    held: AtomicBool,
    killed: AtomicBool,
    dead: AtomicBool,
}

impl OrcaTerminalPane {
    pub fn new(
        pane_id: PaneId,
        domain_id: DomainId,
        binding: TerminalBinding,
        size: TerminalSize,
        remote: TerminalWriter,
        client: OrcaClient,
    ) -> (Arc<OrcaTerminalPane>, flume::Receiver<Vec<u8>>) {
        let (input_tx, input_rx) = flume::unbounded::<Vec<u8>>();
        let term_config = Arc::new(config::TermConfig::new());
        let mut terminal = Terminal::new(
            size,
            term_config,
            "orca",
            "1.0",
            Box::new(ChannelWriter {
                tx: input_tx.clone(),
            }),
        );
        terminal.set_notification_handler(Box::new(PaneNotifHandler::new(pane_id)));
        let pane = Arc::new(OrcaTerminalPane {
            pane_id,
            domain_id,
            binding,
            terminal: Mutex::new(terminal),
            writer: Mutex::new(Box::new(ChannelWriter { tx: input_tx })),
            remote: Mutex::new(remote),
            client,
            me: Mutex::new(Weak::new()),
            io_generation: AtomicU64::new(0),
            last_size: Mutex::new(size),
            agent_state: Mutex::new(None),
            held: AtomicBool::new(false),
            killed: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        });
        *pane.me.lock() = Arc::downgrade(&pane);
        (pane, input_rx)
    }

    pub(crate) fn size(&self) -> TerminalSize {
        *self.last_size.lock()
    }

    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Relaxed) && !self.dead.load(Ordering::Relaxed)
    }

    pub(crate) fn set_agent_state(&self, state: Option<String>) -> bool {
        let mut slot = self.agent_state.lock();
        if *slot == state {
            return false;
        }
        *slot = state;
        true
    }

    pub fn binding(&self) -> &TerminalBinding {
        &self.binding
    }

    fn kill_remote(&self) {
        if self.killed.swap(true, Ordering::Relaxed) {
            return;
        }
        let client = self.client.clone();
        let terminal = self.binding.terminal.clone();
        let worktree = self.binding.worktree_selector.clone();
        let session_tab_id = self.binding.session_tab_id();
        promise::spawn::spawn(async move {
            if client.close_terminal(&terminal).await.is_ok() {
                return;
            }
            let _ = client.close_session_tab(&worktree, &session_tab_id).await;
        })
        .detach();
    }

    pub fn start_io(&self, handle: TerminalHandle, input_rx: flume::Receiver<Vec<u8>>) {
        self.spawn_output(handle);

        let weak = self.me.lock().clone();
        promise::spawn::spawn(async move {
            while let Ok(bytes) = input_rx.recv_async().await {
                let Some(pane) = weak.upgrade() else {
                    break;
                };
                let remote = pane.remote.lock().clone();
                drop(pane);
                let _ = remote.input(&bytes).await;
            }
        })
        .detach();
    }

    pub(crate) fn resume(&self, handle: TerminalHandle) {
        *self.remote.lock() = handle.writer();
        self.held.store(false, Ordering::Relaxed);
        self.spawn_output(handle);

        let remote = self.remote.lock().clone();
        let size = *self.last_size.lock();
        promise::spawn::spawn(async move {
            let _ = remote.resize(size.rows as u32, size.cols as u32).await;
        })
        .detach();
        Mux::notify_from_any_thread(MuxNotification::PaneOutput(self.pane_id));
    }

    fn spawn_output(&self, handle: TerminalHandle) {
        let generation = self.io_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let weak = self.me.lock().clone();
        let output_rx = handle.output();
        promise::spawn::spawn_into_main_thread(async move {
            let mut outcome = StreamOutcome::Disconnected;
            while let Ok(event) = output_rx.recv_async().await {
                let Some(pane) = weak.upgrade() else {
                    return;
                };
                if pane.io_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                match event {
                    TerminalStreamEvent::Output(bytes) => {
                        pane.terminal.lock().advance_bytes(&bytes);
                        Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id));
                    }
                    TerminalStreamEvent::SnapshotStart(_) => {
                        pane.terminal.lock().advance_bytes(b"\x1bc");
                    }
                    TerminalStreamEvent::SnapshotChunk(bytes) => {
                        pane.terminal.lock().advance_bytes(&bytes);
                    }
                    TerminalStreamEvent::SnapshotEnd => {
                        Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id));
                    }
                    TerminalStreamEvent::Resized { .. } => {}
                    TerminalStreamEvent::Error(message) => {
                        log::warn!("orca terminal stream error: {message}");
                    }
                    TerminalStreamEvent::Disconnected => break,
                    TerminalStreamEvent::End => {
                        outcome = StreamOutcome::Ended;
                        break;
                    }
                }
            }
            let Some(pane) = weak.upgrade() else {
                return;
            };
            if pane.io_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            match outcome {
                StreamOutcome::Disconnected => {
                    pane.held.store(true, Ordering::Relaxed);
                    Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id));
                }
                StreamOutcome::Ended => pane.declare_dead(),
            }
        })
        .detach();
    }

    pub(crate) fn declare_dead(&self) {
        self.dead.store(true, Ordering::Relaxed);
        let pane_id = self.pane_id;
        Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane_id));
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(self.domain_id) {
            if let Some(orca) = domain.downcast_ref::<OrcaDomain>() {
                orca.forget_terminal(&self.binding.terminal);
            }
        }
        match config::configuration().exit_behavior {
            config::ExitBehavior::Hold => mux.prune_dead_windows(),
            config::ExitBehavior::Close | config::ExitBehavior::CloseOnCleanExit => {
                self.kill_remote();
                mux.remove_pane(pane_id)
            }
        }
    }
}

enum StreamOutcome {
    Disconnected,
    Ended,
}

impl Pane for OrcaTerminalPane {
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
        if self.is_held() {
            return format!("⌁ {title}");
        }
        let glyph = match self.agent_state.lock().as_deref() {
            Some("working") => "● ",
            Some("blocked") | Some("waiting") => "⚠ ",
            Some("done") => "✓ ",
            _ => "",
        };
        format!("{glyph}{title}")
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
        let remote = self.remote.lock().clone();
        let rows = size.rows as u32;
        let cols = size.cols as u32;
        promise::spawn::spawn(async move {
            let _ = remote.resize(rows, cols).await;
        })
        .detach();
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
        // Window teardown detaches the domain before removing panes; kill the
        // remote terminal only for explicit closes (mirrors ClientPane::kill).
        let detached = Mux::get()
            .get_domain(self.domain_id)
            .is_none_or(|domain| domain.state() == DomainState::Detached);
        if !detached {
            self.kill_remote();
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
        Url::from_file_path(&self.binding.worktree_path).ok()
    }
}
