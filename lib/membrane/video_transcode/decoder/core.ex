defmodule Membrane.VideoTranscode.Decoder.Core do
  @moduledoc false

  alias Membrane.Buffer
  alias Membrane.Instrumentation
  alias Membrane.Instrumentation.FrameTrace
  alias Membrane.RawVideo
  alias Membrane.VideoTranscode.Decoder.{Common, Native, ReleaseDispatcherCustodian}
  alias VideoInterop.{AbandonmentGuard, Colorimetry, Format, Frame, LeaseOwner, Rect}
  alias VideoInterop.DMABuf.Format, as: StorageFormat

  @nv12_fourcc :binary.decode_unsigned("NV12", :little)
  @release_timeout_ms 5_000

  @color_primaries %{
    1 => :bt709,
    4 => :bt470_m,
    5 => :bt470_bg,
    6 => :smpte170m,
    7 => :smpte240m,
    8 => :film,
    9 => :bt2020,
    10 => :smpte428,
    11 => :smpte431,
    12 => :smpte432,
    22 => :ebu3213
  }
  @color_transfers %{
    1 => :bt709,
    4 => :gamma22,
    5 => :gamma28,
    6 => :smpte170m,
    7 => :smpte240m,
    8 => :linear,
    9 => :log,
    10 => :log_sqrt,
    11 => :iec61966_2_4,
    12 => :bt1361,
    13 => :iec61966_2_1,
    14 => :bt2020_10,
    15 => :bt2020_12,
    16 => :smpte2084,
    17 => :smpte428,
    18 => :arib_std_b67
  }
  @color_matrices %{
    0 => :rgb,
    1 => :bt709,
    4 => :fcc,
    5 => :bt470_bg,
    6 => :smpte170m,
    7 => :smpte240m,
    8 => :ycgco,
    9 => :bt2020_ncl,
    10 => :bt2020_cl,
    11 => :smpte2085,
    12 => :chroma_derived_ncl,
    13 => :chroma_derived_cl,
    14 => :ictcp
  }
  @color_ranges %{1 => :limited, 2 => :full}
  @chroma_locations %{
    1 => :left,
    2 => :center,
    3 => :top_left,
    4 => :top,
    5 => :bottom_left,
    6 => :bottom
  }

  def init(opts, codec) do
    {[],
     %{
       codec: codec,
       decoder_ref: nil,
       stream_format_sent?: false,
       output_stream_format: nil,
       input_framerate: nil,
       output: opts.output,
       output_format: opts.output_format,
       hw_device: opts.hw_device,
       decoder: opts.decoder,
       max_in_flight: opts.max_in_flight,
       lease_owner: nil,
       release_dispatcher: nil,
       release_dispatcher_custodian: nil
     }}
  end

  def setup(state) do
    case start_release_lifecycle(state) do
      {:ok, state} ->
        case Native.create(
               state.codec,
               state.output,
               output_format_for_nif(state),
               state.hw_device,
               state.decoder
             ) do
          decoder when is_reference(decoder) ->
            {[], %{state | decoder_ref: decoder}}

          other ->
            _result = stop_release_lifecycle(state)
            raise "failed to create #{codec_label(state.codec)} decoder: #{inspect(other)}"
        end

      {:error, reason} ->
        raise "failed to start #{codec_label(state.codec)} decoder lifecycle: #{inspect(reason)}"
    end
  end

  def handle_buffer(:input, buffer, ctx, %{decoder_ref: decoder} = state) do
    dts = Common.to_codec_time_base_truncated(buffer.dts)
    pts = Common.to_codec_time_base_truncated(buffer.pts)
    input_trace = trace_decoder_input(buffer, state)

    result =
      Instrumentation.measure(
        [:nif, decoder_metric(state.codec), :decode],
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

        case state.output do
          :dmabuf ->
            wrap_dmabuf_outputs(pts_list, frames, state, in_stream_format, input_trace)

          :raw ->
            {actions, state} = maybe_send_raw_stream_format(state, in_stream_format)
            {actions ++ wrap_raw_outputs(pts_list, frames, input_trace), state}
        end

      {:error, reason} ->
        raise "failed to decode #{codec_label(state.codec)} frame: #{inspect(reason)}"

      other ->
        raise "invalid #{codec_label(state.codec)} decoder result: #{inspect(other)}"
    end
  end

  def handle_stream_format(format, state) do
    framerate = Map.get(format, :framerate)

    {[],
     %{
       state
       | stream_format_sent?: false,
         output_stream_format: nil,
         input_framerate: framerate
     }}
  end

  def handle_end_of_stream(%{decoder_ref: decoder} = state) do
    result =
      Instrumentation.measure(
        [:nif, decoder_metric(state.codec), :flush],
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

        case state.output do
          :dmabuf ->
            {actions, state} = wrap_dmabuf_outputs(pts_list, frames, state, nil, nil)
            {actions ++ [end_of_stream: :output], state}

          :raw ->
            {wrap_raw_outputs(pts_list, frames, nil) ++ [end_of_stream: :output], state}
        end

      {:error, reason} ->
        raise "native #{codec_label(state.codec)} decoder failed to flush: #{inspect(reason)}"
    end
  end

  def handle_info(
        {:video_interop_lease_release_failed, owner, _token, _metadata, reason},
        %{lease_owner: owner} = state
      ) do
    raise "decoded DMA-BUF release failed: #{inspect(reason)}; output=#{inspect(state.output)}"
  end

  def handle_info(_message, state), do: {[], state}

  def terminate(state) do
    if state.decoder_ref do
      :ok = Native.close(state.decoder_ref)
    end

    case stop_release_lifecycle(state) do
      :ok -> {[terminate: :normal], %{state | decoder_ref: nil}}
      {:error, reason} -> {[terminate: {:video_interop_shutdown_failed, reason}], state}
    end
  end

  defp start_release_lifecycle(%{output: :raw} = state), do: {:ok, state}

  defp start_release_lifecycle(%{output: :dmabuf} = state) do
    if Native.release_dispatcher_quarantined() do
      {:error, :release_dispatcher_quarantined}
    else
      case ReleaseDispatcherCustodian.start(self()) do
        {:ok, custodian, dispatcher} ->
          case start_lease_owner(state, dispatcher) do
            {:ok, owner} ->
              {:ok,
               %{
                 state
                 | lease_owner: owner,
                   release_dispatcher: dispatcher,
                   release_dispatcher_custodian: custodian
               }}

            {:error, reason} ->
              cleanup_unpublished_dispatcher(custodian, dispatcher)
              {:error, reason}
          end

        {:error, reason} ->
          {:error, reason}
      end
    end
  end

  defp start_lease_owner(state, dispatcher) do
    guard_factory = fn owner, token, holder ->
      Native.new_abandonment_guard(dispatcher, owner, token, holder)
    end

    LeaseOwner.start_link(
      producer: self(),
      notify: self(),
      max_active: state.max_in_flight,
      release: {Native, :release_frame, []},
      release_retry: {:exponential, initial_ms: 10, max_ms: 1_000, max_attempts: :infinity},
      abandonment_guard_factory: guard_factory
    )
  end

  defp cleanup_unpublished_dispatcher(custodian, dispatcher) do
    case Native.close_release_dispatcher(dispatcher, @release_timeout_ms) do
      {:ok, true} ->
        _result = ReleaseDispatcherCustodian.release_joined(custodian)
        :ok

      _error ->
        _newly_quarantined? = Native.quarantine_release_dispatchers()
        :error
    end
  end

  defp stop_release_lifecycle(%{output: :raw}), do: :ok

  defp stop_release_lifecycle(state) do
    with :ok <- LeaseOwner.drain(state.lease_owner, @release_timeout_ms),
         {:ok, true} <-
           Native.close_release_dispatcher(state.release_dispatcher, @release_timeout_ms),
         :ok <- ReleaseDispatcherCustodian.release_joined(state.release_dispatcher_custodian) do
      :ok
    else
      error ->
        _newly_quarantined? = Native.quarantine_release_dispatchers()
        {:error, error}
    end
  end

  defp output_format_for_nif(%{output: :dmabuf}), do: nil
  defp output_format_for_nif(%{output: :raw, output_format: format}), do: format

  defp wrap_dmabuf_outputs([], [], state, _in_stream_format, _input_trace), do: {[], state}

  defp wrap_dmabuf_outputs(pts_list, native_frames, state, in_stream_format, input_trace)
       when length(pts_list) == length(native_frames) do
    framerate = stream_framerate(in_stream_format, state.input_framerate)
    stream_format = dmabuf_stream_format!(native_frames, framerate, state.output_stream_format)

    format_actions =
      if state.stream_format_sent?, do: [], else: [stream_format: {:output, stream_format}]

    case issue_native_frames(native_frames, state.lease_owner) do
      {:ok, frames_and_leases} ->
        case build_dmabuf_buffers(pts_list, frames_and_leases, stream_format, input_trace) do
          {:ok, buffers} ->
            actions = format_actions ++ [buffer: {:output, buffers}]
            {actions, %{state | stream_format_sent?: true, output_stream_format: stream_format}}

          {:error, reason} ->
            Enum.each(frames_and_leases, fn {_frame, lease} -> VideoInterop.release(lease) end)
            raise "failed to build canonical decoded frame: #{inspect(reason)}"
        end

      {:error, reason} ->
        raise "failed to issue decoded frame lease: #{inspect(reason)}"
    end
  end

  defp wrap_dmabuf_outputs(pts_list, native_frames, _state, _in_stream_format, _input_trace) do
    release_native_frames(native_frames)

    raise "native decoder returned mismatched PTS/frame counts: #{length(pts_list)}/#{length(native_frames)}"
  end

  defp dmabuf_stream_format!(native_frames, framerate, previous_format) do
    first = hd(native_frames)

    format = %Format{
      width: first.width,
      height: first.height,
      framerate: normalize_framerate(framerate),
      storage: %StorageFormat{fourcc: @nv12_fourcc, modifier: first.modifier},
      acquire_sync: :sync_file,
      colorimetry: native_colorimetry(first),
      pixel_aspect_ratio: native_pixel_aspect_ratio(first),
      interlace_mode: native_interlace_mode(first)
    }

    valid_frames? =
      Enum.all?(native_frames, fn frame ->
        frame.width == format.width and frame.height == format.height and
          frame.modifier == format.storage.modifier and
          native_colorimetry(frame) == format.colorimetry and
          native_pixel_aspect_ratio(frame) == format.pixel_aspect_ratio and
          native_interlace_mode(frame) == format.interlace_mode and
          VideoInterop.validate(frame.descriptor) == :ok
      end)

    cond do
      VideoInterop.validate(format) != :ok ->
        release_native_frames(native_frames)
        raise "invalid decoded DMA-BUF stream format"

      not (is_nil(previous_format) or previous_format == format) ->
        release_native_frames(native_frames)
        raise "decoded DMA-BUF format changed without an input stream-format change"

      not valid_frames? ->
        release_native_frames(native_frames)
        raise "native decoder returned an invalid DMA-BUF descriptor"

      true ->
        format
    end
  end

  defp issue_native_frames(native_frames, owner),
    do: do_issue_native_frames(native_frames, owner, [])

  defp do_issue_native_frames([], _owner, issued), do: {:ok, Enum.reverse(issued)}

  defp do_issue_native_frames([native_frame | rest], owner, issued) do
    case LeaseOwner.issue(owner, native_frame.keepalive) do
      {:ok, lease} ->
        do_issue_native_frames(rest, owner, [{native_frame, lease} | issued])

      {:error, {:caller_owned, reason}} ->
        :ok = Native.release_frame(native_frame.keepalive)
        release_native_frames(rest)
        Enum.each(issued, fn {_frame, lease} -> VideoInterop.release(lease) end)
        {:error, {:caller_owned, reason}}

      {:error, {:transferred, reason}} ->
        release_native_frames(rest)
        Enum.each(issued, fn {_frame, lease} -> VideoInterop.release(lease) end)
        {:error, {:transferred, reason}}
    end
  end

  defp build_dmabuf_buffers(pts_list, frames_and_leases, stream_format, input_trace) do
    pts_list
    |> Enum.zip(frames_and_leases)
    |> Enum.reduce_while({:ok, []}, fn {pts, {native_frame, lease}}, {:ok, buffers} ->
      case build_dmabuf_buffer(pts, native_frame, lease, stream_format, input_trace) do
        {:ok, buffer} -> {:cont, {:ok, [buffer | buffers]}}
        {:error, reason} -> {:halt, {:error, reason}}
      end
    end)
    |> case do
      {:ok, buffers} -> {:ok, Enum.reverse(buffers)}
      error -> error
    end
  end

  defp build_dmabuf_buffer(pts, native_frame, lease, stream_format, input_trace) do
    try do
      if not AbandonmentGuard.valid?(lease.abandonment_guard) do
        raise ArgumentError, "decoded DMA-BUF lease is missing its abandonment guard"
      end

      frame = %Frame{
        coded_width: native_frame.width,
        coded_height: native_frame.height,
        visible_rect: %Rect{x: 0, y: 0, width: native_frame.width, height: native_frame.height},
        format: stream_format,
        storage: native_frame.descriptor,
        acquire_sync: %VideoInterop.SyncFile{acquire_fence_fd: native_frame.acquire_fence_fd},
        lease: lease
      }

      with :ok <- VideoInterop.validate(frame, stream_format) do
        buffer_pts = Common.to_membrane_time_base_truncated(pts)
        trace = output_trace(input_trace, buffer_pts)
        trace = Instrumentation.mark_trace(trace, :decoder_output, %{output: :dmabuf})

        Instrumentation.emit_frame_stage(
          :video_transcode,
          trace,
          :decoder_output,
          %{output_frames: 1},
          %{output: :dmabuf}
        )

        {:ok,
         %Buffer{
           pts: buffer_pts,
           payload: frame,
           metadata: output_metadata(%{}, trace)
         }}
      end
    rescue
      error -> {:error, {:exception, error, __STACKTRACE__}}
    catch
      kind, reason -> {:error, {kind, reason, __STACKTRACE__}}
    end
  end

  defp release_native_frames(frames), do: Enum.each(frames, &Native.release_frame(&1.keepalive))

  defp wrap_raw_outputs([], [], _input_trace), do: []

  defp wrap_raw_outputs(pts_list, frames, input_trace) do
    pts_list
    |> Enum.zip(frames)
    |> Enum.map(fn {pts, payload} ->
      buffer_pts = Common.to_membrane_time_base_truncated(pts)
      trace = output_trace(input_trace, buffer_pts)
      trace = Instrumentation.mark_trace(trace, :decoder_output, %{output: :raw})

      Instrumentation.emit_frame_stage(
        :video_transcode,
        trace,
        :decoder_output,
        %{output_frames: 1},
        %{output: :raw}
      )

      %Buffer{pts: buffer_pts, payload: payload, metadata: output_metadata(%{}, trace)}
    end)
    |> then(&[buffer: {:output, &1}])
  end

  defp maybe_send_raw_stream_format(%{stream_format_sent?: true} = state, _in_sf),
    do: {[], state}

  defp maybe_send_raw_stream_format(%{decoder_ref: decoder} = state, in_sf) do
    {:ok, width, height, pix_fmt} = Native.get_metadata(decoder)

    sf = %RawVideo{
      pixel_format: pix_fmt,
      width: width,
      height: height,
      framerate: stream_framerate(in_sf, state.input_framerate) || {0, 1},
      aligned: true
    }

    {[stream_format: {:output, sf}], %{state | stream_format_sent?: true}}
  end

  defp stream_framerate(format, fallback) when is_map(format),
    do: Map.get(format, :framerate) || fallback

  defp stream_framerate(_format, fallback), do: fallback

  defp normalize_framerate({numerator, denominator})
       when is_integer(numerator) and numerator > 0 and is_integer(denominator) and
              denominator > 0,
       do: {numerator, denominator}

  defp normalize_framerate(_framerate), do: nil

  defp trace_decoder_input(buffer, state) do
    trace = FrameTrace.fetch(buffer) || Instrumentation.derive_trace(nil, pts: buffer.pts)
    trace = Instrumentation.mark_trace(trace, :decoder_input, %{output: state.output})

    Instrumentation.emit_frame_stage(
      :video_transcode,
      trace,
      :decoder_input,
      %{payload_bytes: Membrane.Payload.size(buffer.payload)},
      %{output: state.output}
    )

    trace
  end

  defp output_trace(input_trace, pts), do: Instrumentation.derive_trace(input_trace, pts: pts)

  defp output_metadata(metadata, nil), do: metadata
  defp output_metadata(metadata, trace), do: Map.put(metadata, FrameTrace.metadata_key(), trace)

  defp native_colorimetry(frame) do
    %Colorimetry{
      primaries: Map.get(@color_primaries, frame.color_primaries, :unspecified),
      transfer: Map.get(@color_transfers, frame.color_transfer, :unspecified),
      matrix: Map.get(@color_matrices, frame.color_matrix, :unspecified),
      range: Map.get(@color_ranges, frame.color_range, :unspecified),
      chroma_location: Map.get(@chroma_locations, frame.chroma_location, :unspecified)
    }
  end

  defp native_pixel_aspect_ratio(frame),
    do: {frame.pixel_aspect_ratio_num, frame.pixel_aspect_ratio_den}

  defp native_interlace_mode(%{interlaced: false}), do: :progressive
  defp native_interlace_mode(%{top_field_first: true}), do: :interlaced_top_first
  defp native_interlace_mode(_frame), do: :interlaced_bottom_first

  defp codec_label(:h264), do: "H.264"
  defp codec_label(:h265), do: "H.265"

  defp decoder_metric(:h264), do: :h264_decoder
  defp decoder_metric(:h265), do: :h265_decoder

  defp nif_result_label({:ok, _pts_list, _frames}), do: :ok
  defp nif_result_label({:error, _reason}), do: :error
  defp nif_result_label(_other), do: :other
end
