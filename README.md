# Membrane Video Transcode

Hardware-aware H.264 and H.265 decoding elements for the
[Membrane Framework](https://membrane.stream).

This package owns codec work only. It does not open displays, perform DRM/KMS modesetting, or
present frames. Use
[`membrane_video_interop`](https://github.com/emerge-elixir/membrane_video_interop) as a separate
transport dependency when decoded frames need to reach a consumer.

## Current elements

- `Membrane.H264.Decoder` accepts Annex B H.264 access units.
- `Membrane.H265.Decoder` accepts H.265 access units.
- `output: :dmabuf` emits canonical leased `%VideoInterop.Frame{}` values directly as Membrane
  buffer payloads. The current canonical format is NV12 DMA-BUF with a uniform modifier and a
  concrete `%VideoInterop.SyncFile{}` acquire fence.
- `output: :raw` emits copied buffers described by `%Membrane.RawVideo{}`.
- VAAPI, V4L2 Request, V4L2 M2M, and software FFmpeg backends are selectable.
- Runtime instrumentation remains available through `Membrane.Instrumentation`.

Encoding and composed decode/encode transcoding elements belong in this package as they are added.
Presentation sinks intentionally do not.

## Installation

```elixir
def deps do
  [
    {:membrane_video_transcode,
     git: "https://github.com/colibri-cam/membrane_video_transcode.git"},
    {:video_interop, "~> 0.1.0"},
    {:membrane_video_interop, path: "../membrane_video_interop"}
  ]
end
```

Until `membrane_video_interop` is published, applications can use its sibling checkout as shown
above. `membrane_video_transcode` itself depends only on the published `video_interop` package; it
does not depend on a transport or renderer.

## Canonical decoded output

Pace file input before hardware decode so leased surfaces are not decoded far ahead of playback:

```elixir
child(:file, %Membrane.File.Source{
  location: "clip.h264",
  content_format: Membrane.H264
})
|> child(:parser, %Membrane.H264.Parser{
  output_alignment: :au,
  output_stream_structure: :annexb,
  generate_best_effort_timestamps: %{framerate: {24, 1}}
})
|> child(:realtimer, Membrane.Realtimer)
|> child(:decoder, %Membrane.H264.Decoder{
  output: :dmabuf,
  decoder: :vaapi,
  hw_device: "/dev/dri/renderD128",
  max_in_flight: 4
})
|> child(:sink, %Membrane.VideoInterop.Sink{
  submit: {MyConsumer, :submit, [consumer]},
  target: :preview
})
```

The sink invokes `MyConsumer.submit(frame, :preview, consumer)`. Every normal callback return must
consume the frame, either by transferring it to another VideoInterop consumer or by calling
`VideoInterop.release/1`.

The decoder exports the DMA-BUF reservation fence with
`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` after each decoded frame is received. Native frame storage and the
acquire fence remain alive until the frame's bounded lease is released. Descriptor file descriptors
are local to one OS process.

## Backends

Use an explicit backend when deployment capabilities are known:

- `:vaapi` uses the configured render node and is the recommended Linux desktop path.
- `:v4l2request` and `:v4l2m2m` require matching FFmpeg codecs and target hardware.
- `:software` is intended for `output: :raw`; it cannot produce DMA-BUF storage.
- `:auto` probes hardware codecs before falling back to the software decoder.

## Cross-compiling for Nerves

When `NERVES_SDK_SYSROOT` is set, `mix.exs` maps the Nerves C compiler prefix to the matching Rust
target and passes the target linker and FFmpeg paths to Rustler. The current mapping includes the
standard ARMv6, ARMv7, AArch64, and x86_64 Nerves targets.

Raspberry Pi deployments must use the `ffmpeg-rpi` libraries and headers supplied by
`colibri-cam/nerves_system_gs`, or an equivalent patched FFmpeg build with V4L2 Request, DRM, and
SAND support. Nerves cross-compilation enables the crate's `rpi` feature, which forwards to
`ffmpeg-next/rpi`; that feature exposes the patched pixel formats but does not supply FFmpeg itself.
Stock upstream FFmpeg is not a substitute for this target contract.
