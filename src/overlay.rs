use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{EventLoop, channel},
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

pub enum UiMsg {
    Listening(&'static str),
    Level(f32),
    Pause(f32),
    Partial(String),
    Transcribing,
    Done(String),
    Error(String),
    Hide,
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Listening,
    Transcribing,
    Done,
    Error,
}

const W: u32 = 640;
const BASE_H: u32 = 60;
const LINE_H: f32 = 24.0;
const MAX_LINES: usize = 3;
const BOTTOM: i32 = 110;
const TEXT_X: f32 = 74.0;
const TEXT_RIGHT: f32 = 96.0;

struct Surf {
    layer: LayerSurface,
    pool: SlotPool,
    h: u32,
    want_h: u32,
    scale: i32,
    configured: bool,
    frame_pending: bool,
}

struct Ui {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    qh: QueueHandle<Ui>,
    surf: Option<Surf>,
    mode: Mode,
    label: &'static str,
    text: String,
    levels: VecDeque<f32>,
    pause: f32,
    since: Instant,
    hide_at: Option<Instant>,
    dirty: bool,
    fonts: FontSystem,
    cache: SwashCache,
}

pub fn run(rx: channel::Channel<UiMsg>) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<Ui>(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<Ui> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow!("wayland source: {}", e.error))?;
    event_loop
        .handle()
        .insert_source(rx, |ev, _, ui: &mut Ui| {
            if let channel::Event::Msg(m) = ev {
                ui.handle(m);
            }
        })
        .map_err(|e| anyhow!("ui channel: {}", e.error))?;

    let mut ui = Ui {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh)?,
        layer_shell: LayerShell::bind(&globals, &qh)?,
        shm: Shm::bind(&globals, &qh)?,
        qh,
        surf: None,
        mode: Mode::Listening,
        label: "",
        text: String::new(),
        levels: VecDeque::from(vec![0.0; 24]),
        pause: 0.0,
        since: Instant::now(),
        hide_at: None,
        dirty: false,
        fonts: FontSystem::new(),
        cache: SwashCache::new(),
    };

    loop {
        let animating = ui.surf.is_some() && matches!(ui.mode, Mode::Listening | Mode::Transcribing);
        let timeout = if animating { Duration::from_millis(33) } else { Duration::from_millis(250) };
        event_loop.dispatch(Some(timeout), &mut ui)?;
        ui.tick();
    }
}

impl Ui {
    fn handle(&mut self, m: UiMsg) {
        match m {
            UiMsg::Listening(label) => {
                self.mode = Mode::Listening;
                self.label = label;
                self.text.clear();
                self.levels.iter_mut().for_each(|l| *l = 0.0);
                self.pause = 0.0;
                self.since = Instant::now();
                self.hide_at = None;
                self.show();
            }
            UiMsg::Level(l) => {
                let v = (l * 9.0).sqrt().min(1.0);
                self.levels.pop_front();
                self.levels.push_back(v);
            }
            UiMsg::Pause(p) => self.pause = p,
            UiMsg::Partial(t) => {
                if self.mode == Mode::Listening {
                    self.text = t;
                }
            }
            UiMsg::Transcribing => {
                self.mode = Mode::Transcribing;
                self.pause = 0.0;
                self.since = Instant::now();
            }
            UiMsg::Done(t) => {
                self.mode = Mode::Done;
                self.text = t;
                self.hide_at = Some(Instant::now() + Duration::from_millis(1400));
            }
            UiMsg::Error(t) => {
                self.mode = Mode::Error;
                self.text = t;
                self.show();
                self.hide_at = Some(Instant::now() + Duration::from_millis(2600));
            }
            UiMsg::Hide => self.surf = None,
        }
        self.dirty = true;
    }

    fn show(&mut self) {
        if self.surf.is_some() {
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        if let Ok(region) = Region::new(&self.compositor) {
            surface.set_input_region(Some(region.wl_region()));
        }
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("voicetype"), None);
        layer.set_anchor(Anchor::BOTTOM);
        layer.set_margin(0, 0, BOTTOM, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(W, BASE_H);
        layer.commit();
        let pool = match SlotPool::new((W * BASE_H * 4 * 4) as usize, &self.shm) {
            Ok(p) => p,
            Err(_) => return,
        };
        self.surf = Some(Surf { layer, pool, h: BASE_H, want_h: BASE_H, scale: 1, configured: false, frame_pending: false });
    }

    fn tick(&mut self) {
        if let Some(t) = self.hide_at {
            if Instant::now() >= t {
                self.hide_at = None;
                self.surf = None;
                return;
            }
        }
        if self.surf.is_some() && matches!(self.mode, Mode::Listening | Mode::Transcribing) {
            self.dirty = true;
        }
        let ready = self.surf.as_ref().is_some_and(|s| s.configured && !s.frame_pending);
        if self.dirty && ready {
            self.draw();
        }
    }

    fn display_text(&mut self, width: f32, scale: f32) -> (Buffer, usize, bool) {
        let (body, placeholder) = match (self.mode, self.text.is_empty()) {
            (Mode::Listening, true) if self.pause > 0.0 => ("Sending soon — keep talking to continue".to_string(), true),
            (Mode::Listening, true) => ("Listening…".to_string(), true),
            (Mode::Transcribing, true) => ("Transcribing…".to_string(), true),
            _ => (self.text.clone(), false),
        };
        let mut words: Vec<&str> = body.split_whitespace().collect();
        let metrics = Metrics::new(17.0 * scale, LINE_H * scale);
        let attrs = Attrs::new().family(Family::Name("Inter")).weight(Weight(if placeholder { 400 } else { 500 }));
        loop {
            let mut shown = words.join(" ");
            if words.len() < body.split_whitespace().count() {
                shown.insert_str(0, "… ");
            }
            let mut buf = Buffer::new(&mut self.fonts, metrics);
            buf.set_wrap(Wrap::WordOrGlyph);
            buf.set_size(Some(width * scale), None);
            buf.set_text(&shown, &attrs, Shaping::Advanced, None);
            buf.shape_until_scroll(&mut self.fonts, false);
            let lines = buf.layout_runs().count().max(1);
            if lines <= MAX_LINES || words.len() <= 1 {
                return (buf, lines.min(MAX_LINES), placeholder);
            }
            let drop = (words.len() / 8).max(1);
            words.drain(..drop);
        }
    }

    fn draw(&mut self) {
        self.dirty = false;
        let Some(scale) = self.surf.as_ref().map(|s| s.scale) else { return };
        let sf = scale as f32;
        let text_w = W as f32 - TEXT_X - TEXT_RIGHT;
        let (mut buf, lines, placeholder) = self.display_text(text_w, sf);
        let want_h = BASE_H + (lines as u32 - 1) * LINE_H as u32;

        let surf = self.surf.as_mut().unwrap();
        if want_h != surf.want_h {
            surf.want_h = want_h;
            surf.layer.set_size(W, want_h);
            surf.layer.commit();
        }
        let h = surf.h;
        let (pw, ph) = (W * scale as u32, h * scale as u32);
        let Some(mut pix) = Pixmap::new(pw, ph) else { return };

        let pill = rounded(1.0 * sf, 1.0 * sf, pw as f32 - 2.0 * sf, ph as f32 - 2.0 * sf, 22.0 * sf);
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color_rgba8(20, 19, 26, 238);
        pix.fill_path(&pill, &paint, FillRule::Winding, Transform::identity(), None);
        paint.set_color_rgba8(255, 255, 255, 34);
        pix.stroke_path(&pill, &paint, &Stroke { width: 1.0 * sf, ..Default::default() }, Transform::identity(), None);

        let cy = 30.0 * sf;
        let t = self.since.elapsed().as_secs_f32();
        match self.mode {
            Mode::Listening => {
                let n = 7;
                let recent: Vec<f32> = self.levels.iter().rev().take(n).copied().collect();
                for (i, lv) in recent.iter().rev().enumerate() {
                    let wobble = 0.12 + 0.08 * ((t * 7.0 + i as f32 * 0.9).sin() * 0.5 + 0.5);
                    let bh = ((lv.max(wobble)) * 26.0).max(4.0) * sf;
                    let x = (22.0 + i as f32 * 5.5) * sf;
                    paint.set_color_rgba8(255, 98, 88, 255);
                    if let Some(r) = Rect::from_xywh(x, cy - bh / 2.0, 3.2 * sf, bh) {
                        let p = rounded(r.x(), r.y(), r.width(), r.height(), 1.6 * sf);
                        pix.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
                    }
                }
            }
            Mode::Transcribing => {
                for i in 0..3 {
                    let a = ((t * 5.0 - i as f32 * 0.7).sin() * 0.5 + 0.5) * 200.0 + 55.0;
                    paint.set_color_rgba8(190, 180, 255, a as u8);
                    if let Some(p) = PathBuilder::from_circle((28.0 + i as f32 * 11.0) * sf, cy, 3.6 * sf) {
                        pix.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
                    }
                }
            }
            Mode::Done => {
                paint.set_color_rgba8(74, 222, 128, 255);
                let mut pb = PathBuilder::new();
                pb.move_to(25.0 * sf, cy);
                pb.line_to(32.0 * sf, cy + 7.0 * sf);
                pb.line_to(46.0 * sf, cy - 8.0 * sf);
                if let Some(p) = pb.finish() {
                    let st = Stroke { width: 3.4 * sf, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
                    pix.stroke_path(&p, &paint, &st, Transform::identity(), None);
                }
            }
            Mode::Error => {
                paint.set_color_rgba8(248, 113, 113, 255);
                if let Some(p) = PathBuilder::from_circle(36.0 * sf, cy, 9.0 * sf) {
                    pix.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
                }
            }
        }

        if self.mode == Mode::Listening && self.pause > 0.0 {
            let inset = 24.0 * sf;
            let track_w = pw as f32 - 2.0 * inset;
            let y = ph as f32 - 6.0 * sf;
            paint.set_color_rgba8(255, 255, 255, 26);
            pix.fill_path(&rounded(inset, y, track_w, 2.5 * sf, 1.25 * sf), &paint, FillRule::Winding, Transform::identity(), None);
            paint.set_color_rgba8(74, 222, 128, 230);
            pix.fill_path(&rounded(inset, y, (track_w * self.pause).max(2.5 * sf), 2.5 * sf, 1.25 * sf), &paint, FillRule::Winding, Transform::identity(), None);
        }

        let color = match (self.mode, placeholder) {
            (_, true) => Color::rgba(255, 255, 255, 120),
            (Mode::Error, _) => Color::rgba(254, 202, 202, 255),
            (Mode::Transcribing, _) => Color::rgba(255, 255, 255, 170),
            _ => Color::rgba(255, 255, 255, 245),
        };
        let total_h = lines as f32 * LINE_H;
        let ty = ((h as f32 - total_h) / 2.0 * sf) as i32;
        blend_text(&mut pix, &mut buf, &mut self.fonts, &mut self.cache, color, (TEXT_X * sf) as i32, ty);

        let mut chip = Buffer::new(&mut self.fonts, Metrics::new(11.0 * sf, 14.0 * sf));
        chip.set_size(Some(80.0 * sf), None);
        chip.set_text(
            self.label,
            &Attrs::new().family(Family::Name("Inter")).weight(Weight(600)),
            Shaping::Advanced,
            None,
        );
        chip.shape_until_scroll(&mut self.fonts, false);
        let cw = chip.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
        let cx = pw as f32 - cw - 22.0 * sf;
        let chip_rect = rounded(cx - 8.0 * sf, cy - 10.0 * sf, cw + 16.0 * sf, 20.0 * sf, 10.0 * sf);
        paint.set_color_rgba8(255, 255, 255, 22);
        pix.fill_path(&chip_rect, &paint, FillRule::Winding, Transform::identity(), None);
        blend_text(&mut pix, &mut chip, &mut self.fonts, &mut self.cache, Color::rgba(255, 255, 255, 150), cx as i32, (cy - 7.0 * sf) as i32);

        let surf = self.surf.as_mut().unwrap();
        let Ok((wbuf, canvas)) = surf.pool.create_buffer(pw as i32, ph as i32, pw as i32 * 4, wl_shm::Format::Argb8888) else {
            return;
        };
        for (dst, src) in canvas.chunks_exact_mut(4).zip(pix.data().chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
        let ws = surf.layer.wl_surface();
        ws.set_buffer_scale(scale);
        ws.damage_buffer(0, 0, pw as i32, ph as i32);
        ws.frame(&self.qh, FrameCallbackData(ws.clone()));
        surf.frame_pending = true;
        if wbuf.attach_to(ws).is_ok() {
            surf.layer.commit();
        }
    }
}

fn blend_text(pix: &mut Pixmap, buf: &mut Buffer, fonts: &mut FontSystem, cache: &mut SwashCache, color: Color, ox: i32, oy: i32) {
    let (w, h) = (pix.width() as i32, pix.height() as i32);
    let data = pix.data_mut();
    buf.draw(fonts, cache, color, |x, y, gw, gh, c| {
        let a = c.a() as u32;
        if a == 0 {
            return;
        }
        for yy in (oy + y).max(0)..(oy + y + gh as i32).min(h) {
            for xx in (ox + x).max(0)..(ox + x + gw as i32).min(w) {
                let i = ((yy * w + xx) * 4) as usize;
                let inv = 255 - a;
                data[i] = ((c.r() as u32 * a + data[i] as u32 * inv) / 255) as u8;
                data[i + 1] = ((c.g() as u32 * a + data[i + 1] as u32 * inv) / 255) as u8;
                data[i + 2] = ((c.b() as u32 * a + data[i + 2] as u32 * inv) / 255) as u8;
                data[i + 3] = (a + data[i + 3] as u32 * inv / 255) as u8;
            }
        }
    });
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(Rect::from_xywh(x, y, w.max(1.0), h.max(1.0)).unwrap()))
}

impl CompositorHandler for Ui {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, factor: i32) {
        if let Some(s) = self.surf.as_mut() {
            s.scale = factor.max(1);
            self.dirty = true;
        }
    }

    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        if let Some(s) = self.surf.as_mut() {
            s.frame_pending = false;
        }
        if self.dirty {
            self.draw();
        }
    }

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Ui {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Ui {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.surf = None;
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, cfg: LayerSurfaceConfigure, _: u32) {
        if let Some(s) = self.surf.as_mut() {
            if cfg.new_size.1 > 0 {
                s.h = cfg.new_size.1;
            }
            s.configured = true;
            s.frame_pending = false;
        }
        self.draw();
    }
}

impl ShmHandler for Ui {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Ui);

impl ProvidesRegistryState for Ui {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(Ui);
