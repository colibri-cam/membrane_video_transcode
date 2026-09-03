defmodule Membrane.VideoTranscode.Decoder.Common do
  @moduledoc false

  @codec_time_base 90_000
  @no_pts -9_223_372_036_854_775_808

  @doc """
  Converts time in Membrane time base (1 ns) to the codec time base (1/90,000 s).
  """
  @spec to_codec_time_base_truncated(Membrane.Time.t() | nil) :: integer
  def to_codec_time_base_truncated(nil), do: @no_pts

  def to_codec_time_base_truncated(timestamp) do
    (timestamp * @codec_time_base)
    |> div(Membrane.Time.second())
  end

  @doc """
  Converts time from the codec time base (1/90,000 s) to Membrane time base (1 ns).
  """
  @spec to_membrane_time_base_truncated(integer) :: Membrane.Time.t() | nil
  def to_membrane_time_base_truncated(@no_pts), do: nil

  def to_membrane_time_base_truncated(timestamp) do
    (timestamp * Membrane.Time.second())
    |> div(@codec_time_base)
  end
end
