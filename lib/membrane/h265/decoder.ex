defmodule Membrane.H265.Decoder do
  @moduledoc """
  Decodes H265 into canonical DMA-BUF frames, legacy DRM Prime descriptors, or raw video frames.

  DMA-BUF output sends empty buffers with `%Membrane.DMABuf.VideoFrame{}` under the `:dmabuf`
  metadata key. The native frame and duplicated object fds are retired by an isolated
  `Membrane.DMABuf.LeaseOwner`. Legacy Prime output remains available for rollback.
  """

  use Membrane.Filter

  alias __MODULE__.Native
  alias Membrane.Buffer
  alias Membrane.Instrumentation
  alias Membrane.Instrumentation.FrameTrace
  alias Membrane.H265
  alias Membrane.H265.Common
  alias Membrane.DMABuf.{FourCC, Lease, LeaseOwner, Rect, Validator, VideoFormat, VideoFrame}
  alias Membrane.PrimeDesc
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
  @nv12_fourcc FourCC.from_string!("NV12")

  @typedoc """
  Decoder backend to use.
  """
  @type decoder_backend :: :auto | :vaapi | :v4l2request | :v4l2m2m | :software

  @typedoc """
  Decoder output mode.
  """
  @type output_mode :: :dmabuf | :prime | :raw

  def_options(
    output: [
      spec: output_mode(),
      default: :dmabuf,
      description: "Whether to emit canonical DMA-BUF, legacy DRM Prime, or raw frames"
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
        %VideoFormat{fourcc: @nv12_fourcc},
        %PrimeFormat{},
        %RawVideo{pixel_format: format, aligned: true} when format in @formats
      )
  )

  @impl true
  def handle_init(_ctx, opts) do
    worker = maybe_start_worker(opts.output)
    lease_owner = maybe_start_lease_owner(opts.output)

    state = %{
      decoder_ref: nil,
      stream_format_sent?: false,
      output_stream_format: nil,
      input_framerate: nil,
      output: opts.output,
      output_format: opts.output_format,
      hw_device: opts.hw_device,
      decoder: opts.decoder,
      worker: worker,
      lease_owner: lease_owner
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
    input_trace = trace_decoder_input(buffer, state)

    result =
      Instrumentation.measure(
        [:nif, :h265_prime_decoder, :decode],
        %{
          decoder: state.decoder,
          hw_device: state.hw_device,
          output: state.output,
          payload_bytes: Membrane.Payload.size(buffer.payload)
        },
        fn ->
          result = Native.decode(decoder, buffer.payload, pts, dts)

          measurements =
            case result do
              {:ok, pts_list, frames} -> %{frames: length(frames), output_pts: length(pts_list)}
              _other -> %{}
            end

          {result, measurements, %{result: nif_result_label(result)}}
        end
      )

    case result do
      {:ok, pts_list, frames} ->
        in_stream_format = ctx.pads.input.stream_format

        if state.output == :dmabuf do
          wrap_dmabuf_outputs(pts_list, frames, state, in_stream_format, input_trace)
        else
          {actions, state} = maybe_send_stream_format(state, in_stream_format)
          bufs = wrap_outputs(state.output, pts_list, frames, state.worker, input_trace)
          {actions ++ bufs, state}
        end

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
  def handle_stream_format(:input, format, _ctx, state) do
    framerate = if match?(%H265{}, format), do: format.framerate, else: nil

    {[],
     %{
       state
       | stream_format_sent?: false,
         output_stream_format: nil,
         input_framerate: framerate
     }}
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, %{decoder_ref: decoder} = state) do
    result =
      Instrumentation.measure(
        [:nif, :h265_prime_decoder, :flush],
        %{decoder: state.decoder, hw_device: state.hw_device, output: state.output},
        fn ->
          result = Native.flush(decoder)

          measurements =
            case result do
              {:ok, pts_list, frames} -> %{frames: length(frames), output_pts: length(pts_list)}
              _other -> %{}
            end

          {result, measurements, %{result: nif_result_label(result)}}
        end
      )

    case result do
      {:ok, pts_list, frames} ->
        :ok = Native.close(decoder)
        state = %{state | decoder_ref: nil}

        if state.output == :dmabuf do
          in_stream_format = %H265{framerate: state.input_framerate}
          {actions, state} = wrap_dmabuf_outputs(pts_list, frames, state, in_stream_format, nil)
          {actions ++ [end_of_stream: :output], state}
        else
          bufs = wrap_outputs(state.output, pts_list, frames, state.worker, nil)
          {bufs ++ [end_of_stream: :output], state}
        end

      {:error, reason} ->
        raise "Native decoder failed to flush: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_info(
        {:membrane_dmabuf_lease_release_failed, owner, _token, _metadata, reason},
        _ctx,
        %{lease_owner: owner} = state
      ) do
    raise "DMA-BUF backend release failed: #{inspect(reason)}; state=#{inspect(state.output)}"
  end

  def handle_info(_message, _ctx, state), do: {[], state}

  @impl true
  def handle_terminate_request(
        _ctx,
        %{decoder_ref: decoder, worker: worker, lease_owner: lease_owner} = state
      ) do
    :erlang.garbage_collect(self())

    if decoder do
      :ok = Native.close(decoder)
    end

    if worker && Process.alive?(worker) do
      Process.exit(worker, :shutdown)
    end

    if lease_owner && Process.alive?(lease_owner) do
      _result = LeaseOwner.close(lease_owner)
    end

    {[terminate: :normal], %{state | decoder_ref: nil}}
  end

  defp maybe_start_worker(:prime) do
    {:ok, worker} = Task.start_link(fn -> worker_loop() end)
    Process.flag(:trap_exit, true)
    worker
  end

  defp maybe_start_worker(_output), do: nil

  defp maybe_start_lease_owner(:dmabuf) do
    {:ok, owner} =
      LeaseOwner.start_link(
        producer: self(),
        release: fn keepalive ->
          case Native.release_frame(keepalive) do
            :ok -> :ok
            other -> {:error, other}
          end
        end
      )

    owner
  end

  defp maybe_start_lease_owner(_output), do: nil

  defp output_format_for_nif(%{output: output}) when output in [:prime, :dmabuf], do: nil
  defp output_format_for_nif(%{output: :raw, output_format: format}), do: format

  defp wrap_dmabuf_outputs([], [], state, _in_stream_format, _input_trace), do: {[], state}

  defp wrap_dmabuf_outputs(pts_list, native_frames, state, in_stream_format, input_trace)
       when length(pts_list) == length(native_frames) do
    framerate =
      case in_stream_format do
        %H265{framerate: value} -> value
        _other -> state.input_framerate
      end

    stream_format = dmabuf_stream_format!(native_frames, framerate, state.output_stream_format)

    format_actions =
      if state.stream_format_sent?, do: [], else: [stream_format: {:output, stream_format}]

    {buffers, _issued_leases} =
      Enum.zip(pts_list, native_frames)
      |> Enum.reduce({[], []}, fn {pts, native_frame}, {buffers, leases} ->
        case build_dmabuf_buffer(pts, native_frame, stream_format, state.lease_owner, input_trace) do
          {:ok, buffer, lease} ->
            {[buffer | buffers], [lease | leases]}

          {:error, reason} ->
            Enum.each(leases, &Lease.release/1)
            Enum.each(native_frames, &Native.release_frame(&1.keepalive))
            raise "Failed to export canonical DMA-BUF frame: #{inspect(reason)}"
        end
      end)

    actions =
      case Enum.reverse(buffers) do
        [] -> format_actions
        buffers -> format_actions ++ [buffer: {:output, buffers}]
      end

    {actions, %{state | stream_format_sent?: true, output_stream_format: stream_format}}
  end

  defp wrap_dmabuf_outputs(pts_list, native_frames, _state, _in_stream_format, _input_trace) do
    Enum.each(native_frames, &Native.release_frame(&1.keepalive))

    raise "Native decoder returned mismatched PTS/frame counts: #{length(pts_list)}/#{length(native_frames)}"
  end

  defp dmabuf_stream_format!(native_frames, framerate, previous_format) do
    first = hd(native_frames)

    format = %VideoFormat{
      width: first.width,
      height: first.height,
      framerate: framerate,
      fourcc: @nv12_fourcc,
      modifier: first.modifier
    }

    with :ok <- Validator.validate_format(format),
         true <- is_nil(previous_format) or previous_format == format,
         true <-
           Enum.all?(native_frames, fn frame ->
             frame.width == format.width and frame.height == format.height and
               frame.modifier == format.modifier and
               Validator.validate_descriptor(frame.descriptor) == :ok
           end) do
      format
    else
      false ->
        Enum.each(native_frames, &Native.release_frame(&1.keepalive))
        raise "Native DMA-BUF frame changed format without an input stream-format change"

      {:error, reason} ->
        Enum.each(native_frames, &Native.release_frame(&1.keepalive))
        raise "Invalid DMA-BUF stream format: #{inspect(reason)}"
    end
  end

  defp build_dmabuf_buffer(pts, native_frame, stream_format, lease_owner, input_trace) do
    with {:ok, lease} <- LeaseOwner.issue(lease_owner, native_frame.keepalive) do
      try do
        frame = %VideoFrame{
          coded_width: native_frame.width,
          coded_height: native_frame.height,
          visible_rect: %Rect{x: 0, y: 0, width: native_frame.width, height: native_frame.height},
          descriptor: native_frame.descriptor,
          synchronization: :implicit,
          lease: lease
        }

        case Validator.validate_frame_against_format(frame, stream_format) do
          :ok ->
            buffer_pts = Common.to_membrane_time_base_truncated(pts)
            trace = output_trace(input_trace, buffer_pts)
            trace = Instrumentation.mark_trace(trace, :decoder_output, %{output: :dmabuf})

            Instrumentation.emit_frame_stage(
              :prime_decoder,
              trace,
              :decoder_output,
              %{output_frames: 1},
              %{output: :dmabuf}
            )

            buffer = %Buffer{
              pts: buffer_pts,
              payload: <<>>,
              metadata: output_metadata(%{dmabuf: frame}, trace)
            }

            {:ok, buffer, lease}

          {:error, reason} ->
            Lease.release(lease)
            {:error, reason}
        end
      rescue
        error ->
          Lease.release(lease)
          {:error, {:exception, error, __STACKTRACE__}}
      catch
        kind, reason ->
          Lease.release(lease)
          {:error, {kind, reason, __STACKTRACE__}}
      end
    end
  end

  defp wrap_outputs(_output, [], [], _worker, _input_trace), do: []

  defp wrap_outputs(:prime, pts_list, descs, worker, input_trace) do
    Enum.zip(pts_list, descs)
    |> Enum.map(fn {pts, desc} ->
      buffer_pts = Common.to_membrane_time_base_truncated(pts)
      trace = output_trace(input_trace, buffer_pts)
      trace = Instrumentation.mark_trace(trace, :decoder_output, %{output: :prime})

      Instrumentation.emit_frame_stage(
        :prime_decoder,
        trace,
        :decoder_output,
        %{output_frames: 1},
        %{output: :prime}
      )

      %Buffer{
        pts: buffer_pts,
        payload: <<>>,
        metadata: prime_output_metadata(desc, worker, trace)
      }
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp wrap_outputs(:raw, pts_list, frames, _worker, input_trace) do
    Enum.zip(pts_list, frames)
    |> Enum.map(fn {pts, payload} ->
      buffer_pts = Common.to_membrane_time_base_truncated(pts)
      trace = output_trace(input_trace, buffer_pts)
      trace = Instrumentation.mark_trace(trace, :decoder_output, %{output: :raw})

      Instrumentation.emit_frame_stage(
        :prime_decoder,
        trace,
        :decoder_output,
        %{output_frames: 1},
        %{output: :raw}
      )

      %Buffer{pts: buffer_pts, payload: payload, metadata: output_metadata(%{}, trace)}
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

  defp trace_decoder_input(buffer, state) do
    trace = FrameTrace.fetch(buffer) || Instrumentation.derive_trace(nil, pts: buffer.pts)
    trace = Instrumentation.mark_trace(trace, :decoder_input, %{output: state.output})

    Instrumentation.emit_frame_stage(
      :prime_decoder,
      trace,
      :decoder_input,
      %{payload_bytes: Membrane.Payload.size(buffer.payload)},
      %{output: state.output}
    )

    trace
  end

  defp output_trace(input_trace, pts) do
    Instrumentation.derive_trace(input_trace, pts: pts)
  end

  defp output_metadata(metadata, nil), do: metadata

  defp output_metadata(metadata, trace) do
    Map.put(metadata, FrameTrace.metadata_key(), trace)
  end

  defp prime_output_metadata(desc, worker, trace) do
    desc =
      desc
      |> prime_desc_fields()
      |> Map.merge(%{owner_pid: worker, trace_token: trace_token(trace)})
      |> then(&struct!(PrimeDesc, &1))

    %{drm_prime: desc}
    |> output_metadata(trace)
  end

  defp prime_desc_fields(desc) do
    cond do
      is_struct(desc) ->
        Map.from_struct(desc)

      is_map(desc) ->
        Map.delete(desc, :__struct__)

      true ->
        raise "Unexpected prime descriptor: #{inspect(desc)}"
    end
  end

  defp trace_token(nil), do: nil
  defp trace_token(trace), do: FrameTrace.token(trace)

  defp nif_result_label({:ok, _pts_list, _frames}), do: :ok
  defp nif_result_label({:error, _reason}), do: :error
  defp nif_result_label(_other), do: :other
end
