defmodule Membrane.H265.PrimeDecoder do
  @moduledoc """
  Variant of `Membrane.H265Decoder` that returns DRM Prime descriptors instead of
  raw frame payloads. Each decoded frame is sent downstream as an empty buffer
  with the descriptor attached under the `:drm_prime` metadata key.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.DRM.Prime
  alias Membrane.H265
  alias Membrane.Time

  def_input_pad(:input,
    flow_control: :auto,
    accepted_format: %H265{alignment: :au}
  )

  def_output_pad(:output,
    flow_control: :auto,
    accepted_format: %Prime{}
  )

  @impl true
  def handle_init(_ctx, _opts) do
    state = %{decoder_ref: nil, stream_format_sent?: false}
    {[], state}
  end

  @impl true
  def handle_setup(_ctx, state) do
    decoder =
      case Native.create() do
        {:error, reason} -> raise "Error creating decoder #{inspect(reason)}"
        decoder -> decoder
      end

    {[], %{state | decoder_ref: decoder}}
  end

  @impl true
  def handle_buffer(:input, buffer, _ctx, %{decoder_ref: decoder} = state) do
    pts = to_us(buffer.pts)
    dts = to_us(buffer.dts)

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
      %Buffer{pts: Time.microseconds(p), payload: <<>>, metadata: %{drm_prime: desc}}
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp to_us(nil), do: 0
  defp to_us(time), do: Time.as_microseconds(time, :round)

  defp maybe_send_stream_format(%{stream_format_sent?: true} = state), do: {[], state}

  defp maybe_send_stream_format(%{decoder_ref: decoder} = state) do
    {:ok, width, height} = Native.get_metadata(decoder)

    sf = %Prime{
      width: width,
      height: height,
      fd: -1,
      pitches: [],
      offsets: []
    }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end
end
