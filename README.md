# ecdysis

[![Crates.io](https://img.shields.io/crates/v/ecdysis.svg)](https://crates.io/crates/ecdysis) [![Documentation](https://docs.rs/ecdysis/badge.svg)](https://docs.rs/ecdysis) [![License](https://img.shields.io/crates/l/ecdysis.svg)](https://github.com/cloudflare/ecdysis/blob/main/LICENSE) [![Rust 2021](https://img.shields.io/badge/rust-2021-blue.svg)](https://www.rust-lang.org)

_Graceful restarts in Rust_

---

> ecdysis | _ˈekdəsəs_ |
>
> **noun**
> 
> &emsp;&emsp;the process of shedding the old skin (in reptiles) or casting off the outer cuticle (in insects and other arthropods).

---

The `ecdysis` library is based on the [go library called tableflip](https://github.com/cloudflare/tableflip).

It is sometimes useful to update the running code and / or configuration of a network service, without disrupting existing connections. Usually, this is achieved by starting a new process, somehow transferring clients to it and then exiting the old process.

There are [many ways to implement graceful upgrades](https://blog.cloudflare.com/graceful-upgrades-in-go/). They vary wildly in the trade-offs they make, and how much control they afford the user. This library has the following goals:

* No old code keeps running after a successful upgrade
* The new process has a grace period for performing initialisation
* Crashing during initialisation is OK
* Only a single upgrade is ever run in parallel

## Features

`ecdysis` provides an extension for working with [Tokio streams](https://docs.rs/tokio-stream/latest/tokio_stream/) in a reactor environment when the feature `tokio_ecdysis` is enabled (enabled by default).

Two features are provided for integrating `ecdysis` with `systemd`. `systemd_notify` enables service status notifications; `systemd_sockets` enables support for `systemd` named sockets. A compound `systemd` feature to enable both the `systemd_notify` and `systemd_sockets` features at once is also provided.

### `systemd-notify` support

Using the `systemd_notify` crate feature enables automatic compatibility and integration with `systemd`'s process status notification capabilities [^1] (this is disabled by default). To use this feature, the following requirements must be met:

1. Using the `systemd_notify` crate feature also enables the `tokio_ecdysis` feature and requires using [tokio](https://crates.io/crates/tokio).
2. `systemd-notify` integration only works with `systemd` version `>= v253`.
3. When using systemd integration,  `Type=notify-reload` _must_ be set in the applications service unit configuration file.

Failing to meet these requirements will cause `ecdysis` to fail to initialize.

[^1]: See the following _manpages_ for context on `systemd-notify`: [`systemd.service`](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html), [`systemd-notify`](https://www.freedesktop.org/software/systemd/man/latest/systemd-notify.html), and [`sd_notify`](https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html).

### `systemd` named socket support

Using the `systemd_sockets` crate feature enables `ecdysis` to manage and persist `systemd`-spawned named sockets[^2] (this is disabled by default). To use this feature, the following requirements must be met:

1. As for the `systemd_notify` feature, using the `systemd_sockets` feature enables the `tokio_ecdysis` feature and requires using [tokio](https://crates.io/crates/tokio).
2. `ecdysis` only supports _named_ `systemd` sockets. Anonymous file descriptors, and file descriptors with duplicate or special names (documented in `sd_listen_fds`[^2]) will be ignored.

Failing to meet these requirements will cause `ecdysis` to fail to initialize.

[^2]: See the following _manpages_ for context on `systemd` named sockets: [`systemd.socket`](https://www.freedesktop.org/software/systemd/man/latest/systemd.socket.html), and [`sd_listen_fds`](https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html).


## Limitations

`ecdysis` does not work on Microsoft Windows.

This code has only been properly tested on Linux (more specifically, Debian Bullseye, Bookworm, and Trixie), but may also work on other UNIX-like or POSIX-compliant OS's such as Mac OS X or FreeBSD.

# How ecdysis compares to [cloudflare/shellflip](https://github.com/cloudflare/shellflip)

`ecdysis` and `shellflip` are both crates intended to facilitate graceful process reloads, but they cater to slightly different needs. Both support the [Tokio runtime](https://tokio.rs/) and `systemd-notify`, enabling seamless integration with modern process management tools. However, their approaches to handling process reloads differ significantly. `shellflip` is opinionated, assuming that the service is running under systemd and built with [tokio](https://crates.io/crates/tokio).
`shellflip` also focuses on enabling the transfer of arbitrary data between parent and child, and provides utilities and abstractions to simplify this process.

In contrast, `ecdysis` specializes in socket inheritance and rebinding, making it ideal for scenarios where passing network connections from parent to child is crucial. While both crates support arbitrary data sharing, `ecdysis` requires users to handle slightly more complexity, such as manually serializing the data via a named pipe.
`ecdysis` is less opinionated; it does not assume `systemd` by default and supports a synchronous, [tokio](https://crates.io/crates/tokio)-less mode for use cases that do not require asynchronous processing.

In summary, `shellflip` is a more user-friendly solution for arbitrary data sharing in `systemd` environments, while `ecdysis` shines when socket inheritance and rebinding are prioritized.

# Contributing

Contributing to `ecdysis` is welcome and encouraged. We appreciate contributions in various forms, including bug reports, feature requests, and pull requests. To ensure that contributions are useful and effective, we have established guidelines that contributors should follow.

Code contributions should adhere to good development practices, including good code organization and proper documentation. Code contributions should be written in idiomatic Rust, and be accompanied by thorough tests to ensure their quality and reliability. Additionally, code contributions must not contain any proprietary code, data, or intellectual property. All contributions, whether bug reports, feature requests, or code contributions, should be made under the terms of the project's [license](#license).

Contributions will be reviewed and addressed on a best-effort basis. The maintainers of this project reserve the right to prioritize, reject, defer, or modify any contribution submitted for inclusion. They also reserve the right to request changes to code, documentation, or other content in contributions. Changes and fixes that relate to the operation and improvement of Cloudflare's own systems and services will be given priority over other contributions. This ensures that the project remains aligned with the needs of its primary stakeholders and users. 

# License

Copyright 2025 (c) Cloudflare, Inc.

Licensed under the Apache License, Version 2.0 (the "License"); you may not use this file except in compliance with the License. A copy of the license is included in the [LICENSE](./LICENSE) file and online at

> <http://www.apache.org/licenses/LICENSE-2.0>

Unless required by applicable law or agreed to in writing, software distributed under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the License for the specific language governing permissions and limitations under the License.
