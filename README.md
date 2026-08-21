# Membrane Video Transcode

Hardware-aware video decoding, encoding, and transcoding elements for the
[Membrane Framework](https://membrane.stream).

This package owns codec work only. It does not open displays, perform DRM/KMS modesetting, or
present frames. Connect canonical output to
[`membrane_video_interop`](https://github.com/emerge-elixir/membrane_video_interop) when decoded
frames need to be rendered.

## Current elements

- `Membrane.H265.Decoder` decodes H.265 access units with VAAPI, V4L2 Request, V4L2 M2M, or
  software FFmpeg backends.
- `output: :dmabuf` emits empty Membrane buffers containing a canonical `%VideoInterop.Frame{}`
  under the reserved `:video_interop` metadata key.
- `output: :raw` emits copied `%Membrane.RawVideo{}` frames.
- Runtime instrumentation remains available through `Membrane.Instrumentation`.

Encoding and composed decode/encode transcoding elements belong in this package as they are added.
Presentation sinks and legacy DRM Prime descriptors intentionally do not.

## Installation

```elixir
def deps do
  [
    {:membrane_video_transcode,
     git: "https://github.com/colibri-cam/membrane_video_transcode.git"}
  ]
end
```

The package currently uses sibling path dependencies for the unpublished interoperability stack:

```text
/workspace/video_interop
/workspace/membrane_video_interop
/workspace/colibri/membrane_video_transcode
```

## Canonical decoded output

```elixir
child(:decoder, %Membrane.H265.Decoder{
  output: :dmabuf,
  decoder: :auto,
  max_in_flight: 16
})
|> child(:display, %Membrane.VideoInterop.Sink{consumer: consumer})
```

Canonical native resources use bounded leases, idempotent native retirement, and authenticated
abandonment guards. DMA-BUF descriptors and file descriptors remain local to one OS process.

## Cross-compiling for Nerves

When `NERVES_SDK_SYSROOT` is set, `mix.exs` maps the Nerves C compiler prefix to the matching Rust
target and passes the target linker and FFmpeg paths to Rustler. The current mapping includes the
standard ARMv6, ARMv7, AArch64, and x86_64 Nerves targets.

The target sysroot must provide FFmpeg with the hardware decoder support selected at runtime.
