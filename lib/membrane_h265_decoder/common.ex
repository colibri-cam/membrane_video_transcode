defmodule Membrane.H265Decoder.Common do
  @moduledoc false

  @h265_time_base 90_000
  @no_pts -9_223_372_036_854_775_808

  @doc """
  Converts time in membrane time base (1 [ns]) to h265 time base (1/90_000 [s])
  """
  @spec to_h265_time_base_truncated(Membrane.Time.t() | nil) :: integer
  def to_h265_time_base_truncated(nil), do: @no_pts

  def to_h265_time_base_truncated(timestamp) do
    (timestamp * @h265_time_base)
    |> div(Membrane.Time.second())
  end

  @doc """
  Converts time from h265 time base (1/90_000 [s]) to membrane time base (1 [ns])
  """
  @spec to_membrane_time_base_truncated(integer) :: Membrane.Time.t() | nil
  def to_membrane_time_base_truncated(@no_pts), do: nil

  def to_membrane_time_base_truncated(timestamp) do
    (timestamp * Membrane.Time.second())
    |> div(@h265_time_base)
  end
end
