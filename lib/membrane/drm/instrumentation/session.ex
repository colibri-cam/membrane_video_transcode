defmodule Membrane.DRM.Instrumentation.Session do
  @moduledoc false

  use GenServer
  require Logger

  alias Membrane.DRM.Instrumentation.MetricCollector

  @snapshot_table :membrane_drm_instrumentation_snapshots

  defstruct [
    :name,
    :config,
    :callback_shards,
    :nif_collector,
    :frame_collector,
    :current_snapshot,
    :snapshot_timer
  ]

  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts) do
    GenServer.start_link(__MODULE__, opts)
  end

  @spec route_info(pid()) :: map()
  def route_info(pid), do: GenServer.call(pid, :route_info)

  @spec update_config(pid(), map()) :: :ok
  def update_config(pid, config), do: GenServer.call(pid, {:update_config, config})

  @spec snapshot(pid()) :: map()
  def snapshot(pid), do: GenServer.call(pid, :snapshot)

  @impl true
  def init(opts) do
    config = Keyword.fetch!(opts, :config)
    Process.flag(:trap_exit, true)
    state = build_state(config) |> publish_snapshot() |> schedule_snapshot()
    {:ok, state}
  end

  @impl true
  def handle_call(:route_info, _from, state) do
    {:reply, build_route_info(state), state}
  end

  def handle_call(:snapshot, _from, state) do
    snapshot = refresh_snapshot(state)
    {:reply, snapshot.current_snapshot, snapshot}
  end

  def handle_call({:update_config, config}, _from, state) do
    cancel_snapshot(state.snapshot_timer)
    teardown_collectors(state)
    next_state = build_state(config) |> publish_snapshot() |> schedule_snapshot()
    {:reply, :ok, next_state}
  end

  @impl true
  def handle_info(:snapshot_tick, state) do
    next_state = refresh_snapshot(state) |> schedule_snapshot()
    {:noreply, next_state}
  end

  def handle_info({:EXIT, _pid, _reason}, state) do
    {:noreply, state}
  end

  @impl true
  def terminate(_reason, state) do
    cancel_snapshot(state.snapshot_timer)
    teardown_collectors(state)

    if :ets.whereis(@snapshot_table) != :undefined do
      :ets.delete(@snapshot_table, state.name)
    end

    :ok
  end

  defp build_state(config) do
    callback_shards = start_callback_shards(config)
    nif_collector = start_optional_collector(config.nif_metrics, config)
    frame_collector = start_optional_collector(config.frame_metrics, config)

    %__MODULE__{
      name: config.name,
      config: config,
      callback_shards: callback_shards,
      nif_collector: nif_collector,
      frame_collector: frame_collector,
      current_snapshot: empty_snapshot(config)
    }
  end

  defp start_callback_shards(config) do
    for _index <- 1..config.callback_shards do
      {:ok, pid} =
        MetricCollector.start_link(
          kind: :timer,
          windows_ms: config.average_windows_ms,
          resolution_ms: config.bucket_resolution_ms
        )

      Process.link(pid)
      pid
    end
  end

  defp start_optional_collector([], _config), do: nil

  defp start_optional_collector(_metrics, config) do
    {:ok, pid} =
      MetricCollector.start_link(
        kind: :timer,
        windows_ms: config.average_windows_ms,
        resolution_ms: config.bucket_resolution_ms
      )

    Process.link(pid)
    pid
  end

  defp build_route_info(state) do
    %{
      callbacks: build_callback_routes(state.config, state.callback_shards),
      nifs:
        build_simple_routes(
          state.config.nif_metrics,
          state.config.pipeline_prefix,
          state.nif_collector
        ),
      frames:
        build_simple_routes(
          state.config.frame_metrics,
          state.config.pipeline_prefix,
          state.frame_collector
        )
    }
  end

  defp build_callback_routes(config, shards) do
    shard_count = length(shards)

    for metric <- config.callback_metrics,
        callback <- metric.callbacks do
      collector = Enum.at(shards, :erlang.phash2({metric.component_key, callback}, shard_count))

      %{
        component_key: metric.component_key,
        event_key: callback,
        metric_key: {metric.label, callback},
        path_prefix: config.pipeline_prefix,
        sample_rate: metric.sample_rate,
        collector: collector
      }
    end
  end

  defp build_simple_routes([], _path_prefix, _collector), do: []

  defp build_simple_routes(metrics, path_prefix, collector) do
    for metric <- metrics,
        value <- metric.values do
      %{
        component_key: metric.component_key,
        event_key: value,
        metric_key: {metric.label, value},
        path_prefix: path_prefix,
        sample_rate: metric.sample_rate,
        collector: collector
      }
    end
  end

  defp refresh_snapshot(state) do
    publish_snapshot(state)
  end

  defp publish_snapshot(state) do
    now_ms = System.monotonic_time(:millisecond)

    snapshot = %{
      session: state.name,
      generated_at_ms: now_ms,
      windows_ms: state.config.average_windows_ms,
      callback_metrics: collect_snapshots(state.callback_shards, now_ms),
      nif_metrics: collect_snapshot(state.nif_collector, now_ms),
      frame_metrics: collect_snapshot(state.frame_collector, now_ms)
    }

    :ets.insert(@snapshot_table, {state.name, snapshot})
    maybe_write_snapshot(state.config.snapshot_file, snapshot)
    %{state | current_snapshot: snapshot}
  end

  defp collect_snapshots(shards, now_ms) do
    Enum.reduce(shards, %{}, fn shard, acc ->
      Map.merge(acc, MetricCollector.snapshot(shard, now_ms))
    end)
  end

  defp collect_snapshot(nil, _now_ms), do: %{}
  defp collect_snapshot(pid, now_ms), do: MetricCollector.snapshot(pid, now_ms)

  defp empty_snapshot(config) do
    %{
      session: config.name,
      generated_at_ms: System.monotonic_time(:millisecond),
      windows_ms: config.average_windows_ms,
      callback_metrics: %{},
      nif_metrics: %{},
      frame_metrics: %{}
    }
  end

  defp maybe_write_snapshot(nil, _snapshot), do: :ok
  defp maybe_write_snapshot("", _snapshot), do: :ok

  defp maybe_write_snapshot(path, snapshot) do
    formatted = format_snapshot(snapshot)
    dir = Path.dirname(path)
    tmp_path = path <> ".tmp"

    with :ok <- File.mkdir_p(dir),
         :ok <- File.write(tmp_path, formatted <> "\n"),
         :ok <- File.rename(tmp_path, path) do
      :ok
    else
      {:error, reason} ->
        Logger.warning("Failed to write instrumentation snapshot to #{path}: #{inspect(reason)}")
        :ok
    end
  end

  defp format_snapshot(snapshot) do
    [
      "Membrane DRM instrumentation snapshot",
      "Session #{inspect(snapshot.session)}",
      format_metric_section("Callbacks", snapshot.callback_metrics),
      format_metric_section("NIFs", snapshot.nif_metrics),
      format_metric_section("Frames", snapshot.frame_metrics)
    ]
    |> Enum.reject(&(&1 in [nil, ""]))
    |> Enum.join("\n")
  end

  defp format_metric_section(_title, metrics) when map_size(metrics) == 0, do: nil

  defp format_metric_section(title, metrics) do
    lines =
      metrics
      |> Enum.sort_by(fn {key, _value} -> inspect(key) end)
      |> Enum.map(fn {key, windows} ->
        summaries =
          windows
          |> Enum.sort_by(fn {window_ms, _summary} -> window_ms end)
          |> Enum.map_join(" ", fn {window_ms, summary} ->
            values =
              summary
              |> Enum.reject(fn {_metric, value} -> is_nil(value) end)
              |> Enum.map_join(
                ",",
                fn {metric, value} ->
                  formatted_value = if is_float(value), do: Float.round(value, 3), else: value
                  "#{metric}=#{formatted_value}"
                end
              )

            "[#{window_ms}ms #{values}]"
          end)

        "- #{inspect(key)} #{summaries}"
      end)

    Enum.join([title | lines], "\n")
  end

  defp teardown_collectors(state) do
    Enum.each(state.callback_shards, &Process.exit(&1, :shutdown))
    if state.nif_collector, do: Process.exit(state.nif_collector, :shutdown)
    if state.frame_collector, do: Process.exit(state.frame_collector, :shutdown)
  end

  defp schedule_snapshot(state) do
    cancel_snapshot(state.snapshot_timer)

    ref =
      if is_integer(state.config.snapshot_interval_ms) and state.config.snapshot_interval_ms > 0 do
        Process.send_after(self(), :snapshot_tick, state.config.snapshot_interval_ms)
      end

    %{state | snapshot_timer: ref}
  end

  defp cancel_snapshot(nil), do: :ok

  defp cancel_snapshot(ref) do
    Process.cancel_timer(ref)
    :ok
  end
end
