defmodule Membrane.H265.Decoder do
  @moduledoc """
  Membrane filter that decodes H265 video using a Rustler NIF with VAAPI acceleration.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.H265
  alias Membrane.H265.Common
  alias Membrane.RawVideo

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
    accepted_format: %RawVideo{pixel_format: format, aligned: true} when format in @formats
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
  def handle_buffer(:input, buffer, ctx, %{decoder_ref: decoder} = state) do
    dts = Common.to_h265_time_base_truncated(buffer.dts)
    pts = Common.to_h265_time_base_truncated(buffer.pts)

    case Native.decode(decoder, buffer.payload, pts, dts) do
      {:ok, pts_list, frames} ->
        in_stream_format = ctx.pads.input.stream_format
        {actions, state} = maybe_send_stream_format(state, in_stream_format)
        bufs = wrap_frames(pts_list, frames)
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
    with {:ok, pts_list, frames} <- Native.flush(decoder),
         bufs <- wrap_frames(pts_list, frames) do
      _ = Native.close(decoder)
      new_state = %{state | decoder_ref: nil}
      {bufs ++ [end_of_stream: :output], new_state}
    else
      {:error, reason} ->
        raise "Native decoder failed to flush: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_terminate_request(_ctx, %{decoder_ref: decoder} = state) do
    if decoder do
      _ = Native.close(decoder)
    end

    {[terminate: :normal], %{state | decoder_ref: nil}}
  end

  defp wrap_frames([], []), do: []

  defp wrap_frames(pts_list, frames) do
    Enum.zip(pts_list, frames)
    |> Enum.map(fn {p, payload} ->
      %Buffer{pts: Common.to_membrane_time_base_truncated(p), payload: payload}
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp maybe_send_stream_format(%{stream_format_sent?: true} = state, _in_sf), do: {[], state}

  defp maybe_send_stream_format(%{decoder_ref: decoder} = state, in_sf) do
    {:ok, width, height, pix_fmt} = Native.get_metadata(decoder)

    framerate =
      case in_sf do
        %H265{framerate: in_framerate} -> in_framerate
        _ -> {0, 1}
      end

    sf =
      %RawVideo{
        pixel_format: pix_fmt,
        width: width,
        height: height,
        framerate: framerate,
        aligned: true
      }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end
end
