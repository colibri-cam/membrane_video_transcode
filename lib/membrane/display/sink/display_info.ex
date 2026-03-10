defmodule Membrane.Display.Sink.DisplayInfo do
  @moduledoc """
  Information about DRM resources selected by the sink.
  """

  @enforce_keys [:card_path, :connector_id, :connector_type, :crtc_id, :plane_id, :mode]
  defstruct [:card_path, :connector_id, :connector_type, :crtc_id, :plane_id, :mode]
end
