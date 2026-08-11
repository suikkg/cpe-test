# Third-Party Notices

This project can distribute the third-party component below in its Windows
release archive. The component remains the property of its respective owner
and is provided under its own license. Its inclusion does not change the MIT
license that applies to cpe_test itself.

## Microsoft ctsTraffic

- Component: `ctsTraffic.exe` x64
- Version: 2.0.4.0
- Copyright: Microsoft Corporation
- License: Apache License 2.0
- Source repository: <https://github.com/microsoft/ctsTraffic>
- Pinned source commit: `4468b8085b6ac58c531596216f10e812287174ea`
- Upstream binary path: `Releases/2.0.4.0/x64/ctsTraffic.exe`
- Expected SHA-256: `9ac0f6a19da355343133f658c6e5d0dc40919f9a13020f86621194c31ce20b12`

The release workflow downloads the binary from the immutable commit above,
verifies the SHA-256 before packaging, and places the upstream Apache-2.0
license in the Windows archive as `LICENSE-ctsTraffic-Apache-2.0.txt`.

## Windows Implementation Library (WIL)

ctsTraffic uses Microsoft's Windows Implementation Library (WIL), which is
licensed under the MIT License. The Windows release archive includes the WIL
license as `LICENSE-WIL-MIT.txt`. The WIL source repository is available at
<https://github.com/microsoft/wil>.

No Microsoft trademark rights are granted by these notices, and Microsoft
does not endorse this project.
