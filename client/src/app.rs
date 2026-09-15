//! The `eframe` application: document state, tools, input handling and UI.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::{
    ClientId, ClientMessage, CursorState, LamportClock, Rgba, ServerMessage, Stroke, StrokeDelta,
    StrokeId, StrokeOp, StrokeSet, geom,
};
use egui::{
    Align2, Area, Color32, CursorIcon, Frame, Id, Key, Modifiers, Order, PointerButton, Pos2, Rect,
    RichText, Sense, Stroke as EguiStroke, Ui, Vec2,
};
use tracing::{debug, warn};

use crate::canvas::{self, Camera};
use crate::gpu::StrokeRenderer;
use crate::net::{NetConfig, NetEvent, NetHandle};

/// Curated brush palette shown as swatches in the toolbar.
pub const PALETTE: [Rgba; 10] = [
    Rgba::from_hex(0x1e1e2e),
    Rgba::from_hex(0xf38ba8),
    Rgba::from_hex(0xfab387),
    Rgba::from_hex(0xf9e2af),
    Rgba::from_hex(0xa6e3a1),
    Rgba::from_hex(0x94e2d5),
    Rgba::from_hex(0x89b4fa),
    Rgba::from_hex(0xcba6f7),
    Rgba::from_hex(0xf5c2e7),
    Rgba::from_hex(0xffffff),
];

/// Pick a stable presence colour for a peer from its id.
pub fn presence_color(id: ClientId) -> Rgba {
    let bytes = id.as_bytes();
    let mix = bytes
        .iter()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(*b as u32));
    // Skip the near-black and white entries: they are poor cursor colours.
    PALETTE[1 + (mix as usize % (PALETTE.len() - 2))]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pen,
    Eraser,
    Pan,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Pen => "✏ Pen",
            Tool::Eraser => "◻ Eraser",
            Tool::Pan => "✋ Pan",
        }
    }

    fn hotkey(self) -> &'static str {
        match self {
            Tool::Pen => "P",
            Tool::Eraser => "E",
            Tool::Pan => "H",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Connection {
    Connecting {
        attempt: u32,
    },
    /// Socket open, waiting for `Welcome`.
    Handshaking,
    Online,
    Offline {
        reason: String,
    },
}

/// The replicated stroke set plus a counter that advances on every visible
/// change, so renderers can tell at a glance whether anything moved.
#[derive(Default)]
struct Document {
    set: StrokeSet,
    generation: u64,
}

impl Document {
    fn apply(&mut self, op: StrokeOp) -> bool {
        let changed = self.set.apply(op);
        self.generation += u64::from(changed);
        changed
    }

    fn apply_all(&mut self, ops: impl IntoIterator<Item = StrokeOp>) -> usize {
        let changed = self.set.apply_all(ops);
        self.generation += changed as u64;
        changed
    }

    fn merge(&mut self, other: StrokeSet) -> usize {
        let changed = self.set.merge(other);
        self.generation += changed as u64;
        changed
    }
}

impl std::ops::Deref for Document {
    type Target = StrokeSet;
    fn deref(&self) -> &StrokeSet {
        &self.set
    }
}

/// A stroke being drawn locally: the payload plus how much of it has already
/// been streamed to peers as `StrokeDelta`s.
struct Drawing {
    stroke: Stroke,
    sent: usize,
}

const MAX_HISTORY: usize = 200;
const CURSOR_SEND_INTERVAL: Duration = Duration::from_millis(33);
/// Minimum pointer travel (screen px) before a new point is recorded.
const MIN_SEGMENT_PX: f32 = 1.5;
/// RDP tolerance (screen px at drawing zoom) applied when a stroke is committed.
const SIMPLIFY_PX: f32 = 0.4;

pub struct WeavedrawApp {
    // Identity & document
    me: ClientId,
    name: String,
    presence: Rgba,
    room: String,
    doc: Document,
    clock: LamportClock,

    // Editing
    tool: Tool,
    brush_color: Rgba,
    brush_width: f32,
    drawing: Option<Drawing>,
    /// Ops applied in one gesture (a stroke or an eraser drag). Undo pops one.
    undo: Vec<Vec<StrokeOp>>,
    redo: Vec<Vec<StrokeOp>>,
    /// Ids tombstoned during the current eraser drag (undo groups them).
    erasing: Option<Vec<StrokeOp>>,

    // View
    camera: Camera,
    space_held: bool,
    /// GPU-resident committed strokes; `None` falls back to painting
    /// through egui every frame (e.g. when eframe runs without OpenGL).
    renderer: Option<StrokeRenderer>,
    /// Last frame's CPU time as reported by eframe (for the status bar).
    frame_cpu: Option<Duration>,

    // Presence
    peers: HashMap<ClientId, CursorState>,
    /// In-progress strokes from peers, keyed by stroke id.
    previews: HashMap<StrokeId, Stroke>,
    last_cursor: Option<CursorState>,
    last_cursor_sent: Instant,

    // Network
    net: NetHandle,
    conn: Connection,
    rtt: Option<Duration>,
}

impl WeavedrawApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: NetConfig) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let net = crate::net::spawn(config.clone(), cc.egui_ctx.clone());
        let renderer = match cc.gl.clone().map(StrokeRenderer::new) {
            Some(Ok(r)) => Some(r),
            Some(Err(e)) => {
                warn!("GPU stroke renderer unavailable, using CPU path: {e}");
                None
            }
            None => {
                warn!("no OpenGL context, using CPU path");
                None
            }
        };
        Self {
            me: config.client_id,
            name: config.name,
            presence: config.color,
            room: config.room,
            doc: Document::default(),
            clock: LamportClock::new(config.client_id),
            tool: Tool::Pen,
            brush_color: PALETTE[6],
            brush_width: 4.0,
            drawing: None,
            undo: Vec::new(),
            redo: Vec::new(),
            erasing: None,
            camera: Camera::default(),
            space_held: false,
            renderer,
            frame_cpu: None,
            peers: HashMap::new(),
            previews: HashMap::new(),
            last_cursor: None,
            last_cursor_sent: Instant::now(),
            net,
            conn: Connection::Connecting { attempt: 1 },
            rtt: None,
        }
    }

    // ---- replication ------------------------------------------------------

    /// Apply local ops to the document, ship them, and record them for undo.
    fn commit(&mut self, ops: Vec<StrokeOp>, record: bool) {
        if ops.is_empty() {
            return;
        }
        self.doc.apply_all(ops.iter().cloned());
        if record {
            self.undo.push(ops.clone());
            if self.undo.len() > MAX_HISTORY {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.net.send(ClientMessage::Ops(ops));
    }

    fn invert(&mut self, ops: &[StrokeOp]) -> Vec<StrokeOp> {
        // Reverse order so a group undoes cleanly; timestamps stay ascending.
        ops.iter()
            .rev()
            .filter_map(|op| {
                let ts = self.clock.tick();
                self.doc.inverse(op, ts)
            })
            .collect()
    }

    fn undo(&mut self) {
        let Some(group) = self.undo.pop() else { return };
        let inverse = self.invert(&group);
        self.doc.apply_all(inverse.iter().cloned());
        self.net.send(ClientMessage::Ops(inverse.clone()));
        self.redo.push(inverse);
    }

    fn redo(&mut self) {
        let Some(group) = self.redo.pop() else { return };
        let inverse = self.invert(&group);
        self.doc.apply_all(inverse.iter().cloned());
        self.net.send(ClientMessage::Ops(inverse.clone()));
        self.undo.push(inverse);
    }

    fn clear_mine(&mut self) {
        let ops = self.doc.clear_ops_for(self.me, &mut self.clock);
        self.commit(ops, true);
    }

    fn handle_net(&mut self) {
        let events: Vec<NetEvent> = self.net.poll().collect();
        for ev in events {
            match ev {
                NetEvent::Connecting { attempt } => self.conn = Connection::Connecting { attempt },
                NetEvent::Connected => self.conn = Connection::Handshaking,
                NetEvent::Rtt(d) => self.rtt = Some(d),
                NetEvent::Disconnected { reason } => {
                    self.conn = Connection::Offline { reason };
                    self.rtt = None;
                    self.peers.clear();
                    self.previews.clear();
                }
                NetEvent::Message(msg) => self.handle_server(msg),
            }
        }
    }

    fn handle_server(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Welcome {
                snapshot, peers, ..
            } => {
                self.conn = Connection::Online;
                self.merge_snapshot(snapshot);
                self.peers = peers.into_iter().map(|p| (p.client_id, p)).collect();
                self.previews.clear();
                // Force the next frame to (re)announce where we are.
                self.last_cursor = None;
            }
            ServerMessage::Snapshot(set) => self.merge_snapshot(set),
            ServerMessage::Ops { from, ops } => {
                debug!(%from, n = ops.len(), "remote ops");
                for op in ops {
                    self.clock.observe(op.timestamp());
                    if let StrokeOp::Add { stroke, .. } = &op {
                        self.previews.remove(&stroke.id);
                    }
                    self.doc.apply(op);
                }
            }
            ServerMessage::Cursor(c) => {
                if c.client_id != self.me {
                    self.peers.insert(c.client_id, c);
                }
            }
            ServerMessage::StrokeDelta(d) => self.apply_delta(d),
            ServerMessage::PeerJoined(p) => {
                if p.client_id != self.me {
                    self.peers.insert(p.client_id, p);
                }
            }
            ServerMessage::PeerLeft(id) => {
                self.peers.remove(&id);
                self.previews.retain(|_, s| s.client_id != id);
            }
            ServerMessage::Rejected(r) => {
                warn!("server rejected us: {r}");
                self.conn = Connection::Offline {
                    reason: format!("rejected: {r}"),
                };
            }
            ServerMessage::Pong(_) => {}
        }
    }

    fn merge_snapshot(&mut self, set: StrokeSet) {
        if let Some(ts) = set.latest_timestamp() {
            self.clock.observe(ts);
        }
        let changed = self.doc.merge(set);
        debug!(changed, strokes = self.doc.len(), "snapshot merged");
    }

    fn apply_delta(&mut self, d: StrokeDelta) {
        if d.client_id == self.me || self.doc.contains(d.stroke_id) {
            return;
        }
        let StrokeDelta {
            stroke_id,
            client_id,
            color,
            width,
            points,
        } = d;
        self.previews
            .entry(stroke_id)
            .or_insert_with(|| Stroke {
                id: stroke_id,
                client_id,
                color,
                width,
                points: Vec::new(),
            })
            .points
            .extend(points);
    }

    // ---- input ------------------------------------------------------------

    fn handle_shortcuts(&mut self, ui: &Ui) {
        let mut undo = false;
        let mut redo = false;
        let mut clear = false;
        let mut tool = None;
        let mut width_step = 0i8;
        let mut reset_view = false;

        // A focused text field (e.g. the colour picker's hex box) owns the keyboard.
        if ui.ctx().egui_wants_keyboard_input() {
            self.space_held = false;
            return;
        }
        ui.input_mut(|i| {
            self.space_held = i.key_down(Key::Space);
            undo = i.consume_key(Modifiers::COMMAND, Key::Z);
            redo = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
                || i.consume_key(Modifiers::COMMAND, Key::Y);
            clear = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Backspace);
            reset_view = i.consume_key(Modifiers::NONE, Key::Num0)
                || i.consume_key(Modifiers::COMMAND, Key::Num0);
            if i.consume_key(Modifiers::NONE, Key::P) {
                tool = Some(Tool::Pen);
            }
            if i.consume_key(Modifiers::NONE, Key::E) {
                tool = Some(Tool::Eraser);
            }
            if i.consume_key(Modifiers::NONE, Key::H) {
                tool = Some(Tool::Pan);
            }
            if i.consume_key(Modifiers::NONE, Key::OpenBracket) {
                width_step = -1;
            }
            if i.consume_key(Modifiers::NONE, Key::CloseBracket) {
                width_step = 1;
            }
        });

        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if clear {
            self.clear_mine();
        }
        if let Some(t) = tool {
            self.tool = t;
        }
        if width_step != 0 {
            let step = if self.brush_width < 8.0 { 1.0 } else { 2.0 };
            self.brush_width = (self.brush_width + step * width_step as f32).clamp(1.0, 32.0);
        }
        if reset_view {
            self.camera = Camera::default();
        }
    }

    fn canvas_input(&mut self, ui: &mut Ui, response: &egui::Response, rect: Rect) {
        // ---- zoom & scroll-pan (work regardless of tool) --------------------
        if response.hovered() {
            let (zoom, scroll, ctrl) =
                ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.modifiers.command));
            let anchor = response.hover_pos().unwrap_or_else(|| rect.center());
            if zoom != 1.0 {
                self.camera.zoom_at(rect, anchor, zoom);
            } else if ctrl && scroll.y != 0.0 {
                self.camera.zoom_at(rect, anchor, (scroll.y * 0.005).exp());
            } else if scroll != Vec2::ZERO {
                self.camera.pan(scroll);
            }
        }

        // ---- drag-pan: middle button, Space, or the Pan tool --------------
        let pan_tool = self.tool == Tool::Pan || self.space_held;
        if response.dragged_by(PointerButton::Middle)
            || (pan_tool && response.dragged_by(PointerButton::Primary))
        {
            self.camera.pan(response.drag_delta());
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            return;
        }
        if pan_tool {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
            self.cancel_gesture();
            return;
        }

        match self.tool {
            Tool::Pen => self.pen_input(ui, response, rect),
            Tool::Eraser => self.eraser_input(ui, response, rect),
            Tool::Pan => {}
        }
    }

    /// Drop any half-finished gesture (e.g. the tool changed mid-drag).
    fn cancel_gesture(&mut self) {
        self.drawing = None;
        if let Some(group) = self.erasing.take() {
            self.finish_erase(group);
        }
    }

    fn pen_input(&mut self, ui: &mut Ui, response: &egui::Response, rect: Rect) {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);

        if response.drag_started_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let world = self.camera.to_world(rect, pos);
            let stroke =
                Stroke::new(self.me, self.brush_color, self.brush_width).with_points([world]);
            self.drawing = Some(Drawing { stroke, sent: 0 });
        }

        if response.dragged_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(d) = &mut self.drawing
        {
            let world = self.camera.to_world(rect, pos);
            let far_enough = d
                .stroke
                .points
                .last()
                .is_none_or(|last| last.distance(world) * self.camera.zoom >= MIN_SEGMENT_PX);
            if far_enough {
                d.stroke.points.push(world);
            }
        }

        // Stream the new points to peers.
        if let Some(d) = &mut self.drawing
            && d.sent < d.stroke.points.len()
        {
            let delta = StrokeDelta {
                stroke_id: d.stroke.id,
                client_id: self.me,
                color: d.stroke.color,
                width: d.stroke.width,
                points: d.stroke.points[d.sent..].to_vec(),
            };
            d.sent = d.stroke.points.len();
            self.net.send(ClientMessage::StrokeDelta(delta));
        }

        if response.drag_stopped_by(PointerButton::Primary)
            && let Some(mut d) = self.drawing.take()
        {
            // Shed redundant samples before the stroke is replicated; the
            // tolerance is a fraction of a pixel at the zoom it was drawn at.
            d.stroke.points = geom::simplify(&d.stroke.points, SIMPLIFY_PX / self.camera.zoom);
            let ts = self.clock.tick();
            self.commit(
                vec![StrokeOp::Add {
                    stroke: d.stroke,
                    ts,
                }],
                true,
            );
        }
    }

    fn eraser_input(&mut self, ui: &mut Ui, response: &egui::Response, rect: Rect) {
        ui.ctx().set_cursor_icon(CursorIcon::None);
        let tolerance = 6.0 / self.camera.zoom;

        let pressed = response.drag_started_by(PointerButton::Primary)
            || response.clicked_by(PointerButton::Primary);
        if pressed && self.erasing.is_none() {
            self.erasing = Some(Vec::new());
        }

        let held = response.dragged_by(PointerButton::Primary) || pressed;
        if held && let Some(pos) = response.interact_pointer_pos() {
            let world = self.camera.to_world(rect, pos);
            // Topmost hit wins: iterate in reverse draw order.
            let hit = self
                .doc
                .visible()
                .rev()
                .find(|s| canvas::hit_test(s, world, tolerance))
                .map(|s| s.id);
            if let Some(id) = hit {
                let op = StrokeOp::Remove {
                    id,
                    ts: self.clock.tick(),
                };
                self.doc.apply(op.clone());
                self.net.send(ClientMessage::Ops(vec![op.clone()]));
                if let Some(group) = &mut self.erasing {
                    group.push(op);
                }
            }
        }

        if (response.drag_stopped_by(PointerButton::Primary) || (pressed && !response.dragged()))
            && let Some(group) = self.erasing.take()
        {
            self.finish_erase(group);
        }
    }

    fn finish_erase(&mut self, group: Vec<StrokeOp>) {
        if !group.is_empty() {
            self.undo.push(group);
            if self.undo.len() > MAX_HISTORY {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    /// Announce pointer position/drawing state to peers, rate-limited.
    fn send_cursor(&mut self, response: &egui::Response, rect: Rect) {
        if self.conn != Connection::Online {
            return;
        }
        let position = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos())
            .map(|p| self.camera.to_world(rect, p));
        let mut state = CursorState::new(self.me, self.name.clone(), self.presence);
        state.position = position;
        state.drawing = self.drawing.is_some();

        let changed = self.last_cursor.as_ref() != Some(&state);
        let due = self.last_cursor_sent.elapsed() >= CURSOR_SEND_INTERVAL;
        // Leaving the canvas is sent immediately so our cursor doesn't linger.
        let urgent = position.is_none()
            && self
                .last_cursor
                .as_ref()
                .is_some_and(|c| c.position.is_some());
        if changed && (due || urgent) {
            self.net.send(ClientMessage::Cursor(state.clone()));
            self.last_cursor = Some(state);
            self.last_cursor_sent = Instant::now();
        }
    }

    // ---- painting ---------------------------------------------------------

    fn paint_canvas(
        &mut self,
        ui: &Ui,
        painter: &egui::Painter,
        rect: Rect,
        pointer: Option<Pos2>,
    ) {
        let visuals = ui.visuals();
        painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
        canvas::paint_grid(
            painter,
            &self.camera,
            rect,
            visuals.weak_text_color().gamma_multiply(0.35),
        );

        match &mut self.renderer {
            Some(r) => {
                r.sync(ui.ctx(), &self.doc, self.camera.zoom, self.doc.generation);
                painter.add(r.paint(rect, self.camera));
            }
            None => {
                for stroke in self.doc.visible() {
                    canvas::paint_stroke(painter, &self.camera, rect, stroke, 1.0);
                }
            }
        }
        for preview in self.previews.values() {
            canvas::paint_stroke(painter, &self.camera, rect, preview, 0.7);
        }
        if let Some(d) = &self.drawing {
            canvas::paint_stroke(painter, &self.camera, rect, &d.stroke, 1.0);
        }

        // Remote cursors.
        for peer in self.peers.values() {
            let Some(p) = peer.position else { continue };
            let s = self.camera.to_screen(rect, p);
            if !rect.contains(s) {
                continue;
            }
            let color = canvas::to_color32(peer.color);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    s,
                    s + Vec2::new(0.0, 16.0),
                    s + Vec2::new(4.5, 12.5),
                    s + Vec2::new(11.0, 11.0),
                ],
                color,
                EguiStroke::new(1.0, Color32::BLACK),
            ));
            let label = if peer.drawing {
                format!("{} ✏", peer.name)
            } else {
                peer.name.clone()
            };
            let galley =
                painter.layout_no_wrap(label, egui::FontId::proportional(12.0), Color32::BLACK);
            let origin = s + Vec2::new(14.0, 14.0);
            let bg = Rect::from_min_size(origin, galley.size() + Vec2::splat(6.0));
            painter.rect_filled(bg, 4.0, color);
            painter.galley(origin + Vec2::splat(3.0), galley, Color32::BLACK);
        }

        // Eraser reticle.
        if self.tool == Tool::Eraser
            && !self.space_held
            && let Some(p) = pointer
        {
            painter.circle_stroke(p, 8.0, EguiStroke::new(1.0, visuals.strong_text_color()));
        }
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        Area::new(Id::new("toolbar"))
            .order(Order::Foreground)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 12.0))
            .show(ctx, |ui| {
                Frame::window(ui.style()).inner_margin(8.0).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for tool in [Tool::Pen, Tool::Eraser, Tool::Pan] {
                            let selected = self.tool == tool;
                            if ui
                                .selectable_label(selected, tool.label())
                                .on_hover_text(format!("Shortcut: {}", tool.hotkey()))
                                .clicked()
                            {
                                self.cancel_gesture();
                                self.tool = tool;
                            }
                        }
                        ui.separator();

                        for color in PALETTE {
                            let c32 = canvas::to_color32(color);
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                            let selected = self.brush_color == color;
                            let ring = if selected {
                                EguiStroke::new(2.0, ui.visuals().strong_text_color())
                            } else if resp.hovered() {
                                EguiStroke::new(1.0, ui.visuals().text_color())
                            } else {
                                EguiStroke::new(1.0, ui.visuals().weak_text_color())
                            };
                            ui.painter().circle(rect.center(), 8.0, c32, ring);
                            if resp.clicked() {
                                self.brush_color = color;
                            }
                        }
                        let mut custom = canvas::to_color32(self.brush_color);
                        if ui.color_edit_button_srgba(&mut custom).changed() {
                            self.brush_color = canvas::from_color32(custom);
                        }
                        ui.separator();

                        ui.add(
                            egui::Slider::new(&mut self.brush_width, 1.0..=32.0)
                                .show_value(false)
                                .logarithmic(true),
                        )
                        .on_hover_text(format!("Width {:.0}  ([ / ])", self.brush_width));
                        let preview = ui.allocate_exact_size(Vec2::splat(34.0), Sense::hover()).0;
                        ui.painter().circle_filled(
                            preview.center(),
                            (self.brush_width * 0.5).clamp(1.0, 16.0),
                            canvas::to_color32(self.brush_color),
                        );
                        ui.separator();

                        if ui
                            .add_enabled(!self.undo.is_empty(), egui::Button::new("Undo"))
                            .on_hover_text("Undo (Ctrl+Z)")
                            .clicked()
                        {
                            self.undo();
                        }
                        if ui
                            .add_enabled(!self.redo.is_empty(), egui::Button::new("Redo"))
                            .on_hover_text("Redo (Ctrl+Shift+Z)")
                            .clicked()
                        {
                            self.redo();
                        }
                        if ui
                            .button("Clear")
                            .on_hover_text("Clear my strokes (Ctrl+Shift+Backspace)")
                            .clicked()
                        {
                            self.clear_mine();
                        }
                    });
                });
            });
    }

    fn peer_list(&self, ctx: &egui::Context) {
        Area::new(Id::new("peers"))
            .order(Order::Foreground)
            .anchor(Align2::RIGHT_TOP, Vec2::new(-12.0, 12.0))
            .interactable(false)
            .show(ctx, |ui| {
                Frame::window(ui.style()).inner_margin(8.0).show(ui, |ui| {
                    let mut row = |color: Rgba, name: &str, drawing: bool, me: bool| {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                            ui.painter()
                                .circle_filled(r.center(), 5.0, canvas::to_color32(color));
                            let mut text = RichText::new(name);
                            if me {
                                text = text.strong();
                            }
                            ui.label(text);
                            if drawing {
                                ui.label(RichText::new("✏").small());
                            }
                        });
                    };
                    row(
                        self.presence,
                        &format!("{} (you)", self.name),
                        self.drawing.is_some(),
                        true,
                    );
                    let mut peers: Vec<&CursorState> = self.peers.values().collect();
                    peers.sort_by(|a, b| a.name.cmp(&b.name));
                    for p in peers {
                        row(p.color, &p.name, p.drawing, false);
                    }
                });
            });
    }

    fn status_bar(&self, ctx: &egui::Context) {
        Area::new(Id::new("status"))
            .order(Order::Foreground)
            .anchor(Align2::LEFT_BOTTOM, Vec2::new(12.0, -12.0))
            .interactable(false)
            .show(ctx, |ui| {
                Frame::window(ui.style()).inner_margin(6.0).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (dot, text) = match &self.conn {
                            Connection::Online => {
                                (Color32::from_rgb(0xa6, 0xe3, 0xa1), "online".to_owned())
                            }
                            Connection::Handshaking => {
                                (Color32::from_rgb(0xf9, 0xe2, 0xaf), "joining…".to_owned())
                            }
                            Connection::Connecting { attempt } => (
                                Color32::from_rgb(0xf9, 0xe2, 0xaf),
                                if *attempt > 1 {
                                    format!("reconnecting (#{attempt})…")
                                } else {
                                    "connecting…".to_owned()
                                },
                            ),
                            Connection::Offline { reason } => (
                                Color32::from_rgb(0xf3, 0x8b, 0xa8),
                                format!("offline — {reason}"),
                            ),
                        };
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, dot);
                        ui.label(text);
                        ui.separator();
                        ui.label(format!("room {}", self.room));
                        ui.separator();
                        ui.label(format!("{} strokes", self.doc.len()));
                        ui.separator();
                        ui.label(format!("{:.0}%", self.camera.zoom * 100.0))
                            .on_hover_text("Zoom (0 to reset)");
                        if let Some(rtt) = self.rtt {
                            ui.separator();
                            ui.label(format!("{} ms", rtt.as_millis()))
                                .on_hover_text("Round-trip time to the server");
                        }
                        if let Some(cpu) = self.frame_cpu {
                            ui.separator();
                            let detail = match &self.renderer {
                                Some(r) => format!(
                                    "GPU: {} chunks, {} cached meshes, {} tessellated this frame",
                                    r.chunk_count(),
                                    r.cached_meshes(),
                                    r.built_this_frame()
                                ),
                                None => "CPU rendering".to_owned(),
                            };
                            ui.label(format!("{:.1} ms/frame", cpu.as_secs_f64() * 1e3))
                                .on_hover_text(format!("CPU time per frame · {detail}"));
                        }
                    });
                });
            });
    }
}

impl eframe::App for WeavedrawApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(r) = &mut self.renderer {
            r.destroy();
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.frame_cpu = frame.info().cpu_usage.map(Duration::from_secs_f32);
        self.handle_net();
        self.handle_shortcuts(ui);

        // The root `Ui` has no margin, so the canvas fills the window and the
        // floating panels are layered on top of it.
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = response.rect;
        self.canvas_input(ui, &response, rect);
        self.send_cursor(&response, rect);
        let pointer = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        self.paint_canvas(ui, &painter, rect, pointer);

        let ctx = ui.ctx().clone();
        self.toolbar(&ctx);
        self.peer_list(&ctx);
        self.status_bar(&ctx);

        // Keep streaming while a gesture is in flight even if the pointer is
        // momentarily still; otherwise egui only repaints on input/net events.
        if self.drawing.is_some() || self.erasing.is_some() {
            ctx.request_repaint_after(CURSOR_SEND_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn presence_color_is_stable_and_in_palette() {
        let id = Uuid::new_v4();
        let c = presence_color(id);
        assert_eq!(c, presence_color(id));
        assert!(PALETTE[1..PALETTE.len() - 1].contains(&c));
    }
}
