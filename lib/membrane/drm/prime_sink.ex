defmodule Membrane.DRM.PrimeSink do
  @moduledoc """
  Sink that displays either DRM Prime descriptors or raw video frames on a DRM
  display.

  DRM Prime frames are scanned out directly by the native NIF without copying.
  Raw frames are copied into reusable scanout buffers inside the native sink.
  """

  use Membrane.Sink

  require Membrane.Logger

  alias Membrane.Buffer
  alias Membrane.DRM.Instrumentation
  alias Membrane.DRM.Instrumentation.FrameTrace
  alias Membrane.DRM.Instrumentation.TraceToken
  alias Membrane.DRM.PrimeSink.DisplayInfo
  alias Membrane.DRM.PrimeSink.Native
  alias Membrane.PrimeFormat
  alias Membrane.RawVideo

  @raw_formats [:I420, :I422, :I444, :RGB, :BGRA, :RGBA, :NV12, :NV21, :YV12, :AYUV, :YUY2]

  def_options(
    card: [
      spec: String.t(),
      default: "/dev/dri/card0",
      description: "Graphic card to use"
    ],
    preferred_mode: [
      spec: {pos_integer(), pos_integer(), pos_integer()} | nil,
      default: nil,
      description:
        "Preferred video mode as {width, height, framerate}; defaults to the input stream format when available"
    ],
    ignore_pts: [
      spec: boolean,
      default: false,
      description: "Consume frames as fast as possible, skips frames beetween vblanks"
    ],
    pixel_format: [
      spec: atom() | nil,
      default: nil,
      description: "Expected raw input pixel format; defaults to the stream format"
    ]
  )

  def_input_pad(:input,
    accepted_format:
      any_of(%PrimeFormat{}, %RawVideo{pixel_format: format} when format in @raw_formats),
    flow_control: :manual,
    demand_unit: :buffers
  )

  @impl true
  def handle_init(_ctx, opts) do
    {[],
     %{
       ignore_pts: opts.ignore_pts,
       backend: nil,
       display: nil,
       last_pts: nil,
       last_desc: nil,
       last_payload: nil,
       last_trace: nil,
       card: opts.card,
       preferred_mode: opts.preferred_mode,
       pixel_format: opts.pixel_format,
       raw_stream_format: nil
     }}
  end

  @impl true
  def handle_setup(_ctx, state) do
    {[], state}
  end

  @impl true
  def handle_stream_format(:input, %PrimeFormat{} = format, _ctx, %{display: nil} = state) do
    mode = state.preferred_mode || stream_format_mode(format)

    {:ok, info, display} =
      Instrumentation.measure(
        [:nif, :drm_prime_sink, :init_display],
        %{backend: :prime, card: state.card, preferred_mode: mode},
        fn ->
          result = Native.init_display(state.card, mode, self())
          {result, %{}, %{result: nif_result_label(result)}}
        end
      )

    if info do
      log_display_info(info)
    end

    {[],
     %{
       state
       | display: display,
         backend: :prime,
         last_pts: nil,
         last_desc: nil,
         last_payload: nil,
         last_trace: nil
     }}
  end

  def handle_stream_format(:input, %PrimeFormat{}, _ctx, %{backend: :prime} = state),
    do: {[], state}

  def handle_stream_format(:input, %PrimeFormat{}, _ctx, _state) do
    raise "Stream format changed while playing. Switching between raw and DRM Prime is not supported."
  end

  def handle_stream_format(:input, %RawVideo{} = format, _ctx, %{display: nil} = state) do
    pixel_format = state.pixel_format || format.pixel_format

    if pixel_format != format.pixel_format do
      raise "Configured pixel format #{inspect(pixel_format)} does not match stream pixel format #{inspect(format.pixel_format)}"
    end

    mode = state.preferred_mode || raw_stream_format_mode(format)

    {:ok, info, display} =
      Instrumentation.measure(
        [:nif, :drm_prime_sink, :init_raw_display],
        %{backend: :raw, card: state.card, preferred_mode: mode, pixel_format: pixel_format},
        fn ->
          result =
            Native.init_raw_display(
              state.card,
              pixel_format,
              format.width,
              format.height,
              mode,
              self()
            )

          {result, %{}, %{result: nif_result_label(result)}}
        end
      )

    if info do
      log_display_info(info)
    end

    {[],
     %{
       state
       | display: display,
         backend: :raw,
         pixel_format: pixel_format,
         last_pts: nil,
         last_desc: nil,
         last_payload: nil,
         last_trace: nil,
         raw_stream_format: raw_stream_signature(format)
     }}
  end

  def handle_stream_format(:input, %RawVideo{} = format, _ctx, %{backend: :raw} = state) do
    if state.raw_stream_format != raw_stream_signature(format) do
      raise "Raw stream format changed while playing. This is not supported."
    end

    {[], state}
  end

  def handle_stream_format(:input, %RawVideo{}, _ctx, _state) do
    raise "Stream format changed while playing. Switching between raw and DRM Prime is not supported."
  end

  @impl true
  def handle_info({:display_waiting, reason}, _ctx, state) do
    Membrane.Logger.warning("Waiting for DRM display hot-plug on #{state.card}: #{reason}")
    {[], state}
  end

  def handle_info({:display_connected, %DisplayInfo{} = info}, _ctx, state) do
    log_display_info(info)
    {[], state}
  end

  def handle_info({:display_disconnected, reason}, _ctx, state) do
    Membrane.Logger.warning("Lost DRM display on #{state.card}, waiting for hot-plug: #{reason}")
    {[], state}
  end

  def handle_info({:trace_event, stage, %TraceToken{} = token, duration_ns}, _ctx, state) do
    measurements = if is_integer(duration_ns), do: %{duration_ns: duration_ns}, else: %{}

    Instrumentation.emit_frame_stage_from_token(
      :drm_prime_sink_native,
      token,
      stage,
      measurements,
      %{backend: state.backend, card: state.card}
    )

    {[], state}
  end

  @impl true
  def handle_start_of_stream(:input, _ctx, state) do
    {[demand: :input], state}
  end

  @impl true
  def handle_buffer(
        :input,
        %Buffer{pts: pts, metadata: %{drm_prime: desc}} = buffer,
        _ctx,
        %{ignore_pts: true} = state
      ) do
    :erlang.garbage_collect(self())
    trace = trace_from_buffer(buffer) || trace_from_desc(desc)
    trace = emit_sink_stage(trace, :sink_input, %{backend: :prime, ignore_pts: true})

    result =
      Instrumentation.measure(
        [:nif, :drm_prime_sink, :display_prime],
        %{backend: :prime, card: state.card, ignore_pts: true},
        fn ->
          result = Native.display_prime(state.display, desc)
          {result, %{}, %{result: display_result_label(result)}}
        end
      )

    case result do
      :ok ->
        Membrane.Logger.debug("Displayed frame: #{inspect(desc)}")
        trace = emit_sink_stage(trace, :sink_submitted, %{backend: :prime, ignore_pts: true})
        {[demand: :input], %{state | last_pts: pts, last_desc: desc, last_trace: trace}}

      {:error, reason} ->
        Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_buffer(
        :input,
        %Buffer{pts: pts, metadata: %{drm_prime: desc}} = buffer,
        _ctx,
        state
      ) do
    :erlang.garbage_collect(self())
    trace = trace_from_buffer(buffer) || trace_from_desc(desc)
    trace = emit_sink_stage(trace, :sink_input, %{backend: :prime, ignore_pts: false})

    actions =
      case state do
        %{last_pts: nil, last_desc: nil} ->
          result =
            Instrumentation.measure(
              [:nif, :drm_prime_sink, :display_prime],
              %{backend: :prime, card: state.card, ignore_pts: false, path: :immediate},
              fn ->
                result = Native.display_prime(state.display, desc)
                {result, %{}, %{result: display_result_label(result)}}
              end
            )

          case result do
            :ok ->
              Membrane.Logger.debug("Displayed frame: #{inspect(desc)}")
              emit_sink_stage(trace, :sink_submitted, %{backend: :prime, path: :immediate})
              [demand: :input, start_timer: {:demand_timer, :no_interval}]

            {:error, reason} ->
              Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
              raise "Failed to display frame: #{inspect(reason)}"
          end

        %{last_pts: last_pts} ->
          emit_sink_stage(trace, :sink_buffered, %{backend: :prime, path: :timer})
          [timer_interval: {:demand_timer, pts - last_pts}]
      end

    {actions, %{state | last_pts: pts, last_desc: desc, last_trace: trace}}
  end

  @impl true
  def handle_buffer(
        :input,
        %Buffer{payload: payload, pts: pts} = buffer,
        _ctx,
        %{backend: :raw, ignore_pts: true} = state
      ) do
    payload = Membrane.Payload.to_binary(payload)
    trace = trace_from_buffer(buffer)
    trace = emit_sink_stage(trace, :sink_input, %{backend: :raw, ignore_pts: true})

    result =
      Instrumentation.measure(
        [:nif, :drm_prime_sink, :display_frame],
        %{backend: :raw, card: state.card, ignore_pts: true, payload_bytes: byte_size(payload)},
        fn ->
          result = Native.display_frame(state.display, payload)
          {result, %{}, %{result: display_result_label(result)}}
        end
      )

    case result do
      :ok ->
        trace = emit_sink_stage(trace, :sink_submitted, %{backend: :raw, ignore_pts: true})
        {[demand: :input], %{state | last_pts: pts, last_payload: payload, last_trace: trace}}

      {:error, reason} ->
        Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_buffer(
        :input,
        %Buffer{payload: payload, pts: pts} = buffer,
        _ctx,
        %{backend: :raw} = state
      ) do
    payload = Membrane.Payload.to_binary(payload)

    trace =
      emit_sink_stage(trace_from_buffer(buffer), :sink_input, %{backend: :raw, ignore_pts: false})

    actions =
      case state do
        %{last_pts: nil, last_payload: nil} ->
          result =
            Instrumentation.measure(
              [:nif, :drm_prime_sink, :display_frame],
              %{
                backend: :raw,
                card: state.card,
                ignore_pts: false,
                path: :immediate,
                payload_bytes: byte_size(payload)
              },
              fn ->
                result = Native.display_frame(state.display, payload)
                {result, %{}, %{result: display_result_label(result)}}
              end
            )

          case result do
            :ok ->
              emit_sink_stage(trace, :sink_submitted, %{backend: :raw, path: :immediate})
              [demand: :input, start_timer: {:demand_timer, :no_interval}]

            {:error, reason} ->
              Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
              raise "Failed to display frame: #{inspect(reason)}"
          end

        %{last_pts: last_pts} ->
          emit_sink_stage(trace, :sink_buffered, %{backend: :raw, path: :timer})
          [timer_interval: {:demand_timer, pts - last_pts}]
      end

    {actions, %{state | last_pts: pts, last_payload: payload, last_trace: trace}}
  end

  @impl true
  def handle_tick(:demand_timer, _ctx, %{display: nil} = state) do
    {[], state}
  end

  def handle_tick(:demand_timer, _ctx, %{backend: :prime} = state) do
    :erlang.garbage_collect(self())

    result =
      Instrumentation.measure(
        [:nif, :drm_prime_sink, :display_prime],
        %{backend: :prime, card: state.card, ignore_pts: false, path: :timer},
        fn ->
          result = Native.display_prime(state.display, state.last_desc)
          {result, %{}, %{result: display_result_label(result)}}
        end
      )

    case result do
      :ok ->
        Membrane.Logger.debug("Displayed frame: #{inspect(state.last_desc)}")
        emit_sink_stage(state.last_trace, :sink_submitted, %{backend: :prime, path: :timer})

        {[timer_interval: {:demand_timer, :no_interval}, demand: :input],
         %{state | last_desc: nil, last_trace: nil}}

      {:error, reason} ->
        Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  def handle_tick(:demand_timer, _ctx, %{backend: :raw} = state) do
    result =
      Instrumentation.measure(
        [:nif, :drm_prime_sink, :display_frame],
        %{
          backend: :raw,
          card: state.card,
          ignore_pts: false,
          path: :timer,
          payload_bytes: byte_size(state.last_payload)
        },
        fn ->
          result = Native.display_frame(state.display, state.last_payload)
          {result, %{}, %{result: display_result_label(result)}}
        end
      )

    case result do
      :ok ->
        emit_sink_stage(state.last_trace, :sink_submitted, %{backend: :raw, path: :timer})

        {[timer_interval: {:demand_timer, :no_interval}, demand: :input],
         %{state | last_payload: nil, last_trace: nil}}

      {:error, reason} ->
        Membrane.Logger.error("Failed to display frame: #{inspect(reason)}")
        raise "Failed to display frame: #{inspect(reason)}"
    end
  end

  @impl true
  def handle_end_of_stream(:input, _ctx, %{display: display} = state) do
    if display do
      case close_display(display, state) do
        :ok -> :ok
        {:error, reason} -> Membrane.Logger.error("Failed to close display: #{inspect(reason)}")
      end
    end

    actions =
      if state.ignore_pts,
        do: [],
        else: [stop_timer: :demand_timer]

    :erlang.garbage_collect(self())

    {actions,
     %{
       state
       | display: nil,
         backend: nil,
         last_pts: nil,
         last_desc: nil,
         last_payload: nil,
         last_trace: nil,
         raw_stream_format: nil
     }}
  end

  @impl true
  def handle_terminate_request(_ctx, %{display: display} = state) do
    if display do
      case close_display(display, state) do
        :ok -> :ok
        {:error, reason} -> Membrane.Logger.warning("Failed to close display: #{inspect(reason)}")
      end
    end

    {[terminate: :normal],
     %{
       state
       | display: nil,
         backend: nil,
         last_pts: nil,
         last_desc: nil,
         last_payload: nil,
         last_trace: nil,
         raw_stream_format: nil
     }}
  end

  defp stream_format_mode(%PrimeFormat{width: width, height: height, framerate: framerate}) do
    case framerate_to_hz(framerate) do
      nil -> nil
      hz -> {width, height, hz}
    end
  end

  defp raw_stream_format_mode(%RawVideo{width: width, height: height, framerate: framerate}) do
    case framerate_to_hz(framerate) do
      nil -> nil
      hz -> {width, height, hz}
    end
  end

  defp raw_stream_signature(%RawVideo{} = format) do
    {format.pixel_format, format.width, format.height, format.framerate}
  end

  defp framerate_to_hz({num, den})
       when is_integer(num) and is_integer(den) and num > 0 and den > 0 do
    div(num + div(den, 2), den)
  end

  defp framerate_to_hz(_framerate), do: nil

  defp log_display_info(%DisplayInfo{} = info) do
    {w, h, r} = info.mode

    Membrane.Logger.info(
      "Using card #{info.card_path}, connector #{info.connector_id} (#{info.connector_type}), " <>
        "plane #{info.plane_id}, mode #{w}x#{h}@#{r}"
    )
  end

  defp trace_from_buffer(%Buffer{} = buffer), do: FrameTrace.fetch(buffer)

  defp trace_from_desc(desc) do
    case desc do
      %{trace_token: %TraceToken{} = token} ->
        if Instrumentation.frame_metrics_enabled?() and token.sampled do
          FrameTrace.derive(nil,
            trace_id: token.trace_id,
            frame_id: token.frame_id,
            created_at_ns: token.created_at_ns,
            sampled?: token.sampled,
            pts: token.pts
          )
        end

      _other ->
        nil
    end
  end

  defp emit_sink_stage(trace, stage, metadata) do
    trace = Instrumentation.mark_trace(trace, stage, metadata)
    Instrumentation.emit_frame_stage(:prime_sink, trace, stage, %{}, metadata)
    trace
  end

  defp display_result_label(:ok), do: :ok
  defp display_result_label({:error, _reason}), do: :error
  defp display_result_label(_other), do: :other

  defp close_display(display, state) do
    Instrumentation.measure(
      [:nif, :drm_prime_sink, :close_display],
      %{backend: state.backend, card: state.card},
      fn ->
        result = Native.close_display(display)
        {result, %{}, %{result: display_result_label(result)}}
      end
    )
  end

  defp nif_result_label({:ok, _info, _display}), do: :ok
  defp nif_result_label({:error, _reason}), do: :error
  defp nif_result_label(_other), do: :other
end
