# Containers — roadmap only (not implemented, Docker not required)

A future `ContainerManager` adapter, a **separate privileged subsystem** disabled in v1: detect an existing Docker installation, list/start/stop
app-managed containers, show logs, apply CPU/RAM limits, mount only explicitly selected workspace folders. Before building it, verify: current
Docker Desktop licensing for the deployment, WSL2/Hyper-V backend requirements, Linux-vs-Windows container support, GPU passthrough limits.
Rules: never install Docker or accept its terms for the user, never reconfigure virtualization, never expose the daemon socket/named pipe to
the internet or to inference clients, require approval for host mounts, published ports, privileged mode and destructive actions, and do not
treat a container as a sandbox for model-generated commands. No container UI or code exists in this repository.
