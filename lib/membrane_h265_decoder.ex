defmodule Membrane.H265Decoder do
  @moduledoc """
  Membrane filter that decodes H265 video using a Rustler NIF with VAAPI acceleration.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.H265
  alias Membrane.RawVideo

  @typedoc """
  Supported output pixel formats.
  """
  @type pixel_format :: :nv12 | :yuv420p | :rgb24

  def_options(
    output_format: [
      spec: pixel_format(),
      default: :nv12,
      description: "Pixel format to use for decoded frames"
    ]
  )

  def_input_pad(:input,
    flow_control: :auto,
    accepted_format: %H265{alignment: :au}
  )

  def_output_pad(:output,
    flow_control: :auto,
    accepted_format:
      %RawVideo{pixel_format: format, aligned: true} when format in [:nv12, :yuv420p, :rgb24]
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
      {:ok, pts_list, frames} ->
        if frames == [] do
          {[], state}
        else
          {actions, state} = maybe_send_stream_format(state)

          bufs =
            Enum.zip(pts_list, frames)
            |> Enum.map(fn {p, payload} ->
              %Buffer{pts: p, payload: payload}
            end)

          {actions ++ [buffer: {:output, bufs}], state}
        end

      {:error, reason} ->
        raise "Failed to decode frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_stream_format(:input, _format, _ctx, state) do
    {[], %{state | stream_format_sent?: false}}
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, state) do
    {[end_of_stream: :output], state}
  end

  defp maybe_send_stream_format(%{stream_format_sent?: true} = state), do: {[], state}

  defp maybe_send_stream_format(%{decoder_ref: decoder} = state) do
    {:ok, width, height, pix_fmt} = Native.get_metadata(decoder)

    sf =
      %RawVideo{
        pixel_format: pix_fmt,
        width: width,
        height: height,
        framerate: {0, 1},
        aligned: true
      }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end
end
