defmodule Membrane.DRM.Prime do
  @moduledoc """
  Descriptor for video frames shared using the DRM Prime mechanism.

  The struct carries all information needed to import a frame into the DRM
  subsystem without copying the data. It is intended to be attached to
  `Membrane.Buffer` metadata under the `:drm_prime` key.
  """

  @enforce_keys [:fd, :width, :height, :pixel_format, :pitches, :offsets]
  defstruct [:fd, :width, :height, :pixel_format, :pitches, :offsets]
end
