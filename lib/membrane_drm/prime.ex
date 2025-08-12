defmodule Membrane.DRM.PrimeFormat do
  @moduledoc """
  Descriptor for video frames shared using the DRM Prime mechanism.

  The struct carries all information needed to import a frame into the DRM
  subsystem without copying the data. It is intended to be attached to
  `Membrane.Buffer` metadata under the `:drm_prime` key.
  """

  @enforce_keys [:width, :height, :framerate]
  defstruct [:width, :height, :framerate]
end

defmodule Membrane.DRM.Prime do
  @moduledoc """
  Descriptor for video frames shared using the DRM Prime mechanism.

  The struct carries all information needed to import a frame into the DRM
  subsystem without copying the data. It is intended to be attached to
  `Membrane.Buffer` metadata under the `:drm_prime` key.
  """

  @enforce_keys [:planes, :width, :height, :format]
  defstruct [:planes, :width, :height, :format]
end

defmodule Membrane.DRM.PrimePlane do
  @moduledoc false

  @enforce_keys [:fd, :pitch, :offset]
  defstruct [:fd, :pitch, :offset, :modifier]
end
