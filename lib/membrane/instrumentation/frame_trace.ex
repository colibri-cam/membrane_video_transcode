defmodule Membrane.Instrumentation.FrameTrace do
  @moduledoc """
  Lightweight per-frame trace used to correlate latency stages.
  """

  alias Membrane.Buffer
  alias Membrane.Instrumentation.TraceToken

  @metadata_key :drm_trace

  @enforce_keys [:trace_id, :frame_id, :created_at_ns, :sampled?]
  defstruct [
    :trace_id,
    :frame_id,
    :created_at_ns,
    :sampled?,
    :pts,
    marks: %{},
    annotations: %{}
  ]

  @type t :: %__MODULE__{
          trace_id: integer(),
          frame_id: integer(),
          created_at_ns: integer(),
          sampled?: boolean(),
          pts: Membrane.Time.t() | nil,
          marks: %{optional(atom()) => integer()},
          annotations: %{optional(atom()) => map()}
        }

  @spec new(keyword()) :: t()
  def new(opts \\ []) do
    trace_id = Keyword.get_lazy(opts, :trace_id, &unique_id/0)
    frame_id = Keyword.get_lazy(opts, :frame_id, &unique_id/0)

    created_at_ns =
      Keyword.get_lazy(opts, :created_at_ns, fn -> System.monotonic_time(:nanosecond) end)

    %__MODULE__{
      trace_id: trace_id,
      frame_id: frame_id,
      created_at_ns: created_at_ns,
      sampled?: Keyword.get(opts, :sampled?, true),
      pts: Keyword.get(opts, :pts),
      marks: %{},
      annotations: %{}
    }
  end

  @spec derive(t() | nil, keyword()) :: t()
  def derive(parent, opts \\ [])

  def derive(nil, opts), do: new(opts)

  def derive(%__MODULE__{} = parent, opts) do
    new(
      opts
      |> Keyword.put_new(:trace_id, parent.trace_id)
      |> Keyword.put_new(:created_at_ns, parent.created_at_ns)
      |> Keyword.put_new(:sampled?, parent.sampled?)
    )
  end

  @spec mark(t(), atom(), map()) :: t()
  def mark(%__MODULE__{} = trace, stage, attrs \\ %{}) when is_atom(stage) do
    at_ns = System.monotonic_time(:nanosecond)

    %__MODULE__{
      trace
      | marks: Map.put(trace.marks, stage, at_ns),
        annotations: update_annotations(trace.annotations, stage, attrs)
    }
  end

  @spec age_ns(t()) :: non_neg_integer()
  def age_ns(%__MODULE__{created_at_ns: created_at_ns}) do
    System.monotonic_time(:nanosecond) - created_at_ns
  end

  @spec token(t()) :: TraceToken.t()
  def token(%__MODULE__{} = trace) do
    %TraceToken{
      trace_id: trace.trace_id,
      frame_id: trace.frame_id,
      created_at_ns: trace.created_at_ns,
      sampled: trace.sampled?,
      pts: trace.pts
    }
  end

  @spec fetch(Buffer.t()) :: t() | nil
  def fetch(%Buffer{metadata: metadata}) when is_map(metadata),
    do: Map.get(metadata, @metadata_key)

  def fetch(_buffer), do: nil

  @spec put(Buffer.t(), t()) :: Buffer.t()
  def put(%Buffer{} = buffer, %__MODULE__{} = trace) do
    %{buffer | metadata: Map.put(buffer.metadata || %{}, @metadata_key, trace)}
  end

  @spec metadata_key() :: atom()
  def metadata_key, do: @metadata_key

  defp unique_id do
    System.unique_integer([:positive, :monotonic])
  end

  defp update_annotations(annotations, _stage, attrs) when attrs == %{}, do: annotations

  defp update_annotations(annotations, stage, attrs) do
    Map.put(annotations, stage, attrs)
  end
end
