defmodule Membrane.Instrumentation.MetricCollector do
  @moduledoc false

  use GenServer

  alias Membrane.Instrumentation.WindowedStats

  defstruct [:kind, :windows_ms, :resolution_ms, metrics: %{}]

  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts) do
    GenServer.start_link(__MODULE__, opts)
  end

  @spec snapshot(pid(), integer()) :: map()
  def snapshot(pid, now_ms) do
    GenServer.call(pid, {:snapshot, now_ms})
  end

  @impl true
  def init(opts) do
    {:ok,
     %__MODULE__{
       kind: Keyword.fetch!(opts, :kind),
       windows_ms: Keyword.fetch!(opts, :windows_ms),
       resolution_ms: Keyword.fetch!(opts, :resolution_ms),
       metrics: %{}
     }}
  end

  @impl true
  def handle_call({:snapshot, now_ms}, _from, state) do
    snapshot =
      Enum.into(state.metrics, %{}, fn {key, stats} ->
        {key, WindowedStats.snapshot(stats, now_ms)}
      end)

    {:reply, snapshot, state}
  end

  @impl true
  def handle_info({:record, metric_key, timestamp_ms, value}, state) do
    stats =
      Map.get_lazy(state.metrics, metric_key, fn ->
        WindowedStats.new(state.kind, state.windows_ms, state.resolution_ms)
      end)

    {:noreply,
     %{
       state
       | metrics:
           Map.put(state.metrics, metric_key, WindowedStats.record(stats, timestamp_ms, value))
     }}
  end
end
