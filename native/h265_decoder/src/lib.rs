use std::ffi::CString;
use std::ptr;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::codec::packet::Packet;
use ffmpeg_next::sys;
use ffmpeg_next::{
    codec, format,
    software::scaling::{context::Context as Scaler, flag::Flags},
    util::{error::EAGAIN, frame::Video},
};
use rustler::{Atom, Binary, Env, Error, NifResult, OwnedBinary, ResourceArc, Term};

const NO_PTS: i64 = i64::MIN;

struct DecoderInner {
    decoder: codec::decoder::Video,
    target: format::Pixel,
    target_atom: Atom,
    scaler: Option<Scaler>,
}

pub struct Decoder {
    inner: Mutex<DecoderInner>,
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.decoder.flush();
            inner.scaler.take();
        }
    }
}

fn init_decoder(target: format::Pixel, atom: Atom) -> Result<Decoder> {
    ffmpeg::init().context("ffmpeg init failed")?;
    let mut use_v4l2 = false;
    let hevc = if let Some(codec) = codec::decoder::find_by_name("hevc_v4l2request") {
        use_v4l2 = true;
        codec
    } else if let Some(codec) = codec::decoder::find_by_name("hevc_v4l2m2m") {
        use_v4l2 = true;
        codec

    } else {
        codec::decoder::find(codec::Id::HEVC).ok_or_else(|| anyhow!("no hevc codec"))?
    };
    let mut decoder = codec::decoder::new();
    unsafe {
        let mut hw_device_ctx = ptr::null_mut();
        if !use_v4l2 && std::path::Path::new("/dev/dri/renderD128").exists() {
            if let Ok(path) = CString::new("/dev/dri/renderD128") {
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
    let opened = decoder.open_as(hevc).context("open codec")?;
    let video = opened.video().context("video decoder")?;
    Ok(Decoder {
        inner: Mutex::new(DecoderInner {
            decoder: video,
            target,
            target_atom: atom,
            scaler: None,
        }),
    })
}

#[rustler::nif]
fn create(format: Atom) -> NifResult<ResourceArc<Decoder>> {
    let pix = pixel_from_atom(format).ok_or(Error::Atom("bad_pixel_format"))?;
    init_decoder(pix, format)
        .map(ResourceArc::new)
        .map_err(|_| Error::Atom("create_failed"))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn decode<'a>(
    env: Env<'a>,
    state: ResourceArc<Decoder>,
    data: Binary<'a>,
    pts: i64,
    dts: i64,
) -> NifResult<(Atom, Vec<i64>, Vec<Binary<'a>>)> {
    let mut packet = Packet::copy(data.as_slice());
    packet.set_pts((pts != NO_PTS).then_some(pts));
    packet.set_dts((dts != NO_PTS).then_some(dts));

    let mut inner = state.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner
        .decoder
        .send_packet(&packet)
        .map_err(|_| Error::Atom("send_packet"))?;

    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    let mut mapped = Video::empty();
    let mut converted = Video::empty();

    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                let mut src_ref: &Video = &decoded;
                if decoded.format() == format::Pixel::VAAPI {
                    let res = unsafe {
                        sys::av_hwframe_transfer_data(mapped.as_mut_ptr(), decoded.as_ptr(), 0)
                    };
                    if res < 0 {
                        return Err(Error::Atom("map_frame"));
                    }
                    mapped.set_pts(decoded.pts());
                    src_ref = &mapped;
                }

                if src_ref.format() != inner.target {
                    if inner.scaler.is_none() {
                        let scaler = Scaler::get(
                            src_ref.format(),
                            src_ref.width(),
                            src_ref.height(),
                            inner.target,
                            src_ref.width(),
                            src_ref.height(),
                            Flags::FAST_BILINEAR,
                        )
                        .map_err(|_| Error::Atom("scaler"))?;
                        inner.scaler = Some(scaler);
                    }
                    let scaler = inner.scaler.as_mut().unwrap();
                    scaler
                        .run(src_ref, &mut converted)
                        .map_err(|_| Error::Atom("scale"))?;
                    converted.set_pts(src_ref.pts());
                    src_ref = &converted;
                }

                let format: sys::AVPixelFormat = inner.target.into();
                let buf_size = unsafe {
                    sys::av_image_get_buffer_size(
                        format,
                        src_ref.width() as i32,
                        src_ref.height() as i32,
                        1,
                    )
                };
                if buf_size < 0 {
                    return Err(Error::Atom("buffer_size"));
                }
                let mut out =
                    OwnedBinary::new(buf_size as usize).ok_or(Error::Atom("alloc_binary"))?;
                let copy_res = unsafe {
                    sys::av_image_copy_to_buffer(
                        out.as_mut_slice().as_mut_ptr(),
                        buf_size,
                        (*src_ref.as_ptr()).data.as_ptr() as *const *const u8,
                        (*src_ref.as_ptr()).linesize.as_ptr(),
                        format,
                        src_ref.width() as i32,
                        src_ref.height() as i32,
                        1,
                    )
                };
                if copy_res < 0 {
                    return Err(Error::Atom("copy"));
                }
                pts_list.push(src_ref.pts().unwrap_or(NO_PTS));
                frames.push(out.release(env));
                unsafe {
                    sys::av_frame_unref(decoded.as_mut_ptr());
                    sys::av_frame_unref(mapped.as_mut_ptr());
                    sys::av_frame_unref(converted.as_mut_ptr());
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(_) => return Err(Error::Atom("decode")),
        }
    }

    Ok((atoms::ok(), pts_list, frames))
}

#[rustler::nif(schedule = "DirtyCpu")]
fn flush<'a>(
    env: Env<'a>,
    state: ResourceArc<Decoder>,
) -> NifResult<(Atom, Vec<i64>, Vec<Binary<'a>>)> {
    let mut inner = state.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner
        .decoder
        .send_eof()
        .map_err(|_| Error::Atom("send_eof"))?;

    let mut frames = Vec::new();
    let mut pts_list = Vec::new();
    let mut decoded = Video::empty();
    let mut mapped = Video::empty();
    let mut converted = Video::empty();

    loop {
        match inner.decoder.receive_frame(&mut decoded) {
            Ok(_) => {
                let mut src_ref: &Video = &decoded;
                if decoded.format() == format::Pixel::VAAPI {
                    let res = unsafe {
                        sys::av_hwframe_transfer_data(mapped.as_mut_ptr(), decoded.as_ptr(), 0)
                    };
                    if res < 0 {
                        return Err(Error::Atom("map_frame"));
                    }
                    mapped.set_pts(decoded.pts());
                    src_ref = &mapped;
                }

                if src_ref.format() != inner.target {
                    if inner.scaler.is_none() {
                        let scaler = Scaler::get(
                            src_ref.format(),
                            src_ref.width(),
                            src_ref.height(),
                            inner.target,
                            src_ref.width(),
                            src_ref.height(),
                            Flags::FAST_BILINEAR,
                        )
                        .map_err(|_| Error::Atom("scaler"))?;
                        inner.scaler = Some(scaler);
                    }
                    let scaler = inner.scaler.as_mut().unwrap();
                    scaler
                        .run(src_ref, &mut converted)
                        .map_err(|_| Error::Atom("scale"))?;
                    converted.set_pts(src_ref.pts());
                    src_ref = &converted;
                }

                let format: sys::AVPixelFormat = inner.target.into();
                let buf_size = unsafe {
                    sys::av_image_get_buffer_size(
                        format,
                        src_ref.width() as i32,
                        src_ref.height() as i32,
                        1,
                    )
                };
                if buf_size < 0 {
                    return Err(Error::Atom("buffer_size"));
                }
                let mut out =
                    OwnedBinary::new(buf_size as usize).ok_or(Error::Atom("alloc_binary"))?;
                let copy_res = unsafe {
                    sys::av_image_copy_to_buffer(
                        out.as_mut_slice().as_mut_ptr(),
                        buf_size,
                        (*src_ref.as_ptr()).data.as_ptr() as *const *const u8,
                        (*src_ref.as_ptr()).linesize.as_ptr(),
                        format,
                        src_ref.width() as i32,
                        src_ref.height() as i32,
                        1,
                    )
                };
                if copy_res < 0 {
                    return Err(Error::Atom("copy"));
                }
                pts_list.push(src_ref.pts().unwrap_or(NO_PTS));
                frames.push(out.release(env));
                unsafe {
                    sys::av_frame_unref(decoded.as_mut_ptr());
                    sys::av_frame_unref(mapped.as_mut_ptr());
                    sys::av_frame_unref(converted.as_mut_ptr());
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(ffmpeg::Error::Other { errno }) if errno == EAGAIN => break,
            Err(_) => return Err(Error::Atom("decode")),
        }
    }
    inner.decoder.flush();
    inner.scaler.take();

    Ok((atoms::ok(), pts_list, frames))
}

#[rustler::nif]
fn close(state: ResourceArc<Decoder>) -> NifResult<Atom> {
    let mut inner = state.inner.lock().map_err(|_| Error::Atom("lock"))?;
    inner.decoder.flush();
    inner.scaler.take();
    Ok(atoms::ok())
}

#[rustler::nif]
fn get_metadata(state: ResourceArc<Decoder>) -> NifResult<(Atom, u32, u32, Atom)> {
    let inner = state.inner.lock().map_err(|_| Error::Atom("lock"))?;
    let width = inner.decoder.width();
    let height = inner.decoder.height();
    Ok((atoms::ok(), width, height, inner.target_atom))
}

#[allow(non_snake_case)]
mod atoms {
    rustler::atoms! {
        ok,
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

fn pixel_from_atom(atom: Atom) -> Option<format::Pixel> {
    if atom == atoms::I420() {
        Some(format::Pixel::YUV420P)
    } else if atom == atoms::I422() {
        Some(format::Pixel::YUV422P)
    } else if atom == atoms::I444() {
        Some(format::Pixel::YUV444P)
    } else if atom == atoms::RGB() {
        Some(format::Pixel::RGB24)
    } else if atom == atoms::BGRA() {
        Some(format::Pixel::BGRA)
    } else if atom == atoms::RGBA() {
        Some(format::Pixel::RGBA)
    } else if atom == atoms::NV12() {
        Some(format::Pixel::NV12)
    } else if atom == atoms::NV21() {
        Some(format::Pixel::NV21)
    } else if atom == atoms::YV12() {
        Some(format::Pixel::YUV420P)
    } else if atom == atoms::AYUV() {
        Some(format::Pixel::AYUV64LE)
    } else if atom == atoms::YUY2() {
        Some(format::Pixel::YUYV422)
    } else {
        None
    }
}

#[allow(non_local_definitions)]
fn on_load(env: Env, _info: Term) -> bool {
    let _ = rustler::resource!(Decoder, env);
    true
}

rustler::init!("Elixir.Membrane.H265.Decoder.Native", load = on_load);
