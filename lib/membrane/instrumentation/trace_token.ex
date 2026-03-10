defmodule Membrane.Instrumentation.TraceToken do
  @moduledoc false

  @enforce_keys [:trace_id, :frame_id, :created_at_ns, :sampled]
  defstruct [:trace_id, :frame_id, :created_at_ns, :sampled, :pts]

  @type t :: %__MODULE__{
          trace_id: integer(),
          frame_id: integer(),
          created_at_ns: integer(),
          sampled: boolean(),
          pts: Membrane.Time.t() | nil
        }
end
