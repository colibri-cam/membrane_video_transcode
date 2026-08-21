use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::ptr;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use drm_fourcc::DrmFourcc;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::sys;
use ffmpeg_next::{
    Codec, codec,
    format::pixel::Pixel,
    software::scaling::{context::Context as Scaler, flag::Flags},
    util::{error::EAGAIN, frame::Video},
};
use rustler::types::reference::Reference;
use rustler::{
    Atom, Binary, Encoder, Env, Error, LocalPid, NifResult, OwnedBinary, ResourceArc, Term,
};
use video_interop::{
    AbandonmentGuard, Descriptor, Layer, Modifier, Object, Plane, ReleaseDispatcher,
    is_abandonment_guard_resource, new_abandonment_guard as make_abandonment_guard,
};

const NO_PTS: i64 = i64::MIN;
const DRM_FORMAT_MOD_INVALID: u64 = (1_u64 << 56) - 1;

// get_format callback: choose DRM_PRIME when offered
unsafe extern "C" fn get_format_drm_prime(
    _ctx: *mut sys::AVCodecContext,
    pix_fmts: *const sys::AVPixelFormat,
) -> sys::AVPixelFormat {
    unsafe {
        let mut p = pix_fmts;
        while !p.is_null() && *p != sys::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME {
                return *p;
            }
            p = p.add(1);
        }
        *pix_fmts
    }
}

#[derive(Clone, Copy)]
enum Backend {
    Auto,
    Vaapi,
    V4l2Request,
    V4l2M2M,
    Software,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Dmabuf,
    Raw,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScalerSpec {
    src_format: Pixel,
    src_width: u32,
    src_height: u32,
    dst_format: Pixel,
    dst_width: u32,
    dst_height: u32,
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.H265.Decoder.Native.DMABufFrame"]
struct DmabufFrame {
    width: u32,
    height: u32,
    modifier: Modifier,
    descriptor: Descriptor,
    keepalive: ResourceArc<Keepalive>,
}

struct DecoderInner {
    decoder: codec::decoder::Video,
    output_mode: OutputMode,
    raw_target: Option<Pixel>,
    raw_target_atom: Option<Atom>,
    scaler: Option<Scaler>,
    scaler_spec: Option<ScalerSpec>,
    width: u32,
    height: u32,
}

struct Decoder {
    inner: Mutex<DecoderInner>,
}

struct KeepaliveResources {
    frame: Video,
    object_fds: Vec<OwnedFd>,
}

pub struct Keepalive {
    resources: Mutex<Option<KeepaliveResources>>,
}

#[rustler::resource_impl]
impl rustler::Resource for Decoder {}

#[rustler::resource_impl]
impl rustler::Resource for Keepalive {}

impl Keepalive {
    fn new(frame: Video, object_fds: Vec<OwnedFd>) -> Self {
        Self {
            resources: Mutex::new(Some(KeepaliveResources { frame, object_fds })),
        }
    }

    fn release(&self) {
        if let Ok(mut guard) = self.resources.lock()
            && let Some(mut resources) = guard.take()
        {
            unsafe {
                sys::av_frame_unref(resources.frame.as_mut_ptr());
            }
            resources.object_fds.clear();
        }
    }
}

impl Drop for Keepalive {
    fn drop(&mut self) {
        self.release();
    }
}

unsafe impl Send for Keepalive {}
unsafe impl Sync for Keepalive {}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.decoder.flush();
            inner.scaler.take();
            inner.scaler_spec.take();
        }
    }
}

fn find_any(names: &[&str]) -> Option<Codec> {
    names
        .iter()
        .find_map(|name| codec::decoder::find_by_name(name))
}

fn init_decoder(
    output_mode: OutputMode,
    raw_target: Option<(Pixel, Atom)>,
    hw_device: Option<String>,
    backend: Backend,
) -> Result<Decoder> {
    ffmpeg::init().context("ffmpeg init failed")?;
    let mut use_v4l2 = matches!(backend, Backend::V4l2Request | Backend::V4l2M2M);
    let hevc = match backend {
        Backend::Auto => {
            if let Some(codec) = find_any(&["hevc_v4l2request", "h265_v4l2request"]) {
                use_v4l2 = true;
                codec
            } else if let Some(codec) = find_any(&["hevc_v4l2m2m", "h265_v4l2m2m"]) {
                use_v4l2 = true;
                codec
            } else {
                codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?
            }
        }
        Backend::V4l2Request => find_any(&["hevc_v4l2request", "h265_v4l2request"])
            .ok_or_else(|| anyhow!("no v4l2request codec"))?,
        Backend::V4l2M2M => find_any(&["hevc_v4l2m2m", "h265_v4l2m2m"])
            .ok_or_else(|| anyhow!("no v4l2m2m codec"))?,
        Backend::Vaapi | Backend::Software => {
            codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?
        }
    };

    let mut decoder = codec::decoder::new();
    unsafe {
        if use_v4l2 {
            let mut hw_device_ctx = ptr::null_mut();
            let ret = sys::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                sys::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if ret < 0 || hw_device_ctx.is_null() {
                return Err(anyhow!("av_hwdevice_ctx_create DRM failed: {ret}"));
            }
            (*decoder.as_mut_ptr()).hw_device_ctx = sys::av_buffer_ref(hw_device_ctx);
            (*decoder.as_mut_ptr()).get_format = Some(get_format_drm_prime);
            sys::av_buffer_unref(&mut hw_device_ctx);
        } else if matches!(backend, Backend::Vaapi | Backend::Auto) {
            let path_str = hw_device.unwrap_or_else(|| "/dev/dri/renderD129".to_string());
            if Path::new(&path_str).exists() {
                let mut hw_device_ctx = ptr::null_mut();
                if let Ok(path) = CString::new(path_str)
                    && sys::av_hwdevice_ctx_create(
                        &mut hw_device_ctx,
                        sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                        path.as_ptr(),
                        ptr::null_mut(),
                        0,
                    ) >= 0
                {
                    (*decoder.as_mut_ptr()).hw_device_ctx = sys::av_buffer_ref(hw_device_ctx);
                    sys::av_buffer_unref(&mut hw_device_ctx);
                }
            }
        }

        (*decoder.as_mut_ptr()).extra_hw_frames = 16;
    }

    let opened = decoder.open_as(hevc).context("open codec")?;
    let video = opened.video().context("video decoder")?;
    let (raw_target, raw_target_atom) =
        raw_target.map_or((None, None), |(pixel, atom)| (Some(pixel), Some(atom)));

    Ok(Decoder {
        inner: Mutex::new(DecoderInner {
            decoder: video,
            output_mode,
            raw_target,
            raw_target_atom,
            scaler: None,
            scaler_spec: None,
            width: 0,
            height: 0,
        }),
    })
}

fn derive_fourcc(desc: &sys::AVDRMFrameDescriptor) -> Result<DrmFourcc> {
    match desc.nb_layers {
        1 => DrmFourcc::try_from(desc.layers[0].format)
            .map_err(|_| anyhow!("unsupported fourcc {:#x}", desc.layers[0].format)),
        2 => {
            let l0 = DrmFourcc::try_from(desc.layers[0].format);
            let l1 = DrmFourcc::try_from(desc.layers[1].format);
            match (l0, l1) {
                (Ok(DrmFourcc::R8), Ok(DrmFourcc::Gr88))
                | (Ok(DrmFourcc::Gr88), Ok(DrmFourcc::R8)) => Ok(DrmFourcc::Nv12),
                #[cfg(feature = "rpi")]
                (Ok(DrmFourcc::R8), Ok(DrmFourcc::R8)) => Ok(DrmFourcc::Nv12),
                #[cfg(feature = "rpi")]
                (Ok(DrmFourcc::R16), Ok(DrmFourcc::Gr1616))
                | (Ok(DrmFourcc::Gr1616), Ok(DrmFourcc::R16)) => Ok(DrmFourcc::P010),
                _ => Err(anyhow!(
                    "unsupported layer formats {:#x} and {:#x}",
                    desc.layers[0].format,
                    desc.layers[1].format
                )),
            }
        }
        _ => Err(anyhow!(
            "unsupported DRM layer combination {}",
            desc.nb_layers
        )),
    }
}

fn owned_drm_frame(frame: &Video) -> Result<Video> {
    if frame.format() == Pixel::DRM_PRIME {
        let mut owned = Video::empty();
        let result = unsafe { sys::av_frame_ref(owned.as_mut_ptr(), frame.as_ptr()) };
        if result < 0 {
            return Err(anyhow!("av_frame_ref failed: {result}"));
        }
        return Ok(owned);
    }

    let hw_frames_ctx = unsafe { (*frame.as_ptr()).hw_frames_ctx };
    if hw_frames_ctx.is_null() {
        return Err(anyhow!("no hw_frames_ctx"));
    }

    let mut drm = Video::empty();
    unsafe {
        (*drm.as_mut_ptr()).format = sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*drm.as_mut_ptr()).width = frame.width() as i32;
        (*drm.as_mut_ptr()).height = frame.height() as i32;

        let ctx_ref = sys::av_buffer_ref(hw_frames_ctx);
        if ctx_ref.is_null() {
            return Err(anyhow!("av_buffer_ref failed"));
        }
        (*drm.as_mut_ptr()).hw_frames_ctx = ctx_ref;

        const AV_HWFRAME_MAP_DRM_PRIME: i32 = 0x0002_0000;
        let flags = (sys::AV_HWFRAME_MAP_READ as i32) | AV_HWFRAME_MAP_DRM_PRIME;
        let result = sys::av_hwframe_map(drm.as_mut_ptr(), frame.as_ptr(), flags);
        if result < 0 {
            sys::av_frame_unref(drm.as_mut_ptr());
            return Err(anyhow!("av_hwframe_map failed: {result}"));
        }
    }

    Ok(drm)
}

fn export_dmabuf(frame: &Video) -> Result<DmabufFrame> {
    let drm_frame = owned_drm_frame(frame)?;
    let desc_ptr = unsafe { (*drm_frame.as_ptr()).data[0] as *const sys::AVDRMFrameDescriptor };
    if desc_ptr.is_null() {
        return Err(anyhow!("no drm descriptor"));
    }

    let desc = unsafe { &*desc_ptr };
    if !(1..=4).contains(&desc.nb_objects) || !(1..=4).contains(&desc.nb_layers) {
        return Err(anyhow!(
            "invalid AVDRM object/layer counts: {}/{}",
            desc.nb_objects,
            desc.nb_layers
        ));
    }
    if derive_fourcc(desc)? != DrmFourcc::Nv12 {
        return Err(anyhow!("canonical DMA-BUF output currently requires NV12"));
    }

    let mut object_fds = Vec::with_capacity(desc.nb_objects as usize);
    let mut objects = Vec::with_capacity(desc.nb_objects as usize);
    let mut stream_modifier = None;

    for object_index in 0..desc.nb_objects as usize {
        let source = &desc.objects[object_index];
        if source.fd < 0 {
            return Err(anyhow!("object {object_index} has a negative fd"));
        }
        let duplicated = unsafe { libc::fcntl(source.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            return Err(anyhow!(
                "failed to duplicate object {object_index}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let owned = unsafe { OwnedFd::from_raw_fd(duplicated) };
        let modifier = if source.format_modifier == DRM_FORMAT_MOD_INVALID {
            Modifier::Implicit
        } else {
            Modifier::Explicit(source.format_modifier)
        };
        if let Some(expected) = stream_modifier {
            if expected != modifier {
                return Err(anyhow!("DMA-BUF objects use different modifiers"));
            }
        } else {
            stream_modifier = Some(modifier);
        }
        let size = u64::try_from(source.size).map_err(|_| anyhow!("object size overflow"))?;
        objects.push(Object {
            fd: owned.as_raw_fd(),
            size,
            modifier,
        });
        object_fds.push(owned);
    }

    let layer_order: Vec<usize> = if desc.nb_layers == 1 {
        if desc.layers[0].format != DrmFourcc::Nv12 as u32 {
            return Err(anyhow!("single-layer descriptor is not NV12"));
        }
        vec![0]
    } else if desc.nb_layers == 2 {
        let first = DrmFourcc::try_from(desc.layers[0].format)
            .map_err(|_| anyhow!("unsupported first layer format"))?;
        let second = DrmFourcc::try_from(desc.layers[1].format)
            .map_err(|_| anyhow!("unsupported second layer format"))?;
        match (first, second) {
            (DrmFourcc::R8, DrmFourcc::Gr88) => vec![0, 1],
            (DrmFourcc::Gr88, DrmFourcc::R8) => vec![1, 0],
            #[cfg(feature = "rpi")]
            (DrmFourcc::R8, DrmFourcc::R8) => vec![0, 1],
            _ => return Err(anyhow!("unsupported NV12 layer ordering")),
        }
    } else {
        return Err(anyhow!(
            "NV12 export requires one or two layers, got {}",
            desc.nb_layers
        ));
    };

    let mut planes = Vec::new();
    for layer_index in layer_order {
        let layer = &desc.layers[layer_index];
        if !(1..=4).contains(&layer.nb_planes) {
            return Err(anyhow!(
                "invalid AVDRM plane count in layer {layer_index}: {}",
                layer.nb_planes
            ));
        }
        for plane_index in 0..layer.nb_planes as usize {
            let plane = layer.planes[plane_index];
            planes.push(Plane {
                object_index: u32::try_from(plane.object_index)
                    .map_err(|_| anyhow!("object index overflow"))?,
                pitch: u32::try_from(plane.pitch).map_err(|_| anyhow!("pitch overflow"))?,
                offset: u64::try_from(plane.offset).map_err(|_| anyhow!("offset overflow"))?,
            });
        }
    }
    if planes.len() != 2 {
        return Err(anyhow!(
            "NV12 export requires two planes, got {}",
            planes.len()
        ));
    }

    let descriptor = Descriptor {
        version: 1,
        objects,
        layers: vec![Layer {
            fourcc: DrmFourcc::Nv12 as u32,
            planes,
        }],
    };
    descriptor.validate().map_err(|error| anyhow!(error))?;

    let modifier = stream_modifier.ok_or_else(|| anyhow!("missing DMA-BUF modifier"))?;
    let keepalive = ResourceArc::new(Keepalive::new(drm_frame, object_fds));

    Ok(DmabufFrame {
        width: frame.width(),
        height: frame.height(),
        modifier,
        descriptor,
        keepalive,
    })
}

fn frame_needs_transfer(frame: &Video) -> bool {
    unsafe { !(*frame.as_ptr()).hw_frames_ctx.is_null() }
}

fn ensure_scaler(inner: &mut DecoderInner, source: &Video, target: Pixel) -> Result<()> {
    let spec = ScalerSpec {
        src_format: source.format(),
        src_width: source.width(),
        src_height: source.height(),
        dst_format: target,
        dst_width: source.width(),
        dst_height: source.height(),
    };

    if inner.scaler_spec != Some(spec) {
        inner.scaler = Some(
            Scaler::get(
                spec.src_format,
                spec.src_width,
                spec.src_height,
                spec.dst_format,
                spec.dst_width,
                spec.dst_height,
                Flags::FAST_BILINEAR,
            )
            .context("create scaler")?,
        );
        inner.scaler_spec = Some(spec);
    }

    Ok(())
}

fn copy_frame_to_binary<'a>(env: Env<'a>, frame: &Video) -> Result<Term<'a>> {
    let format: sys::AVPixelFormat = frame.format().into();
    let width = frame.width() as i32;
    let height = frame.height() as i32;
    let buf_size = unsafe { sys::av_image_get_buffer_size(format, width, height, 1) };
    if buf_size < 0 {
        return Err(anyhow!("buffer_size"));
    }

    let mut out = OwnedBinary::new(buf_size as usize).ok_or_else(|| anyhow!("alloc_binary"))?;
    let copy_res = unsafe {
        sys::av_image_copy_to_buffer(
            out.as_mut_slice().as_mut_ptr(),
            buf_size,
            (*frame.as_ptr()).data.as_ptr() as *const *const u8,
            (*frame.as_ptr()).linesize.as_ptr(),
            format,
            width,
            height,
            1,
        )
    };
    if copy_res < 0 {
        return Err(anyhow!("copy"));
    }

    Ok(out.release(env).encode(env))
}

fn export_raw_frame<'a>(
    env: Env<'a>,
    inner: &mut DecoderInner,
    decoded: &Video,
    mapped: &mut Video,
    converted: &mut Video,
) -> Result<Term<'a>> {
    let target = inner
        .raw_target
        .ok_or_else(|| anyhow!("missing raw output format"))?;
    let mut source = decoded;

    if frame_needs_transfer(decoded) {
        let res =
            unsafe { sys::av_hwframe_transfer_data(mapped.as_mut_ptr(), decoded.as_ptr(), 0) };
        if res < 0 {
            return Err(anyhow!("av_hwframe_transfer_data failed: {res}"));
        }
        mapped.set_pts(decoded.pts());
        source = mapped;
    }

    if source.format() != target {
        ensure_scaler(inner, source, target)?;
        let scaler = inner
            .scaler
            .as_mut()
            .ok_or_else(|| anyhow!("missing scaler"))?;
        scaler.run(source, converted).context("scale frame")?;
        converted.set_pts(source.pts());
        source = converted;
    }

    copy_frame_to_binary(env, source)
}

fn cleanup_frames(decoded: &mut Video, mapped: &mut Video, converted: &mut Video) {
    unsafe {
        sys::av_frame_unref(decoded.as_mut_ptr());
        sys::av_frame_unref(mapped.as_mut_ptr());
        sys::av_frame_unref(converted.as_mut_ptr());
    }
}

type DecodeResult<'a> = (Vec<i64>, Vec<Term<'a>>);
type DecodeResultWithOk<'a> = (Atom, Vec<i64>, Vec<Term<'a>>);

fn decode_frames<'a>(env: Env<'a>, inner: &mut DecoderInner) -> Result<DecodeResult<'a>> {
    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    let mut mapped = Video::empty();
    let mut converted = Video::empty();

    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                inner.width = decoded.width();
                inner.height = decoded.height();
                let pts = decoded.pts().unwrap_or(NO_PTS);

                let emitted = match inner.output_mode {
                    OutputMode::Dmabuf => Some(export_dmabuf(&decoded)?.encode(env)),
                    OutputMode::Raw => Some(export_raw_frame(
                        env,
                        inner,
                        &decoded,
                        &mut mapped,
                        &mut converted,
                    )?),
                };

                if let Some(frame) = emitted {
                    pts_list.push(pts);
                    frames.push(frame);
                }

                cleanup_frames(&mut decoded, &mut mapped, &mut converted);
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(err) => return Err(anyhow!("decode failed: {err}")),
        }
    }

    Ok((pts_list, frames))
}

static RELEASE_DISPATCHER_QUARANTINED: AtomicBool = AtomicBool::new(false);
static RELEASE_DISPATCHER_ADMISSION: Mutex<()> = Mutex::new(());

fn lifecycle_error(message: impl Into<String>) -> Error {
    Error::Term(Box::new((atoms::error(), message.into())))
}

#[rustler::nif(schedule = "DirtyIo")]
fn start_release_dispatcher() -> NifResult<(Atom, ResourceArc<ReleaseDispatcher>)> {
    let _admission = RELEASE_DISPATCHER_ADMISSION
        .lock()
        .map_err(|_| lifecycle_error("release dispatcher admission lock poisoned"))?;

    if RELEASE_DISPATCHER_QUARANTINED.load(Ordering::Acquire) {
        return Err(lifecycle_error(
            "release dispatcher admission is disabled until cold VM restart",
        ));
    }

    ReleaseDispatcher::start("membrane-video-transcode-release")
        .map(|dispatcher| (atoms::ok(), dispatcher))
        .map_err(|error| lifecycle_error(format!("could not start release dispatcher: {error}")))
}

#[rustler::nif(schedule = "DirtyIo")]
fn quarantine_release_dispatchers() -> NifResult<bool> {
    let _admission = RELEASE_DISPATCHER_ADMISSION
        .lock()
        .map_err(|_| lifecycle_error("release dispatcher admission lock poisoned"))?;
    Ok(!RELEASE_DISPATCHER_QUARANTINED.swap(true, Ordering::SeqCst))
}

#[rustler::nif]
fn release_dispatcher_quarantined() -> bool {
    RELEASE_DISPATCHER_QUARANTINED.load(Ordering::Acquire)
}

#[rustler::nif(schedule = "DirtyIo")]
fn close_release_dispatcher(
    dispatcher: ResourceArc<ReleaseDispatcher>,
    timeout_ms: u64,
) -> NifResult<(Atom, bool)> {
    dispatcher
        .close_and_join(Duration::from_millis(timeout_ms))
        .map(|()| (atoms::ok(), true))
        .map_err(|error| lifecycle_error(format!("could not close release dispatcher: {error}")))
}

#[rustler::nif]
fn new_abandonment_guard_resource<'a>(
    dispatcher: ResourceArc<ReleaseDispatcher>,
    owner: LocalPid,
    token: Term<'a>,
    holder: Reference<'a>,
) -> NifResult<(Atom, ResourceArc<AbandonmentGuard>)> {
    make_abandonment_guard(dispatcher, owner, token, holder)
        .map(|guard| (atoms::ok(), guard))
        .map_err(|error| lifecycle_error(format!("could not create abandonment guard: {error}")))
}

#[rustler::nif]
fn abandonment_guard_resource(term: Term<'_>) -> bool {
    is_abandonment_guard_resource(term)
}

#[rustler::nif]
fn release_frame(keepalive: ResourceArc<Keepalive>) -> NifResult<Atom> {
    keepalive.release();
    Ok(atoms::ok())
}

#[rustler::nif]
fn create(
    output: Atom,
    output_format: Option<Atom>,
    hw_device: String,
    decoder: Atom,
) -> NifResult<ResourceArc<Decoder>> {
    let path = if hw_device.is_empty() {
        None
    } else {
        Some(hw_device)
    };
    let output_mode = if output == atoms::dmabuf() {
        OutputMode::Dmabuf
    } else if output == atoms::raw() {
        OutputMode::Raw
    } else {
        return Err(Error::BadArg);
    };
    let raw_target = match output_mode {
        OutputMode::Dmabuf => None,
        OutputMode::Raw => {
            let atom = output_format.ok_or(Error::BadArg)?;
            let pixel = pixel_from_atom(atom).ok_or(Error::Atom("bad_pixel_format"))?;
            Some((pixel, atom))
        }
    };
    let backend = if decoder == atoms::auto() {
        Backend::Auto
    } else if decoder == atoms::vaapi() {
        Backend::Vaapi
    } else if decoder == atoms::v4l2request() {
        Backend::V4l2Request
    } else if decoder == atoms::v4l2m2m() {
        Backend::V4l2M2M
    } else if decoder == atoms::software() {
        Backend::Software
    } else {
        return Err(Error::BadArg);
    };

    init_decoder(output_mode, raw_target, path, backend)
        .map(ResourceArc::new)
        .map_err(|err| Error::Term(Box::new((atoms::create_failed(), format!("{err:?}")))))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn decode<'a>(
    env: Env<'a>,
    state: ResourceArc<Decoder>,
    data: Binary<'a>,
    pts: i64,
    dts: i64,
) -> NifResult<DecodeResultWithOk<'a>> {
    let mut packet = Packet::copy(data.as_slice());
    packet.set_pts((pts != NO_PTS).then_some(pts));
    packet.set_dts((dts != NO_PTS).then_some(dts));

    let decoder = &*state;
    let mut inner = decoder.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner
        .decoder
        .send_packet(&packet)
        .map_err(|_| Error::Atom("send_packet"))?;
    let (pts_list, frames) = decode_frames(env, &mut inner).map_err(|_| Error::Atom("decode"))?;

    Ok((atoms::ok(), pts_list, frames))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn flush<'a>(env: Env<'a>, state: ResourceArc<Decoder>) -> NifResult<DecodeResultWithOk<'a>> {
    let decoder = &*state;
    let mut inner = decoder.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner
        .decoder
        .send_eof()
        .map_err(|_| Error::Atom("send_eof"))?;
    let (pts_list, frames) = decode_frames(env, &mut inner).map_err(|_| Error::Atom("decode"))?;

    Ok((atoms::ok(), pts_list, frames))
}

#[rustler::nif]
fn close(state: ResourceArc<Decoder>) -> NifResult<Atom> {
    let decoder = &*state;
    let mut inner = decoder.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner.decoder.flush();
    inner.scaler.take();
    inner.scaler_spec.take();
    Ok(atoms::ok())
}

#[rustler::nif]
fn get_metadata(state: ResourceArc<Decoder>) -> NifResult<(Atom, u32, u32, Option<Atom>)> {
    let decoder = &*state;
    let inner = decoder.inner.lock().map_err(|_| Error::Atom("lock"))?;
    Ok((
        atoms::ok(),
        inner.width,
        inner.height,
        inner.raw_target_atom,
    ))
}

#[allow(non_snake_case)]
mod atoms {
    rustler::atoms! {
        ok,
        error,
        create_failed,
        auto,
        vaapi,
        v4l2request,
        v4l2m2m,
        software,
        dmabuf,
        raw,
        I420,
        I422,
        I444,
        RGB,
        BGRA,
        RGBA,
        NV12,
        NV21,
        YV12,
        AYUV,
        YUY2
    }
}

fn pixel_from_atom(atom: Atom) -> Option<Pixel> {
    if atom == atoms::I420() {
        Some(Pixel::YUV420P)
    } else if atom == atoms::I422() {
        Some(Pixel::YUV422P)
    } else if atom == atoms::I444() {
        Some(Pixel::YUV444P)
    } else if atom == atoms::RGB() {
        Some(Pixel::RGB24)
    } else if atom == atoms::BGRA() {
        Some(Pixel::BGRA)
    } else if atom == atoms::RGBA() {
        Some(Pixel::RGBA)
    } else if atom == atoms::NV12() {
        Some(Pixel::NV12)
    } else if atom == atoms::NV21() {
        Some(Pixel::NV21)
    } else if atom == atoms::YV12() {
        Some(Pixel::YUV420P)
    } else if atom == atoms::AYUV() {
        Some(Pixel::AYUV64LE)
    } else if atom == atoms::YUY2() {
        Some(Pixel::YUYV422)
    } else {
        None
    }
}

rustler::init!("Elixir.Membrane.H265.Decoder.Native");
