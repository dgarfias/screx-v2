# Pairing primitive

`monocypher/` is the unchanged Monocypher 4.0.2 release source from
https://github.com/LoupVaillant/Monocypher/tree/4.0.2/src.
Its dual BSD-2-Clause/CC0 license is included.

Upstream source SHA-256:

* `monocypher.c`: `02174117935699d418443c75a558a287deb06ef8cf7c1adced61d9047d2f323d`
* `monocypher.h`: `fcaf6ed771358bb4f40fba016f6518ae86ec02b1b877d2cc35ad92d3a26fd7b3`

The adapter in `cpace_primitives.c` implements the CPace draft-21
X25519 Elligator2 map using Monocypher's internal constant-time arithmetic.
Unlike Monocypher's public hiding API, CPace consumes 255 input bits, not 254.
The adapter changes only that mask. Both platforms compile the same adapter.
Published CPace vectors must pass before this is used for pairing.

This integration has not received an independent security audit. App Store
encryption export compliance must include this bundled authentication primitive;
do not assume use of Apple's TLS makes all application cryptography exempt.
