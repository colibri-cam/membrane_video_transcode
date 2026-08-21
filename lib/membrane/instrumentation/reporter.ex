defmodule Membrane.Instrumentation.Reporter do
  @moduledoc """
  Attachable Telemetry reporter for Membrane DRM experiments.
  """

  use GenServer
  require Logger

  alias Membrane.Instrumentation

  @native_unit :native

  defstruct handler_id: nil,
            interval_ms: 1_000,
            print_reports: false,
            snapshot_file: nil,
            callback_stats: %{},
            nif_stats: %{},
            frame_stats: %{},
            queue_stats: %{}

  @type stats :: %{
          count: non_neg_integer(),
          total_ns: non_neg_integer(),
          max_ns: non_neg_integer(),
          last_ns: non_neg_integer()
        }

  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts \\ []) do
    GenServer.start_link(__MODULE__, opts, Keyword.take(opts, [:name]))
  end

  @spec snapshot(pid() | GenServer.name()) :: map()
  def snapshot(server), do: GenServer.call(server, :snapshot)

  @spec stop(pid() | GenServer.name()) :: :ok
  def stop(server), do: GenServer.stop(server)

  @impl true
  def init(opts) do
    config = Instrumentation.config()
    handler_id = Keyword.get_lazy(opts, :handler_id, &default_handler_id/0)

    :ok = Instrumentation.attach_default(handler_id, &__MODULE__.handle_event/4, self())

    state = %__MODULE__{
      handler_id: handler_id,
      interval_ms: Keyword.get(opts, :interval_ms, config[:reporter_interval_ms] || 1_000),
      print_reports: Keyword.get(opts, :print_reports, config[:print_reports] || false),
      snapshot_file: Keyword.get(opts, :snapshot_file, config[:snapshot_file])
    }

    schedule_report(state)
    {:ok, state}
  end

  @impl true
  def handle_call(:snapshot, _from, state) do
    {:reply, snapshot_from_state(state), state}
  end

  @impl true
  def handle_cast({:telemetry, event, measurements, metadata}, state) do
    {:noreply, reduce_event(state, event, measurements, metadata)}
  end

  @impl true
  def handle_info(:report, state) do
    snapshot = snapshot_from_state(state)
    formatted = format_snapshot(snapshot)

    if state.print_reports do
      IO.puts(formatted)
    end

    maybe_write_snapshot(state.snapshot_file, formatted)

    schedule_report(state)
    {:noreply, state}
  end

  @impl true
  def terminate(_reason, state) do
    Instrumentation.detach(state.handler_id)
    :ok
  end

  @spec handle_event([atom()], map(), map(), pid()) :: :ok
  def handle_event(event, measurements, metadata, pid) do
    GenServer.cast(pid, {:telemetry, event, measurements, metadata})
  end

  @spec format_snapshot(map()) :: String.t()
  def format_snapshot(snapshot) do
    [
      "Membrane DRM instrumentation snapshot",
      format_section("Callbacks", snapshot.callback_stats),
      format_section("NIFs", snapshot.nif_stats),
      format_frame_section(snapshot.frame_stats),
      format_queue_section(snapshot.queue_stats)
    ]
    |> Enum.reject(&(&1 in [nil, ""]))
    |> Enum.join("\n")
  end

  defp reduce_event(state, [:membrane, :element, callback, :stop], measurements, metadata) do
    name = component_key(metadata, callback)
    duration_ns = System.convert_time_unit(measurements.duration, @native_unit, :nanosecond)
    update_in(state.callback_stats, &update_stats(&1, name, duration_ns))
  end

  defp reduce_event(
         state,
         [:membrane_video_transcode, :nif, component, action, :stop],
         measurements,
         metadata
       ) do
    name = {component, action, metadata[:result]}
    update_in(state.nif_stats, &update_stats(&1, name, measurements.duration_ns))
  end

  defp reduce_event(state, [:membrane_video_transcode, :frame, :stage], measurements, metadata) do
    key = {metadata.component, metadata.stage}

    update_in(state.frame_stats, fn stats_map ->
      stats = Map.get(stats_map, key, %{count: 0, max_age_ns: 0, last_age_ns: 0})

      Map.put(stats_map, key, %{
        stats
        | count: stats.count + 1,
          max_age_ns: max(stats.max_age_ns, measurements.age_ns),
          last_age_ns: measurements.age_ns
      })
    end)
  end

  defp reduce_event(state, [:membrane, :datapoint, datapoint], measurements, metadata)
       when datapoint in [:queue_len, :store, :take, :buffer, :stream_format] do
    key = {datapoint, metadata[:component_path]}
    value = Map.get(measurements, :value, measurements)

    update_in(state.queue_stats, fn stats_map ->
      stats = Map.get(stats_map, key, %{count: 0, last: 0, max: 0})

      Map.put(stats_map, key, %{
        stats
        | count: stats.count + 1,
          last: value,
          max: max(stats.max, extract_numeric(value))
      })
    end)
  end

  defp reduce_event(state, _event, _measurements, _metadata), do: state

  defp update_stats(stats_map, key, duration_ns) do
    stats = Map.get(stats_map, key, %{count: 0, total_ns: 0, max_ns: 0, last_ns: 0})

    Map.put(stats_map, key, %{
      stats
      | count: stats.count + 1,
        total_ns: stats.total_ns + duration_ns,
        max_ns: max(stats.max_ns, duration_ns),
        last_ns: duration_ns
    })
  end

  defp extract_numeric(value) when is_integer(value), do: value
  defp extract_numeric(%{len: len}) when is_integer(len), do: len
  defp extract_numeric(%{size: size}) when is_integer(size), do: size
  defp extract_numeric(_value), do: 0

  defp snapshot_from_state(state) do
    %{
      callback_stats: state.callback_stats,
      nif_stats: state.nif_stats,
      frame_stats: state.frame_stats,
      queue_stats: state.queue_stats
    }
  end

  defp component_key(metadata, callback) do
    {metadata[:component_path], callback}
  end

  defp format_section(_title, stats) when map_size(stats) == 0, do: nil

  defp format_section(title, stats) do
    lines =
      stats
      |> Enum.sort()
      |> Enum.map(fn {key, %{count: count, total_ns: total_ns, max_ns: max_ns, last_ns: last_ns}} ->
        avg_ns = if count == 0, do: 0, else: div(total_ns, count)

        "- #{inspect(key)} count=#{count} avg_ms=#{ns_to_ms(avg_ns)} max_ms=#{ns_to_ms(max_ns)} last_ms=#{ns_to_ms(last_ns)}"
      end)

    Enum.join([title | lines], "\n")
  end

  defp format_frame_section(stats) when map_size(stats) == 0, do: nil

  defp format_frame_section(stats) do
    lines =
      stats
      |> Enum.sort()
      |> Enum.map(fn {key, %{count: count, max_age_ns: max_age_ns, last_age_ns: last_age_ns}} ->
        "- #{inspect(key)} count=#{count} max_age_ms=#{ns_to_ms(max_age_ns)} last_age_ms=#{ns_to_ms(last_age_ns)}"
      end)

    Enum.join(["Frames" | lines], "\n")
  end

  defp format_queue_section(stats) when map_size(stats) == 0, do: nil

  defp format_queue_section(stats) do
    lines =
      stats
      |> Enum.sort()
      |> Enum.map(fn {key, %{count: count, last: last, max: max}} ->
        "- #{inspect(key)} count=#{count} last=#{inspect(last)} max=#{inspect(max)}"
      end)

    Enum.join(["Queues" | lines], "\n")
  end

  defp ns_to_ms(duration_ns) do
    duration_ns
    |> Kernel./(1_000_000)
    |> Float.round(3)
  end

  defp default_handler_id do
    "membrane-drm-reporter-#{System.unique_integer([:positive, :monotonic])}"
  end

  defp maybe_write_snapshot(nil, _snapshot), do: :ok
  defp maybe_write_snapshot("", _snapshot), do: :ok

  defp maybe_write_snapshot(path, snapshot) do
    dir = Path.dirname(path)
    tmp_path = path <> ".tmp"

    with :ok <- File.mkdir_p(dir),
         :ok <- File.write(tmp_path, snapshot <> "\n"),
         :ok <- File.rename(tmp_path, path) do
      :ok
    else
      {:error, reason} ->
        Logger.warning("Failed to write instrumentation snapshot to #{path}: #{inspect(reason)}")
        :ok
    end
  end

  defp schedule_report(%__MODULE__{interval_ms: interval_ms}) when interval_ms > 0 do
    Process.send_after(self(), :report, interval_ms)
  end

  defp schedule_report(_state), do: :ok
end
