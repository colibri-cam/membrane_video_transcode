use std::ffi::CString;
use std::path::Path;
use std::ptr;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use drm_fourcc::DrmFourcc;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::sys;
use ffmpeg_next::{
    codec,
    util::{error::EAGAIN, frame::Video},
};
use rustler::{Atom, Binary, Encoder, Env, NifResult, ResourceArc, Term};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

rustler::atoms! {
    ok
}

const NO_PTS: i64 = i64::MIN;

#[derive(Debug)]
struct Fd(OwnedFd);

impl AsFd for Fd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.PrimePlane"]
struct PrimePlane {
    fd: Fd,
    pitch: u32,
    offset: u32,
    modifier: Option<u64>,
}

#[derive(Debug)]
struct Fourcc(DrmFourcc);

impl Encoder for Fourcc {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        (self.0 as u32).encode(env)
    }
}

impl<'a> rustler::Decoder<'a> for Fourcc {
    fn decode(term: Term<'a>) -> NifResult<Self> {
        let val: u32 = term.decode()?;
        Ok(Fourcc(
            DrmFourcc::try_from(val).map_err(|_| rustler::Error::BadArg)?,
        ))
    }
}

#[derive(rustler::NifStruct)]
#[module = "Membrane.PrimeDesc"]
struct PrimeDesc {
    width: u32,
    height: u32,
    format: Fourcc,
    planes: Vec<PrimePlane>,
}

impl Encoder for Fd {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        let dup_fd = unsafe { libc::dup(self.0.as_raw_fd()) };
        dup_fd.encode(env)
    }
}

impl<'a> rustler::Decoder<'a> for Fd {
    fn decode(term: Term<'a>) -> NifResult<Self> {
        let fd: i32 = term.decode()?;
        if fd < 0 {
            Err(rustler::Error::BadArg)
        } else {
            Ok(Fd(unsafe { OwnedFd::from_raw_fd(fd) }))
        }
    }
}

struct DecoderInner {
    decoder: codec::decoder::Video,
    width: u32,
    height: u32,
}

struct Decoder {
    inner: Mutex<DecoderInner>,
}

#[allow(dead_code)]
struct Keepalive {
    resource: Video,
}

unsafe impl Send for Keepalive {}
unsafe impl Sync for Keepalive {}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.decoder.flush();
        }
    }
}

fn init_decoder(hw_device: Option<String>) -> Result<Decoder> {
    ffmpeg::init().context("ffmpeg init failed")?;
    let use_v4l2 = ["/dev/video11", "/dev/video10", "/dev/video0"]
        .iter()
        .any(|p| Path::new(p).exists());
    let hevc = if use_v4l2 {
        codec::decoder::find_by_name("hevc_v4l2m2m")
            .or_else(|| codec::decoder::find(codec::Id::HEVC))
            .ok_or_else(|| anyhow!("no hevc codec"))?
    } else {
        codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?
    };
    let mut decoder = codec::decoder::new();

    unsafe {
        let mut hw_device_ctx = ptr::null_mut();
        if !use_v4l2 {
            let path_str = hw_device.unwrap_or_else(|| "/dev/dri/renderD128".to_string());
            if Path::new(&path_str).exists() {
                if let Ok(path) = CString::new(path_str) {
                    if sys::av_hwdevice_ctx_create(
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
        }
    }
    let opened = decoder.open_as(hevc).context("open codec")?;
    let video = opened.video().context("video decoder")?;
    Ok(Decoder {
        inner: Mutex::new(DecoderInner {
            decoder: video,
            width: 0,
            height: 0,
        }),
    })
}

#[allow(non_local_definitions)]
fn load(env: rustler::Env, _info: rustler::Term) -> bool {
    assert!(
        rustler::resource!(Keepalive, env),
        "regisetr Keepalive resource failed"
    );
    rustler::resource!(Decoder, env)
}

#[rustler::nif]
fn create(hw_device: String) -> NifResult<ResourceArc<Decoder>> {
    let path = if hw_device.is_empty() {
        None
    } else {
        Some(hw_device)
    };
    init_decoder(path)
        .map(ResourceArc::new)
        .map_err(|_| rustler::Error::Atom("create_failed"))
}

fn export_drm_prime(frame: &Video) -> Result<PrimeDesc> {
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
            sys::av_frame_unref(drm.as_mut_ptr());
            return Err(anyhow!("av_buffer_ref failed"));
        }
        (*drm.as_mut_ptr()).hw_frames_ctx = ctx_ref;
    }
    const AV_HWFRAME_MAP_DRM_PRIME: i32 = 0x0002_0000;
    let flags = (sys::AV_HWFRAME_MAP_READ as i32) | AV_HWFRAME_MAP_DRM_PRIME;
    let res = unsafe { sys::av_hwframe_map(drm.as_mut_ptr(), frame.as_ptr(), flags) };
    if res < 0 {
        unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
        return Err(anyhow!("av_hwframe_map failed: {res}"));
    }
    let desc_ptr = unsafe { (*drm.as_ptr()).data[0] as *const sys::AVDRMFrameDescriptor };
    if desc_ptr.is_null() {
        unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
        return Err(anyhow!("no drm descriptor"));
    }
    let desc = unsafe { &*desc_ptr };
    if desc.nb_objects == 0 || desc.nb_layers == 0 {
        unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
        return Err(anyhow!("empty drm descriptor"));
    }

    let fourcc = DrmFourcc::try_from(desc.layers[0].format as u32)
        .map_err(|_| anyhow!("unsupported fourcc {:#x}", desc.layers[0].format))?;

    // Pre-dup all object fds once
    let mut obj_fds: Vec<Option<OwnedFd>> = Vec::with_capacity(desc.nb_objects as usize);
    for j in 0..desc.nb_objects as usize {
        let ofd = desc.objects[j].fd;
        if ofd >= 0 {
            let dupfd = unsafe { libc::dup(ofd) };
            if dupfd < 0 {
                unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                return Err(anyhow!("dup failed for object {j}"));
            }
            obj_fds.push(Some(unsafe { OwnedFd::from_raw_fd(dupfd) }));
        } else {
            // Leave as None for now; we may patch it up below if it’s a bogus extra object.
            obj_fds.push(None);
        }
    }

    // Heuristic: if we have at least one valid object (usually 0),
    // allow planes whose object points to an invalid fd to reuse object 0.
    let fallback_obj0 = obj_fds
        .get(0)
        .and_then(|x| x.as_ref())
        .map(|fd| fd.as_raw_fd());
    let mut planes = Vec::new();

    for l in 0..desc.nb_layers as usize {
        let layer = &desc.layers[l];
        for i in 0..layer.nb_planes as usize {
            let p = layer.planes[i];
            let obj = p.object_index as isize;
            if obj < 0 || (obj as usize) >= desc.nb_objects as usize {
                unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                return Err(anyhow!("invalid object index {} for plane {}", obj, i));
            }
            let obj_idx = obj as usize;

            // Ensure we have a usable fd for this plane’s object
            let fd_owned = if let Some(fd) = &obj_fds[obj_idx] {
                // Okay, valid object fd
                let dupfd = unsafe { libc::dup(fd.as_raw_fd()) };
                if dupfd < 0 {
                    unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                    return Err(anyhow!("dup failed (plane {i}, object {obj_idx})"));
                }
                unsafe { OwnedFd::from_raw_fd(dupfd) }
            } else if let Some(fd0) = fallback_obj0 {
                // Workaround for buggy “second object with fd = -1” cases:
                // reuse object 0’s fd; planes differ by offset/pitch anyway.
                let dupfd = unsafe { libc::dup(fd0) };
                if dupfd < 0 {
                    unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                    return Err(anyhow!("dup failed (plane {i}, fallback obj0)"));
                }
                unsafe { OwnedFd::from_raw_fd(dupfd) }
            } else {
                unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
                return Err(anyhow!(
                    "no valid dma-buf fd for plane {i} (object {obj_idx})"
                ));
            };

            let obj_desc = desc.objects[obj_idx];
            planes.push(PrimePlane {
                fd: Fd(fd_owned),
                pitch: p.pitch as u32,
                offset: p.offset as u32,
                modifier: Some(obj_desc.format_modifier),
            });
        }
    }

    unsafe { sys::av_frame_unref(drm.as_mut_ptr()) };
    Ok(PrimeDesc {
        width: frame.width(),
        height: frame.height(),
        format: Fourcc(fourcc),
        planes,
    })
}

type DecodeResult<'a> = (Vec<i64>, Vec<Term<'a>>, Vec<ResourceArc<Keepalive>>);

fn decode_frames<'a>(env: Env<'a>, inner: &mut DecoderInner) -> Result<DecodeResult<'a>> {
    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    let mut keepalives = Vec::new();

    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                inner.width = decoded.width();
                inner.height = decoded.height();
                let desc = match export_drm_prime(&decoded) {
                    Ok(desc) => desc,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        unsafe { sys::av_frame_unref(decoded.as_mut_ptr()) };
                        continue;
                    }
                };
                let mut res = Video::empty();
                unsafe {
                    // keepalive now references the same underlying surface
                    sys::av_frame_ref(res.as_mut_ptr(), decoded.as_ptr());
                }
                let keepalive = ResourceArc::new(Keepalive { resource: res });
                keepalives.push(keepalive);
                pts_list.push(decoded.pts().unwrap_or(NO_PTS));
                frames.push(desc.encode(env));
                unsafe { sys::av_frame_unref(decoded.as_mut_ptr()) };
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(e) => {
                eprintln!("Error: {e}");
                return Err(anyhow!("decode failed"));
            }
        }
    }
    Ok((pts_list, frames, keepalives))
}

type DecodeResultWithOk<'a> = (Atom, Vec<i64>, Vec<Term<'a>>, Vec<ResourceArc<Keepalive>>);

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

    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner
        .decoder
        .send_packet(&packet)
        .map_err(|_| rustler::Error::Atom("send_packet"))?;
    let (pts_list, frames, keepalives) = decode_frames(env, &mut inner).map_err(|e| {
        eprintln!("Error: {e}");
        rustler::Error::Atom("decode")
    })?;
    Ok((ok(), pts_list, frames, keepalives))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn flush<'a>(env: Env<'a>, state: ResourceArc<Decoder>) -> NifResult<DecodeResultWithOk<'a>> {
    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner
        .decoder
        .send_eof()
        .map_err(|_| rustler::Error::Atom("send_eof"))?;
    let (pts_list, frames, keepalives) = decode_frames(env, &mut inner).map_err(|e| {
        eprintln!("Error: {e}");
        rustler::Error::Atom("decode")
    })?;
    Ok((ok(), pts_list, frames, keepalives))
}

#[rustler::nif]
fn close(state: ResourceArc<Decoder>) -> NifResult<Atom> {
    let mut inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    inner.decoder.flush();
    Ok(ok())
}

#[rustler::nif]
fn get_metadata(state: ResourceArc<Decoder>) -> NifResult<(Atom, u32, u32)> {
    let inner = state
        .inner
        .lock()
        .map_err(|_| rustler::Error::Atom("lock"))?;
    Ok((ok(), inner.width, inner.height))
}

rustler::init!("Elixir.Membrane.H265.PrimeDecoder.Native", load = load);
