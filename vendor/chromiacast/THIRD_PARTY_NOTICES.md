# Third-party notices

## Open Screen

The Cast protocol implementation was informed by Open Screen's published
protocol documentation and reference implementation. The Cast root
certificates in `src/control/cast_root_ca.pem`,
`src/control/eureka_root_ca.pem`, and
`src/control/cast_crl_root_ca.pem` were sourced from Open Screen.

Open Screen is distributed under the following license:

```text
Copyright 2018 The Chromium Authors

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
   * Neither the name of Google LLC nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

Source: <https://chromium.googlesource.com/openscreen/>

## Android TV protocol references

The optional Android TV implementation is original Rust code informed by the
following protocol sources. Chromiacast does not embed their generated code,
runtime, or assets; it defines the minimal protobuf-compatible field surface it
uses.

- Google's AOSP Google TV Pairing Protocol, revision
  `c731915e80d9e2ccb755b97e7fdd280bbea07f70`, supplied the Polo message schema,
  pairing state machine, and certificate-derived challenge/response algorithm.
  It is licensed under Apache-2.0.
  Source: <https://android.googlesource.com/platform/external/google-tv-pairing-protocol>
- `tronikos/androidtvremote2`, revision
  `e67a1e10335ac0a7e502341f96a82580eed191ab`, supplied an interoperable Remote
  Service v2 protobuf description and behavior reference. It is licensed under
  Apache-2.0. Source: <https://github.com/tronikos/androidtvremote2>
- `JaneAdora/clicker`, revision
  `aaeeebfe331d4bc5baf1018e4fce07c131352b4e`, and `drosoCode/atvremote`,
  revision `967d9ff8e74cae4b5cb149696f7a05ceb0a128c4`, were independent
  interoperability cross-checks for the Remote v2 handshake, feature mask,
  framing, and key injection. Clicker is licensed under MIT OR Apache-2.0;
  atvremote is licensed under MIT.
  Sources: <https://github.com/JaneAdora/clicker> and
  <https://github.com/drosoCode/atvremote>
