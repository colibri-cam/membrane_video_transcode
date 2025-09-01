defmodule Membrane.H265.PrimeDecoder do
  @moduledoc """
  Variant of `Membrane.H265.Decoder` that returns DRM Prime descriptors instead of
  raw frame payloads. Each decoded frame is sent downstream as an empty buffer
  with the descriptor attached under the `:drm_prime` metadata key.

  It also returns keepalive which is a reference to AV frame in GPU
  memory. When reference gets GCed AV frame get's release. Keep
  keepalive in pipeline until prime descriptor reaches consumer.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.PrimeFormat
  alias Membrane.H265
  alias Membrane.H265.Common

  @typedoc """
  Decoder backend to use.
  """
  @type decoder_backend :: :auto | :vaapi | :v4l2request | :v4l2m2m | :software

  def_options(
    hw_device: [
      spec: String.t(),
      default: "/dev/dri/renderD129",
      description: "Hw device to use"
    ],
    decoder: [
      spec: decoder_backend(),
      default: :auto,
      description: "Decoder backend to use"
    ]
  )

  def_input_pad(:input,
    flow_control: :manual,
    demand_unit: :buffers,
    accepted_format: %H265{alignment: :au}
  )

  def_output_pad(:output,
    flow_control: :manual,
    demand_unit: :buffers,
    accepted_format: %PrimeFormat{}
  )

  @impl true
  def handle_init(_ctx, opts) do
    state = %{
      decoder_ref: nil,
      stream_format_sent?: false,
      hw_device: opts.hw_device,
      decoder: opts.decoder,
      queue: [],
      pending_input: 0,
      output_demand: 0,
      end_of_stream?: false
    }

    {[], state}
  end

  @impl true
  def handle_setup(_ctx, state) do
    decoder =
      case Native.create(state.hw_device, state.decoder) do
        {:error, reason} -> raise "Error creating decoder #{inspect(reason)}"
        decoder -> decoder
      end

    {[], %{state | decoder_ref: decoder}}
  end

  @impl true
  def handle_start_of_stream(:input, _ctx, state) do
    {actions, state} = maybe_request_input(state)
    {actions, state}
  end

  @impl true
  def handle_demand(:output, size, _ctx, state) do
    state = %{state | output_demand: state.output_demand + size}
    {buf_actions, state} = send_from_queue(state)
    {demand_actions, state} = maybe_request_input(state)
    {buf_actions ++ demand_actions, state}
  end

  @impl true
  def handle_buffer(:input, buffer, ctx, %{decoder_ref: decoder} = state) do
    dts = Common.to_h265_time_base_truncated(buffer.dts)
    pts = Common.to_h265_time_base_truncated(buffer.pts)

    case Native.decode(decoder, buffer.payload, pts, dts) do
      {:ok, pts_list, descs, keepalives} ->
        Membrane.Logger.debug("#{inspect({pts_list, descs, keepalives})}")
        in_stream_format = ctx.pads.input.stream_format
        {sf_actions, state} = maybe_send_stream_format(state, in_stream_format)
        queue_entries = Enum.zip([pts_list, descs, keepalives])

        state = %{
          state
          | queue: state.queue ++ queue_entries,
            pending_input: max(state.pending_input - 1, 0)
        }

        {buf_actions, state} = send_from_queue(state)
        {demand_actions, state} = maybe_request_input(state)
        {sf_actions ++ buf_actions ++ demand_actions, state}

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
    case Native.flush(decoder) do
      {:ok, pts_list, descs, keepalives} ->
        :ok = Native.close(decoder)
        queue_entries = Enum.zip([pts_list, descs, keepalives])

        state = %{
          state
          | decoder_ref: nil,
            queue: state.queue ++ queue_entries,
            end_of_stream?: true
        }

        {actions, state} = send_from_queue(state)
        {actions, state}

      {:error, reason} ->
        raise "Native decoder failed to flush: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_terminate_request(_ctx, %{decoder_ref: decoder} = state) do
    if decoder do
      :ok = Native.close(decoder)
    end

    {[terminate: :normal], %{state | decoder_ref: nil}}
  end

  defp wrap_descriptor({p, desc, keepalive}) do
    %Buffer{
      pts: Common.to_membrane_time_base_truncated(p),
      payload: <<>>,
      metadata: %{drm_prime: desc, keepalive: keepalive}
    }
  end

  defp send_from_queue(%{queue: queue, output_demand: demand} = state) do
    {to_send, rest} = Enum.split(queue, demand)
    buffers = Enum.map(to_send, &wrap_descriptor/1)

    actions =
      if buffers == [] do
        []
      else
        [buffer: {:output, buffers}]
      end

    state = %{state | queue: rest, output_demand: demand - length(to_send)}

    actions =
      if state.end_of_stream? and state.queue == [] do
        actions ++ [end_of_stream: :output]
      else
        actions
      end

    {actions, state}
  end

  defp maybe_request_input(%{queue: queue, pending_input: pending} = state) do
    free = 2 - (length(queue) + pending)

    cond do
      free <= 0 or state.end_of_stream? or is_nil(state.decoder_ref) ->
        {[], state}

      true ->
        {[demand: {:input, free}], %{state | pending_input: pending + free}}
    end
  end

  defp maybe_send_stream_format(%{stream_format_sent?: true} = state, _in_sf), do: {[], state}

  defp maybe_send_stream_format(%{decoder_ref: decoder} = state, in_sf) do
    {:ok, width, height} = Native.get_metadata(decoder)

    framerate =
      case in_sf do
        %H265{framerate: in_framerate} -> in_framerate
        _ -> {0, 1}
      end

    sf = %PrimeFormat{
      width: width,
      height: height,
      framerate: framerate
    }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end
end
