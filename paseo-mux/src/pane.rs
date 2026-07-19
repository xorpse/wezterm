use mux::domain::DomainId;
use mux::pane::{
    impl_get_lines_via_with_lines, impl_get_logical_lines_via_get_lines, CachePolicy,
    ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, WithPaneLines,
};
use mux::renderable::{
    terminal_for_each_logical_line_in_stable_range_mut, terminal_get_cursor_position,
    terminal_get_dimensions, terminal_get_dirty_lines, terminal_with_lines_mut,
    RenderableDimensions, StableCursorPosition,
};
use mux::{Mux, MuxNotification};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use paseo_client::{TerminalHandle, TerminalStreamEvent, TerminalWriter};
use rangeset::RangeSet;
use std::io::Write;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    KeyCode, KeyModifiers, MouseEvent, StableRowIndex, Terminal, TerminalConfiguration,
    TerminalSize,
};

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

pub struct PaseoTerminalPane {
    pane_id: PaneId,
    domain_id: DomainId,
    remote_terminal_id: String,
    terminal: Mutex<Terminal>,
    writer: Mutex<Box<dyn Write + Send>>,
    remote: TerminalWriter,
    dead: AtomicBool,
}

impl PaseoTerminalPane {
    pub fn new(
        pane_id: PaneId,
        domain_id: DomainId,
        remote_terminal_id: String,
        size: TerminalSize,
        remote: TerminalWriter,
    ) -> (Arc<PaseoTerminalPane>, flume::Receiver<Vec<u8>>) {
        let (input_tx, input_rx) = flume::unbounded::<Vec<u8>>();
        let term_config = Arc::new(config::TermConfig::new());
        let terminal = Terminal::new(
            size,
            term_config,
            "paseo",
            "1.0",
            Box::new(ChannelWriter {
                tx: input_tx.clone(),
            }),
        );
        let pane = Arc::new(PaseoTerminalPane {
            pane_id,
            domain_id,
            remote_terminal_id,
            terminal: Mutex::new(terminal),
            writer: Mutex::new(Box::new(ChannelWriter { tx: input_tx })),
            remote,
            dead: AtomicBool::new(false),
        });
        (pane, input_rx)
    }

    pub fn remote_terminal_id(&self) -> &str {
        &self.remote_terminal_id
    }

    pub fn start_io(self: &Arc<Self>, handle: TerminalHandle, input_rx: flume::Receiver<Vec<u8>>) {
        let weak: Weak<PaseoTerminalPane> = Arc::downgrade(self);
        let output_rx = handle.output();
        promise::spawn::spawn_into_main_thread(async move {
            while let Ok(event) = output_rx.recv_async().await {
                let Some(pane) = weak.upgrade() else {
                    break;
                };
                match event {
                    TerminalStreamEvent::Output(bytes) | TerminalStreamEvent::Restore(bytes) => {
                        pane.terminal.lock().advance_bytes(&bytes);
                        Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id));
                    }
                    TerminalStreamEvent::Snapshot(_) => {}
                }
            }
            if let Some(pane) = weak.upgrade() {
                pane.dead.store(true, Ordering::Relaxed);
                let pane_id = pane.pane_id;
                Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane_id));
                let mux = Mux::get();
                match config::configuration().exit_behavior {
                    config::ExitBehavior::Hold => mux.prune_dead_windows(),
                    config::ExitBehavior::Close | config::ExitBehavior::CloseOnCleanExit => {
                        mux.remove_pane(pane_id)
                    }
                }
            }
        })
        .detach();

        let remote = self.remote.clone();
        promise::spawn::spawn(async move {
            while let Ok(bytes) = input_rx.recv_async().await {
                let _ = remote.send_input(&bytes).await;
            }
        })
        .detach();
    }
}

impl Pane for PaseoTerminalPane {
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
        self.terminal.lock().get_title().to_string()
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
        let remote = self.remote.clone();
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

    fn palette(&self) -> ColorPalette {
        self.terminal.lock().palette()
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.terminal.lock().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.lock().get_config())
    }

    fn copy_user_vars(&self) -> std::collections::HashMap<String, String> {
        self.terminal.lock().user_vars().clone()
    }

    fn is_mouse_grabbed(&self) -> bool {
        self.terminal.lock().is_mouse_grabbed()
    }

    fn is_alt_screen_active(&self) -> bool {
        self.terminal.lock().is_alt_screen_active()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.terminal.lock().get_current_dir().cloned()
    }
}
