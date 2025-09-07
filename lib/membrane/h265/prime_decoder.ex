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
    flow_control: :auto,
    accepted_format: %H265{alignment: :au}
  )

  def_output_pad(:output,
    flow_control: :auto,
    accepted_format: %PrimeFormat{}
  )

  @impl true
  def handle_init(_ctx, opts) do
    {:ok, worker} =
      Task.start_link(fn ->
        worker_loop(self())
      end)

    # If you want to receive :'EXIT' messages from the worker:
    Process.flag(:trap_exit, true)

    state = %{
      decoder_ref: nil,
      stream_format_sent?: false,
      hw_device: opts.hw_device,
      decoder: opts.decoder,
      worker: worker
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
  def handle_buffer(:input, buffer, ctx, %{decoder_ref: decoder} = state) do
    dts = Common.to_h265_time_base_truncated(buffer.dts)
    pts = Common.to_h265_time_base_truncated(buffer.pts)

    case Native.decode(decoder, buffer.payload, pts, dts) do
      {:ok, pts_list, descs} ->
        Membrane.Logger.debug("#{inspect {pts_list, descs}}")
        in_stream_format = ctx.pads.input.stream_format
        {actions, state} = maybe_send_stream_format(state, in_stream_format)
        bufs = wrap_descriptors(pts_list, descs, state.worker)
        {actions ++ bufs, state}

      {:error, reason} ->
        err = "Failed to decode frame: #{inspect(reason)}"
        Membrane.Logger.error("Failed to decode frame: #{inspect(reason)}")
        raise err
      other -> 
        err = "Failed to decode frame: #{inspect(other)}"

        Membrane.Logger.error("Failed to decode frame: #{inspect(other)}")
        raise err
    end
  end

  @impl true
  def handle_stream_format(:input, _format, _ctx, state) do
    {[], %{state | stream_format_sent?: false}}
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, %{decoder_ref: decoder} = state) do
    case Native.flush(decoder) do
      {:ok, pts_list, descs} ->
        :ok = Native.close(decoder)
        bufs = wrap_descriptors(pts_list, descs, state.worker)
        new_state = %{state | decoder_ref: nil}
        {bufs ++ [end_of_stream: :output], new_state}

      {:error, reason} ->
        raise "Native decoder failed to flush: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_terminate_request(_ctx, %{decoder_ref: decoder, worker: worker} = state) do
    :erlang.garbage_collect(self())
    if decoder do
      :ok = Native.close(decoder)
    end

    if Process.alive?(worker), do: Process.exit(worker, :shutdown)

    {[terminate: :normal], %{state | decoder_ref: nil}}
  end

  defp wrap_descriptors([], [], _worker), do: []

  defp wrap_descriptors(pts_list, descs, worker) do
    Enum.zip([pts_list, descs])
    |> Enum.map(fn {p, desc} ->
      %Buffer{
        pts: Common.to_membrane_time_base_truncated(p),
        payload: <<>>,
        metadata: %{drm_prime: Map.put(desc, :owner_pid, worker)}
      }
    end)
    |> then(&[buffer: {:output, &1}])
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

 defp worker_loop(element_pid) do
    receive do
      {:keepalive, keepalive} = msg ->
        Membrane.Logger.debug("#{inspect msg}")
        Native.keepalive_release(keepalive)
        worker_loop(element_pid)

      # If the element dies (they’re linked), the worker will exit automatically.
      # If you set trap_exit above, you can also handle exit signals explicitly:
      {:EXIT, _from, reason} ->
        # optional logging/cleanup
        exit(reason)
    end
  end
end
