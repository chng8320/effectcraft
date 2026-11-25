#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]
//! EffectCraft stream mode.
//!
//! Runs a chain of EffectCraft's effects (the same ids and parameters as the effect catalogue and
//! the engine's commands) over a live video stream: raw 8-bit RGBA frames in on one reader, the
//! processed frames out on one writer, frame by frame, so FFmpeg, GStreamer or a WebRTC agent can
//! sit on either side (`ffmpeg … -f rawvideo -pix_fmt rgba -`). Parameters can change while it
//! runs, through control messages (one JSON object per line), as in SoundCraft's stream mode.
//!
//! Each effect sees one frame at a time at the stream's time. Effects that need other frames,
//! other layers or a project (time remapping, tracking, layer references) run with what one frame
//! gives them; [`chainable`] lists the ones that need nothing more.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, TryRecvError};

use effectcraft_effects::{Buf, EffectCtx, EffectEnv, EffectSpec, Params, default_value, find};
use effectcraft_keyframe::Value;
use effectcraft_raster::Image;
use serde::{Deserialize, Serialize};

mod live;
pub use live::{LiveRun, live};

/// One effect of a chain: its id and the parameters to set.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ChainItem {
    pub id: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub bypass: bool,
}

/// A chain: `{"effects": [...]}` or just the list.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ChainSpec {
    Object { effects: Vec<ChainItem> },
    List(Vec<ChainItem>),
}

impl ChainSpec {
    pub fn items(&self) -> &[ChainItem] {
        match self {
            Self::Object { effects } | Self::List(effects) => effects,
        }
    }
}

/// Stream settings: the frame size and rate.
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    /// Frames per second: the time each effect sees (frame n is at n / fps seconds).
    pub fps: f64,
}

/// Which effect a control message means: its place in the chain, or its id (the first with it).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum EffectRef {
    Index(usize),
    Id(String),
}

/// A live change: `{"set": {"effect": 0 | "ec.blur.gaussian", "param": "blurriness", "value": 12}}`,
/// `{"bypass": {"effect": 1, "on": true}}` or `{"reset": true}` (every parameter back to the chain's).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Set { effect: EffectRef, param: String, value: serde_json::Value },
    Bypass { effect: EffectRef, on: bool },
    Reset(bool),
}

#[derive(Debug)]
pub enum StreamError {
    UnknownEffect(String),
    UnknownParam {
        effect: String,
        param: String,
    },
    BadValue {
        effect: String,
        param: String,
        why: String,
    },
    BadSize(String),
    Io(io::Error),
    /// The live input or output (FFmpeg) failed.
    Live(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownEffect(id) => write!(f, "unknown effect `{id}` (effectcraft-cli stream --list)"),
            Self::UnknownParam { effect, param } => write!(f, "{effect} has no parameter `{param}`"),
            Self::BadValue { effect, param, why } => write!(f, "{effect} {param}: {why}"),
            Self::BadSize(m) => write!(f, "{m}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Live(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A JSON value as the parameter's kind, going by its default: numbers for sliders, angles and
/// popups (0-based), `[x, y]` for points (pixels), `[r, g, b]`/`[r, g, b, a]` (0–1) or `"#rrggbb"`
/// for colours, booleans for checkboxes, strings for text; or EffectCraft's own `{"t":…,"v":…}`.
pub fn coerce(default: &Value, j: &serde_json::Value) -> Result<Value, String> {
    use serde_json::Value as J;
    if let J::Object(o) = j
        && o.contains_key("t")
    {
        return serde_json::from_value(j.clone()).map_err(|e| e.to_string());
    }
    let nums = |n: usize| -> Option<Vec<f64>> {
        let a = j.as_array()?;
        (a.len() == n).then(|| a.iter().filter_map(J::as_f64).collect::<Vec<_>>()).filter(|v| v.len() == n)
    };
    let out = match default {
        Value::Scalar(_) => j.as_f64().map(Value::Scalar),
        Value::Enum(_) => j.as_u64().and_then(|n| u32::try_from(n).ok()).map(Value::Enum),
        Value::Bool(_) => j.as_bool().map(Value::Bool),
        Value::Vec2(_) => nums(2).map(|v| Value::Vec2([v[0], v[1]])),
        Value::Vec3(_) => nums(3).map(|v| Value::Vec3([v[0], v[1], v[2]])),
        Value::Color(_) => match j {
            J::String(s) => hex_color(s).map(Value::Color),
            _ => nums(4).map(|v| Value::Color([v[0], v[1], v[2], v[3]])).or_else(|| nums(3).map(|v| Value::Color([v[0], v[1], v[2], 1.0]))),
        },
        Value::Str(_) => j.as_str().map(|s| Value::Str(s.to_string())),
        _ => None,
    };
    out.ok_or_else(|| format!("{j} doesn't fit this parameter (its default is {})", serde_json::to_string(default).unwrap_or_default()))
}

fn hex_color(s: &str) -> Option<[f64; 4]> {
    let h = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(|b| f64::from(b) / 255.0);
    match h.len() {
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 1.0]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

struct Slot {
    spec: &'static EffectSpec,
    params: Params,
    initial: Params,
    bypass: bool,
    seed: u32,
}

/// A chain ready to run on frames of one size.
pub struct Chain {
    slots: Vec<Slot>,
    cfg: StreamConfig,
}

impl Chain {
    pub fn new(spec: &ChainSpec, cfg: &StreamConfig) -> Result<Self, StreamError> {
        if cfg.width == 0 || cfg.height == 0 || cfg.width > 16384 || cfg.height > 16384 {
            return Err(StreamError::BadSize(format!("frame size {}x{} (1 to 16384 each)", cfg.width, cfg.height)));
        }
        let size = [f64::from(cfg.width), f64::from(cfg.height)];
        let mut slots = Vec::new();
        for (i, item) in spec.items().iter().enumerate() {
            let fx = find(&item.id).ok_or_else(|| StreamError::UnknownEffect(item.id.clone()))?;
            let mut params = Params { values: fx.params.iter().map(|p| (p.id.to_string(), default_value(p, size))).collect() };
            for (k, v) in &item.params {
                let d = params.values.get(k).ok_or_else(|| StreamError::UnknownParam { effect: item.id.clone(), param: k.clone() })?;
                let val = coerce(d, v).map_err(|why| StreamError::BadValue { effect: item.id.clone(), param: k.clone(), why })?;
                params.values.insert(k.clone(), val);
            }
            let seed = 1 + i as u32;
            slots.push(Slot { spec: fx, initial: Params { values: params.values.clone() }, params, bypass: item.bypass, seed });
        }
        Ok(Self { slots, cfg: *cfg })
    }

    /// One frame (premultiplied, the stream's size) at `time` seconds through every effect not
    /// bypassed; the result is cut back to the frame (effects that grow the layer, like blurs and
    /// glows, are cropped to it).
    pub fn process(&self, frame: Image, time: f64) -> Image {
        let size = [f64::from(self.cfg.width), f64::from(self.cfg.height)];
        let mut buf = Buf { img: frame, offset: [0.0, 0.0], scale: 1.0 };
        for (i, s) in self.slots.iter().enumerate().filter(|(_, s)| !s.bypass) {
            let env = EffectEnv { comp_time: time, frame_rate: self.cfg.fps, effect_index: i, ..Default::default() };
            let ctx = EffectCtx { params: &s.params, time, layer_size: size, seed: s.seed, adjustment: false, env };
            buf = effectcraft_effects::apply(s.spec, &ctx, buf);
        }
        crop(&buf, self.cfg.width, self.cfg.height)
    }

    pub fn apply(&mut self, c: &Control) -> Result<(), String> {
        match c {
            Control::Set { effect, param, value } => {
                let s = self.slot(effect).ok_or_else(|| format!("no effect {effect:?} in the chain"))?;
                let d = s.params.values.get(param).ok_or_else(|| format!("{} has no parameter `{param}`", s.spec.id))?;
                let v = coerce(d, value).map_err(|e| format!("{} {param}: {e}", s.spec.id))?;
                s.params.values.insert(param.clone(), v);
            }
            Control::Bypass { effect, on } => self.slot(effect).ok_or_else(|| format!("no effect {effect:?} in the chain"))?.bypass = *on,
            Control::Reset(_) => {
                for s in &mut self.slots {
                    s.params.values = s.initial.values.clone();
                }
            }
        }
        Ok(())
    }

    fn slot(&mut self, r: &EffectRef) -> Option<&mut Slot> {
        match r {
            EffectRef::Index(i) => self.slots.get_mut(*i),
            EffectRef::Id(id) => self.slots.iter_mut().find(|s| s.spec.id == id),
        }
    }
}

/// The frame-sized region of `buf` (layer pixel 0,0 sits at `buf.offset`), transparent where the
/// buffer doesn't reach.
fn crop(buf: &Buf, w: u32, h: u32) -> Image {
    let (ox, oy) = (buf.offset[0].round() as i64, buf.offset[1].round() as i64);
    if ox == 0 && oy == 0 && buf.img.width == w && buf.img.height == h && (buf.scale - 1.0).abs() < 1e-9 {
        return buf.img.clone();
    }
    let mut out = Image::new(w, h);
    let k = if buf.scale > 0.0 { buf.scale } else { 1.0 };
    for y in 0..h {
        for x in 0..w {
            let sx = (f64::from(x) * k).round() as i64 + ox;
            let sy = (f64::from(y) * k).round() as i64 + oy;
            if sx >= 0 && sy >= 0 && (sx as u32) < buf.img.width && (sy as u32) < buf.img.height {
                out.data[(y * w + x) as usize] = buf.img.data[(sy as u32 * buf.img.width + sx as u32) as usize];
            }
        }
    }
    out
}

/// Effects that need nothing but the frame: no other frames, layers, tracks or project.
pub fn chainable() -> Vec<&'static EffectSpec> {
    effectcraft_effects::all()
        .into_iter()
        .filter(|s| {
            !effectcraft_effects::is_time_dependent(s.id)
                && !s.id.starts_with("ec.obsolete.")
                && !s.id.starts_with("ec.audio.")
                && !s.params.iter().any(|p| matches!(p.default, Value::Layer(_) | Value::Path(_)))
        })
        .collect()
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct StreamStats {
    pub frames: u64,
    pub controls: u64,
    pub seconds_of_video: f64,
}

/// Read frames from `input` until it ends, run each through `chain`, write it to `output`.
/// Control messages waiting on `controls` apply between frames; an invalid one goes to `on_error`
/// and the stream goes on. A partial last frame is dropped.
pub fn run(
    chain: &mut Chain,
    input: &mut dyn Read,
    output: &mut dyn Write,
    controls: Option<&Receiver<Control>>,
    mut on_error: impl FnMut(String),
) -> Result<StreamStats, StreamError> {
    let (w, h) = (chain.cfg.width, chain.cfg.height);
    let mut raw = vec![0u8; w as usize * h as usize * 4];
    let mut stats = StreamStats::default();
    loop {
        if let Some(rx) = controls {
            loop {
                match rx.try_recv() {
                    Ok(c) => {
                        stats.controls += 1;
                        if let Err(e) = chain.apply(&c) {
                            on_error(e);
                        }
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
        if read_full(input, &mut raw)? < raw.len() {
            break;
        }
        let time = if chain.cfg.fps > 0.0 { stats.frames as f64 / chain.cfg.fps } else { 0.0 };
        let out = chain.process(Image::from_rgba8(w, h, &raw), time);
        output.write_all(&out.to_rgba8())?;
        stats.frames += 1;
    }
    output.flush()?;
    stats.seconds_of_video = if chain.cfg.fps > 0.0 { stats.frames as f64 / chain.cfg.fps } else { 0.0 };
    Ok(stats)
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// One control message from a line of text: `None` for a blank line.
pub fn parse_control(line: &str) -> Option<Result<Control, String>> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    Some(serde_json::from_str(t).map_err(|e| format!("control {t:?}: {e}")))
}

/// What `effectcraft-cli stream` takes.
pub struct ServeOptions {
    /// The chain: a JSON text or a file holding one.
    pub chain: String,
    pub config: StreamConfig,
    /// A file or FIFO of control messages, one per line.
    pub control: Option<String>,
}

/// Frames on standard input, frames on standard output, a JSON line to `log` when it starts and
/// when it ends.
pub fn serve(opts: &ServeOptions, log: &mut dyn Write) -> Result<StreamStats, StreamError> {
    let spec = load_chain(&opts.chain)?;
    let mut chain = Chain::new(&spec, &opts.config)?;
    let ids: Vec<&str> = spec.items().iter().map(|i| i.id.as_str()).collect();
    let _ = writeln!(
        log,
        "{}",
        serde_json::json!({"stream": "started", "effects": ids, "width": opts.config.width, "height": opts.config.height, "fps": opts.config.fps})
    );
    let rx = opts.control.as_ref().map(|path| {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path.clone();
        std::thread::spawn(move || {
            // A FIFO blocks here until a writer opens it; a plain file is read once.
            let Ok(f) = std::fs::File::open(&path) else { return };
            for line in io::BufRead::lines(io::BufReader::new(f)).map_while(Result::ok) {
                match parse_control(&line) {
                    Some(Ok(c)) => {
                        if tx.send(c).is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => log::warn!("{e}"),
                    None => {}
                }
            }
        });
        rx
    });
    let stdin = io::stdin();
    let stdout = io::stdout();
    let (mut i, mut o) = (stdin.lock(), stdout.lock());
    let mut errs = Vec::new();
    let stats = run(&mut chain, &mut i, &mut o, rx.as_ref(), |e| errs.push(e))?;
    for e in &errs {
        let _ = writeln!(log, "{}", serde_json::json!({"control_error": e}));
    }
    let _ = writeln!(log, "{}", serde_json::json!({"stream": "ended", "stats": stats}));
    Ok(stats)
}

/// A chain from a JSON text or a file holding one.
pub fn load_chain(chain: &str) -> Result<ChainSpec, StreamError> {
    let text = if chain.trim_start().starts_with(['[', '{']) { chain.to_string() } else { std::fs::read_to_string(chain)? };
    serde_json::from_str(&text).map_err(|e| StreamError::Io(io::Error::new(io::ErrorKind::InvalidData, format!("chain: {e}"))))
}

/// The effects a stream can run, with their parameters and defaults (for `--list`).
pub fn list_json() -> serde_json::Value {
    let fx: Vec<serde_json::Value> = chainable()
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id, "name": s.name, "category": s.category,
                "params": s.params.iter().map(|p| serde_json::json!({"id": p.id, "name": p.name, "default": p.default})).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({ "effects": fx })
}

#[cfg(test)]
mod tests;
