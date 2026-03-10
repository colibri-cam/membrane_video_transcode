defmodule Membrane.DRM.Instrumentation.WindowedStats do
  @moduledoc false

  defstruct [:kind, :resolution_ms, :windows_ms, :bucket_count, buckets: %{}]

  @type kind :: :timer | :gauge

  @type bucket :: %{
          id: non_neg_integer(),
          count: non_neg_integer(),
          sum: integer(),
          min: integer() | nil,
          max: integer() | nil,
          last: integer() | nil
        }

  @type t :: %__MODULE__{
          kind: kind(),
          resolution_ms: pos_integer(),
          windows_ms: [pos_integer()],
          bucket_count: pos_integer(),
          buckets: %{optional(non_neg_integer()) => bucket()}
        }

  @spec new(kind(), [pos_integer()], pos_integer()) :: t()
  def new(kind, windows_ms, resolution_ms) when kind in [:timer, :gauge] do
    max_window_ms = Enum.max(windows_ms)
    bucket_count = ceil(max_window_ms / resolution_ms) + 1

    %__MODULE__{
      kind: kind,
      resolution_ms: resolution_ms,
      windows_ms: windows_ms,
      bucket_count: bucket_count,
      buckets: %{}
    }
  end

  @spec record(t(), integer(), integer()) :: t()
  def record(%__MODULE__{} = stats, timestamp_ms, value) when is_integer(timestamp_ms) do
    bucket_id = div(timestamp_ms, stats.resolution_ms)
    bucket_key = Integer.mod(bucket_id, stats.bucket_count)
    bucket = fresh_bucket(stats.buckets[bucket_key], bucket_id)

    updated_bucket = %{
      bucket
      | count: bucket.count + 1,
        sum: bucket.sum + value,
        min: min_value(bucket.min, value),
        max: max_value(bucket.max, value),
        last: value
    }

    %{stats | buckets: Map.put(stats.buckets, bucket_key, updated_bucket)}
  end

  @spec snapshot(t(), integer()) :: %{optional(pos_integer()) => map()}
  def snapshot(%__MODULE__{} = stats, now_ms) when is_integer(now_ms) do
    Enum.into(stats.windows_ms, %{}, fn window_ms ->
      {window_ms, summarize_window(stats, now_ms, window_ms)}
    end)
  end

  defp summarize_window(%__MODULE__{} = stats, now_ms, window_ms) do
    min_bucket_id = div(now_ms - window_ms, stats.resolution_ms)

    stats.buckets
    |> Map.values()
    |> Enum.filter(&(&1.id >= min_bucket_id))
    |> summarize_buckets(stats.kind)
  end

  defp summarize_buckets([], :timer) do
    %{avg_ms: nil, min_ms: nil, max_ms: nil, samples: 0}
  end

  defp summarize_buckets(buckets, :timer) do
    count = Enum.reduce(buckets, 0, &(&1.count + &2))
    sum = Enum.reduce(buckets, 0, &(&1.sum + &2))
    min_value = Enum.reduce(buckets, nil, &min_value(&1.min, &2))
    max_value = Enum.reduce(buckets, nil, &max_value(&1.max, &2))

    %{
      avg_ms: to_ms(div(sum, count)),
      min_ms: to_ms(min_value),
      max_ms: to_ms(max_value),
      samples: count
    }
  end

  defp summarize_buckets([], :gauge) do
    %{avg: nil, min: nil, max: nil, last: nil, samples: 0}
  end

  defp summarize_buckets(buckets, :gauge) do
    count = Enum.reduce(buckets, 0, &(&1.count + &2))
    sum = Enum.reduce(buckets, 0, &(&1.sum + &2))
    min_value = Enum.reduce(buckets, nil, &min_value(&1.min, &2))
    max_value = Enum.reduce(buckets, nil, &max_value(&1.max, &2))
    latest_bucket = Enum.max_by(buckets, & &1.id)

    %{
      avg: if(count == 0, do: nil, else: sum / count),
      min: min_value,
      max: max_value,
      last: latest_bucket && latest_bucket.last,
      samples: count
    }
  end

  defp fresh_bucket(%{id: bucket_id} = bucket, bucket_id), do: bucket

  defp fresh_bucket(_bucket, bucket_id) do
    %{id: bucket_id, count: 0, sum: 0, min: nil, max: nil, last: nil}
  end

  defp min_value(nil, value), do: value
  defp min_value(value, nil), do: value
  defp min_value(left, right), do: min(left, right)

  defp max_value(nil, value), do: value
  defp max_value(value, nil), do: value
  defp max_value(left, right), do: max(left, right)

  defp to_ms(nil), do: nil

  defp to_ms(value_ns) do
    value_ns
    |> Kernel./(1_000_000)
    |> Float.round(3)
  end
end
