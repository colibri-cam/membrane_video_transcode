defmodule Membrane.H265.PrimeDecoder do
  @moduledoc """
  Variant of `Membrane.H265Decoder` that returns DRM Prime descriptors instead of
  raw frame payloads. Each decoded frame is sent downstream as an empty buffer
  with the descriptor attached under the `:drm_prime` metadata key.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.H265
  alias Membrane.DRM.Prime

  @typedoc """
  Supported output pixel formats.
  """
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

  def_options(
    output_format: [
      spec: pixel_format(),
      default: :NV12,
      description: "Pixel format to use for decoded frames"
    ]
  )

  def_input_pad(:input,
    flow_control: :auto,
    accepted_format: %H265{alignment: :au}
  )

  def_output_pad(:output,
    flow_control: :auto,
    accepted_format: %Prime{}
  )

  @impl true
  def handle_init(_ctx, opts) do
    state = %{decoder_ref: nil, stream_format_sent?: false, output_format: opts.output_format}
    {[], state}
  end

  @impl true
  def handle_setup(_ctx, %{output_format: fmt} = state) do
    decoder =
      case Native.create(fmt) do
        {:error, reason} -> raise "Error creating decoder #{inspect(reason)}"
        decoder -> decoder
      end

    {[], %{state | decoder_ref: decoder}}
  end

  @impl true
  def handle_buffer(:input, buffer, _ctx, %{decoder_ref: decoder} = state) do
    pts = buffer.pts || 0
    dts = buffer.dts || 0

    case Native.decode(decoder, buffer.payload, pts, dts) do
      {:ok, pts_list, descs} ->
        {actions, state} = maybe_send_stream_format(state)
        bufs = wrap_descriptors(pts_list, descs)
        {actions ++ bufs, state}

      {:error, reason} ->
        raise "Failed to decode frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_stream_format(:input, _format, _ctx, state) do
    {[], %{state | stream_format_sent?: false}}
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, %{decoder_ref: decoder} = state) do
    with {:ok, pts_list, descs} <- Native.flush(decoder),
         bufs <- wrap_descriptors(pts_list, descs) do
      new_state = %{state | decoder_ref: nil}
      {bufs ++ [end_of_stream: :output], new_state}
    else
      {:error, reason} ->
        raise "Native decoder failed to flush: #{inspect(reason)}"
    end
  end

  defp wrap_descriptors([], []), do: []

  defp wrap_descriptors(pts_list, descs) do
    Enum.zip(pts_list, descs)
    |> Enum.map(fn {p, desc} ->
      %Buffer{pts: p, payload: <<>>, metadata: %{drm_prime: desc}}
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp maybe_send_stream_format(%{stream_format_sent?: true} = state), do: {[], state}

  defp maybe_send_stream_format(%{decoder_ref: decoder} = state) do
    {:ok, width, height, pix_fmt} = Native.get_metadata(decoder)

    sf = %Prime{
      pixel_format: pix_fmt,
      width: width,
      height: height,
      fd: -1,
      pitches: [],
      offsets: []
    }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end
end
