defmodule Membrane.H265.PrimeDecoder do
  @moduledoc """
  Decodes H265 into either DRM Prime descriptors or raw video frames.

  Prime output sends empty buffers with descriptors attached under the
  `:drm_prime` metadata key. Raw output sends copied frame payloads.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.H265
  alias Membrane.H265.Common
  alias Membrane.PrimeFormat
  alias Membrane.RawVideo

  @typedoc """
  Supported raw output pixel formats.
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

  @typedoc """
  Decoder backend to use.
  """
  @type decoder_backend :: :auto | :vaapi | :v4l2request | :v4l2m2m | :software

  @typedoc """
  Decoder output mode.
  """
  @type output_mode :: :prime | :raw

  def_options(
    output: [
      spec: output_mode(),
      default: :prime,
      description: "Whether to emit DRM Prime descriptors or raw frames"
    ],
    output_format: [
      spec: pixel_format(),
      default: :NV12,
      description: "Pixel format to use for raw decoded frames"
    ],
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
    accepted_format:
      any_of(
        %PrimeFormat{},
        %RawVideo{pixel_format: format, aligned: true} when format in @formats
      )
  )

  @impl true
  def handle_init(_ctx, opts) do
    worker = maybe_start_worker(opts.output)

    state = %{
      decoder_ref: nil,
      stream_format_sent?: false,
      output: opts.output,
      output_format: opts.output_format,
      hw_device: opts.hw_device,
      decoder: opts.decoder,
      worker: worker
    }

    {[], state}
  end

  @impl true
  def handle_setup(_ctx, state) do
    decoder =
      case Native.create(
             state.output,
             output_format_for_nif(state),
             state.hw_device,
             state.decoder
           ) do
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
        bufs = wrap_outputs(state.output, pts_list, frames, state.worker)
        {actions ++ bufs, state}

      {:error, reason} ->
        err = "Failed to decode frame: #{inspect(reason)}"
        Membrane.Logger.error("#{err}")
        raise err

      other ->
        err = "Failed to decode frame: #{inspect(other)}"
        Membrane.Logger.error("#{err}")
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
      {:ok, pts_list, frames} ->
        :ok = Native.close(decoder)
        bufs = wrap_outputs(state.output, pts_list, frames, state.worker)
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

    if worker && Process.alive?(worker) do
      Process.exit(worker, :shutdown)
    end

    {[terminate: :normal], %{state | decoder_ref: nil}}
  end

  defp maybe_start_worker(:prime) do
    {:ok, worker} = Task.start_link(fn -> worker_loop() end)
    Process.flag(:trap_exit, true)
    worker
  end

  defp maybe_start_worker(:raw), do: nil

  defp output_format_for_nif(%{output: :prime}), do: nil
  defp output_format_for_nif(%{output: :raw, output_format: format}), do: format

  defp wrap_outputs(_output, [], [], _worker), do: []

  defp wrap_outputs(:prime, pts_list, descs, worker) do
    Enum.zip(pts_list, descs)
    |> Enum.map(fn {pts, desc} ->
      %Buffer{
        pts: Common.to_membrane_time_base_truncated(pts),
        payload: <<>>,
        metadata: %{drm_prime: Map.put(desc, :owner_pid, worker)}
      }
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp wrap_outputs(:raw, pts_list, frames, _worker) do
    Enum.zip(pts_list, frames)
    |> Enum.map(fn {pts, payload} ->
      %Buffer{pts: Common.to_membrane_time_base_truncated(pts), payload: payload}
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp maybe_send_stream_format(%{stream_format_sent?: true} = state, _in_sf), do: {[], state}

  defp maybe_send_stream_format(%{decoder_ref: decoder, output: :prime} = state, in_sf) do
    {:ok, width, height, _} = Native.get_metadata(decoder)

    framerate =
      case in_sf do
        %H265{framerate: in_framerate} -> in_framerate
        _ -> {0, 1}
      end

    sf = %PrimeFormat{width: width, height: height, framerate: framerate}

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end

  defp maybe_send_stream_format(%{decoder_ref: decoder, output: :raw} = state, in_sf) do
    {:ok, width, height, pix_fmt} = Native.get_metadata(decoder)

    framerate =
      case in_sf do
        %H265{framerate: in_framerate} -> in_framerate
        _ -> {0, 1}
      end

    sf = %RawVideo{
      pixel_format: pix_fmt,
      width: width,
      height: height,
      framerate: framerate,
      aligned: true
    }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end

  defp worker_loop do
    receive do
      {:keepalive, keepalive} ->
        Native.keepalive_release(keepalive)
        worker_loop()

      {:EXIT, _from, reason} ->
        exit(reason)
    end
  end
end
