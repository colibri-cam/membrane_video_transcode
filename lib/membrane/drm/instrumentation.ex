defmodule Membrane.DRM.Instrumentation do
  @moduledoc """
  Runtime-configurable instrumentation for Membrane DRM pipelines.
  """

  alias Membrane.ComponentPath
  alias Membrane.DRM.Instrumentation.FrameTrace
  alias Membrane.DRM.Instrumentation.Manager
  alias Membrane.DRM.Instrumentation.Reporter
  alias Membrane.DRM.Instrumentation.Router
  alias Membrane.DRM.Instrumentation.Supervisor
  alias Membrane.DRM.Instrumentation.TraceToken

  @app :membrane_drm_sink
  @event_prefix [:membrane_drm]

  @default_events [
    [:membrane, :element, :handle_setup, :stop],
    [:membrane, :element, :handle_start_of_stream, :stop],
    [:membrane, :element, :handle_stream_format, :stop],
    [:membrane, :element, :handle_buffer, :stop],
    [:membrane, :element, :handle_tick, :stop],
    [:membrane, :element, :handle_info, :stop],
    [:membrane, :element, :handle_end_of_stream, :stop],
    [:membrane, :element, :handle_terminate_request, :stop],
    [:membrane_drm, :nif, :h265_prime_decoder, :decode, :stop],
    [:membrane_drm, :nif, :h265_prime_decoder, :flush, :stop],
    [:membrane_drm, :nif, :drm_prime_sink, :init_display, :stop],
    [:membrane_drm, :nif, :drm_prime_sink, :init_raw_display, :stop],
    [:membrane_drm, :nif, :drm_prime_sink, :display_prime, :stop],
    [:membrane_drm, :nif, :drm_prime_sink, :display_frame, :stop],
    [:membrane_drm, :nif, :drm_prime_sink, :close_display, :stop],
    [:membrane_drm, :frame, :stage]
  ]

  @type config :: keyword()

  @spec config() :: config()
  def config do
    Application.get_env(@app, __MODULE__, [])
  end

  @spec ensure_started() :: :ok
  def ensure_started do
    case Process.whereis(Supervisor) do
      nil ->
        case Supervisor.start_link() do
          {:ok, pid} ->
            Process.unlink(pid)
            :ok

          {:error, {:already_started, pid}} ->
            Process.unlink(pid)
            :ok

          other ->
            raise "Failed to start instrumentation supervisor: #{inspect(other)}"
        end

      _pid ->
        :ok
    end
  end

  @spec enabled?() :: boolean()
  def enabled? do
    Manager.runtime_state().active_sessions > 0
  end

  @spec custom_metrics_enabled?() :: boolean()
  def custom_metrics_enabled? do
    Manager.runtime_state().custom_metrics?
  end

  @spec frame_metrics_enabled?() :: boolean()
  def frame_metrics_enabled? do
    Manager.runtime_state().frame_metrics?
  end

  @spec start_session(keyword()) :: {:ok, term()} | {:error, term()}
  def start_session(opts) do
    ensure_started()
    Manager.start_session(opts)
  end

  @spec update_session(term(), keyword()) :: :ok | {:error, term()}
  def update_session(name, opts) do
    ensure_started()
    Manager.update_session(name, opts)
  end

  @spec stop_session(term()) :: :ok
  def stop_session(name) do
    ensure_started()
    Manager.stop_session(name)
  end

  @spec list_sessions() :: [term()]
  def list_sessions do
    ensure_started()
    Manager.list_sessions()
  end

  @spec snapshot(term()) :: {:ok, map()} | {:error, :not_found}
  def snapshot(name) do
    ensure_started()
    Manager.snapshot(name)
  end

  @spec attach_default(term(), :telemetry.handler_function(), term()) ::
          :ok | {:error, :already_exists}
  def attach_default(handler_id, fun, config \\ nil) do
    :telemetry.attach_many(handler_id, @default_events, fun, config)
  end

  @spec detach(term()) :: :ok | {:error, :not_found}
  def detach(handler_id), do: :telemetry.detach(handler_id)

  @spec default_events() :: [[atom()]]
  def default_events, do: @default_events

  @spec start_reporter(keyword()) :: GenServer.on_start()
  def start_reporter(opts \\ []), do: Reporter.start_link(opts)

  @spec measure([atom()], map(), (-> {term(), map(), map()} | term())) :: term()
  def measure(event_suffix, metadata, fun) do
    if custom_metrics_enabled?() do
      started_at_ns = System.monotonic_time(:nanosecond)
      metadata = add_component_context(metadata)

      case fun.() do
        {result, measurements, stop_metadata}
        when is_map(measurements) and is_map(stop_metadata) ->
          emit(
            event_suffix ++ [:stop],
            Map.put(
              measurements,
              :duration_ns,
              System.monotonic_time(:nanosecond) - started_at_ns
            ),
            Map.merge(metadata, stop_metadata)
          )

          result

        result ->
          emit(
            event_suffix ++ [:stop],
            %{duration_ns: System.monotonic_time(:nanosecond) - started_at_ns},
            metadata
          )

          result
      end
    else
      case fun.() do
        {result, _measurements, _metadata} -> result
        result -> result
      end
    end
  end

  @spec sampled_trace(keyword()) :: FrameTrace.t() | nil
  def sampled_trace(opts \\ []) do
    if frame_metrics_enabled?() and sample?() do
      FrameTrace.new(Keyword.put_new(opts, :sampled?, true))
    end
  end

  @spec derive_trace(FrameTrace.t() | nil, keyword()) :: FrameTrace.t() | nil
  def derive_trace(parent_trace, opts \\ []) do
    if frame_metrics_enabled?() and (parent_trace != nil or sample?()) do
      FrameTrace.derive(parent_trace, Keyword.put_new(opts, :sampled?, true))
    end
  end

  @spec mark_trace(FrameTrace.t() | nil, atom(), map()) :: FrameTrace.t() | nil
  def mark_trace(trace, stage, attrs \\ %{})

  def mark_trace(nil, _stage, _attrs), do: nil

  def mark_trace(%FrameTrace{} = trace, stage, attrs) do
    if frame_metrics_enabled?() do
      FrameTrace.mark(trace, stage, attrs)
    else
      trace
    end
  end

  @spec emit_frame_stage(atom(), FrameTrace.t() | nil, atom(), map(), map()) :: :ok
  def emit_frame_stage(component, trace, stage, measurements \\ %{}, metadata \\ %{})

  def emit_frame_stage(_component, nil, _stage, _measurements, _metadata), do: :ok

  def emit_frame_stage(component, %FrameTrace{} = trace, stage, measurements, metadata) do
    if frame_metrics_enabled?() do
      emit(
        [:frame, :stage],
        Map.merge(
          %{age_ns: FrameTrace.age_ns(trace), at_ns: System.monotonic_time(:nanosecond)},
          measurements
        ),
        Map.merge(
          add_component_context(metadata),
          %{
            component: component,
            stage: stage,
            trace_id: trace.trace_id,
            frame_id: trace.frame_id,
            pts: trace.pts,
            sampled?: trace.sampled?
          }
        )
      )
    else
      :ok
    end
  end

  @spec emit_frame_stage_from_token(atom(), TraceToken.t() | nil, atom(), map(), map()) :: :ok
  def emit_frame_stage_from_token(component, token, stage, measurements \\ %{}, metadata \\ %{})

  def emit_frame_stage_from_token(_component, nil, _stage, _measurements, _metadata), do: :ok

  def emit_frame_stage_from_token(component, %TraceToken{} = token, stage, measurements, metadata) do
    if frame_metrics_enabled?() do
      emit(
        [:frame, :stage],
        Map.merge(
          %{
            age_ns: System.monotonic_time(:nanosecond) - token.created_at_ns,
            at_ns: System.monotonic_time(:nanosecond)
          },
          measurements
        ),
        Map.merge(
          add_component_context(metadata),
          %{
            component: component,
            stage: stage,
            trace_id: token.trace_id,
            frame_id: token.frame_id,
            pts: token.pts,
            sampled?: token.sampled
          }
        )
      )
    else
      :ok
    end
  end

  @spec emit([atom()], map(), map()) :: :ok
  def emit(event_suffix, measurements, metadata) do
    if enabled?() do
      :telemetry.execute(
        @event_prefix ++ event_suffix,
        measurements,
        add_component_context(metadata)
      )
    else
      :ok
    end
  end

  @spec routes() :: map()
  def routes, do: Router.routes()

  defp sample? do
    true
  end

  defp add_component_context(metadata) do
    metadata
    |> Map.new()
    |> Map.put_new(:component_path, ComponentPath.get())
  end
end
