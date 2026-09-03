defmodule Membrane.H264.Decoder do
  @moduledoc """
  Decodes Annex B H.264 access units into leased NV12 DMA-BUF `%VideoInterop.Frame{}` payloads or
  copied raw frames.

  DMA-BUF output carries a concrete sync-file acquire fence. Its native storage and fence lifetime
  are owned by a bounded `VideoInterop.LeaseOwner` with authenticated abandonment guards.
  Presentation remains outside this package; connect canonical output to
  `Membrane.VideoInterop.Sink` when frames need to be rendered.
  """

  use Membrane.Filter

  alias Membrane.H264
  alias Membrane.RawVideo
  alias Membrane.VideoTranscode.Decoder.Core
  alias VideoInterop.Format
  alias VideoInterop.DMABuf.Format, as: StorageFormat

  @typedoc "Supported copied output pixel formats."
  @type pixel_format ::
          :I420
          | :I422
          | :I444
          | :RGB
          | :BGRA
          | :RGBA
          | :NV12
          | :NV21
          | :YV12
          | :AYUV
          | :YUY2

  @formats [:I420, :I422, :I444, :RGB, :BGRA, :RGBA, :NV12, :NV21, :YV12, :AYUV, :YUY2]
  @nv12_fourcc :binary.decode_unsigned("NV12", :little)

  @typedoc "Decoder backend to use."
  @type decoder_backend :: :auto | :vaapi | :v4l2request | :v4l2m2m | :software

  @typedoc "Decoder output mode."
  @type output_mode :: :dmabuf | :raw

  def_options(
    output: [
      spec: output_mode(),
      default: :dmabuf,
      description: "Whether to emit canonical DMA-BUF or copied raw frames"
    ],
    output_format: [
      spec: pixel_format(),
      default: :NV12,
      description: "Pixel format to use for copied raw output"
    ],
    hw_device: [
      spec: String.t(),
      default: "/dev/dri/renderD128",
      description: "Hardware decode device"
    ],
    decoder: [
      spec: decoder_backend(),
      default: :auto,
      description: "Decoder backend to use"
    ],
    max_in_flight: [
      spec: pos_integer(),
      default: 16,
      description: "Maximum number of canonical frame leases"
    ]
  )

  def_input_pad(:input,
    flow_control: :auto,
    accepted_format: %H264{alignment: :au, stream_structure: :annexb}
  )

  def_output_pad(:output,
    flow_control: :auto,
    accepted_format:
      any_of(
        %Format{storage: %StorageFormat{fourcc: @nv12_fourcc}},
        %RawVideo{pixel_format: format, aligned: true} when format in @formats
      )
  )

  @impl true
  def handle_init(_ctx, opts), do: Core.init(opts, :h264)

  @impl true
  def handle_setup(_ctx, state), do: Core.setup(state)

  @impl true
  def handle_buffer(:input, buffer, ctx, state),
    do: Core.handle_buffer(:input, buffer, ctx, state)

  @impl true
  def handle_stream_format(:input, format, _ctx, state),
    do: Core.handle_stream_format(format, state)

  @impl true
  def handle_end_of_stream(:input, _ctx, state), do: Core.handle_end_of_stream(state)

  @impl true
  def handle_info(message, _ctx, state), do: Core.handle_info(message, state)

  @impl true
  def handle_terminate_request(_ctx, state), do: Core.terminate(state)
end
