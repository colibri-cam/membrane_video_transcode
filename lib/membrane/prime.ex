defmodule Membrane.PrimeFormat do
  @moduledoc """
  Membrane format for Prime descriptors
  """

  @enforce_keys [:width, :height, :framerate]
  defstruct [:width, :height, :framerate]
end

defmodule Membrane.PrimeDesc do
  @moduledoc """
  Descriptor for video frames shared using the DRM Prime mechanism.

  The struct carries all information needed to import a frame into the DRM
  subsystem without copying the data. It is intended to be attached to
  `Membrane.Buffer` metadata under the `:drm_prime` key.
  """

  @enforce_keys [:planes, :objects, :width, :height, :format, :keepalive]
  defstruct [:planes, :objects, :width, :height, :format, :keepalive, :owner_pid, :trace_token]
end

defmodule Membrane.PrimeObject do
  @moduledoc false

  @enforce_keys [:fd]
  defstruct [:fd, :modifier]
end

defmodule Membrane.PrimePlane do
  @moduledoc false

  @enforce_keys [:obj_idx, :pitch, :offset]
  defstruct [:obj_idx, :pitch, :offset]
end
