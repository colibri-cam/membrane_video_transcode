defmodule Membrane.Instrumentation.SessionConfig do
  @moduledoc false

  @default_windows [:timer.seconds(1), :timer.seconds(5), :timer.seconds(30)]
  @default_resolution 250
  @default_snapshot_interval :timer.seconds(1)
  @default_shard_count 4

  @spec normalize(keyword(), keyword()) :: map()
  def normalize(opts, defaults) do
    name = Keyword.fetch!(opts, :name)

    {pipeline_prefix, pipeline_pid} =
      normalize_pipeline(Keyword.get(opts, :pipeline), Keyword.get(opts, :path_prefix))

    windows_ms =
      normalize_windows(
        Keyword.get(opts, :average_windows, defaults[:average_windows] || @default_windows)
      )

    resolution_ms =
      normalize_positive(
        Keyword.get(
          opts,
          :bucket_resolution,
          defaults[:bucket_resolution] || @default_resolution
        ),
        :bucket_resolution
      )

    snapshot_interval_ms =
      normalize_positive(
        Keyword.get(
          opts,
          :snapshot_interval,
          defaults[:snapshot_interval] || @default_snapshot_interval
        ),
        :snapshot_interval
      )

    shard_count =
      normalize_positive(
        Keyword.get(opts, :callback_shards, defaults[:callback_shards] || @default_shard_count),
        :callback_shards
      )

    if resolution_ms > Enum.min(windows_ms) do
      raise ArgumentError, "bucket_resolution must be <= smallest average window"
    end

    %{
      name: name,
      pipeline_prefix: pipeline_prefix,
      pipeline_pid: pipeline_pid,
      average_windows_ms: windows_ms,
      bucket_resolution_ms: resolution_ms,
      snapshot_interval_ms: snapshot_interval_ms,
      callback_shards: shard_count,
      snapshot_file: Keyword.get(opts, :snapshot_file, defaults[:snapshot_file]),
      callback_metrics:
        normalize_callback_metrics(
          Keyword.get(opts, :callback_metrics, defaults[:callback_metrics] || [])
        ),
      nif_metrics:
        normalize_simple_metrics(
          Keyword.get(opts, :nif_metrics, defaults[:nif_metrics] || []),
          :metrics
        ),
      frame_metrics:
        normalize_simple_metrics(
          Keyword.get(opts, :frame_metrics, defaults[:frame_metrics] || []),
          :stages
        )
    }
  end

  @spec normalize_component(term()) :: String.t()
  def normalize_component(component) when is_atom(component), do: inspect(component)
  def normalize_component(component) when is_binary(component), do: component
  def normalize_component(component), do: inspect(component)

  defp normalize_callback_metrics(metrics) do
    Enum.map(metrics, fn metric ->
      component = Keyword.fetch!(metric, :component)
      callbacks = List.wrap(Keyword.fetch!(metric, :callbacks))

      %{
        label: component,
        component_key: normalize_component(component),
        callbacks: callbacks,
        sample_rate: normalize_sample_rate(Keyword.get(metric, :sample_rate, 1))
      }
    end)
  end

  defp normalize_simple_metrics(metrics, field) do
    Enum.map(metrics, fn metric ->
      component = Keyword.fetch!(metric, :component)
      values = List.wrap(Keyword.get(metric, field) || Keyword.fetch!(metric, singular(field)))

      %{
        label: component,
        component_key: normalize_component(component),
        values: values,
        sample_rate: normalize_sample_rate(Keyword.get(metric, :sample_rate, 1))
      }
    end)
  end

  defp singular(:metrics), do: :metric
  defp singular(:stages), do: :stage

  defp normalize_windows(windows) do
    windows
    |> List.wrap()
    |> Enum.map(&normalize_positive(&1, :average_windows))
    |> Enum.uniq()
    |> Enum.sort()
    |> case do
      [] -> raise ArgumentError, "average_windows must not be empty"
      values -> values
    end
  end

  defp normalize_positive(value, _field) when is_integer(value) and value > 0, do: value

  defp normalize_positive(value, field) do
    raise ArgumentError, "#{field} must be a positive integer, got: #{inspect(value)}"
  end

  defp normalize_sample_rate(value) when is_integer(value) and value > 0, do: value

  defp normalize_sample_rate(value) do
    raise ArgumentError, "sample_rate must be a positive integer, got: #{inspect(value)}"
  end

  defp normalize_pipeline(nil, nil), do: {nil, nil}

  defp normalize_pipeline(pid, _path_prefix) when is_pid(pid) do
    {[pid_segment(pid)], pid}
  end

  defp normalize_pipeline(nil, path_prefix) do
    {normalize_pipeline_prefix(path_prefix), nil}
  end

  defp normalize_pipeline(value, _path_prefix) do
    {[container_segment(value)], nil}
  end

  defp normalize_pipeline_prefix(prefix) when is_list(prefix) and prefix != [] do
    Enum.map(prefix, &normalize_path_segment/1)
  end

  defp normalize_pipeline_prefix(pid) when is_pid(pid) do
    [pid_segment(pid)]
  end

  defp normalize_pipeline_prefix(value) do
    [container_segment(value)]
  end

  defp normalize_path_segment(segment) when is_binary(segment), do: segment
  defp normalize_path_segment(segment), do: container_segment(segment)

  defp pid_segment(pid) do
    pid
    |> :erlang.pid_to_list()
    |> to_string()
    |> Kernel.<>("/")
  end

  defp container_segment(value) when is_binary(value) do
    if String.ends_with?(value, "/"), do: value, else: value <> "/"
  end

  defp container_segment(value) when is_atom(value) do
    inspect(value) <> "/"
  end

  defp container_segment(value) do
    inspect(value) <> "/"
  end
end
