defmodule Membrane.Instrumentation.Router do
  @moduledoc false

  @routing_key {__MODULE__, :routes}

  @spec put_routes(map()) :: :ok
  def put_routes(routes) do
    :persistent_term.put(@routing_key, routes)
  end

  @spec routes() :: map()
  def routes do
    :persistent_term.get(@routing_key, %{
      active_sessions: 0,
      custom_metrics?: false,
      frame_metrics?: false,
      callbacks: %{},
      nifs: %{},
      frames: %{}
    })
  end

  @spec handle_event([atom()], map(), map(), term()) :: :ok
  def handle_event([:membrane, :element, callback, :stop], measurements, metadata, _config) do
    component_path = metadata[:component_path] || []
    component_key = List.last(component_path)

    duration_ns = System.convert_time_unit(measurements.duration, :native, :nanosecond)
    dispatch(routes().callbacks, {callback, component_key}, component_path, duration_ns)
  end

  def handle_event([:membrane_drm, :nif, _domain, metric, :stop], measurements, metadata, _config) do
    component_path = metadata[:component_path] || []
    component_key = List.last(component_path)
    dispatch(routes().nifs, {metric, component_key}, component_path, measurements.duration_ns)
  end

  def handle_event([:membrane_drm, :frame, :stage], measurements, metadata, _config) do
    component_path = metadata[:component_path] || []
    component_key = List.last(component_path)

    dispatch(
      routes().frames,
      {metadata.stage, component_key},
      component_path,
      measurements.age_ns
    )
  end

  def handle_event(_event, _measurements, _metadata, _config), do: :ok

  defp dispatch(route_index, key, component_path, value) do
    timestamp_ms = System.monotonic_time(:millisecond)

    route_index
    |> Map.get(key, [])
    |> Enum.each(fn route ->
      if match_route?(route, component_path) and sampled?(route.sample_rate) do
        send(route.collector, {:record, route.metric_key, timestamp_ms, value})
      end
    end)
  end

  defp match_route?(%{path_prefix: nil}, _component_path), do: true

  defp match_route?(%{path_prefix: prefix}, component_path),
    do: List.starts_with?(component_path, prefix)

  defp sampled?(1), do: true

  defp sampled?(sample_rate),
    do: rem(System.unique_integer([:positive, :monotonic]), sample_rate) == 0
end
