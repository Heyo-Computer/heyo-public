use anyhow::{Context, Result, anyhow, bail};
use smithay_client_toolkit::{
    delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shm::{Shm, ShmHandler, raw::RawPool},
};
use std::io::{BufWriter, Write};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_shm},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

#[derive(Clone, Copy, Debug)]
struct Constraints {
    width: u32,
    height: u32,
    formats: [Option<wl_shm::Format>; 8],
    n_formats: usize,
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,

    constraints: Constraints,
    constraints_done: bool,
    session_stopped: bool,

    captured_transform: Option<u32>,
    frame_ready: bool,
    frame_failed: Option<u32>,
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers!(OutputState);
}

delegate_output!(App);
delegate_shm!(App);
delegate_registry!(App);

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for App {
    fn event(
        _: &mut Self, _: &ExtOutputImageCaptureSourceManagerV1,
        _: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for App {
    fn event(
        _: &mut Self, _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for App {
    fn event(
        _: &mut Self, _: &ExtImageCopyCaptureManagerV1,
        _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for App {
    fn event(
        state: &mut Self, _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event::*;
        match event {
            BufferSize { width, height } => {
                state.constraints.width = width;
                state.constraints.height = height;
            }
            ShmFormat { format } => {
                if let WEnum::Value(fmt) = format {
                    if state.constraints.n_formats < state.constraints.formats.len() {
                        state.constraints.formats[state.constraints.n_formats] = Some(fmt);
                        state.constraints.n_formats += 1;
                    }
                }
            }
            DmabufDevice { .. } | DmabufFormat { .. } => {}
            Done => state.constraints_done = true,
            Stopped => state.session_stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for App {
    fn event(
        state: &mut Self, _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event::*;
        match event {
            Transform { transform } => {
                if let WEnum::Value(t) = transform {
                    state.captured_transform = Some(t as u32);
                }
            }
            Damage { .. } | PresentationTime { .. } => {}
            Ready => state.frame_ready = true,
            Failed { reason } => {
                state.frame_failed = Some(match reason {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(v) => v,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        _: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

/// Capture an output to a PNG byte buffer. When `max_width` is set and the
/// capture's long edge exceeds it, the image is downscaled (aspect preserved)
/// before encoding — this bounds the base64 payload the MCP `screenshot` tool
/// returns into the model's context. The CLI passes `None` (full resolution).
pub fn capture_png(target_output: Option<&str>, max_width: Option<u32>) -> Result<Vec<u8>> {
    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).context("compositor lacks wl_shm")?,
        constraints: Constraints { width: 0, height: 0, formats: [None; 8], n_formats: 0 },
        constraints_done: false,
        session_stopped: false,
        captured_transform: None,
        frame_ready: false,
        frame_failed: None,
    };

    // Ensure outputs are populated so we can match by name.
    event_queue.roundtrip(&mut app)?;
    event_queue.roundtrip(&mut app)?;

    let output = pick_output(&app, target_output)?;

    let source_mgr: ExtOutputImageCaptureSourceManagerV1 = globals
        .bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(&qh, 1..=1, ())
        .context("compositor lacks ext_output_image_capture_source_manager_v1")?;
    let copy_mgr: ExtImageCopyCaptureManagerV1 = globals
        .bind::<ExtImageCopyCaptureManagerV1, _, _>(&qh, 1..=1, ())
        .context("compositor lacks ext_image_copy_capture_manager_v1")?;

    let source = source_mgr.create_source(&output, &qh, ());
    let session = copy_mgr.create_session(&source, Options::empty(), &qh, ());

    // Wait for buffer constraints.
    while !app.constraints_done && !app.session_stopped {
        event_queue.blocking_dispatch(&mut app)?;
    }
    if app.session_stopped {
        bail!("capture session stopped before delivering constraints");
    }
    let width = app.constraints.width;
    let height = app.constraints.height;
    if width == 0 || height == 0 {
        bail!("compositor advertised zero buffer dimensions");
    }
    // Pick an SHM format; prefer Xrgb8888/Argb8888.
    let fmt = pick_format(&app.constraints).context("no supported shm format advertised")?;
    let (bpp, _has_alpha) = format_bpp(fmt);
    let stride = (width as usize) * (bpp as usize);
    let size = stride * (height as usize);

    let mut pool = RawPool::new(size, &app.shm).context("create shm pool")?;
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        fmt,
        (),
        &qh,
    );

    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();

    // Wait for ready or failed.
    while !app.frame_ready && app.frame_failed.is_none() && !app.session_stopped {
        event_queue.blocking_dispatch(&mut app)?;
    }
    if let Some(reason) = app.frame_failed {
        bail!("frame capture failed (reason={reason})");
    }
    if !app.frame_ready {
        bail!("session stopped before frame ready");
    }

    // Read pixels out of the pool.
    let mem = pool.mmap();
    let pixels = &mem[..size];
    let rgba = convert_to_rgba(pixels, width, height, stride, fmt)?;

    let img = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| anyhow!("invalid image dimensions"))?;
    let dynimg = image::DynamicImage::ImageRgba8(img);
    let dynimg = match max_width {
        Some(max) if width.max(height) > max => {
            dynimg.resize(max, max, image::imageops::FilterType::Triangle)
        }
        _ => dynimg,
    };
    let mut png: Vec<u8> = Vec::new();
    dynimg.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;

    // Cleanup.
    drop(buffer);
    drop(frame);
    drop(session);
    drop(source);
    drop(source_mgr);
    drop(copy_mgr);
    Ok(png)
}

pub fn run(target_output: Option<&str>, file: Option<&str>) -> Result<()> {
    let png = capture_png(target_output, None)?;
    let mut out: Box<dyn Write> = match file {
        Some(path) => Box::new(BufWriter::new(
            std::fs::File::create(path).context("create output file")?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };
    out.write_all(&png)?;
    out.flush()?;
    Ok(())
}

fn pick_output(app: &App, name: Option<&str>) -> Result<wl_output::WlOutput> {
    let outputs: Vec<_> = app.output_state.outputs().collect();
    if outputs.is_empty() {
        bail!("no wl_outputs available");
    }
    if let Some(want) = name {
        for o in &outputs {
            if let Some(info) = app.output_state.info(o) {
                if info.name.as_deref() == Some(want) {
                    return Ok(o.clone());
                }
            }
        }
        bail!("no output named '{want}' (try `computer outputs`)");
    }
    Ok(outputs.into_iter().next().unwrap())
}

fn pick_format(c: &Constraints) -> Option<wl_shm::Format> {
    let prio = [
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Abgr8888,
    ];
    for p in prio {
        for f in c.formats.iter().take(c.n_formats).flatten() {
            if *f == p {
                return Some(p);
            }
        }
    }
    // Fallback: first available.
    c.formats.iter().take(c.n_formats).flatten().next().copied()
}

fn format_bpp(fmt: wl_shm::Format) -> (u32, bool) {
    use wl_shm::Format::*;
    match fmt {
        Xrgb8888 | Xbgr8888 => (4, false),
        Argb8888 | Abgr8888 => (4, true),
        _ => (4, false),
    }
}

fn convert_to_rgba(src: &[u8], w: u32, h: u32, stride: usize, fmt: wl_shm::Format) -> Result<Vec<u8>> {
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    use wl_shm::Format::*;
    for y in 0..(h as usize) {
        let row = &src[y * stride..y * stride + (w as usize) * 4];
        let dst_row = &mut out[y * (w as usize) * 4..(y + 1) * (w as usize) * 4];
        for x in 0..(w as usize) {
            let s = &row[x * 4..x * 4 + 4];
            let d = &mut dst_row[x * 4..x * 4 + 4];
            // Wayland SHM is little-endian: bytes [B,G,R,A] for ARGB8888.
            match fmt {
                Argb8888 | Xrgb8888 => {
                    d[0] = s[2]; // R
                    d[1] = s[1]; // G
                    d[2] = s[0]; // B
                    d[3] = if matches!(fmt, Argb8888) { s[3] } else { 0xFF };
                }
                Abgr8888 | Xbgr8888 => {
                    d[0] = s[0]; // R
                    d[1] = s[1]; // G
                    d[2] = s[2]; // B
                    d[3] = if matches!(fmt, Abgr8888) { s[3] } else { 0xFF };
                }
                _ => bail!("unsupported pixel format"),
            }
        }
    }
    Ok(out)
}
