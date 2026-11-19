 This repository should build an OCI image

 - image should be based on the OpenShell gateway image
 - image should start a gateway admission hook first on port 12777
 - the admission hook should check sandbox creation and update mount path (for docker and podman) and convert windows path to machine path
 - the admission hook should be Rust based
 - the image shoud first start the admission hook then the gateway

 Ask question if you need to
